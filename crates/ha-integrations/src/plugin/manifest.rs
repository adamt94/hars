//! Plugin manifest (`plugin.json`), translations, icons, and brand image
//! lookup for out-of-process `hars` plugins (`docs/adr/0011-plugin-system.md`).
//!
//! A plugin lives in `<config>/plugins/<domain>/`:
//!
//! ```text
//! <config>/plugins/<domain>/
//!   plugin.json           # manifest (schema_version 1)
//!   icon.png              # optional local brand icon (and dark_icon.png, logo.png, ...)
//!   translations/en.json  # optional HA-format translation strings
//!   icons.json            # optional HA-format entity/service icons
//!   bin/<executable>      # static executable (for `stdio` transport)
//! ```

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// Supported `plugin.json` schema version.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Entity domains a plugin may declare in `capabilities.entity_domains`.
pub const SUPPORTED_ENTITY_DOMAINS: &[&str] = &[
    "light",
    "switch",
    "button",
    "scene",
    "vacuum",
    "select",
    "number",
    "cover",
    "lock",
    "fan",
    "sensor",
    "binary_sensor",
];

/// Built-in `hars` domains that a plugin may not shadow.
const BUILTIN_DOMAINS: &[&str] = &[
    "api",
    "auth",
    "automation",
    "binary_sensor",
    "brands",
    "button",
    "climate",
    "config",
    "cover",
    "device_tracker",
    "fan",
    "frontend",
    "hars_plugins",
    "history",
    "homeassistant",
    "http",
    "hue",
    "labs",
    "lifx",
    "light",
    "lock",
    "logbook",
    "lovelace",
    "met",
    "mobile_app",
    "notify",
    "number",
    "onboarding",
    "persistent_notification",
    "person",
    "recorder",
    "repairs",
    "run",
    "scene",
    "select",
    "sensor",
    "sun",
    "switch",
    "switchbot_cloud",
    "system_log",
    "vacuum",
    "weather",
    "websocket_api",
    "zone",
];

/// Brand image filenames accepted from a plugin directory (matching HA's
/// `brands` component).
const ALLOWED_BRAND_IMAGES: &[&str] = &[
    "icon.png",
    "logo.png",
    "icon@2x.png",
    "logo@2x.png",
    "dark_icon.png",
    "dark_logo.png",
    "dark_icon@2x.png",
    "dark_logo@2x.png",
];

/// Errors encountered while loading or validating a plugin manifest.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid plugin manifest: {0}")]
    Invalid(String),
}

/// How `hars` communicates with a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Transport {
    /// `hars` spawns a local executable and exchanges NDJSON JSON-RPC over
    /// `stdin`/`stdout`.
    Stdio {
        /// Path relative to the plugin directory (no `..` or absolute paths).
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// A sidecar process (e.g. a Python/Node container) listens on or connects
    /// through a Unix domain socket.
    UnixSocket {
        /// Optional relative path under `<config>/plugins/run/`; defaults to
        /// `<domain>.sock`.
        #[serde(default)]
        path: Option<String>,
    },
}

/// Declared capabilities enforced by `hars` at runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Entity domains this plugin is permitted to register (`light`, `switch`,
    /// `button`, `scene`, `sensor`, `binary_sensor`).
    #[serde(default)]
    pub entity_domains: Vec<String>,
    /// Config entry schema version written to `core.config_entries`.
    #[serde(default = "default_entry_version")]
    pub config_entry_version: u32,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            entity_domains: Vec::new(),
            config_entry_version: default_entry_version(),
        }
    }
}

fn default_entry_version() -> u32 {
    1
}

fn default_iot_class() -> String {
    "local_polling".into()
}

fn default_true() -> bool {
    true
}

/// `plugin.json` stored inside `<config>/plugins/<domain>/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub schema_version: u32,
    pub domain: String,
    pub name: String,
    pub version: String,
    #[serde(default = "default_iot_class")]
    pub iot_class: String,
    #[serde(default = "default_true")]
    pub config_flow: bool,
    #[serde(default)]
    pub documentation: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    pub transport: Transport,
    #[serde(default)]
    pub capabilities: Capabilities,
}

