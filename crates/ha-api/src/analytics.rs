//! The `analytics` integration's preferences (`components/analytics`),
//! kept in HA's `.storage/core.analytics` (1.1) so they survive a move
//! between HA and `hars`.
//!
//! `hars` never sends analytics anywhere, whatever the preferences say:
//! HA's `async_schedule` (the daily report to analytics.home-assistant.io)
//! has no counterpart. The preferences are only stored and shown, so the
//! onboarding step and the settings page work.

use ha_core::storage::Store;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

const KEY: &str = "core.analytics";

/// `PREFERENCE_SCHEMA`'s keys, in the order `Analytics.preferences` lists
/// them.
pub(crate) const PREFERENCES: [&str; 5] =
    ["base", "diagnostics", "usage", "statistics", "snapshots"];

/// `AnalyticsData`, field for field; `asdict` writes these five.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Data {
    onboarded: bool,
    /// What was chosen, as merged (`dict.update`); unset ones read false.
    preferences: Map<String, Value>,
    uuid: Option<String>,
    #[serde(default)]
    submission_identifier: Option<String>,
    #[serde(default)]
    snapshot_submission_time: Option<f64>,
}

/// The stored preferences, and where they're saved.
#[derive(Default)]
pub struct Analytics {
    /// The config directory; `None` keeps everything in memory (tests).
    dir: Option<PathBuf>,
    data: Mutex<Data>,
    /// Saves run one at a time, so an older copy never lands last.
    save_turn: tokio::sync::Mutex<()>,
}

fn store(dir: &Path) -> Store {
    Store::new(dir, KEY, 1, 1, false)
}

impl Analytics {
    /// Whether `.storage/core.analytics` decodes as [`Analytics::load`]
    /// decodes it (a missing file does): for checking a copy before
    /// importing it.
    pub fn check(config_dir: &Path) -> Result<(), ha_core::storage::StoreError> {
        store(config_dir).load::<Data>().map(drop)
    }

    /// Read `.storage/core.analytics` (blocking; at start-up). A file that
    /// can't be read is logged and treated as no choice made yet.
    pub fn load(config_dir: &Path) -> Self {
        let data = store(config_dir).load::<Data>().unwrap_or_else(|e| {
            tracing::error!("{e}; analytics preferences start empty");
            None
        });
        Self {
            dir: Some(config_dir.to_owned()),
            data: Mutex::new(data.unwrap_or_default()),
            save_turn: tokio::sync::Mutex::new(()),
        }
    }

    /// `analytics`: the preferences (each one, false unless chosen) and
    /// whether a choice was made.
    pub(crate) fn json(&self) -> Value {
        let d = self.data.lock();
        json!({"preferences": preferences(&d), "onboarded": d.onboarded})
    }

    /// `save_preferences`: merge `chosen` (already validated against
    /// `PREFERENCE_SCHEMA`), mark the choice made, and save. Returns the
    /// preferences as `analytics/preferences` answers them.
    pub(crate) async fn save(&self, chosen: Map<String, Value>) -> std::io::Result<Value> {
        let _turn = self.save_turn.lock().await;
        let (data, answer) = {
            let mut d = self.data.lock();
            d.preferences.extend(chosen);
            d.onboarded = true;
            (d.clone(), json!({"preferences": preferences(&d)}))
        };
        if let Some(dir) = self.dir.clone() {
            tokio::task::spawn_blocking(move || store(&dir).save(&data))
                .await
                .map_err(std::io::Error::other)??;
        }
        Ok(answer)
    }
}

/// `Analytics.preferences`: every key, false unless chosen.
fn preferences(d: &Data) -> Value {
    let mut out = Map::new();
    for key in PREFERENCES {
        let on = d.preferences.get(key).and_then(Value::as_bool) == Some(true);
        out.insert(key.into(), Value::Bool(on));
    }
    Value::Object(out)
}
