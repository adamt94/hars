//! User-friendly plugin installer and built-in `hars_plugins` config flow
//! ("Hars Plugins" store inside Home Assistant's **Add integration** dialog).
//!
//! Supports three installation sources:
//! 1. **Curated catalog** (`plugins.json`): picks the matching OS/arch artifact
//!    automatically and verifies its SHA-256 checksum.
//! 2. **Direct bundle URL**: downloads a plugin bundle descriptor (`bundle.json`)
//!    with optional expected SHA-256 verification.
//! 3. **Local directory** (CLI): validates `plugin.json` and copies atomically
//!    into `<config>/plugins/<domain>/`.

use super::PluginManager;
use super::manifest::{
    InstalledPlugin, PluginManifest, Transport, is_reserved_domain, valid_domain,
};
use crate::flow::{Cx, Field, Flow, Kind, Step, StepFuture};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Built-in domain for the in-UI Plugin Store config flow.
pub const STORE_DOMAIN: &str = "hars_plugins";

/// Default curated plugin catalog URL (overridable via `HARS_PLUGIN_CATALOG`
/// or in tests).
pub const DEFAULT_CATALOG_URL: &str =
    "https://raw.githubusercontent.com/adamt94/hars/main/plugins.json";

/// Maximum download size for a single plugin binary (32 MiB).
const MAX_ARTIFACT_BYTES: usize = 32 * 1024 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Curated catalog (`plugins.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub schema_version: u32,
    #[serde(default)]
    pub plugins: Vec<CatalogPlugin>,
}

/// One installable plugin in a [`Catalog`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogPlugin {
    pub domain: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub version: String,
    /// Map of `<os>-<arch>` (e.g. `linux-x86_64`, `linux-aarch64`,
    /// `darwin-aarch64`) to its binary artifact.
    #[serde(default)]
    pub artifacts: HashMap<String, Artifact>,
    /// URL of `plugin.json` (or inline manifest).
    #[serde(default)]
    pub manifest: Option<PluginManifest>,
    #[serde(default)]
    pub manifest_url: Option<String>,
    /// Optional `translations/en.json` URL.
    #[serde(default)]
    pub translations_en_url: Option<String>,
    /// Optional `icons.json` URL.
    #[serde(default)]
    pub icons_url: Option<String>,
    /// Optional `icon.png` URL.
    #[serde(default)]
    pub icon_png_url: Option<String>,
}

/// Downloadable binary artifact pinned by SHA-256.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub url: String,
    pub sha256: String,
}

/// Errors during plugin installation or removal.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("invalid catalog or bundle JSON: {0}")]
    Json(String),
    #[error("plugin {0:?} not found in catalog")]
    NotFound(String),
    #[error("no artifact available for target {0:?}")]
    UnsupportedTarget(String),
    #[error("SHA-256 mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("download exceeds maximum size of {0} bytes")]
    TooLarge(usize),
    #[error("invalid plugin: {0}")]
    Manifest(#[from] super::manifest::ManifestError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Validate a remote download URL before fetching it: requires `https://` and
/// rejects loopback/private/link-local IP literals and `localhost` unless
/// `allow_insecure_http` is explicitly enabled (tests / local dev).
pub(crate) fn validate_remote_url(url: &str, allow_insecure_http: bool) -> Result<(), StoreError> {
    let rest = if let Some(r) = url.strip_prefix("https://") {
        r
    } else if allow_insecure_http && let Some(r) = url.strip_prefix("http://") {
        r
    } else {
        return Err(StoreError::InvalidUrl(format!(
            "only https:// URLs are allowed (got {url:?})"
        )));
    };
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(StoreError::InvalidUrl(format!(
            "invalid authority in {url:?}"
        )));
    }
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    if host.is_empty() {
        return Err(StoreError::InvalidUrl(format!("missing host in {url:?}")));
    }
    if !allow_insecure_http {
        let lower = host.to_ascii_lowercase();
        if lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".local") {
            return Err(StoreError::InvalidUrl(format!(
                "refusing local host {host:?} in plugin URL"
            )));
        }
        if let Ok(ip) = host.parse::<IpAddr>()
            && is_private_or_local_ip(ip)
        {
            return Err(StoreError::InvalidUrl(format!(
                "refusing private or loopback IP {ip} in plugin URL"
            )));
        }
    }
    Ok(())
}