impl PluginManifest {
    /// Validate every field and check that paths cannot escape `plugin_dir`.
    pub fn validate(&self, expected_dir_name: Option<&str>) -> Result<(), ManifestError> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(ManifestError::Invalid(format!(
                "unsupported schema_version {} (expected {MANIFEST_SCHEMA_VERSION})",
                self.schema_version
            )));
        }
        if !valid_domain(&self.domain) {
            return Err(ManifestError::Invalid(format!(
                "invalid domain {:?}: use lowercase ASCII letters, digits and single underscores",
                self.domain
            )));
        }
        if BUILTIN_DOMAINS.contains(&self.domain.as_str()) {
            return Err(ManifestError::Invalid(format!(
                "domain {:?} is reserved for a built-in integration",
                self.domain
            )));
        }
        if let Some(dir_name) = expected_dir_name
            && dir_name != self.domain
        {
            return Err(ManifestError::Invalid(format!(
                "plugin directory {:?} does not match manifest domain {:?}",
                dir_name, self.domain
            )));
        }
        if self.name.trim().is_empty() {
            return Err(ManifestError::Invalid("name must not be empty".into()));
        }
        if self.version.trim().is_empty() {
            return Err(ManifestError::Invalid("version must not be empty".into()));
        }
        match &self.transport {
            Transport::Stdio { command, .. } => {
                validate_relative_path(command, "transport.command")?;
                validate_command_not_metadata(command)?;
            }
            Transport::UnixSocket { path: Some(p) } => {
                validate_relative_path(p, "transport.path")?;
            }
            Transport::UnixSocket { path: None } => {}
        }
        for d in &self.capabilities.entity_domains {
            if !SUPPORTED_ENTITY_DOMAINS.contains(&d.as_str()) {
                return Err(ManifestError::Invalid(format!(
                    "unsupported entity domain {:?} (supported: {})",
                    d,
                    SUPPORTED_ENTITY_DOMAINS.join(", ")
                )));
            }
        }
        if self.capabilities.config_entry_version == 0 {
            return Err(ManifestError::Invalid(
                "capabilities.config_entry_version must be >= 1".into(),
            ));
        }
        Ok(())
    }
}

/// Home Assistant's `valid_domain`: lowercase ASCII letters, digits and single
/// underscores, not starting or ending with `_`, up to 64 bytes.
pub fn valid_domain(d: &str) -> bool {
    !d.is_empty()
        && d.len() <= 64
        && !d.starts_with('_')
        && !d.ends_with('_')
        && !d.contains("__")
        && d.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Whether `domain` is reserved for a built-in integration or runtime subdir.
pub fn is_reserved_domain(domain: &str) -> bool {
    BUILTIN_DOMAINS.contains(&domain)
}

/// Reject `transport.command` values that collide with manifest/metadata files
/// so installing a binary cannot overwrite `plugin.json` or assets.
fn validate_command_not_metadata(raw: &str) -> Result<(), ManifestError> {
    let comps: Vec<&str> = Path::new(raw)
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    let blocked = match comps.as_slice() {
        [single] => {
            *single == "plugin.json"
                || *single == "icons.json"
                || ALLOWED_BRAND_IMAGES.contains(single)
        }
        [first, ..] => *first == "translations",
        [] => true,
    };
    if blocked {
        return Err(ManifestError::Invalid(format!(
            "transport.command {raw:?} collides with plugin metadata files"
        )));
    }
    Ok(())
}

/// Ensure a relative path stays strictly inside its base directory (no `/`,
/// `..`, or drive prefixes).
pub(crate) fn validate_relative_path(raw: &str, field: &str) -> Result<(), ManifestError> {
    if raw.is_empty() {
        return Err(ManifestError::Invalid(format!("{field} must not be empty")));
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(ManifestError::Invalid(format!(
            "{field} must be a relative path, got {raw:?}"
        )));
    }
    let mut has_normal = false;
    for comp in path.components() {
        match comp {
            Component::Normal(_) => has_normal = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ManifestError::Invalid(format!(
                    "{field} may not escape the plugin directory: {raw:?}"
                )));
            }
        }
    }
    if !has_normal {
        return Err(ManifestError::Invalid(format!(
            "{field} must point to a file: {raw:?}"
        )));
    }
    Ok(())
}

