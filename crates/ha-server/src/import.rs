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
/// Where a commit keeps the destination's previous files until it's done.
const BACKUP_DIR: &str = ".import-backup";
const USER_DATA_PREFIX: &str = "frontend.user_data_";

/// The stores `hars` reads, with whether HA writes them owner-only.
const STORES: &[(&str, bool)] = &[
    ("auth", true),
    ("auth_provider.homeassistant", true),
    ("onboarding", true),
    ("core.config", true),
    ("frontend.system_data", false),
    (ha_registry::ENTITY_KEY, false),
    (ha_registry::DEVICE_KEY, false),
    (ha_registry::AREA_KEY, false),
    (ha_registry::FLOOR_KEY, false),
    (ha_registry::LABEL_KEY, false),
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
    /// Supported stores the destination had and the source doesn't: removed,
    /// so the destination matches the source.
    pub removed: Vec<String>,
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
    let dest = std::fs::canonicalize(&opts.dest)?;
    if dest == source {
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

    let mut report = Report {
        importer_version: IMPORTER_VERSION,
        ha_version,
        dry_run: opts.dry_run,
        imported: Vec::new(),
        replaced: Vec::new(),
        removed: Vec::new(),
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

    if !source.join(".storage").is_dir() {
        bail!(
            "{} has no .storage directory: has this Home Assistant been set up?",
            source.display()
        );
    }
    let backup = dest.join(BACKUP_DIR);
    if backup.exists() {
        bail!(
            "an earlier import could not roll back and left the previous files in {}; \
             move them back into .storage (or delete that directory) first",
            backup.display()
        );
    }

    // 1. Read the source's supported stores once, into a staging directory.
    //    Validation and the commit both use exactly these bytes.
    let staging = Staging::new(&dest)?;
    for (key, kind) in storage_keys(&source)? {
        let Some(private) = supported(&key) else {
            report.skipped.push(Skipped {
                reason: unsupported_reason(&key).into(),
                key,
            });
            continue;
        };
        if kind != Kind::File {
            bail!("{key} in the source is not a regular file (a symlink?); copy the real file");
        }
        let bytes = std::fs::read(source.join(".storage").join(&key))?;
        Store::new(&staging.root, key.clone(), 1, 1, private).save_raw(&bytes)?;
    }

    // 2. Validate the staged copy with the loaders the server uses. Auth and
    //    core.config must load; frontend settings that don't are left out.
    ha_auth::Auth::load(&staging.root, ha_core::Clock::system()).map_err(|e| {
        anyhow::anyhow!(
            "the auth stores don't load ({}); is the copy consistent?",
            sanitized(&e)
        )
    })?;
    ha_core::config::CoreConfig::load(&staging.root)
        .map_err(|e| anyhow::anyhow!("core.config doesn't load ({})", sanitized(&e)))?;
    ha_registry::Registries::load_strict(&staging.root)
        .map_err(|e| anyhow::anyhow!("the registries don't load ({})", sanitized(&e)))?;
    ha_api::Lovelace::load(&staging.root)
        .map_err(|e| anyhow::anyhow!("the dashboards don't load ({})", sanitized(&e)))?;
    for key in staging.keys()? {
        if key == "frontend.system_data" || key.starts_with(USER_DATA_PREFIX) {
            let store = Store::new(&staging.root, key.clone(), 1, 1, false);
            if let Err(e) = store.load::<Map<String, Value>>() {
                std::fs::remove_file(store.path())?;
                report.skipped.push(Skipped {
                    key,
                    reason: format!("doesn't load ({})", sanitized(&e)),
                });
            }
        }
    }

    // 3. The plan: the destination's supported stores become exactly the
    //    staged ones. A store the source doesn't have is removed, since a
    //    missing store means defaults (no users, not onboarded).
    let staged = staging.keys()?;
    let mut in_dest: Vec<String> = Vec::new();
    for (key, kind) in storage_keys(&dest)? {
        if supported(&key).is_some() {
            if kind != Kind::File {
                bail!("{key} in {} is not a regular file", dest.display());
            }
            in_dest.push(key);
        }
    }
    let read = |dir: &Path, key: &str| std::fs::read(dir.join(".storage").join(key)).ok();
    for key in &in_dest {
        match staged.contains(key) {
            false => report.removed.push(key.clone()),
            true if read(&dest, key) != read(&staging.root, key) => {
                report.replaced.push(key.clone())
            }
            true => {}
        }
    }
    report.imported = staged.clone();
    if !opts.force && !opts.dry_run && !(report.replaced.is_empty() && report.removed.is_empty()) {
        let mut changed = report.replaced.clone();
        changed.extend(report.removed.iter().map(|k| format!("{k} (removed)")));
        bail!(
            "{} already has different {}; re-run with --force to replace them",
            dest.display(),
            changed.join(", ")
        );
    }
    if opts.dry_run {
        return Ok(report);
    }

    // 4. Commit: everything or nothing.
    let mut changes: Vec<Change> = Vec::new();
    for key in &staged {
        if read(&dest, key) != read(&staging.root, key) {
            changes.push(Change::Put(key.clone()));
        }
    }
    changes.extend(report.removed.iter().cloned().map(Change::Remove));
    commit(&staging, &dest, &changes, |from, to| {
        std::fs::rename(from, to)
    })?;

    let json = serde_json::to_vec_pretty(&report)?;
    std::fs::write(dest.join(REPORT_FILE), json)?;
    Ok(report)
}

/// Whether `hars` reads a store, and whether HA writes it owner-only.
fn supported(key: &str) -> Option<bool> {
    STORES
        .iter()
        .find(|(k, _)| *k == key)
        .map(|&(_, private)| private)
        .or_else(|| key.starts_with(USER_DATA_PREFIX).then_some(false))
        .or_else(|| ha_api::Lovelace::is_store(key).then_some(false))
}

fn unsupported_reason(key: &str) -> &'static str {
    match key {
        "core.config_entries" => "integrations: see config_entries",
        k if k.starts_with("core.") && k.ends_with("_registry") => {
            "this registry is not used by hars yet"
        }
        k if k.starts_with("lovelace") => "dashboards are not imported yet",
        _ => "not used by hars yet",
    }
}

/// A store error without the data: serde's messages can quote file contents.
fn sanitized(e: &ha_core::storage::StoreError) -> String {
    use ha_core::storage::StoreError;
    match e {
        StoreError::Json { source, .. } => format!(
            "{:?} error at line {} column {}",
            source.classify(),
            source.line(),
            source.column()
        ),
        StoreError::Io { source, .. } => format!("{:?}", source.kind()),
        StoreError::TooNew {
            found, supported, ..
        }
        | StoreError::TooOld {
            found, supported, ..
        } => {
            format!("version {found}, expected {supported}")
        }
    }
}

/// `dest/.import-staging`, laid out like a config dir; removed when dropped.
struct Staging {
    root: PathBuf,
}

impl Staging {
    fn new(dest: &Path) -> anyhow::Result<Self> {
        let root = dest.join(".import-staging");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".storage"))?;
        Ok(Self { root })
    }

    fn keys(&self) -> anyhow::Result<Vec<String>> {
        Ok(storage_keys(&self.root)?
            .into_iter()
            .map(|(k, _)| k)
            .collect())
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug)]
enum Change {
    Put(String),
    Remove(String),
}

