//! `hars import <HA config dir>`: take over a Home Assistant install.
//!
//! `hars` keeps HA's own `.storage` formats, so a store `hars` understands is
//! validated with the loader `hars` itself uses and then copied byte for
//! byte: users, password hashes and refresh tokens (ids, client ids,
//! `jwt_key`s) arrive unchanged, and the browser and phone sessions logged in
//! to HA keep working. Everything else is listed in `import-report.json`.
//!
//! Which releases: `hars` behaves like one pinned release ([`PINNED_HA`])
//! and reads and writes only that release's storage formats. A store or a
//! config entry that Home Assistant 2026.8 to 2026.11 writes in a newer
//! form is first turned back into that form ([`convert`], in the staged
//! copy; the rules go by storage version). After that the rule starts from
//! Home Assistant's own for a downgrade, and is stricter where `hars` could
//! not keep the data: a store `hars` reads with a newer major storage
//! version (`helpers/storage.py` raises `UnsupportedStorageVersionError`;
//! for a store its bootstrap loads, HA then starts in recovery mode), or a
//! config entry of a newer version for an integration `hars` runs
//! (`ConfigEntry.async_migrate`: `migration_error`), refuses the directory
//! whole, with the store and versions named. A newer *minor*
//! version is compatible by HA's rule and is read where `hars`'s own
//! reader and writer make that safe ([`Minor`] says which stores); the
//! rest are refused too. `tests/vectors/import/downgrade.json` is what
//! 2026.6.1's base bootstrap (registries, config entries, restore state)
//! did with each fixture, unconverted; [`TESTED_HA`] lists the releases
//! with one.
//!
//! The source directory is only read. Give it a consistent copy, taken with
//! HA stopped (or a backup), not a live directory HA is writing to.

mod convert;

pub use convert::{ConvertedEntry, ConvertedStore, Dropped, ParentDevice, SplitDevice};

use anyhow::{Context, bail};
use ha_core::storage::Store;
use serde::Serialize;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// Bumped whenever what the importer does changes.
pub const IMPORTER_VERSION: u32 = 4;
/// The Home Assistant release `hars` behaves like, whichever release the
/// files come from (docs/ARCHITECTURE.md, "Home Assistant compatibility").
pub const PINNED_HA: &str = "2026.6.1";
/// The oldest release line `hars` imports (year, month): older ones have
/// formats its loaders don't migrate.
const OLDEST_HA: (u32, u32) = (2026, 6);
/// The release lines `hars import` is tested against, oldest first: for
/// each, `tests/vectors/import/` holds a config directory made by that
/// release's own code (`scripts/vectors/import_releases.py`). This is the
/// one place the list lives; the messages and the report read it.
pub const TESTED_HA: &[(u32, u32)] = &[
    (2026, 6),
    (2026, 7),
    (2026, 8),
    (2026, 9),
    (2026, 10),
    (2026, 11),
];
const REPORT_FILE: &str = "import-report.json";
/// Where a commit keeps the destination's previous files until it's done.
const BACKUP_DIR: &str = ".import-backup";
const USER_DATA_PREFIX: &str = "frontend.user_data_";

/// Which minor storage versions of a store `hars` takes, within the major
/// version it reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Minor {
    /// Any. A newer one is read as HA's `Store` reads it after a downgrade
    /// (the data as it is), and the next write is at `hars`'s own minor
    /// version, with what `hars`'s writer for that store keeps.
    Any,
    /// Not a newer one: 2026.6.1's own migration fails on it (`person`), or
    /// `hars` only reads the store and would never bring it back to its
    /// own version (the floor, label and category registries). An older
    /// one is taken: `person` version 1 is migrated by the loader, and the
    /// read-only registries are read as they are and left at the version
    /// they have (HA would migrate and save them).
    NotNewer,
    /// Only the one `hars` writes. The entity, device and area registries
    /// (`check_minor`) and the config entries (`writable`) are never
    /// written over a file of another minor version, so every later change
    /// would be lost at the next restart. HA migrates an older file and
    /// saves it when it loads it, so a directory 2026.6 has run on is at
    /// these versions.
    Exact,
}

/// A store `hars` reads: whether it is written owner-only, the storage
/// version (major, minor) `hars` reads and writes (2026.6.1's), and the
/// minor versions it takes.
type StoreSpec = (&'static str, bool, (u32, u32), Minor);

const STORES: &[StoreSpec] = &[
    ("auth", true, (1, 1), Minor::Any),
    ("auth_provider.homeassistant", true, (1, 1), Minor::Any),
    ("onboarding", true, (4, 1), Minor::Any),
    ("core.config", true, (1, 4), Minor::Any),
    ("frontend.system_data", false, (1, 1), Minor::Any),
    // Integrations' settings (hosts, keys, tokens): owner-only, as in HA.
    ("core.config_entries", true, (1, 5), Minor::Exact),
    ("core.labs", true, (1, 1), Minor::Any),
    // The analytics choices made at onboarding, and the install's uuid.
    ("core.analytics", false, (1, 1), Minor::Any),
    // Last states of entities that restore themselves (scenes, ...).
    ("core.restore_state", false, (1, 1), Minor::Any),
    (ha_registry::ENTITY_KEY, false, (1, 22), Minor::Exact),
    (ha_registry::DEVICE_KEY, false, (1, 12), Minor::Exact),
    (ha_registry::AREA_KEY, false, (1, 9), Minor::Exact),
    (ha_registry::FLOOR_KEY, false, (1, 3), Minor::NotNewer),
    (ha_registry::LABEL_KEY, false, (1, 2), Minor::NotNewer),
    // The names of the categories the automation list groups by (read
    // only).
    (ha_registry::CATEGORY_KEY, false, (1, 2), Minor::NotNewer),
    // The webhook ids of removed app registrations, which answer 410.
    (
        ha_integrations::mobile_app::DOMAIN,
        false,
        (1, 1),
        Minor::Any,
    ),
];

/// The integrations `hars` runs, by domain: `SUPPORTED`'s and the
/// companion app's registrations.
fn runs(domain: &str) -> bool {
    ha_integrations::SUPPORTED.contains(&domain) || domain == ha_integrations::mobile_app::DOMAIN
}

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
    /// The release the files come from (`.HA_VERSION`).
    pub ha_version: String,
    /// Whether `hars`'s tests cover imports from that release's line (its
    /// year and month, [`TESTED_HA`]; not each patch release). If not, its
    /// stores still passed every check here.
    pub ha_version_tested: bool,
    /// The release `hars` behaves like after the import ([`PINNED_HA`]),
    /// whatever `ha_version` is.
    pub behaves_like: &'static str,
    /// Stores of a newer minor storage version than `hars` writes: read as
    /// 2026.6.1 would read them, and written back at `hars_writes`,
    /// without what 2026.6.1 doesn't know, the next time they change.
    pub newer_stores: Vec<NewerStore>,
    /// Stores Home Assistant 2026.8 or later wrote in a newer form, which
    /// were put back into the form `hars` reads and writes before they
    /// were checked and copied.
    pub converted: Vec<ConvertedStore>,
    /// Config entries brought back to the version `hars` runs.
    pub converted_entries: Vec<ConvertedEntry>,
    /// What the older form has no place for, left out.
    pub dropped: Vec<Dropped>,
    /// Devices Home Assistant 2026.8 split by integration, with the files
    /// that still use the old id.
    pub split_devices: Vec<SplitDevice>,
    /// Devices with child devices (2026.9) that a YAML file or dashboard
    /// names by id: there Home Assistant reaches the children's entities
    /// through the parent, and `hars` doesn't.
    pub parent_devices: Vec<ParentDevice>,
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
    /// YAML files copied (or that would be).
    pub yaml: Vec<String>,
    /// Each automation in the YAML, and whether hars can run it.
    pub automations: Vec<AutomationReport>,
}