/// One installed plugin's metadata, loaded from disk without spawning it.
#[derive(Debug, Clone)]
pub struct InstalledPlugin {
    pub manifest: PluginManifest,
    pub dir: PathBuf,
    /// Pre-flattened translation JSON fragments keyed by HA category
    /// (`title`, `config`, `entity`, `entity_component`, `services`, ...).
    /// Each value is ready-to-join `"key":"value",...` pairs (matching
    /// `ha-api`'s `i18n.rs`).
    translations: HashMap<String, String>,
    /// Pre-serialised icon JSON trees keyed by HA icon category (`entity`,
    /// `services`, ...).
    icons: HashMap<String, String>,
}

impl InstalledPlugin {
    /// Load and validate a plugin from `<plugins_dir>/<domain>/`.
    pub fn load(dir: &Path) -> Result<Self, ManifestError> {
        let manifest_path = dir.join("plugin.json");
        let raw = std::fs::read_to_string(&manifest_path).map_err(|source| ManifestError::Io {
            path: manifest_path.clone(),
            source,
        })?;
        let manifest: PluginManifest =
            serde_json::from_str(&raw).map_err(|source| ManifestError::Json {
                path: manifest_path,
                source,
            })?;
        let dir_name = dir.file_name().and_then(|n| n.to_str());
        manifest.validate(dir_name)?;

        let translations = load_translations(dir, &manifest);
        let icons = load_icons(dir);

        Ok(Self {
            manifest,
            dir: dir.to_path_buf(),
            translations,
            icons,
        })
    }

    pub fn domain(&self) -> &str {
        &self.manifest.domain
    }

    /// Resolve the executable path for a `Stdio` plugin.
    pub fn stdio_command(&self) -> Option<(PathBuf, &[String])> {
        match &self.manifest.transport {
            Transport::Stdio { command, args } => Some((self.dir.join(command), args.as_slice())),
            Transport::UnixSocket { .. } => None,
        }
    }

    /// Resolve the Unix socket path for a `UnixSocket` plugin under
    /// `<plugins_dir>/run/`.
    pub fn unix_socket_path(&self, plugins_dir: &Path) -> Option<PathBuf> {
        match &self.manifest.transport {
            Transport::UnixSocket { path } => {
                let run_dir = plugins_dir.join("run");
                match path {
                    Some(rel) => Some(run_dir.join(rel)),
                    None => Some(run_dir.join(format!("{}.sock", self.manifest.domain))),
                }
            }
            Transport::Stdio { .. } => None,
        }
    }

    /// Whether this plugin is allowed to manage entities in `entity_domain`.
    pub fn allows_entity_domain(&self, entity_domain: &str) -> bool {
        self.manifest
            .capabilities
            .entity_domains
            .iter()
            .any(|d| d == entity_domain)
    }

    /// HA's `manifest/get` and `manifest/list` JSON shape for a custom
    /// integration (`is_built_in: false`).
    pub fn manifest_json(&self) -> String {
        let mut obj = Map::new();
        obj.insert("domain".into(), json!(self.manifest.domain));
        obj.insert("name".into(), json!(self.manifest.name));
        obj.insert("version".into(), json!(self.manifest.version));
        obj.insert("config_flow".into(), json!(self.manifest.config_flow));
        obj.insert("integration_type".into(), json!("hub"));
        obj.insert("iot_class".into(), json!(self.manifest.iot_class));
        obj.insert("is_built_in".into(), json!(false));
        obj.insert("overwrites_built_in".into(), json!(false));
        obj.insert("codeowners".into(), json!([]));
        obj.insert("dependencies".into(), json!([]));
        obj.insert("requirements".into(), json!([]));
        if let Some(doc) = &self.manifest.documentation {
            obj.insert("documentation".into(), json!(doc));
        }
        Value::Object(obj).to_string()
    }