fn is_private_or_local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 64)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Host target key used in catalog `artifacts` maps (`linux-x86_64`,
/// `linux-aarch64`, `darwin-aarch64`, `darwin-x86_64`, ...).
pub fn current_target() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

/// Compute lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{b:02x}");
    }
    hex
}

fn http_client(allow_insecure_http: bool) -> Result<ha_http::Client, StoreError> {
    let mut b = ha_http::Client::builder(crate::USER_AGENT);
    if !allow_insecure_http {
        b = b.public_https_only();
    }
    b.build()
        .map(|c| c.with_timeouts(HTTP_TIMEOUT, HTTP_TIMEOUT))
        .map_err(|e| StoreError::Http(e.to_string()))
}

async fn fetch_bytes(
    client: &ha_http::Client,
    url: &str,
    allow_insecure_http: bool,
) -> Result<Vec<u8>, StoreError> {
    validate_remote_url(url, allow_insecure_http)?;
    let res = client.get(url, &[]).await.map_err(|e| match e {
        ha_http::Error::TooLarge(n) => StoreError::TooLarge(n),
        other => StoreError::Http(format!("{url}: {other}")),
    })?;
    if !res.status.is_success() {
        return Err(StoreError::Http(format!("{url}: HTTP {}", res.status)));
    }
    Ok(res.body.to_vec())
}

/// Fetch and parse a curated catalog JSON from `url`.
pub async fn fetch_catalog(url: &str) -> Result<Catalog, StoreError> {
    fetch_catalog_with_policy(url, env_allow_insecure_http()).await
}

pub(crate) async fn fetch_catalog_with_policy(
    url: &str,
    allow_insecure_http: bool,
) -> Result<Catalog, StoreError> {
    let client = http_client(allow_insecure_http)?;
    let bytes = fetch_bytes(&client, url, allow_insecure_http).await?;
    let catalog: Catalog =
        serde_json::from_slice(&bytes).map_err(|e| StoreError::Json(e.to_string()))?;
    if catalog.schema_version != 1 {
        return Err(StoreError::Json(format!(
            "unsupported catalog schema_version {}",
            catalog.schema_version
        )));
    }
    Ok(catalog)
}

/// Install a plugin from a local directory into `plugins_dir/<domain>/`
/// atomically, validating `plugin.json` and the staged copy before swapping.
pub fn install_from_dir(source: &Path, plugins_dir: &Path) -> Result<InstalledPlugin, StoreError> {
    let validated = InstalledPlugin::load(source)?;
    let domain = validated.domain().to_owned();

    std::fs::create_dir_all(plugins_dir)?;
    let staging_parent = plugins_dir.join(format!(".staging-{domain}-{:x}", rand::random::<u64>()));
    let staging = staging_parent.join(&domain);
    let stage_result = (|| -> Result<(), StoreError> {
        copy_dir_recursive(source, &staging)?;
        ensure_stdio_executable(&staging, &validated.manifest)?;
        // Validate the staged directory BEFORE touching any existing install.
        let _ = InstalledPlugin::load(&staging)?;
        Ok(())
    })();
    if let Err(e) = stage_result {
        let _ = std::fs::remove_dir_all(&staging_parent);
        return Err(e);
    }
    let dest = plugins_dir.join(&domain);
    let swap = swap_into_place(&staging, &dest);
    let _ = std::fs::remove_dir_all(&staging_parent);
    swap?;

    Ok(InstalledPlugin::load(&dest)?)
}

/// Install a plugin described by a [`CatalogPlugin`] entry (either from a
/// curated catalog or fetched directly as a single bundle JSON URL).
pub async fn install_catalog_plugin(
    entry: &CatalogPlugin,
    plugins_dir: &Path,
    target: &str,
) -> Result<InstalledPlugin, StoreError> {
    install_catalog_plugin_with_policy(entry, plugins_dir, target, env_allow_insecure_http()).await
}

pub(crate) struct FetchedCatalogPlugin {
    pub(crate) manifest: PluginManifest,
    binary_bytes: Option<Vec<u8>>,
    translations_en: Option<Vec<u8>>,
    icons: Option<Vec<u8>>,
    icon_png: Option<Vec<u8>>,
}