#[derive(Debug, Serialize)]
pub struct NewerStore {
    pub key: String,
    /// The file's storage version, `major.minor`.
    pub version: String,
    pub hars_writes: String,
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
    // Before the destination is made: inside the source it would be
    // a directory written into the folder the import only reads.
    // It is then made at that resolved path, not the one given: making
    // `source/new/../../dest` part by part would make `source/new`.
    let planned = planned_path(&opts.dest)?;
    if planned.starts_with(&source) {
        bail!("the destination is inside the source, or is the source; use a directory outside it");
    }
    // And by what the directories are, not only what they are called: one
    // directory mounted in two places has two paths.
    if let Some(overlap) = overlap(&source, &planned, file_id) {
        bail!("{overlap}");
    }
    std::fs::create_dir_all(&planned)?;
    let dest = std::fs::canonicalize(&planned)?;
    if dest.starts_with(&source) {
        bail!("the destination is inside the source, or is the source; use a directory outside it");
    }
    // The import stages, backs up and replaces files under the destination.
    if source.starts_with(&dest) {
        bail!("the source is inside the destination; copy it somewhere else first");
    }

    let ha_version = std::fs::read_to_string(source.join(".HA_VERSION"))
        .context("no .HA_VERSION: is this a Home Assistant config directory?")?
        .trim()
        .to_owned();
    let Some(line) = release_line(&ha_version) else {
        bail!(".HA_VERSION says {ha_version:?}, which is not a Home Assistant version");
    };
    if line < OLDEST_HA {
        bail!(
            "this is Home Assistant {ha_version}; hars imports {}. \
             Update Home Assistant to {} first (not to a later release), \
             then copy its config again",
            importable(),
            line_name(newest_tested())
        );
    }

    let mut report = Report {
        importer_version: IMPORTER_VERSION,
        ha_version_tested: TESTED_HA.contains(&line),
        ha_version,
        behaves_like: PINNED_HA,
        newer_stores: Vec::new(),
        converted: Vec::new(),
        converted_entries: Vec::new(),
        dropped: Vec::new(),
        split_devices: Vec::new(),
        parent_devices: Vec::new(),
        dry_run: opts.dry_run,
        imported: Vec::new(),
        replaced: Vec::new(),
        removed: Vec::new(),
        skipped: Vec::new(),
        config_entries: Vec::new(),
        yaml: Vec::new(),
        automations: Vec::new(),
    };