    /// HA's `integration/descriptions` entry under `custom.integration.<domain>`.
    /// Matches HA 2026.6.1's `loader.py` shape for custom integrations:
    /// `integration_type`, `config_flow`, `iot_class`, `name`,
    /// `single_config_entry`, and `overwrites_built_in` (and no empty
    /// `iot_standards` array).
    pub fn description_json(&self) -> String {
        json!({
            "integration_type": "hub",
            "config_flow": self.manifest.config_flow,
            "iot_class": self.manifest.iot_class,
            "name": self.manifest.name,
            "single_config_entry": false,
            "overwrites_built_in": false,
        })
        .to_string()
    }

    /// Flattened translation pairs (`"k":"v",...`) for `category`, matching
    /// `ha-api`'s `i18n.rs`.
    pub fn translations_for(&self, category: &str) -> Option<&str> {
        self.translations.get(category).map(String::as_str)
    }

    /// Serialised icon JSON tree for `category`, matching `ha-api`'s `i18n.rs`.
    pub fn icons_for(&self, category: &str) -> Option<&str> {
        self.icons.get(category).map(String::as_str)
    }

    /// Candidate path for a local brand image (`icon.png`, `logo.png`, ...).
    /// Pure path check (no filesystem I/O); callers must read via
    /// [`Self::read_brand_image`] on a blocking thread.
    pub fn brand_image_candidate(&self, image: &str) -> Option<(PathBuf, PathBuf)> {
        if !ALLOWED_BRAND_IMAGES.contains(&image) {
            return None;
        }
        Some((self.dir.clone(), self.dir.join(image)))
    }

    /// Read a local brand image safely (blocking I/O): refuses symlinks,
    /// verifies canonical containment inside the plugin directory, and uses a
    /// bounded `Read::take` cap of 1 MiB so a growing file cannot allocate
    /// unbounded memory.
    pub fn read_brand_image(&self, image: &str) -> Option<Vec<u8>> {
        use std::io::Read;
        const MAX_BRAND_IMAGE_BYTES: u64 = 1024 * 1024;
        let (root, path) = self.brand_image_candidate(image)?;
        let link_meta = std::fs::symlink_metadata(&path).ok()?;
        if !link_meta.file_type().is_file() || link_meta.len() > MAX_BRAND_IMAGE_BYTES {
            return None;
        }
        let canon_root = std::fs::canonicalize(&root).ok()?;
        let canon_path = std::fs::canonicalize(&path).ok()?;
        if !canon_path.starts_with(&canon_root) {
            return None;
        }
        let file = std::fs::File::open(&canon_path).ok()?;
        let open_meta = file.metadata().ok()?;
        if !open_meta.is_file() || open_meta.len() > MAX_BRAND_IMAGE_BYTES {
            return None;
        }
        // Verify the opened descriptor is the exact same inode that passed the
        // symlink + canonical containment check, closing the open TOCTOU gap.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if (open_meta.dev(), open_meta.ino()) != (link_meta.dev(), link_meta.ino()) {
                return None;
            }
        }
        let mut buf = Vec::with_capacity(open_meta.len().min(MAX_BRAND_IMAGE_BYTES) as usize);
        file.take(MAX_BRAND_IMAGE_BYTES + 1)
            .read_to_end(&mut buf)
            .ok()?;
        if buf.len() as u64 > MAX_BRAND_IMAGE_BYTES {
            return None;
        }
        Some(buf)
    }
}