pub(crate) async fn fetch_catalog_plugin_with_policy(
    entry: &CatalogPlugin,
    target: &str,
    allow_insecure_http: bool,
) -> Result<FetchedCatalogPlugin, StoreError> {
    let client = http_client(allow_insecure_http)?;
    let manifest: PluginManifest = if let Some(m) = &entry.manifest {
        m.clone()
    } else if let Some(url) = &entry.manifest_url {
        let bytes = fetch_bytes(&client, url, allow_insecure_http).await?;
        serde_json::from_slice(&bytes).map_err(|e| StoreError::Json(e.to_string()))?
    } else {
        return Err(StoreError::Json(
            "catalog entry must include `manifest` or `manifest_url`".into(),
        ));
    };
    manifest.validate(Some(&entry.domain))?;

    let binary_bytes = match &manifest.transport {
        Transport::Stdio { .. } => {
            let artifact = entry
                .artifacts
                .get(target)
                .ok_or_else(|| StoreError::UnsupportedTarget(target.to_owned()))?;
            let artifact_client = client.clone().with_max_body(MAX_ARTIFACT_BYTES);
            let bytes = fetch_bytes(&artifact_client, &artifact.url, allow_insecure_http).await?;
            let actual = sha256_hex(&bytes);
            if !actual.eq_ignore_ascii_case(&artifact.sha256) {
                return Err(StoreError::ChecksumMismatch {
                    expected: artifact.sha256.clone(),
                    actual,
                });
            }
            Some(bytes)
        }
        Transport::UnixSocket { .. } => None,
    };

    let translations_en = match &entry.translations_en_url {
        Some(url) => Some(fetch_bytes(&client, url, allow_insecure_http).await?),
        None => None,
    };
    let icons = match &entry.icons_url {
        Some(url) => Some(fetch_bytes(&client, url, allow_insecure_http).await?),
        None => None,
    };
    let icon_png = match &entry.icon_png_url {
        Some(url) => Some(fetch_bytes(&client, url, allow_insecure_http).await?),
        None => None,
    };

    Ok(FetchedCatalogPlugin {
        manifest,
        binary_bytes,
        translations_en,
        icons,
        icon_png,
    })
}

pub(crate) async fn commit_fetched_plugin(
    fetched: FetchedCatalogPlugin,
    plugins_dir: &Path,
) -> Result<InstalledPlugin, StoreError> {
    let FetchedCatalogPlugin {
        manifest,
        binary_bytes,
        translations_en,
        icons,
        icon_png,
    } = fetched;
    let plugins_dir = plugins_dir.to_path_buf();
    let domain = manifest.domain.clone();
    tokio::task::spawn_blocking(move || -> Result<InstalledPlugin, StoreError> {
        std::fs::create_dir_all(&plugins_dir)?;
        let staging_parent =
            plugins_dir.join(format!(".staging-{domain}-{:x}", rand::random::<u64>()));
        let staging = staging_parent.join(&domain);
        std::fs::create_dir_all(&staging)?;
        let write_result = (|| -> Result<(), StoreError> {
            std::fs::write(
                staging.join("plugin.json"),
                serde_json::to_vec_pretty(&manifest)
                    .map_err(|e| StoreError::Json(e.to_string()))?,
            )?;
            if let (Transport::Stdio { command, .. }, Some(bytes)) =
                (&manifest.transport, binary_bytes)
            {
                let bin_path = staging.join(command);
                if let Some(parent) = bin_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&bin_path, bytes)?;
                set_executable(&bin_path)?;
            }
            if let Some(bytes) = translations_en {
                std::fs::create_dir_all(staging.join("translations"))?;
                std::fs::write(staging.join("translations/en.json"), bytes)?;
            }
            if let Some(bytes) = icons {
                std::fs::write(staging.join("icons.json"), bytes)?;
            }
            if let Some(bytes) = icon_png {
                std::fs::write(staging.join("icon.png"), bytes)?;
            }
            // Validate the complete staged plugin BEFORE swapping over any
            // existing working installation.
            let _ = InstalledPlugin::load(&staging)?;
            Ok(())
        })();
        if let Err(e) = write_result {
            let _ = std::fs::remove_dir_all(&staging_parent);
            return Err(e);
        }
        let dest = plugins_dir.join(&domain);
        let swap = swap_into_place(&staging, &dest);
        let _ = std::fs::remove_dir_all(&staging_parent);
        swap?;
        Ok(InstalledPlugin::load(&dest)?)
    })
    .await
    .map_err(|e| StoreError::Io(std::io::Error::other(e)))?
}