    if !source.join(".storage").is_dir() {
        bail!(
            "{} has no .storage directory: has this Home Assistant been set up?",
            source.display()
        );
    }
    // Through a symlinked `.storage` the two directories could be one
    // (the roots were compared above): what the import clears or replaces
    // in the destination would be the source's files.
    for (dir, whose) in [(&source, "source"), (&dest, "destination")] {
        let storage = dir.join(".storage");
        if std::fs::symlink_metadata(&storage).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!(
                "the {whose}'s .storage ({}) is a symlink; use the real directory",
                storage.display()
            );
        }
    }
    // Checked before anything is replaced: the report is written last.
    if std::fs::symlink_metadata(dest.join(REPORT_FILE)).is_ok_and(|m| !m.is_file()) {
        bail!(
            "{} is not a regular file; move it away first",
            dest.join(REPORT_FILE).display()
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
        let mode = if private { 0o600 } else { 0o644 };
        staging.put(&Path::new(".storage").join(&key), &bytes, mode)?;
    }

    // 1a. Before anything is loaded: what Home Assistant 2026.8 to 2026.11
    //     store in a newer form goes back to 2026.6.1's, in the staged
    //     copy. Then a store, or a config entry, still newer than hars can
    //     read refuses the whole import.
    let converted = convert::run(&staging.root, &report.ha_version)?;
    report.converted = converted.stores;
    report.converted_entries = converted.entries;
    report.dropped = converted.dropped;
    report.split_devices = converted.split_devices;
    report.parent_devices = converted.parent_devices;
    // An action on a label reaches a labelled device's children from
    // 2026.10 (`helpers/target.py`; 2026.9 keeps a label to the devices
    // that have it), so before that there is nothing to say of labels.
    if release_line(&report.ha_version).is_some_and(|line| line < (2026, 10)) {
        for parent in &mut report.parent_devices {
            parent.labels.clear();
        }
    }
    report.newer_stores = check_store_versions(&staging, &report.ha_version)?;
    let entries = load_entries(&staging.root)?;
    check_entry_versions(&entries, &report.ha_version)?;
    report.config_entries = entry_reports(&entries);

    // 1b. The YAML configuration, staged byte for byte with its mode bits
    //     (`secrets.yaml` owner-only).
    let mut secret_values = Vec::new();
    if source.join("configuration.yaml").is_file() {
        (report.yaml, secret_values) = yaml_files(&source)?;
        for rel in &report.yaml {
            check_path(&dest, Path::new(rel), false)?;
        }
    }
    for rel in &report.yaml {
        let from = source.join(rel);
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&from)?.permissions().mode() & 0o777
        };
        #[cfg(not(unix))]
        let mode = 0o644;
        let mode = if rel.ends_with("secrets.yaml") {
            0o600
        } else {
            mode
        };
        staging.put(Path::new(rel), &std::fs::read(&from)?, mode)?;
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
    ha_components::Components::load_strict(&staging.root)
        .map_err(|e| anyhow::anyhow!("the zones or people don't load ({})", sanitized(&e)))?;
    ha_api::Lovelace::load_strict(&staging.root)
        .map_err(|e| anyhow::anyhow!("the dashboards don't load ({})", sanitized(&e)))?;
    // `load_strict` reads the categories leniently (hars only lists them,
    // and a scope that isn't a list of rows lists as empty).
    #[derive(serde::Deserialize)]
    struct Categories {
        #[serde(default)]
        #[allow(dead_code)]
        categories: std::collections::BTreeMap<String, Vec<Map<String, Value>>>,
    }
    Store::new(&staging.root, ha_registry::CATEGORY_KEY, 1, 2, false)
        .load::<Categories>()
        .map_err(|e| anyhow::anyhow!("core.category_registry doesn't load ({})", sanitized(&e)))?;
    ha_integrations::mobile_app::MobileApp::check_store(&staging.root)
        .map_err(|e| anyhow::anyhow!("mobile_app's store doesn't load ({})", sanitized(&e)))?;
    ha_core::restore::RestoreState::check(&staging.root)
        .map_err(|e| anyhow::anyhow!("core.restore_state doesn't load ({})", sanitized(&e)))?;
    ha_api::Labs::check(&staging.root)
        .map_err(|e| anyhow::anyhow!("core.labs doesn't load ({})", sanitized(&e)))?;
    ha_api::Analytics::check(&staging.root)
        .map_err(|e| anyhow::anyhow!("core.analytics doesn't load ({})", sanitized(&e)))?;
    if !report.yaml.is_empty() {
        let secrets = ha_core::yaml::Secrets {
            config_dir: staging.root.clone(),
        };
        // The loader's errors name the problem, not secret values.
        let config =
            ha_core::yaml::load_file(&staging.root.join("configuration.yaml"), Some(&secrets))
                .map_err(|e| {
                    anyhow::anyhow!(
                        "configuration.yaml doesn't load ({})",
                        redact(&e.to_string(), &secret_values)
                    )
                })?;
        // The device ids that mean something else in hars than in the
        // Home Assistant the files come from, and what to say of each.
        let changed_ids: Vec<(&str, String)> = report
            .split_devices
            .iter()
            .map(|s| {
                let why = format!(
                    "it names a device by the id {}, which Home Assistant 2026.8 replaced \
                     with {}: in hars the id reaches no device",
                    s.old_id,
                    s.new_ids.join(" and ")
                );
                (s.old_id.as_str(), why)
            })
            .chain(report.parent_devices.iter().map(|p| {
                let why = format!(
                    "it names the device {}, which has child devices: Home Assistant 2026.9 \
                     and later reach their entities through it, hars only its own",
                    p.id
                );
                (p.id.as_str(), why)
            }))
            .collect();
        for item in ha_automation::from_config(&config) {
            // Secret values never reach the report or the terminal.
            let text = |k: &str| {
                item.get(k)
                    .and_then(Value::as_str)
                    .map(|v| redact(v, &secret_values))
            };
            let parsed = ha_automation::config::parse(&item);
            // As loaded, so however the YAML spells the id.
            let loaded = item.to_string();
            let mut warnings: Vec<&str> = changed_ids
                .iter()
                .filter(|(id, _)| loaded.contains(id))
                .map(|(_, why)| why.as_str())
                .collect();
            if excludes(&item) {
                warnings.push(
                    "a target in it has `exclude` (Home Assistant 2026.11), which hars doesn't \
                     have: that action fails when it runs",
                );
            }
            report.automations.push(AutomationReport {
                id: text("id"),
                alias: text("alias"),
                status: if parsed.is_ok() { "ok" } else { "unavailable" },
                reason: parsed.err().map(|e| redact(&e, &secret_values)),
                warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
            });
        }
        // Where else such an id is used: each YAML file by its text, and
        // the configuration as loaded (which reads an id however it is
        // spelt) when no file's text shows it.
        let loaded = config.to_json().to_string();
        let files: Vec<(String, String)> = report
            .yaml
            .iter()
            .filter(|rel| !rel.ends_with("secrets.yaml"))
            .filter_map(|rel| {
                let text = std::fs::read_to_string(staging.root.join(rel)).ok()?;
                Some((rel.clone(), text))
            })
            .collect();
        let yaml_uses = |id: &str| -> Vec<String> {
            // In a comment only: not a use.
            if !loaded.contains(id) {
                return Vec::new();
            }
            let mut found: Vec<String> = files
                .iter()
                .filter(|(_, text)| text.contains(id))
                .map(|(rel, _)| rel.clone())
                .collect();
            if found.is_empty() {
                found.push("configuration.yaml or a file it includes".into());
            }
            found
        };
        for split in &mut report.split_devices {
            split.used_in = yaml_uses(&split.old_id);
        }
        for parent in &mut report.parent_devices {
            parent.used_in = yaml_uses(&parent.id);
        }
    }
    // And the dashboards, as loaded.
    for key in staging.keys()? {
        if !ha_api::Lovelace::is_store(&key) {
            continue;
        }
        let store = Store::new(&staging.root, key.clone(), 1, 1, false);
        let Ok(Some(dashboard)) = store.load::<Value>() else {
            continue;
        };
        let loaded = dashboard.to_string();
        for split in &mut report.split_devices {
            if loaded.contains(&split.old_id) {
                split.used_in.push(format!(".storage/{key}"));
            }
        }
        for parent in &mut report.parent_devices {
            if loaded.contains(&parent.id) {
                parent.used_in.push(format!(".storage/{key}"));
            }
        }
    }
    // A parent nothing names by id, and with no label an action could
    // name, needs no mention.
    report
        .parent_devices
        .retain(|p| !p.used_in.is_empty() || !p.labels.is_empty());
    for key in staging.keys()? {
        if key == "frontend.system_data" || key.starts_with(USER_DATA_PREFIX) {
            let store = Store::new(&staging.root, key.clone(), 1, 1, false);
            if let Err(e) = store.load::<Map<String, Value>>() {
                std::fs::remove_file(store.path())?;
                report.newer_stores.retain(|s| s.key != key);
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
    // The plan compares names, so first every name it will use must be the
    // name the destination's file system has for that file: one that is
    // there under another spelling (`AUTH` for `auth`, where case doesn't
    // count) would be replaced without being seen as there, or put and
    // removed in one import.
    let stores = STORES.iter().map(|(key, ..)| (*key).to_owned());
    for key in staged.iter().cloned().chain(stores) {
        exact_name(&dest, &Path::new(".storage").join(key))?;
    }
    for rel in &report.yaml {
        exact_name(&dest, Path::new(rel))?;
    }
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
            // Left out because it doesn't load, not because the source
            // has none: the destination keeps its own.
            false if report.skipped.iter().any(|s| s.key == *key) => {}
            false => report.removed.push(key.clone()),
            true if read(&dest, key) != read(&staging.root, key) => {
                report.replaced.push(key.clone())
            }
            true => {}
        }
    }
    for rel in &report.yaml {
        if dest.join(rel).exists()
            && std::fs::read(dest.join(rel)).ok() != std::fs::read(staging.root.join(rel)).ok()
        {
            report.replaced.push(rel.clone());
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
            changes.push(Change::Put(format!(".storage/{key}")));
        }
    }
    changes.extend(
        report
            .removed
            .iter()
            .map(|k| Change::Remove(format!(".storage/{k}"))),
    );
    for rel in &report.yaml {
        // A secrets file is owner-only even when its bytes didn't change:
        // then too it is replaced by the staged one, in the commit, where
        // a failure is rolled back. (Not changed in place: the file there
        // may be a hard link to another, the source's even.)
        #[cfg(unix)]
        let loose = rel.ends_with("secrets.yaml")
            && std::fs::metadata(dest.join(rel)).is_ok_and(|m| {
                use std::os::unix::fs::PermissionsExt;
                m.permissions().mode() & 0o777 != 0o600
            });
        #[cfg(not(unix))]
        let loose = false;
        if loose || std::fs::read(dest.join(rel)).ok() != std::fs::read(staging.root.join(rel)).ok()
        {
            changes.push(Change::Put(rel.clone()));
        }
    }
    commit(&staging, &dest, &changes, |from, to| {
        std::fs::rename(from, to)
    })?;

    let json = serde_json::to_vec_pretty(&report)?;
    // A new file, made in the staging directory (this run's own) and
    // renamed into place: never a half-written report, and nothing
    // written through, or over, a file that was already there.
    let staged = staging.root.join(REPORT_FILE);
    let written = std::fs::write(&staged, json)
        .and_then(|()| std::fs::rename(&staged, dest.join(REPORT_FILE)));
    if let Err(e) = written {
        // The files are in place and can't be taken back now: what the
        // report would have said is said here.
        let said = summary(&report, &dest);
        let said: Vec<&str> = said
            .lines()
            .filter(|l| !l.starts_with("Report: "))
            .collect();
        println!("{}", said.join("\n"));
        return Err(e).with_context(|| {
            format!(
                "everything above was imported, but {REPORT_FILE} could not be written in {}",
                dest.display()
            )
        });
    }
    Ok(report)
}

