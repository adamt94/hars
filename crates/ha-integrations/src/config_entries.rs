//! `.storage/core.config_entries` (1.5) at runtime: the entries as loaded,
//! and HA's `async_update_entry` for integrations that change their data
//! (the companion app's registration), saved 1 s after the last change
//! (`config_entries.py`'s `SAVE_DELAY`).

use ha_core::Hass;
use ha_core::storage::{DelayedSave, Store};
use parking_lot::RwLock;
use serde::Serialize;
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub const KEY: &str = "core.config_entries";
const MINOR: u32 = 5;
const SAVE_DELAY: Duration = Duration::from_secs(1);

pub struct ConfigEntries {
    hass: Hass,
    /// `None`: there was no file (a fresh install).
    entries: RwLock<Option<Vec<Map<String, Value>>>>,
    save: Option<Arc<DelayedSave>>,
}

impl std::fmt::Debug for ConfigEntries {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigEntries").finish_non_exhaustive()
    }
}

fn store(dir: &Path) -> Store {
    // Private (0600, not HA's 0644): it holds the app's secret and tokens
    // (spec-registry-writes.md decision D3). HA reads it either way.
    Store::new(dir, KEY, 1, MINOR, true)
}

#[derive(Default, serde::Deserialize)]
struct File {
    #[serde(default)]
    entries: Vec<Map<String, Value>>,
}

impl ConfigEntries {
    /// Read the store (blocking) and save changes back to it. One that
    /// can't be read is logged and treated as no entries, so nothing
    /// starts, and it is never overwritten.
    pub fn load(hass: &Hass, config_dir: &Path) -> Arc<Self> {
        let (entries, writable) = match store(config_dir).load_with_minor::<File>() {
            Ok(Some((file, minor))) => (Some(file.entries), minor == MINOR),
            Ok(None) => (None, true),
            Err(e) => {
                tracing::error!("{e}; starting no integrations");
                (Some(Vec::new()), false)
            }
        };
        Arc::new_cyclic(|weak: &std::sync::Weak<Self>| {
            let (weak, dir) = (weak.clone(), config_dir.to_owned());
            let save = writable.then(|| {
                DelayedSave::spawn(KEY, move || match weak.upgrade() {
                    Some(this) => this.write(&dir),
                    None => Ok(()),
                })
            });
            if !writable {
                tracing::warn!("{KEY} is not at minor version {MINOR}; it won't be written");
            }
            Self {
                hass: hass.clone(),
                entries: RwLock::new(entries),
                save,
            }
        })
    }

    /// Kept in memory only (tests).
    pub fn in_memory(hass: &Hass, entries: Vec<Map<String, Value>>) -> Arc<Self> {
        Arc::new(Self {
            hass: hass.clone(),
            entries: RwLock::new(Some(entries)),
            save: None,
        })
    }

    /// The entries; `None` on a fresh install.
    pub fn list(&self) -> Option<Vec<Map<String, Value>>> {
        self.entries.read().clone()
    }

    /// An entry's domain and whether it's disabled, for the registries.
    pub fn info(&self, entry_id: &str) -> Option<ha_registry::EntryInfo> {
        let guard = self.entries.read();
        let entry = guard
            .iter()
            .flatten()
            .find(|e| e.get("entry_id").and_then(Value::as_str) == Some(entry_id))?;
        Some(ha_registry::EntryInfo {
            domain: entry.get("domain")?.as_str()?.to_owned(),
            disabled: entry.get("disabled_by").is_some_and(|d| !d.is_null()),
        })
    }

    /// `async_update_entry(entry, data=…)`: when the data differs it is
    /// replaced, `modified_at` moves and the save is scheduled. `false`
    /// for no change (or no such entry).
    pub fn update_data(&self, entry_id: &str, data: Map<String, Value>) -> bool {
        {
            let mut guard = self.entries.write();
            let Some(entry) = guard
                .iter_mut()
                .flatten()
                .find(|e| e.get("entry_id").and_then(Value::as_str) == Some(entry_id))
            else {
                return false;
            };
            if entry.get("data").and_then(Value::as_object) == Some(&data) {
                return false;
            }
            entry.insert("data".into(), Value::Object(data));
            entry.insert(
                "modified_at".into(),
                Value::String(ha_core::isoformat(self.hass.clock().now())),
            );
        }
        if let Some(save) = &self.save {
            save.schedule(SAVE_DELAY);
        }
        true
    }