pub(crate) async fn install_catalog_plugin_with_policy(
    entry: &CatalogPlugin,
    plugins_dir: &Path,
    target: &str,
    allow_insecure_http: bool,
) -> Result<InstalledPlugin, StoreError> {
    let fetched = fetch_catalog_plugin_with_policy(entry, target, allow_insecure_http).await?;
    commit_fetched_plugin(fetched, plugins_dir).await
}

/// Install a plugin from a direct bundle JSON URL (single [`CatalogPlugin`]
/// JSON document).
pub async fn install_from_bundle_url(
    url: &str,
    plugins_dir: &Path,
    target: &str,
) -> Result<InstalledPlugin, StoreError> {
    install_from_bundle_url_with_policy(url, plugins_dir, target, env_allow_insecure_http()).await
}

pub(crate) async fn fetch_bundle_with_policy(
    url: &str,
    allow_insecure_http: bool,
) -> Result<CatalogPlugin, StoreError> {
    let client = http_client(allow_insecure_http)?;
    let bytes = fetch_bytes(&client, url, allow_insecure_http).await?;
    serde_json::from_slice(&bytes).map_err(|e| StoreError::Json(e.to_string()))
}

pub(crate) async fn install_from_bundle_url_with_policy(
    url: &str,
    plugins_dir: &Path,
    target: &str,
    allow_insecure_http: bool,
) -> Result<InstalledPlugin, StoreError> {
    let entry = fetch_bundle_with_policy(url, allow_insecure_http).await?;
    install_catalog_plugin_with_policy(&entry, plugins_dir, target, allow_insecure_http).await
}

pub(crate) fn env_allow_insecure_http() -> bool {
    std::env::var_os("HARS_PLUGIN_ALLOW_INSECURE_HTTP").is_some_and(|v| v == "1")
}

/// Remove an installed plugin directory from disk. Refuses reserved domains
/// (such as `run`) and directories without a `plugin.json`.
pub fn remove_from_dir(plugins_dir: &Path, domain: &str) -> Result<bool, StoreError> {
    if !valid_domain(domain) || is_reserved_domain(domain) {
        return Err(StoreError::NotFound(domain.to_owned()));
    }
    let dir = plugins_dir.join(domain);
    if !dir.join("plugin.json").is_file() {
        return Ok(false);
    }
    std::fs::remove_dir_all(dir)?;
    Ok(true)
}