/// Whether a `target` anywhere in an automation has `exclude`, which
/// 2026.11 added (`helpers/target.py`) and 2026.6.1's service schemas
/// reject.
fn excludes(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, inner)| {
            (key == "target" && inner.get("exclude").is_some()) || excludes(inner)
        }),
        Value::Array(items) => items.iter().any(excludes),
        _ => false,
    }
}

/// Whether the source and the place the destination will be are one
/// directory, or one is inside the other, going by what each directory is
/// to its file system (`id`) and not by its path: the same directory
/// mounted twice (two bind mounts, say) has two paths that share nothing.
/// Checked before anything is made. The paths are absolute.
fn overlap<Id: PartialEq>(
    source: &Path,
    planned: &Path,
    id: impl Fn(&Path) -> Option<Id>,
) -> Option<&'static str> {
    let source_id = id(source)?;
    // The destination, or the nearest directory above it that exists, and
    // every directory above that.
    if planned.ancestors().filter_map(&id).any(|a| a == source_id) {
        return Some(
            "the destination is inside the source, or is the source (the same directory under \
             another path); use a directory outside it",
        );
    }
    let dest_id = id(planned)?;
    if source.ancestors().filter_map(&id).any(|a| a == dest_id) {
        return Some(
            "the source is inside the destination (the same directory under another path); \
             copy it somewhere else first",
        );
    }
    None
}

/// Whether a name is one the file system has under another spelling: it
/// finds a file by it, and the directory doesn't list it.
fn is_alias(listed: &[std::ffi::OsString], name: &std::ffi::OsStr, found: bool) -> bool {
    found && !listed.iter().any(|l| l == name)
}

/// Each part of `rel` that exists under `root` must be listed there by
/// exactly that name. Asked of the file system, which alone knows the
/// names it folds; the import stops at one it would otherwise take for
/// another file.
fn exact_name(root: &Path, rel: &Path) -> anyhow::Result<()> {
    let mut at = root.to_path_buf();
    for part in rel.components() {
        let dir = at.clone();
        at.push(part);
        let found = std::fs::symlink_metadata(&at).is_ok();
        if !found {
            return Ok(());
        }
        let listed: Vec<std::ffi::OsString> = std::fs::read_dir(&dir)
            .with_context(|| format!("listing {}", dir.display()))?
            .filter_map(|e| Some(e.ok()?.file_name()))
            .collect();
        if is_alias(&listed, part.as_os_str(), found) {
            let theirs = listed
                .iter()
                .find(|l| same_file(&dir.join(l), &at))
                .map_or("a file of another name".into(), |l| {
                    l.to_string_lossy().into_owned()
                });
            bail!(
                "{theirs} in {} and {}, which the import brings or hars reads, are one file \
                 there: its file system takes the two names for the same. Rename or remove \
                 {theirs}, then import again. Nothing was imported",
                dir.display(),
                rel.display()
            );
        }
    }
    Ok(())
}

/// Whether two paths are the same file or directory to the file system,
/// whatever they are called: the same device and inode. (Elsewhere than
/// Unix, the same resolved path.)
fn same_file(a: &Path, b: &Path) -> bool {
    file_id(a).is_some_and(|id| Some(id) == file_id(b))
}

/// What a file is to its file system, links followed: device and inode.
#[cfg(unix)]
fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn file_id(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// Where a path that may not exist yet will be: absolute, each part that
/// exists resolved (symlinks), and a `..` taken off what is before it, as
/// the system takes it once the directories are made.
fn planned_path(path: &Path) -> std::io::Result<PathBuf> {
    let mut out = PathBuf::new();
    for part in std::path::absolute(path)?.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            part => {
                out.push(part);
                if let Ok(real) = std::fs::canonicalize(&out) {
                    out = real;
                }
            }
        }
    }
    Ok(out)
}

/// Whether `hars` reads a store, and whether it is written owner-only.
fn supported(key: &str) -> Option<bool> {
    spec(key).map(|(_, private, ..)| private)
}

/// The store's row in [`STORES`], or the one its family has.
fn spec(key: &str) -> Option<StoreSpec> {
    if let Some(spec) = STORES.iter().find(|(k, ..)| *k == key) {
        return Some(*spec);
    }
    if key == ha_components::PERSON_KEY {
        // Version 1 (`persons`) is migrated by the loader. A newer minor
        // version isn't taken: `PersonStore._async_migrate_func` runs on
        // any other version and expects the version 1 shape.
        Some(("", false, (2, 1), Minor::NotNewer))
    } else if key.starts_with(USER_DATA_PREFIX)
        || ha_api::Lovelace::is_store(key)
        || ha_components::Components::is_store(key)
    {
        Some(("", false, (1, 1), Minor::Any))
    } else {
        None
    }
}

/// `2026.7.4` (or `2026.7.0b3`, `2026.8.0.dev202607010215`) as `(2026, 7)`.
fn release_line(version: &str) -> Option<(u32, u32)> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let mut parts = version.split('.');
    let (year, month) = (parts.next()?, parts.next()?);
    if !digits(year) || !digits(month) {
        return None;
    }
    // The patch number, then at most a beta tag (`b3`) and one `.dev<date>`.
    let patch = parts.next()?;
    let (number, beta) = patch.split_once('b').unwrap_or((patch, "0"));
    let dev = parts
        .next()
        .is_none_or(|p| p.strip_prefix("dev").is_some_and(digits));
    let (year, month) = (year.parse().ok()?, month.parse().ok()?);
    let ok = digits(number) && digits(beta) && dev && parts.next().is_none();
    (ok && (1..=12).contains(&month)).then_some((year, month))
}

fn line_name((year, month): (u32, u32)) -> String {
    format!("{year}.{month}")
}

fn newest_tested() -> (u32, u32) {
    *TESTED_HA.last().expect("a tested release")
}

/// `2026.6 to 2026.11`.
fn importable() -> String {
    let first = *TESTED_HA.first().expect("a tested release");
    format!("{} to {}", line_name(first), line_name(newest_tested()))
}

