//! `hars import <HA config dir>`: take over a Home Assistant 2026.6 install.
//!
//! `hars` keeps HA's own `.storage` formats, so a store `hars` understands is
//! validated with the loader `hars` itself uses and then copied byte for
//! byte: users, password hashes and refresh tokens (ids, client ids,
//! `jwt_key`s) arrive unchanged, and the browser and phone sessions logged in
//! to HA keep working. Everything else is listed in `import-report.json`.
//!
//! The source directory is only read. Give it a consistent copy, taken with
//! HA stopped (or a backup), not a live directory HA is writing to.

use anyhow::{Context, bail};
use ha_core::storage::Store;
use serde::Serialize;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// Bumped whenever what the importer does changes.
pub const IMPORTER_VERSION: u32 = 1;
/// The HA release whose formats `hars` reads.
const SUPPORTED_HA: &str = "2026.6.";
const REPORT_FILE: &str = "import-report.json";
const USER_DATA_PREFIX: &str = "frontend.user_data_";

/// The stores `hars` reads, with whether HA writes them owner-only.
const STORES: &[(&str, bool)] = &[
    ("auth", true),
    ("auth_provider.homeassistant", true),
    ("onboarding", true),
    ("core.config", true),
    ("frontend.system_data", false),
];

/// Integrations `hars` implements, by domain; their config entries are
/// imported once their integration exists.
const SUPPORTED_DOMAINS: &[&str] = &[];