    /// Write what's pending (at stop).
    pub async fn flush(&self) {
        if let Some(save) = &self.save {
            save.flush().await;
        }
    }

    /// HA's layout: each entry one compact line with sorted keys (a
    /// `serde_json::Map` is sorted), in a 2-space envelope.
    fn write(&self, dir: &Path) -> std::io::Result<()> {
        #[derive(Serialize)]
        struct Out {
            entries: Vec<Box<serde_json::value::RawValue>>,
        }
        let entries = self.list().unwrap_or_default();
        let rows = entries
            .iter()
            .map(|e| serde_json::value::to_raw_value(e).map_err(std::io::Error::other))
            .collect::<std::io::Result<_>>()?;
        store(dir).save(&Out { entries: rows })
    }

    #[cfg(test)]
    pub(crate) fn write_to(&self, dir: &Path) -> std::io::Result<()> {
        self.write(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HA's own files (the registry-write runs) are written back byte for
    /// byte.
    #[tokio::test]
    async fn files_round_trip_byte_for_byte() {
        for name in ["before", "after_update_registration"] {
            let text = match name {
                "before" => include_str!("config_entries/vectors/before.json"),
                _ => include_str!("config_entries/vectors/after_update_registration.json"),
            };
            let dir =
                std::env::temp_dir().join(format!("hars-entries-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join(".storage")).unwrap();
            std::fs::write(dir.join(".storage").join(KEY), text).unwrap();
            let hass = Hass::new();
            let entries = ConfigEntries::load(&hass, &dir);
            entries.write_to(&dir).unwrap();
            let written = std::fs::read_to_string(dir.join(".storage").join(KEY)).unwrap();
            assert!(written == text, "{name}:\n{written}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Review regression: a save rewrites every entry, so other entries'
    /// numbers must come back exactly (HA's orjson keeps them).
    #[tokio::test]
    async fn other_entries_keep_their_numbers() {
        let dir = std::env::temp_dir().join(format!("hars-entries-float-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".storage")).unwrap();
        let file = dir.join(".storage").join(KEY);
        let text = r#"{
  "version": 1,
  "minor_version": 5,
  "key": "core.config_entries",
  "data": {
    "entries": [
      {"data":{"a":1},"entry_id":"one","options":{"x":25.096823722419074,"y":1e-7,"z":12345678901234567890}},
      {"data":{"a":1},"entry_id":"two"}
    ]
  }
}"#;
        std::fs::write(&file, text).unwrap();
        let entries = ConfigEntries::load(&Hass::new(), &dir);
        let mut data = Map::new();
        data.insert("a".into(), Value::from(2));
        assert!(entries.update_data("two", data));
        entries.flush().await;
        let saved = std::fs::read_to_string(&file).unwrap();
        assert!(saved.contains(r#""x":25.096823722419074"#), "{saved}");
        assert!(saved.contains(r#""z":12345678901234567890"#), "{saved}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn updates_save_a_second_later_and_no_ops_nothing() {
        let dir = std::env::temp_dir().join(format!("hars-entries-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".storage")).unwrap();
        let file = dir.join(".storage").join(KEY);
        std::fs::write(&file, include_str!("config_entries/vectors/before.json")).unwrap();
        let hass = Hass::new();
        let entries = ConfigEntries::load(&hass, &dir);
        let entry = entries.list().unwrap()[0].clone();
        let id = entry["entry_id"].as_str().unwrap().to_owned();
        let data = entry["data"].as_object().unwrap().clone();
        assert!(!entries.update_data(&id, data.clone()));
        assert!(!entries.update_data("nope", data.clone()));
        let mut changed = data;
        changed.insert("os_version".into(), Value::String("27.0.1".into()));
        assert!(entries.update_data(&id, changed));
        let advance = |ms: u64| async move {
            tokio::time::advance(Duration::from_millis(ms)).await;
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        advance(500).await;
        assert!(!std::fs::read_to_string(&file).unwrap().contains("27.0.1"));
        advance(600).await;
        let saved = std::fs::read_to_string(&file).unwrap();
        assert!(saved.contains("\"os_version\":\"27.0.1\""));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