/// Every staged store's storage version against the one `hars` reads. A
/// newer major version, or a minor version [`Minor`] rules out, is an error
/// naming both; the newer minor versions that are taken are returned for
/// the report. A file that isn't a store envelope is left to its loader,
/// which says so.
fn check_store_versions(staging: &Staging, ha_version: &str) -> anyhow::Result<Vec<NewerStore>> {
    #[derive(serde::Deserialize)]
    struct Head {
        version: u32,
        minor_version: Option<u32>,
    }
    let mut newer = Vec::new();
    for key in staging.keys()? {
        let Some((_, _, reads, minor)) = spec(&key) else {
            continue;
        };
        let bytes = std::fs::read(staging.root.join(".storage").join(&key))?;
        let Ok(head) = serde_json::from_slice::<Head>(&bytes) else {
            continue;
        };
        // `Store.async_load`: no minor version means 1.
        let found = (head.version, head.minor_version.unwrap_or(1));
        if found.0 > reads.0 {
            bail!("{}", too_new(&key, found, reads, ha_version));
        }
        if found.0 < reads.0 || found.1 == reads.1 {
            // An older major version is the loader's to migrate or refuse.
            continue;
        }
        if found.1 < reads.1 {
            if minor == Minor::Exact {
                bail!("{}", too_old(&key, found, reads));
            }
            continue;
        }
        if minor != Minor::Any {
            bail!("{}", too_new(&key, found, reads, ha_version));
        }
        newer.push(NewerStore {
            key,
            version: format!("{}.{}", found.0, found.1),
            hars_writes: format!("{}.{}", reads.0, reads.1),
        });
    }
    Ok(newer)
}

/// Why a store from a newer Home Assistant stops the import.
fn too_new(key: &str, found: (u32, u32), reads: (u32, u32), ha_version: &str) -> String {
    let mut text = format!(
        "{key} is storage version {}.{}, in a config directory of Home Assistant \
         {ha_version}; hars reads version {}.{}, as Home Assistant {PINNED_HA} does",
        found.0, found.1, reads.0, reads.1
    );
    if key == ha_registry::DEVICE_KEY {
        text.push_str(
            ", and converts versions 3.2 and 3.4 to it (Home Assistant 2026.8 to 2026.11)",
        );
    } else if key == ha_registry::ENTITY_KEY {
        text.push_str(
            ", and converts versions 1.23 and 1.24 to it (Home Assistant 2026.8 to 2026.11)",
        );
    }
    text.push_str(". ");
    text.push_str(&cannot_import());
    text
}

/// Why a registry or the config entries at an older minor version stop the
/// import ([`Minor::Exact`]).
fn too_old(key: &str, found: (u32, u32), reads: (u32, u32)) -> String {
    format!(
        "{key} is storage version {}.{}, older than the {}.{} hars reads and writes: Home \
         Assistant brings it up to date when it starts. Start Home Assistant {} on this \
         config directory once, then copy it again. Nothing was imported",
        found.0,
        found.1,
        reads.0,
        reads.1,
        importable()
    )
}

/// What to do about a directory from too new a release.
fn cannot_import() -> String {
    format!(
        "hars imports Home Assistant {}, and none of those releases writes this: import a \
         copy or backup of this config directory made while it ran one of them. Nothing was \
         imported",
        importable()
    )
}

/// `ConfigEntry.async_migrate`: an entry of an integration `hars` runs,
/// written by a newer version of its config flow, can't be set up. The
/// ones with a rule (`lifx` and `met` at 2.1, from 2026.10 and 2026.11)
/// are back at version 1 by now ([`convert`]); any other is refused, and so
/// is a minor version later than the ones the fixtures hold. A
/// disabled or ignored one is refused as well, which is stricter than HA
/// (it only migrates an entry it sets up): enabling it later would
/// otherwise run it.
fn check_entry_versions(entries: &[Map<String, Value>], ha_version: &str) -> anyhow::Result<()> {
    for e in entries {
        let domain = e.get("domain").and_then(Value::as_str).unwrap_or_default();
        // A newer minor version is compatible by Home Assistant's rule, and
        // hars runs one. The import takes the ones it has seen (`hue` 1.2,
        // from 2026.10), not ones no fixture holds.
        let seen = if domain == ha_integrations::hue::DOMAIN {
            2
        } else {
            1
        };
        let minor = e.get("minor_version").and_then(Value::as_u64).unwrap_or(1);
        if runs(domain) && (ha_integrations::entry_too_new(e) || minor > seen) {
            bail!(
                "the {domain} integration's config entry {:?} is version {}.{}, in a config \
                 directory of Home Assistant {ha_version}; hars runs version {} of it, as \
                 Home Assistant {PINNED_HA} does, and has no rule to bring this one back to \
                 it. {}",
                e.get("title").and_then(Value::as_str).unwrap_or_default(),
                e.get("version").cloned().unwrap_or_default(),
                e.get("minor_version").and_then(Value::as_u64).unwrap_or(1),
                ha_integrations::ENTRY_VERSION,
                cannot_import()
            );
        }
    }
    Ok(())
}