fn swap_into_place(staging: &Path, dest: &Path) -> Result<(), std::io::Error> {
    let backup = dest.with_extension(format!("old-{:x}", rand::random::<u64>()));
    let had_old = dest.exists();
    if had_old {
        std::fs::rename(dest, &backup)?;
    }
    match std::fs::rename(staging, dest) {
        Ok(()) => {
            if had_old {
                let _ = std::fs::remove_dir_all(&backup);
            }
            Ok(())
        }
        Err(e) => {
            if had_old {
                let _ = std::fs::rename(&backup, dest);
            }
            let _ = std::fs::remove_dir_all(staging);
            Err(e)
        }
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &to)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

fn ensure_stdio_executable(dir: &Path, manifest: &PluginManifest) -> Result<(), StoreError> {
    if let Transport::Stdio { command, .. } = &manifest.transport {
        let path = dir.join(command);
        let meta = std::fs::symlink_metadata(&path).map_err(|_| {
            StoreError::Manifest(super::manifest::ManifestError::Invalid(format!(
                "missing stdio executable {command:?}"
            )))
        })?;
        if !meta.file_type().is_file() {
            return Err(StoreError::Manifest(
                super::manifest::ManifestError::Invalid(format!(
                    "stdio executable {command:?} is not a regular file"
                )),
            ));
        }
        set_executable(&path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Built-in `hars_plugins` config flow (in-UI Plugin Store)
// ---------------------------------------------------------------------------

/// Manifest JSON for the built-in `hars_plugins` integration.
pub fn store_manifest_json() -> String {
    json!({
        "domain": STORE_DOMAIN,
        "name": "Hars Plugins",
        "version": env!("CARGO_PKG_VERSION"),
        "config_flow": true,
        "integration_type": "service",
        "iot_class": "local_push",
        "is_built_in": true,
        "overwrites_built_in": false,
        "codeowners": [],
        "dependencies": [],
        "requirements": [],
    })
    .to_string()
}

/// `integration/descriptions` entry for `hars_plugins`, matching HA 2026.6.1's
/// shape so the pinned frontend opens the config flow directly on click.
pub fn store_description_json() -> String {
    json!({
        "integration_type": "service",
        "config_flow": true,
        "iot_class": "local_push",
        "name": "Hars Plugins",
    })
    .to_string()
}

/// Built-in English translations for the `hars_plugins` UI store flow.
pub fn store_translations(category: &str) -> Option<&'static str> {
    match category {
        "title" => Some(r#""component.hars_plugins.title":"Hars Plugins""#),
        "config" => Some(concat!(
            r#""component.hars_plugins.config.step.user.title":"Hars Plugin Manager","#,
            r#""component.hars_plugins.config.step.user.description":"Install or remove hars plugins without restarting.","#,
            r#""component.hars_plugins.config.step.user.data.action":"Action","#,
            r#""component.hars_plugins.config.step.install_catalog.title":"Install from plugin catalog","#,
            r#""component.hars_plugins.config.step.install_catalog.description":"Choose a plugin to download and verify for this machine.","#,
            r#""component.hars_plugins.config.step.install_catalog.data.plugin":"Plugin","#,
            r#""component.hars_plugins.config.step.install_url.title":"Install from bundle URL","#,
            r#""component.hars_plugins.config.step.install_url.description":"Enter a plugin bundle JSON URL (verified by SHA-256).","#,
            r#""component.hars_plugins.config.step.install_url.data.url":"Bundle URL","#,
            r#""component.hars_plugins.config.step.remove.title":"Remove installed plugin","#,
            r#""component.hars_plugins.config.step.remove.data.plugin":"Installed plugin","#,
            r#""component.hars_plugins.config.error.catalog_unavailable":"Could not fetch the plugin catalog. Check network access or install from a URL instead.","#,
            r#""component.hars_plugins.config.error.install_failed":"Plugin installation failed. Check the server logs for details.","#,
            r#""component.hars_plugins.config.error.invalid_url":"Enter a valid public https:// bundle URL.","#,
            r#""component.hars_plugins.config.abort.plugin_installed":"Plugin installed! Open Add integration again to set up your devices.","#,
            r#""component.hars_plugins.config.abort.plugin_removed":"Plugin removed.","#,
            r#""component.hars_plugins.config.abort.no_plugins_installed":"No plugins are currently installed.","#,
            r#""component.hars_plugins.config.abort.catalog_empty":"No plugins were found in the catalog.""#,
        )),
        _ => None,
    }
}

/// Interactive config flow for installing and removing plugins inside Home
/// Assistant's standard **Add integration** dialog.
pub(crate) struct StoreFlow {
    manager: Arc<PluginManager>,
    catalog: Option<Catalog>,
}

impl StoreFlow {
    pub fn new(manager: Arc<PluginManager>) -> Self {
        Self {
            manager,
            catalog: None,
        }
    }

    fn user_form(error: Option<&'static str>) -> Step {
        let actions = vec![
            ("install_catalog".into(), "Install from catalog".into()),
            ("install_url".into(), "Install from bundle URL".into()),
            ("remove".into(), "Remove installed plugin".into()),
        ];
        Step::form(
            "user",
            vec![Field::required("action", Kind::Select(actions))],
            error,
        )
    }

    fn catalog_form(catalog: &Catalog, error: Option<&'static str>) -> Step {
        let options: Vec<(String, String)> = catalog
            .plugins
            .iter()
            .map(|p| {
                let label = if p.description.is_empty() {
                    format!("{} ({})", p.name, p.version)
                } else {
                    format!("{} ({}) — {}", p.name, p.version, p.description)
                };
                (p.domain.clone(), label)
            })
            .collect();
        Step::form(
            "install_catalog",
            vec![Field::required("plugin", Kind::Select(options))],
            error,
        )
    }

    fn url_form(error: Option<&'static str>) -> Step {
        Step::form("install_url", vec![Field::required_str("url")], error)
    }

    fn remove_form(manager: &PluginManager) -> Step {
        let installed = manager.list();
        if installed.is_empty() {
            return Step::abort("no_plugins_installed");
        }
        let options: Vec<(String, String)> = installed
            .into_iter()
            .map(|p| (p.domain, format!("{} ({})", p.name, p.version)))
            .collect();
        Step::form(
            "remove",
            vec![Field::required("plugin", Kind::Select(options))],
            None,
        )
    }
}

impl Flow for StoreFlow {
    fn step<'a>(
        &'a mut self,
        _cx: &'a mut Cx<'_>,
        step_id: &'a str,
        input: Option<Map<String, Value>>,
    ) -> StepFuture<'a> {
        Box::pin(async move {
            match step_id {
                "user" => match input {
                    None => Self::user_form(None),
                    Some(data) => match data.get("action").and_then(Value::as_str) {
                        Some("install_catalog") => {
                            let url = self.manager.catalog_url();
                            match fetch_catalog_with_policy(
                                &url,
                                self.manager.allow_insecure_http(),
                            )
                            .await
                            {
                                Ok(cat) if cat.plugins.is_empty() => Step::abort("catalog_empty"),
                                Ok(cat) => {
                                    let form = Self::catalog_form(&cat, None);
                                    self.catalog = Some(cat);
                                    form
                                }
                                Err(e) => {
                                    tracing::warn!("fetching plugin catalog {url}: {e}");
                                    Self::user_form(Some("catalog_unavailable"))
                                }
                            }
                        }
                        Some("install_url") => Self::url_form(None),
                        Some("remove") => Self::remove_form(&self.manager),
                        _ => Self::user_form(None),
                    },
                },
                "install_catalog" => {
                    let Some(cat) = self.catalog.as_ref() else {
                        return Self::user_form(None);
                    };
                    let Some(data) = input else {
                        return Self::catalog_form(cat, None);
                    };
                    let domain = data.get("plugin").and_then(Value::as_str).unwrap_or("");
                    let Some(entry) = cat.plugins.iter().find(|p| p.domain == domain) else {
                        return Self::catalog_form(cat, Some("install_failed"));
                    };
                    match self.manager.install_from_catalog_entry(entry).await {
                        Ok(installed) => {
                            tracing::info!(
                                plugin = %installed.domain(),
                                version = %installed.manifest.version,
                                "installed plugin from catalog"
                            );
                            Step::abort("plugin_installed")
                        }
                        Err(e) => {
                            tracing::error!(plugin = %domain, "catalog install failed: {e}");
                            Self::catalog_form(cat, Some("install_failed"))
                        }
                    }
                }
                "install_url" => {
                    let Some(data) = input else {
                        return Self::url_form(None);
                    };
                    let url = data.get("url").and_then(Value::as_str).unwrap_or("").trim();
                    if validate_remote_url(url, self.manager.allow_insecure_http()).is_err() {
                        return Self::url_form(Some("invalid_url"));
                    }
                    match self.manager.install_from_url(url).await {
                        Ok(installed) => {
                            tracing::info!(
                                plugin = %installed.domain(),
                                version = %installed.manifest.version,
                                "installed plugin from URL"
                            );
                            Step::abort("plugin_installed")
                        }
                        Err(e) => {
                            tracing::error!(url = %url, "URL plugin install failed: {e}");
                            Self::url_form(Some("install_failed"))
                        }
                    }
                }
                "remove" => {
                    let Some(data) = input else {
                        return Self::remove_form(&self.manager);
                    };
                    let domain = data.get("plugin").and_then(Value::as_str).unwrap_or("");
                    match self.manager.remove_plugin(domain).await {
                        Ok(true) => Step::abort("plugin_removed"),
                        Ok(false) => Step::abort("no_plugins_installed"),
                        Err(e) => {
                            tracing::error!(plugin = %domain, "plugin remove failed: {e}");
                            Self::user_form(Some("install_failed"))
                        }
                    }
                }
                _ => Step::abort("unknown"),
            }
        })
    }
}

/// Resolve a plugin source path for CLI `install` (local folder vs URL vs
/// catalog domain name).
pub enum InstallSource {
    LocalDir(PathBuf),
    BundleUrl(String),
    CatalogDomain(String),
}

impl InstallSource {
    pub fn parse(raw: &str) -> Self {
        if raw.starts_with("http://") || raw.starts_with("https://") {
            return Self::BundleUrl(raw.to_owned());
        }
        let path = Path::new(raw);
        if path.exists() || raw.contains('/') || raw.contains('\\') || raw.starts_with('.') {
            return Self::LocalDir(path.to_path_buf());
        }
        Self::CatalogDomain(raw.to_owned())
    }
}