/// Load `translations/en.json` (if present) and flatten each top-level HA
/// category into `"component.<domain>.<category>.<path>":"<value>"` fragments.
fn load_translations(dir: &Path, manifest: &PluginManifest) -> HashMap<String, String> {
    let mut by_category: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let domain = &manifest.domain;

    let path = dir.join("translations").join("en.json");
    if let Ok(raw) = std::fs::read_to_string(&path) {
        match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Object(root)) => {
                for (category, val) in root {
                    let prefix = format!("component.{domain}.{category}");
                    let entries = by_category.entry(category).or_default();
                    flatten_translations(&prefix, &val, entries);
                }
            }
            Ok(_) => tracing::warn!("{}: expected JSON object", path.display()),
            Err(e) => tracing::warn!("{}: {e}", path.display()),
        }
    }

    // As in HA, `title` falls back to the manifest's `name` when `en.json`
    // does not supply one.
    by_category
        .entry("title".into())
        .or_insert_with(|| vec![(format!("component.{domain}.title"), manifest.name.clone())]);

    by_category
        .into_iter()
        .filter_map(|(cat, pairs)| {
            if pairs.is_empty() {
                return None;
            }
            let mut out = String::new();
            for (i, (k, v)) in pairs.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if let (Ok(kj), Ok(vj)) = (serde_json::to_string(&k), serde_json::to_string(&v)) {
                    out.push_str(&kj);
                    out.push(':');
                    out.push_str(&vj);
                }
            }
            (!out.is_empty()).then_some((cat, out))
        })
        .collect()
}

fn flatten_translations(prefix: &str, value: &Value, out: &mut Vec<(String, String)>) {
    match value {
        Value::String(s) => out.push((prefix.to_owned(), s.clone())),
        Value::Object(map) => {
            for (k, v) in map {
                let next = format!("{prefix}.{k}");
                flatten_translations(&next, v, out);
            }
        }
        _ => {}
    }
}