fn unsupported_reason(key: &str) -> &'static str {
    match key {
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
/// Only ever one this run made: a directory already there (an import that
/// was killed, another one running, or someone's files) is never cleared.
struct Staging {
    root: PathBuf,
    /// Every directory and file staged, by the relative path asked for.
    staged: std::cell::RefCell<std::collections::HashSet<PathBuf>>,
}

/// The names in a config directory the importer keeps for itself.
const RESERVED: [&str; 4] = [".storage", ".import-staging", BACKUP_DIR, REPORT_FILE];

impl Staging {
    /// The staging directory, with every reserved name taken in it: the
    /// staging directory is on the destination's file system, so what that
    /// file system takes for one of those names (in another case, or in
    /// some other spelling it folds) can't be staged beside it.
    fn new(dest: &Path) -> anyhow::Result<Self> {
        let root = dest.join(".import-staging");
        std::fs::create_dir(&root).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => anyhow::anyhow!(
                "{} is already there: another import is running, or one was interrupted. \
                 If none is running, delete it and import again",
                root.display()
            ),
            _ => anyhow::Error::from(e).context(format!("making {}", root.display())),
        })?;
        let staging = Self {
            root,
            staged: Default::default(),
        };
        for name in RESERVED {
            if name == REPORT_FILE {
                std::fs::write(staging.root.join(name), "")?;
            } else {
                std::fs::create_dir(staging.root.join(name))?;
            }
        }
        // Stores are staged in it.
        staging.staged.borrow_mut().insert(".storage".into());
        Ok(staging)
    }

    /// Stage a file as a new one, each directory on the way as a new one
    /// too unless it was staged under that same name.
    ///
    /// Two names the destination's file system takes for one file (`a.yaml`
    /// and `A.yaml` where case doesn't count) would otherwise be staged one
    /// over the other and committed as two changes to one file. They are
    /// found by making, not by comparing names: only the file system knows
    /// which names it folds.
    fn put(&self, rel: &Path, bytes: &[u8], mode: u32) -> anyhow::Result<()> {
        use std::io::Write;
        let taken = |at: &Path| {
            let reserved = RESERVED.iter().any(|name| {
                at.parent() == Some(Path::new(""))
                    && same_file(&self.root.join(name), &self.root.join(at))
            });
            if reserved {
                return anyhow::anyhow!("{} is a path hars keeps for itself", rel.display());
            }
            let staged = self.staged.borrow();
            let other = staged
                .iter()
                .find(|other| same_file(&self.root.join(other), &self.root.join(at)));
            anyhow::anyhow!(
                "{} and {} are one file in the destination, whose file system takes the two \
                 names for the same: the import can't bring both. Nothing was imported",
                at.display(),
                other.map_or("another file the import brings".into(), |o| o
                    .display()
                    .to_string())
            )
        };
        let mut dirs: Vec<&Path> = rel.ancestors().skip(1).collect();
        dirs.reverse();
        for dir in dirs.into_iter().filter(|d| !d.as_os_str().is_empty()) {
            if self.staged.borrow().contains(dir) {
                continue;
            }
            match std::fs::create_dir(self.root.join(dir)) {
                Ok(()) => self.staged.borrow_mut().insert(dir.to_path_buf()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(taken(dir)),
                Err(e) => return Err(e.into()),
            };
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        let mut file = match options.open(self.root.join(rel)) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(taken(rel)),
            Err(e) => return Err(e.into()),
        };
        file.write_all(bytes)?;
        // The mode asked for, whatever the umask.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        file.sync_all()?;
        self.staged.borrow_mut().insert(rel.to_path_buf());
        Ok(())
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

/// Apply the changes with renames. Each change is a path relative to the
/// config directory (`.storage/<key>`, or a YAML file). The destination's
/// current files are moved to `.import-backup` first; if any step fails,
/// everything done so far is undone. `rename` is injectable so the
/// rollback can be tested.
fn commit(
    staging: &Staging,
    dest: &Path,
    changes: &[Change],
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> anyhow::Result<()> {
    let backup = dest.join(BACKUP_DIR);
    // `run` refuses to start while a backup is left over; never delete one.
    std::fs::create_dir(&backup).with_context(|| format!("{} already exists", backup.display()))?;
    std::fs::create_dir_all(dest.join(".storage"))?;

    // What was done, to undo in reverse on failure.
    let mut moved_out: Vec<String> = Vec::new();
    let mut put: Vec<String> = Vec::new();
    // Directories this commit made, to remove on rollback (deepest last).
    let mut made: Vec<PathBuf> = Vec::new();
    let mut parent = |p: &Path| -> std::io::Result<()> {
        let Some(d) = p.parent() else { return Ok(()) };
        let mut missing = Vec::new();
        let mut at = Some(d);
        while let Some(x) = at.filter(|x| !x.exists()) {
            missing.push(x.to_path_buf());
            at = x.parent();
        }
        for x in missing.into_iter().rev() {
            std::fs::create_dir(&x)?;
            made.push(x);
        }
        Ok(())
    };
    let result = (|| -> std::io::Result<()> {
        for change in changes {
            let (Change::Put(rel) | Change::Remove(rel)) = change;
            let current = dest.join(rel);
            if current.exists() {
                let saved = backup.join(rel);
                parent(&saved)?;
                // The backup is the only copy of what was there: nothing
                // is moved over a file already in it.
                if std::fs::symlink_metadata(&saved).is_ok() {
                    return Err(std::io::Error::other(format!(
                        "{rel} was already moved aside: two of the files to replace are one file"
                    )));
                }
                rename(&current, &saved)?;
                moved_out.push(rel.clone());
            }
            if let Change::Put(rel) = change {
                parent(&current)?;
                rename(&staging.root.join(rel), &current)?;
                put.push(rel.clone());
            }
        }
        #[cfg(unix)]
        {
            std::fs::File::open(dest.join(".storage"))?.sync_all()?;
            std::fs::File::open(dest)?.sync_all()?;
        }
        Ok(())
    })();

    if let Err(e) = result {
        for rel in put.iter().rev() {
            let _ = std::fs::remove_file(dest.join(rel));
        }
        let mut restored = true;
        for rel in moved_out.iter().rev() {
            restored &= std::fs::rename(backup.join(rel), dest.join(rel)).is_ok();
        }
        for d in made.iter().rev().filter(|d| !d.starts_with(&backup)) {
            let _ = std::fs::remove_dir(d);
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

/// The YAML configuration HA reads, found by loading it with hars's own
/// loader (so exactly the files it follows): `configuration.yaml`, every
/// file its `!include*` tags reach, and each `secrets.yaml` a secret came
/// from. Relative paths, sorted, and the secret values used. Anything
/// outside the config directory, through a symlink, or in a path the
/// importer reserves is refused.
fn yaml_files(source: &Path) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let secrets = ha_core::yaml::Secrets {
        config_dir: source.to_path_buf(),
    };
    let (_, trace) =
        ha_core::yaml::load_file_traced(&source.join("configuration.yaml"), Some(&secrets))
            .map_err(|e| anyhow::anyhow!("configuration.yaml doesn't load ({e})"))?;
    let mut files = Vec::new();
    for f in &trace.files {
        let rel = f
            .strip_prefix(source)
            .ok()
            .and_then(normalize)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "an include reaches {}, outside the config directory",
                    f.display()
                )
            })?;
        check_path(source, &rel, true)?;
        files.push(rel.to_string_lossy().into_owned());
    }
    files.sort();
    files.dedup();
    Ok((files, trace.secret_values))
}

/// `a/./b/../c` as `a/c`, or `None` if it leaves the root (or is absolute).
fn normalize(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::Normal(x) => out.push(x),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

/// A relative YAML path is usable in `root`: not a path the importer or
/// HA's storage owns, no component a symlink, and the file itself, where
/// it exists (in the source it must), a regular file.
fn check_path(root: &Path, rel: &Path, source: bool) -> anyhow::Result<()> {
    let first = rel
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_default();
    // Whatever its case: on a file system that takes `IMPORT-REPORT.JSON`
    // and `import-report.json` for one name, the report would replace it.
    let lower = first.to_lowercase();
    if lower == ".storage" || lower.starts_with(".import") || lower == REPORT_FILE {
        bail!("{} is a path hars keeps for itself", rel.display());
    }
    let mut at = root.to_path_buf();
    let n = rel.components().count();
    for (i, c) in rel.components().enumerate() {
        at.push(c);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_symlink() => {
                bail!(
                    "{} goes through a symlink; copy the real files",
                    rel.display()
                )
            }
            // In the destination too: replacing would move a whole
            // directory aside and then delete it with the backup.
            Ok(m) if i + 1 == n && !m.is_file() => {
                let whose = if source { "" } else { " in the destination" };
                bail!("{} is not a regular file{whose}", rel.display())
            }
            Ok(_) => {}
            // Not in the destination yet: nothing further to check.
            Err(_) if !source => return Ok(()),
            Err(e) => bail!("{}: {:?}", rel.display(), e.kind()),
        }
    }
    Ok(())
}

/// `text` with each secret value replaced.
fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_owned();
    for s in secrets.iter().filter(|s| !s.is_empty()) {
        out = out.replace(s.as_str(), "<secret>");
    }
    out
}

/// What the import did with an automation.
#[derive(Debug, Serialize)]
pub struct AutomationReport {
    pub id: Option<String>,
    pub alias: Option<String>,
    /// `ok`, or `unavailable` (invalid or not supported by hars yet).
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// For one that loads: what it will do differently than in the Home
    /// Assistant it comes from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
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

/// `core.config_entries`' entries.
fn load_entries(config_dir: &Path) -> anyhow::Result<Vec<Map<String, Value>>> {
    #[derive(serde::Deserialize)]
    struct Entries {
        #[serde(default)]
        entries: Vec<Map<String, Value>>,
    }
    let store = Store::new(config_dir, "core.config_entries", 1, 5, true);
    Ok(store
        .load::<Entries>()
        .map_err(|e| anyhow::anyhow!("core.config_entries doesn't load ({})", sanitized(&e)))?
        .map(|data| data.entries)
        .unwrap_or_default())
}

/// Which integrations the install uses.
fn entry_reports(entries: &[Map<String, Value>]) -> Vec<EntryReport> {
    let text = |e: &Map<String, Value>, k: &str| {
        e.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    entries
        .iter()
        .map(|e| {
            let domain = text(e, "domain");
            EntryReport {
                entry_id: text(e, "entry_id"),
                supported: runs(&domain),
                domain,
                title: text(e, "title"),
                disabled: e.get("disabled_by").is_some_and(|v| !v.is_null()),
            }
        })
        .collect()
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
    // What someone coming from a newer release should expect.
    if release_line(&report.ha_version).is_some_and(|l| Some(l) != release_line(PINNED_HA)) {
        out.push_str(&format!(
            "  hars behaves like Home Assistant {PINNED_HA} and shows the {} app: \
             what Home Assistant added after that is not there\n",
            line_name(release_line(PINNED_HA).expect("the pin is a release"))
        ));
    }
    if !report.ha_version_tested {
        out.push_str(&format!(
            "  hars is not tested with Home Assistant {} (it is with {}): \
             every store passed its checks, but look the result over\n",
            report.ha_version,
            importable()
        ));
    }
    for s in &report.newer_stores {
        out.push_str(&format!(
            "  {} is storage version {}: hars writes {} and, like Home Assistant \
             {PINNED_HA}, drops what that version doesn't have when it next changes the store\n",
            s.key, s.version, s.hars_writes
        ));
    }
    // What was put back into the 2026.6.1 form, one change a line.
    for c in &report.converted {
        if c.from == c.to {
            out.push_str(&format!("  converted {}:\n", c.key));
        } else {
            out.push_str(&format!(
                "  converted {} from storage version {} to {}:\n",
                c.key, c.from, c.to
            ));
        }
        for change in &c.changes {
            out.push_str(&format!("    {change}\n"));
        }
    }
    for e in &report.converted_entries {
        out.push_str(&format!(
            "  converted the {} entry {:?} from version {} to {}\n",
            e.domain, e.title, e.from, e.to
        ));
        for change in &e.changes {
            out.push_str(&format!("    {change}\n"));
        }
    }
    for d in &report.dropped {
        out.push_str(&format!(
            "  left out of {}: {} ({})\n",
            d.store, d.what, d.reason
        ));
    }
    for s in &report.split_devices {
        for file in &s.used_in {
            out.push_str(&format!(
                "  {file} still uses the id {} of a device that Home Assistant 2026.8 \
                 replaced with {} ({}): hars finds no device by it; use the new ids {}\n",
                s.old_id,
                match s.new_ids.len() {
                    2 => "two".to_owned(),
                    n => n.to_string(),
                },
                s.name.as_deref().unwrap_or("unnamed"),
                s.new_ids.join(", ")
            ));
        }
    }
    for p in &report.parent_devices {
        let name = p.name.as_deref().unwrap_or("unnamed");
        if !p.used_in.is_empty() {
            out.push_str(&format!(
                "  {} names the device {} ({name}) by id. It has child devices, and Home \
                 Assistant 2026.9 and later reach their entities through it; hars reaches only \
                 its own: name the child devices too ({})\n",
                p.used_in.join(", "),
                p.id,
                p.children.join(", ")
            ));
        }
        if !p.labels.is_empty() {
            out.push_str(&format!(
                "  the device {} ({name}) has child devices and the label(s) {}. An action on \
                 such a label reaches the child devices' entities in Home Assistant 2026.10 and \
                 later; in hars only if they have the label too ({})\n",
                p.id,
                p.labels.join(", "),
                p.children.join(", ")
            ));
        }
    }
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
        out.push_str(&format!("  YAML: {}\n", report.yaml.join(", ")));
    }
    for a in report.automations.iter().filter(|a| a.status != "ok") {
        out.push_str(&format!(
            "  automation {} will be unavailable: {}\n",
            a.alias.as_deref().or(a.id.as_deref()).unwrap_or("?"),
            a.reason.as_deref().unwrap_or_default()
        ));
    }
    for a in report.automations.iter().filter(|a| a.status == "ok") {
        if let Some(warning) = &a.warning {
            out.push_str(&format!(
                "  automation {} loads, but {warning}\n",
                a.alias.as_deref().or(a.id.as_deref()).unwrap_or("?")
            ));
        }
    }
    let ok = report
        .automations
        .iter()
        .filter(|a| a.status == "ok")
        .count();
    if !report.automations.is_empty() {
        out.push_str(&format!(
            "  automations: {ok} of {} supported\n",
            report.automations.len()
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
            Change::Put(".storage/auth".into()),
            Change::Put(".storage/auth_provider.homeassistant".into()),
            Change::Remove(".storage/onboarding".into()),
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

    /// What was moved aside is never written over: a second change to a
    /// file fails and is rolled back to the first's old file, not to the
    /// new one moved over it.
    #[test]
    fn a_backup_is_never_written_over() {
        let dest = std::env::temp_dir().join(format!("hars-import-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::create_dir_all(dest.join(".storage")).unwrap();
        std::fs::write(dest.join(".storage/auth"), "old auth").unwrap();
        let staging = Staging::new(&dest).unwrap();
        staging
            .put(Path::new(".storage/auth"), b"new auth", 0o600)
            .unwrap();
        let changes = [
            Change::Put(".storage/auth".into()),
            Change::Put(".storage/auth".into()),
        ];
        let err = commit(&staging, &dest, &changes, |f, t| std::fs::rename(f, t)).unwrap_err();
        assert!(err.to_string().contains("rolled back"), "{err}");
        assert!(err.to_string().contains("already moved aside"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dest.join(".storage/auth")).unwrap(),
            "old auth"
        );
        assert!(!dest.join(".import-backup").exists());
        drop(staging);
        std::fs::remove_dir_all(dest).unwrap();
    }

    /// One directory under two paths, as two bind mounts of it are: the
    /// paths share nothing, and the directories are the same.
    #[test]
    fn the_same_directory_under_two_paths() {
        // `/mnt/a` and `/mnt/b` are one directory; so then are what is in
        // them. `/mnt/b/new` isn't there yet.
        let id = |p: &Path| match p.to_str()? {
            "/" => Some(1),
            "/mnt" => Some(2),
            "/mnt/a" | "/mnt/b" => Some(3),
            "/mnt/a/ha" | "/mnt/b/ha" => Some(4),
            "/mnt/a/hars" | "/mnt/b/hars" => Some(5),
            "/elsewhere" => Some(6),
            _ => None,
        };
        let check =
            |source: &str, planned: &str| overlap(Path::new(source), Path::new(planned), id);
        let inside_source =
            |o: Option<&str>| o.is_some_and(|o| o.starts_with("the destination is"));
        let inside_dest = |o: Option<&str>| o.is_some_and(|o| o.starts_with("the source is"));
        // The same folder; a destination in the source, there or not yet.
        assert!(inside_source(check("/mnt/a", "/mnt/b")));
        assert!(inside_source(check("/mnt/a", "/mnt/b/hars")));
        assert!(inside_source(check("/mnt/a", "/mnt/b/new/config")));
        assert!(inside_source(check("/mnt/a/ha", "/mnt/b/ha")));
        // The source in the destination.
        assert!(inside_dest(check("/mnt/a/ha", "/mnt/b")));
        // Side by side, under either path, and somewhere else altogether.
        assert_eq!(check("/mnt/a/ha", "/mnt/b/hars"), None);
        assert_eq!(check("/mnt/a/ha", "/mnt/b/new"), None);
        assert_eq!(check("/mnt/a", "/elsewhere"), None);
        assert_eq!(check("/mnt/a", "/elsewhere/new"), None);

        // On the file system: a directory is itself however it is reached.
        let dir = std::env::temp_dir().join(format!("hars-import-same-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ha/sub")).unwrap();
        std::fs::create_dir_all(dir.join("hars")).unwrap();
        assert!(same_file(&dir.join("ha"), &dir.join("ha/sub/..")));
        assert!(!same_file(&dir.join("ha"), &dir.join("hars")));
        assert!(inside_source(overlap(
            &dir.join("ha"),
            &dir.join("ha/sub/../new"),
            file_id
        )));
        assert_eq!(overlap(&dir.join("ha"), &dir.join("hars"), file_id), None);
        #[cfg(unix)]
        {
            // A second path to the source, as a mount gives one.
            std::os::unix::fs::symlink(dir.join("ha"), dir.join("mounted")).unwrap();
            assert!(inside_source(overlap(
                &dir.join("ha"),
                &dir.join("mounted"),
                file_id
            )));
            assert!(inside_dest(overlap(
                &dir.join("ha/sub"),
                &dir.join("mounted"),
                file_id
            )));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A name found though not listed is another file's name.
    #[test]
    fn aliases() {
        let listed = ["AUTH".into(), "core.config".into()];
        let name = |n: &str| std::ffi::OsString::from(n);
        assert!(is_alias(&listed, &name("auth"), true));
        assert!(!is_alias(&listed, &name("core.config"), true));
        // Not there at all: nothing to take it for.
        assert!(!is_alias(&listed, &name("auth"), false));

        // On the file system: a name as listed passes, and so does one
        // that isn't there; where names fold, another spelling doesn't.
        let dir = std::env::temp_dir().join(format!("hars-import-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Parts")).unwrap();
        std::fs::write(dir.join("Parts/a.yaml"), "").unwrap();
        exact_name(&dir, Path::new("Parts/a.yaml")).unwrap();
        exact_name(&dir, Path::new("Parts/b.yaml")).unwrap();
        exact_name(&dir, Path::new("other/a.yaml")).unwrap();
        if dir.join("parts").exists() {
            for rel in ["parts/a.yaml", "Parts/A.yaml", "parts/new.yaml"] {
                let err = exact_name(&dir, Path::new(rel)).unwrap_err().to_string();
                assert!(err.contains("are one file there"), "{rel}: {err}");
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A file is staged as a new one. A name the file system already has
    /// in the staging directory, under whatever spelling, is refused: a
    /// reserved one, or one staged before. (Here the second name is made
    /// behind the importer's back, as a file system that folds names
    /// would make it.)
    #[test]
    fn staging_never_writes_over_a_name_already_there() {
        let dest = std::env::temp_dir().join(format!("hars-import-put-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::create_dir_all(&dest).unwrap();
        let staging = Staging::new(&dest).unwrap();
        staging.put(Path::new("parts/a.yaml"), b"1", 0o644).unwrap();
        staging.put(Path::new("parts/b.yaml"), b"2", 0o644).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |rel: &str| {
                std::fs::metadata(staging.root.join(rel))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777
            };
            staging.put(Path::new("secrets.yaml"), b"3", 0o600).unwrap();
            assert_eq!((mode("parts/a.yaml"), mode("secrets.yaml")), (0o644, 0o600));
        }
        // The same name again.
        let err = staging
            .put(Path::new("parts/a.yaml"), b"x", 0o644)
            .unwrap_err();
        assert!(
            err.to_string().contains("are one file in the destination"),
            "{err}"
        );
        assert_eq!(
            std::fs::read(staging.root.join("parts/a.yaml")).unwrap(),
            b"1"
        );
        // A file and a directory the file system has, though not staged
        // under those names.
        std::fs::write(staging.root.join("parts/C.yaml"), "kept").unwrap();
        std::fs::create_dir(staging.root.join("Other")).unwrap();
        for rel in ["parts/C.yaml", "Other/d.yaml"] {
            let err = staging.put(Path::new(rel), b"x", 0o644).unwrap_err();
            assert!(
                err.to_string().contains("Nothing was imported"),
                "{rel}: {err}"
            );
        }
        assert_eq!(
            std::fs::read(staging.root.join("parts/C.yaml")).unwrap(),
            b"kept"
        );
        // The names the importer keeps for itself.
        for rel in [
            REPORT_FILE,
            ".import-backup/a.yaml",
            ".import-staging/a.yaml",
        ] {
            let err = staging.put(Path::new(rel), b"x", 0o644).unwrap_err();
            assert!(
                err.to_string().contains("is a path hars keeps for itself"),
                "{rel}: {err}"
            );
        }
        // And, where the file system folds them, their other spellings.
        if staging.root.join("IMPORT-REPORT.JSON").exists() {
            for rel in ["IMPORT-REPORT.JSON", ".Storage/x", "Parts/a.yaml"] {
                assert!(staging.put(Path::new(rel), b"x", 0o644).is_err(), "{rel}");
            }
        }
        if staging.root.join("import-report.j\u{17f}on").exists() {
            let err = staging
                .put(Path::new("import-report.j\u{17f}on"), b"x", 0o644)
                .unwrap_err();
            assert!(
                err.to_string().contains("is a path hars keeps for itself"),
                "{err}"
            );
        }
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

    #[test]
    fn release_lines() {
        for (version, line) in [
            ("2026.6.1", Some((2026, 6))),
            ("2026.7.0b3", Some((2026, 7))),
            ("2026.11.2", Some((2026, 11))),
            ("2026.8.0.dev202607010215", Some((2026, 8))),
            ("2025.12.4", Some((2025, 12))),
            ("2026.7", None),
            ("2026.7.", None),
            ("2026.7.wrong", None),
            ("2026.7.4x", None),
            ("2026.7.4.wrong", None),
            ("2026.7.b3", None),
            ("2026.13.0", None),
            ("2026.0.1", None),
            ("2026.8.0.dev1.dev2", None),
            ("x.7.4", None),
            ("", None),
        ] {
            assert_eq!(release_line(version), line, "{version}");
        }
        // Lines compare as releases do.
        assert!(release_line("2025.12.4") < release_line("2026.6.0"));
        assert!(release_line("2026.11.0") > release_line("2026.7.4"));
    }

    /// A rollback removes the directories the commit made.
    #[test]
    fn rollback_removes_new_directories() {
        let dest = std::env::temp_dir().join(format!("hars-import-dirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("z"), "a file").unwrap();
        let staging = Staging::new(&dest).unwrap();
        for rel in ["a/one.yaml", "z/two.yaml"] {
            let p = staging.root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        }
        let changes = [
            Change::Put("a/one.yaml".into()),
            Change::Put("z/two.yaml".into()),
        ];
        let err = commit(&staging, &dest, &changes, |f, t| std::fs::rename(f, t)).unwrap_err();
        assert!(err.to_string().contains("rolled back"), "{err}");
        assert!(!dest.join("a").exists());
        drop(staging);
        std::fs::remove_dir_all(dest).unwrap();
    }
}