/// Apply the changes with renames. The destination's current files are moved
/// to `.import-backup` first; if any step fails, everything done so far is
/// undone. `rename` is injectable so the rollback can be tested.
fn commit(
    staging: &Staging,
    dest: &Path,
    changes: &[Change],
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> anyhow::Result<()> {
    let storage = dest.join(".storage");
    let backup = dest.join(BACKUP_DIR);
    // `run` refuses to start while a backup is left over; never delete one.
    std::fs::create_dir(&backup).with_context(|| format!("{} already exists", backup.display()))?;
    std::fs::create_dir_all(&storage)?;

    // What was done, to undo in reverse on failure.
    let mut moved_out: Vec<String> = Vec::new();
    let mut put: Vec<String> = Vec::new();
    let result = (|| -> std::io::Result<()> {
        for change in changes {
            let (Change::Put(key) | Change::Remove(key)) = change;
            let current = storage.join(key);
            if current.exists() {
                rename(&current, &backup.join(key))?;
                moved_out.push(key.clone());
            }
            if let Change::Put(key) = change {
                rename(&staging.root.join(".storage").join(key), &current)?;
                put.push(key.clone());
            }
        }
        #[cfg(unix)]
        std::fs::File::open(&storage)?.sync_all()?;
        Ok(())
    })();

    if let Err(e) = result {
        for key in put.iter().rev() {
            let _ = std::fs::remove_file(storage.join(key));
        }
        let mut restored = true;
        for key in moved_out.iter().rev() {
            restored &= std::fs::rename(backup.join(key), storage.join(key)).is_ok();
        }
        if restored {
            let _ = std::fs::remove_dir_all(&backup);
            bail!("import failed and was rolled back; nothing changed: {e}");
        }
        bail!(
            "import failed ({e}) and could not be fully rolled back; the previous files are in {}",
            backup.display()
        );
    }
    let _ = std::fs::remove_dir_all(&backup);
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    File,
    Other,
}

/// The `.storage` entries in a config directory, sorted, with whether each
/// is a regular file (symlinks are not followed). Temporary and quarantined
/// files are left out.
/// A missing `.storage` is empty; any other error (permissions) is an error,
/// never an empty list: that would read as "remove everything".
fn storage_keys(config_dir: &Path) -> anyhow::Result<Vec<(String, Kind)>> {
    let mut keys = Vec::new();
    let dir = config_dir.join(".storage");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(keys),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name.contains(".corrupt.") || name.starts_with("tmp") {
            continue;
        }
        let kind = if entry.file_type()?.is_file() {
            Kind::File
        } else {
            Kind::Other
        };
        keys.push((name, kind));
    }
    keys.sort_by(|a, b| a.0.cmp(&b.0));
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
    if !report.removed.is_empty() {
        out.push_str(&format!("  removed: {}\n", report.removed.join(", ")));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A commit that fails half-way leaves the destination as it was.
    #[test]
    fn a_failed_commit_is_rolled_back() {
        let dest =
            std::env::temp_dir().join(format!("hars-import-rollback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        let storage = dest.join(".storage");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("auth"), "old auth").unwrap();
        std::fs::write(storage.join("auth_provider.homeassistant"), "old provider").unwrap();
        std::fs::write(storage.join("onboarding"), "old onboarding").unwrap();

        let staging = Staging::new(&dest).unwrap();
        std::fs::write(staging.root.join(".storage/auth"), "new auth").unwrap();
        std::fs::write(
            staging.root.join(".storage/auth_provider.homeassistant"),
            "new provider",
        )
        .unwrap();
        let changes = [
            Change::Put("auth".into()),
            Change::Put("auth_provider.homeassistant".into()),
            Change::Remove("onboarding".into()),
        ];
        // The fourth rename (moving the new provider in) fails.
        let mut calls = 0;
        let err = commit(&staging, &dest, &changes, |from, to| {
            calls += 1;
            if calls == 4 {
                return Err(std::io::Error::other("disk full"));
            }
            std::fs::rename(from, to)
        })
        .unwrap_err();
        assert!(err.to_string().contains("rolled back"), "{err}");
        for (key, old) in [
            ("auth", "old auth"),
            ("auth_provider.homeassistant", "old provider"),
            ("onboarding", "old onboarding"),
        ] {
            assert_eq!(
                std::fs::read_to_string(storage.join(key)).unwrap(),
                old,
                "{key}"
            );
        }
        assert!(!dest.join(".import-backup").exists());

        // And the same changes, staged again, all apply without the failure.
        drop(staging);
        let staging = Staging::new(&dest).unwrap();
        std::fs::write(staging.root.join(".storage/auth"), "new auth").unwrap();
        std::fs::write(
            staging.root.join(".storage/auth_provider.homeassistant"),
            "new provider",
        )
        .unwrap();
        commit(&staging, &dest, &changes, |from, to| {
            std::fs::rename(from, to)
        })
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(storage.join("auth")).unwrap(),
            "new auth"
        );
        assert!(!storage.join("onboarding").exists());
        drop(staging);
        std::fs::remove_dir_all(dest).unwrap();
    }

    #[test]
    fn errors_never_quote_file_contents() {
        let dir = std::env::temp_dir().join(format!("hars-import-sanitize-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".storage")).unwrap();
        std::fs::write(
            dir.join(".storage/x"),
            r#"{"version": 1, "key": "x", "data": "SYNTHETIC_SECRET_MARKER"}"#,
        )
        .unwrap();
        let err = Store::new(&dir, "x", 1, 1, false)
            .load::<Map<String, Value>>()
            .unwrap_err();
        assert!(
            err.to_string().contains("SYNTHETIC_SECRET_MARKER"),
            "serde does quote it"
        );
        assert!(
            !sanitized(&err).contains("SYNTHETIC"),
            "{}",
            sanitized(&err)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