pub struct Options {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub dry_run: bool,
    /// Overwrite stores the destination already has with different content.
    pub force: bool,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub importer_version: u32,
    pub ha_version: String,
    pub dry_run: bool,
    /// Stores copied (or that would be, in a dry run).
    pub imported: Vec<String>,
    /// Of those, the ones that replaced a different copy in the destination.
    pub replaced: Vec<String>,
    /// Stores left behind, and why.
    pub skipped: Vec<Skipped>,
    /// Every config entry, with whether `hars` can run it yet.
    pub config_entries: Vec<EntryReport>,
    /// YAML configuration found, which isn't imported yet.
    pub yaml: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Skipped {
    pub key: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct EntryReport {
    pub entry_id: String,
    pub domain: String,
    pub title: String,
    pub disabled: bool,
    pub supported: bool,
}

pub fn run(opts: &Options) -> anyhow::Result<Report> {
    let source = std::fs::canonicalize(&opts.source)
        .with_context(|| format!("reading {}", opts.source.display()))?;
    std::fs::create_dir_all(&opts.dest)?;
    if std::fs::canonicalize(&opts.dest)? == source {
        bail!("the source and destination are the same directory");
    }

    let ha_version = std::fs::read_to_string(source.join(".HA_VERSION"))
        .context("no .HA_VERSION: is this a Home Assistant config directory?")?
        .trim()
        .to_owned();
    if !ha_version.starts_with(SUPPORTED_HA) {
        bail!(
            "this is Home Assistant {ha_version}; hars imports {SUPPORTED_HA}x. \
             Update HA to 2026.6 first, then copy its config again"
        );
    }

    // Validate with the loaders the server uses. They only read.
    ha_auth::Auth::load(&source, ha_core::Clock::system())
        .context("the auth stores don't load; is the copy consistent?")?;
    ha_core::config::CoreConfig::load(&source).context("core.config doesn't load")?;

    let mut report = Report {
        importer_version: IMPORTER_VERSION,
        ha_version,
        dry_run: opts.dry_run,
        imported: Vec::new(),
        replaced: Vec::new(),
        skipped: Vec::new(),
        config_entries: config_entries(&source)?,
        yaml: [
            "configuration.yaml",
            "automations.yaml",
            "scripts.yaml",
            "scenes.yaml",
        ]
        .into_iter()
        .filter(|f| source.join(f).is_file())
        .map(str::to_owned)
        .collect(),
    };

    // What to copy: the stores `hars` reads that exist in the source.
    let mut copies: Vec<(String, bool)> = Vec::new();
    for key in storage_keys(&source)? {
        if let Some(&(_, private)) = STORES.iter().find(|(k, _)| *k == key) {
            copies.push((key, private));
        } else if key.starts_with(USER_DATA_PREFIX) {
            let store = Store::new(&source, key.clone(), 1, 1, false);
            match store.load::<Map<String, Value>>() {
                Ok(_) => copies.push((key, false)),
                Err(e) => report.skipped.push(Skipped {
                    key,
                    reason: format!("doesn't load: {e}"),
                }),
            }
        } else {
            let reason = match key.as_str() {
                "core.config_entries" => "integrations: see config_entries".into(),
                k if k.starts_with("core.") && k.ends_with("_registry") => {
                    "registries are not imported yet".into()
                }
                k if k.starts_with("lovelace") => "dashboards are not imported yet".into(),
                _ => "not used by hars yet".into(),
            };
            report.skipped.push(Skipped { key, reason });
        }
    }

    // Refuse to overwrite state the destination changed since, unless asked.
    let changed: Vec<String> = copies
        .iter()
        .filter(|(key, _)| {
            let dest = opts.dest.join(".storage").join(key);
            dest.exists()
                && std::fs::read(&dest).ok()
                    != std::fs::read(source.join(".storage").join(key)).ok()
        })
        .map(|(key, _)| key.clone())
        .collect();
    if !changed.is_empty() && !opts.force && !opts.dry_run {
        bail!(
            "{} already has different {}; re-run with --force to replace them",
            opts.dest.display(),
            changed.join(", ")
        );
    }

    report.replaced = changed;
    for (key, private) in &copies {
        if !opts.dry_run {
            let bytes = std::fs::read(source.join(".storage").join(key))?;
            Store::new(&opts.dest, key.clone(), 1, 1, *private)
                .save_raw(&bytes)
                .with_context(|| format!("writing {key}"))?;
        }
        report.imported.push(key.clone());
    }

    if !opts.dry_run {
        let json = serde_json::to_vec_pretty(&report)?;
        std::fs::write(opts.dest.join(REPORT_FILE), json)?;
    }
    Ok(report)
}

/// The `.storage` keys in a config directory, sorted. Temporary and
/// quarantined files are left out.
fn storage_keys(config_dir: &Path) -> anyhow::Result<Vec<String>> {
    let mut keys = Vec::new();
    let Ok(entries) = std::fs::read_dir(config_dir.join(".storage")) else {
        return Ok(keys);
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name.contains(".corrupt.") || name.starts_with("tmp") {
            continue;
        }
        keys.push(name);
    }
    keys.sort();
    Ok(keys)
}

/// `core.config_entries`: which integrations the install uses.
fn config_entries(config_dir: &Path) -> anyhow::Result<Vec<EntryReport>> {
    #[derive(serde::Deserialize)]
    struct Entries {
        #[serde(default)]
        entries: Vec<Map<String, Value>>,
    }
    let store = Store::new(config_dir, "core.config_entries", 1, 5, true);
    let Some(data) = store
        .load::<Entries>()
        .context("core.config_entries doesn't load")?
    else {
        return Ok(Vec::new());
    };
    let text = |e: &Map<String, Value>, k: &str| {
        e.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Ok(data
        .entries
        .iter()
        .map(|e| {
            let domain = text(e, "domain");
            EntryReport {
                entry_id: text(e, "entry_id"),
                supported: SUPPORTED_DOMAINS.contains(&domain.as_str()),
                domain,
                title: text(e, "title"),
                disabled: e.get("disabled_by").is_some_and(|v| !v.is_null()),
            }
        })
        .collect())
}

/// One line per item, for the terminal.
pub fn summary(report: &Report, dest: &Path) -> String {
    let mut out = format!(
        "Home Assistant {}: {} {} store(s): {}\n",
        report.ha_version,
        if report.dry_run {
            "would import"
        } else {
            "imported"
        },
        report.imported.len(),
        report.imported.join(", ")
    );
    if !report.replaced.is_empty() {
        out.push_str(&format!("  replaced: {}\n", report.replaced.join(", ")));
    }
    for s in &report.skipped {
        out.push_str(&format!("  skipped {}: {}\n", s.key, s.reason));
    }
    let unsupported: Vec<String> = report
        .config_entries
        .iter()
        .filter(|e| !e.supported)
        .map(|e| format!("{} ({})", e.domain, e.title))
        .collect();
    if !unsupported.is_empty() {
        out.push_str(&format!(
            "  integrations not in hars yet: {}\n",
            unsupported.join(", ")
        ));
    }
    if !report.yaml.is_empty() {
        out.push_str(&format!(
            "  YAML not imported yet: {}\n",
            report.yaml.join(", ")
        ));
    }
    if !report.dry_run {
        out.push_str(&format!("Report: {}\n", dest.join(REPORT_FILE).display()));
    }
    out
}