/// Load `icons.json` (if present), keyed by HA icon category.
fn load_icons(dir: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let path = dir.join("icons.json");
    if let Ok(raw) = std::fs::read_to_string(&path) {
        match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Object(root)) => {
                for (category, val) in root {
                    if let Ok(serialised) = serde_json::to_string(&val) {
                        out.insert(category, serialised);
                    }
                }
            }
            Ok(_) => tracing::warn!("{}: expected JSON object", path.display()),
            Err(e) => tracing::warn!("{}: {e}", path.display()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest(domain: &str, command: &str) -> PluginManifest {
        PluginManifest {
            schema_version: 1,
            domain: domain.into(),
            name: "Sample Plug".into(),
            version: "0.1.0".into(),
            iot_class: "local_polling".into(),
            config_flow: true,
            documentation: Some("https://example.invalid/sample".into()),
            description: Some("Demo plugin".into()),
            transport: Transport::Stdio {
                command: command.into(),
                args: vec![],
            },
            capabilities: Capabilities {
                entity_domains: vec!["switch".into(), "sensor".into()],
                config_entry_version: 1,
            },
        }
    }

    #[test]
    fn validates_domains_and_paths() {
        assert!(
            sample_manifest("tplink", "bin/tplink")
                .validate(Some("tplink"))
                .is_ok()
        );
        // Built-in domain collision rejected.
        assert!(
            sample_manifest("hue", "bin/hue")
                .validate(Some("hue"))
                .is_err()
        );
        assert!(
            sample_manifest("hars_plugins", "bin/p")
                .validate(Some("hars_plugins"))
                .is_err()
        );
        // `run` is reserved for sidecar Unix sockets.
        assert!(
            sample_manifest("run", "bin/r")
                .validate(Some("run"))
                .is_err()
        );
        // Command colliding with manifest or metadata files is rejected.
        assert!(
            sample_manifest("tplink", "plugin.json")
                .validate(Some("tplink"))
                .is_err()
        );
        assert!(
            sample_manifest("tplink", "./plugin.json")
                .validate(Some("tplink"))
                .is_err()
        );
        assert!(
            sample_manifest("tplink", "translations/en.json")
                .validate(Some("tplink"))
                .is_err()
        );
        // Invalid domain characters rejected.
        assert!(
            sample_manifest("Bad-Domain", "bin/x")
                .validate(None)
                .is_err()
        );
        assert!(sample_manifest("_leading", "bin/x").validate(None).is_err());
        assert!(
            sample_manifest("double__under", "bin/x")
                .validate(None)
                .is_err()
        );
        // Directory name mismatch rejected.
        assert!(
            sample_manifest("tplink", "bin/tplink")
                .validate(Some("other"))
                .is_err()
        );
        // Path traversal and absolute paths rejected.
        assert!(
            sample_manifest("tplink", "../outside")
                .validate(Some("tplink"))
                .is_err()
        );
        assert!(
            sample_manifest("tplink", "/usr/bin/sh")
                .validate(Some("tplink"))
                .is_err()
        );
        // Unsupported entity domain rejected.
        let mut bad_cap = sample_manifest("tplink", "bin/tplink");
        bad_cap.capabilities.entity_domains.push("camera".into());
        assert!(bad_cap.validate(Some("tplink")).is_err());
    }

    #[test]
    fn loads_manifest_translations_icons_and_brand_image() {
        let dir = std::env::temp_dir().join(format!("hars-plugin-manifest-{}", std::process::id()));
        let plugin_dir = dir.join("tplink");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(plugin_dir.join("translations")).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.json"),
            serde_json::to_string_pretty(&sample_manifest("tplink", "bin/tplink")).unwrap(),
        )
        .unwrap();
        std::fs::write(
            plugin_dir.join("translations/en.json"),
            json!({
                "config": {
                    "step": {
                        "user": {
                            "title": "Connect TP-Link",
                            "data": { "host": "Host" }
                        }
                    },
                    "error": { "cannot_connect": "Cannot connect" }
                }
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            plugin_dir.join("icons.json"),
            json!({ "entity": { "sensor": { "power": { "default": "mdi:flash" } } } }).to_string(),
        )
        .unwrap();
        std::fs::write(plugin_dir.join("icon.png"), b"PNGDATA").unwrap();

        let loaded = InstalledPlugin::load(&plugin_dir).unwrap();
        assert_eq!(loaded.domain(), "tplink");
        assert!(loaded.allows_entity_domain("switch"));
        assert!(!loaded.allows_entity_domain("light"));

        let manifest: Value = serde_json::from_str(&loaded.manifest_json()).unwrap();
        assert_eq!(manifest["domain"], "tplink");
        assert_eq!(manifest["is_built_in"], false);

        let desc: Value = serde_json::from_str(&loaded.description_json()).unwrap();
        assert_eq!(desc["name"], "Sample Plug");
        assert_eq!(desc["config_flow"], true);
        assert_eq!(desc["integration_type"], "hub");
        assert_eq!(desc["single_config_entry"], false);
        assert_eq!(desc["overwrites_built_in"], false);
        assert!(desc.get("iot_standards").is_none());

        let title: Value = serde_json::from_str(&format!(
            "{{{}}}",
            loaded.translations_for("title").unwrap()
        ))
        .unwrap();
        assert_eq!(title["component.tplink.title"], "Sample Plug");

        let cfg: Value = serde_json::from_str(&format!(
            "{{{}}}",
            loaded.translations_for("config").unwrap()
        ))
        .unwrap();
        assert_eq!(cfg["component.tplink.config.step.user.data.host"], "Host");
        assert_eq!(
            cfg["component.tplink.config.error.cannot_connect"],
            "Cannot connect"
        );

        let icons: Value = serde_json::from_str(loaded.icons_for("entity").unwrap()).unwrap();
        assert_eq!(icons["sensor"]["power"]["default"], "mdi:flash");

        assert_eq!(
            loaded.read_brand_image("icon.png").as_deref(),
            Some(b"PNGDATA".as_slice())
        );
        assert!(loaded.read_brand_image("../plugin.json").is_none());
        assert!(loaded.read_brand_image("logo.png").is_none());

        #[cfg(unix)]
        {
            let secret = dir.join("secret.txt");
            std::fs::write(&secret, b"TOPSECRET").unwrap();
            std::os::unix::fs::symlink(&secret, plugin_dir.join("logo.png")).unwrap();
            assert!(loaded.read_brand_image("logo.png").is_none());
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
