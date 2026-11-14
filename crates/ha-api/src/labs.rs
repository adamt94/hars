//! Home Assistant Labs (`components/labs`): the preview features a user can
//! turn on, kept in `.storage/core.labs`.

use ha_core::storage::Store;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// HA 2026.6.1's `generated/labs.py`: (domain, feature, feedback, learn
/// more, report issue), in its order. An empty URL is HA's `""`.
const FEATURES: &[(&str, &str, &str, &str, &str)] = &[
    (
        "analytics",
        "snapshots",
        "https://forms.gle/GqvRmgmghSDco8M46",
        "https://www.home-assistant.io/blog/2026/02/02/about-device-database/",
        "https://github.com/OHF-Device-Database/device-database/issues/new",
    ),
    (
        "automation",
        "new_triggers_conditions",
        "https://forms.gle/fWFZqf5MzuwWTsCH8",
        "https://www.home-assistant.io/blog/2025/12/03/release-202512/#purpose-specific-triggers-and-conditions",
        "https://github.com/home-assistant/core/issues/new?template=bug_report.yml&integration_link=https://www.home-assistant.io/integrations/automation&integration_name=Automation",
    ),
    ("frontend", "winter_mode", "", "", ""),
    (
        "kitchen_sink",
        "special_repair",
        "https://community.home-assistant.io",
        "https://www.home-assistant.io/integrations/kitchen_sink",
        "https://github.com/home-assistant/core/issues/new?template=bug_report.yml&integration_link=https://www.home-assistant.io/integrations/kitchen_sink&integration_name=Kitchen%20Sink",
    ),
];

const KEY: &str = "core.labs";

/// Told of each change to its feature with the new description; returns
/// `false` once it's gone (and is then dropped).
pub type Listener = Box<dyn Fn(&Value) -> bool + Send + Sync>;

/// The enabled preview features, and where they're saved.
pub struct Labs {
    /// `labs/subscribe`rs: (domain, feature, listener).
    listeners: Mutex<Vec<(String, String, Listener)>>,
    /// The config directory; `None` keeps everything in memory (tests).
    dir: Option<PathBuf>,
    enabled: Mutex<Vec<(String, String)>>,
    /// Saves run one at a time, so an older list never lands last.
    save_turn: tokio::sync::Mutex<()>,
}

impl Default for Labs {
    fn default() -> Self {
        Self {
            listeners: Mutex::new(Vec::new()),
            dir: None,
            enabled: Mutex::new(Vec::new()),
            save_turn: tokio::sync::Mutex::new(()),
        }
    }
}

fn feature(
    domain: &str,
    feature: &str,
) -> Option<&'static (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
)> {
    FEATURES.iter().find(|f| f.0 == domain && f.1 == feature)
}

impl Labs {
    /// Read `.storage/core.labs` (blocking; at start-up), dropping features
    /// that no longer exist and saving if any were, as HA. A file that
    /// can't be read is logged and treated as none enabled.
    pub fn load(config_dir: &Path) -> Self {
        #[derive(serde::Deserialize)]
        struct Data {
            #[serde(default)]
            preview_feature_status: Vec<Item>,
        }
        #[derive(serde::Deserialize)]
        struct Item {
            domain: String,
            preview_feature: String,
        }
        let store = store(config_dir);
        let items = match store.load::<Data>() {
            Ok(d) => d.map(|d| d.preview_feature_status).unwrap_or_default(),
            Err(e) => {
                tracing::error!("{e}; no Labs features enabled");
                Vec::new()
            }
        };
        let count = items.len();
        let mut enabled: Vec<(String, String)> = Vec::new();
        for i in items {
            if feature(&i.domain, &i.preview_feature).is_some()
                && !enabled
                    .iter()
                    .any(|(d, f)| *d == i.domain && *f == i.preview_feature)
            {
                enabled.push((i.domain, i.preview_feature));
            }
        }
        let labs = Self {
            listeners: Mutex::new(Vec::new()),
            dir: Some(config_dir.to_owned()),
            enabled: Mutex::new(enabled),
            save_turn: tokio::sync::Mutex::new(()),
        };
        if labs.enabled.lock().len() != count {
            if let Err(e) = labs.write() {
                tracing::error!("saving {KEY}: {e}");
            }
        }
        labs
    }

    pub fn is_enabled(&self, domain: &str, feature: &str) -> bool {
        self.enabled
            .lock()
            .iter()
            .any(|(d, f)| d == domain && f == feature)
    }

    /// `LabPreviewFeature.to_dict`, or `None` for an unknown feature.
    pub fn describe(&self, domain: &str, name: &str, enabled: bool) -> Option<Value> {
        let &(d, f, feedback, learn, report) = feature(domain, name)?;
        Some(json!({
            "preview_feature": f,
            "domain": d,
            "enabled": enabled,
            "is_built_in": true,
            "feedback_url": feedback,
            "learn_more_url": learn,
            "report_issue_url": report,
        }))
    }

    /// `labs/list`: the features of loaded integrations.
    pub fn list(&self, loaded: &[&str]) -> Value {
        let features: Vec<Value> = FEATURES
            .iter()
            .filter(|f| loaded.contains(&f.0))
            .filter_map(|f| self.describe(f.0, f.1, self.is_enabled(f.0, f.1)))
            .collect();
        json!({ "features": features })
    }

    /// Listen to a feature: its description now, taken together with the
    /// registration so no change falls in between. `None` if unknown.
    pub fn subscribe(&self, domain: &str, name: &str, listener: Listener) -> Option<Value> {
        let mut listeners = self.listeners.lock();
        let now = self.describe(domain, name, self.is_enabled(domain, name))?;
        listeners.push((domain.to_owned(), name.to_owned(), listener));
        Some(now)
    }

    /// Turn a feature on or off, save, and tell its listeners (before the
    /// caller answers, as HA's eager listeners do). As HA's store, a failed
    /// save is logged and the change stands. `false` for an unknown feature.
    pub async fn update(&self, domain: &str, name: &str, on: bool) -> bool {
        let Some(changed) = self.describe(domain, name, on) else {
            return false;
        };
        let _turn = self.save_turn.lock().await;
        {
            let mut enabled = self.enabled.lock();
            enabled.retain(|(d, f)| !(d == domain && f == name));
            if on {
                enabled.push((domain.to_owned(), name.to_owned()));
            }
        }
        let snapshot = self.snapshot();
        if let Some(dir) = self.dir.clone() {
            let saved = tokio::task::spawn_blocking(move || store(&dir).save(&snapshot)).await;
            match saved {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::error!("saving {KEY}: {e}"),
                Err(e) => tracing::error!("saving {KEY}: {e}"),
            }
        }
        self.listeners
            .lock()
            .retain(|(d, f, listener)| d != domain || f != name || listener(&changed));
        true
    }

    fn snapshot(&self) -> Value {
        let items: Vec<Value> = self
            .enabled
            .lock()
            .iter()
            .map(|(d, f)| json!({"domain": d, "preview_feature": f}))
            .collect();
        json!({ "preview_feature_status": items })
    }

    fn write(&self) -> std::io::Result<()> {
        match &self.dir {
            Some(dir) => store(dir).save(&self.snapshot()),
            None => Ok(()),
        }
    }
}

/// `.storage/core.labs`, owner-only as HA's `private=True`.
fn store(config_dir: &Path) -> Store {
    Store::new(config_dir, KEY, 1, 1, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loads_drops_stale_and_saves() {
        let dir = std::env::temp_dir().join(format!("hars-labs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = store(&dir);
        store
            .save(&json!({"preview_feature_status": [
                {"domain": "frontend", "preview_feature": "winter_mode"},
                {"domain": "gone", "preview_feature": "old"},
            ]}))
            .unwrap();
        let labs = Labs::load(&dir);
        assert!(labs.is_enabled("frontend", "winter_mode"));
        // The stale one was dropped from the file too.
        let saved: Value = store.load().unwrap().unwrap();
        assert_eq!(saved["preview_feature_status"].as_array().unwrap().len(), 1);

        assert!(labs.update("frontend", "winter_mode", false).await);
        assert!(!labs.is_enabled("frontend", "winter_mode"));
        assert_eq!(
            Labs::load(&dir).snapshot(),
            json!({"preview_feature_status": []})
        );
        assert!(!labs.update("nope", "x", true).await);

        let list = labs.list(&["frontend", "sun"]);
        assert_eq!(
            list,
            json!({"features": [{"preview_feature": "winter_mode", "domain": "frontend", "enabled": false,
                "is_built_in": true, "feedback_url": "", "learn_more_url": "", "report_issue_url": ""}]})
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn listeners_hear_changes_even_when_saving_fails() {
        // A config "directory" that is a file: every save fails.
        let file = std::env::temp_dir().join(format!("hars-labs-file-{}", std::process::id()));
        std::fs::write(&file, b"not a directory").unwrap();
        let labs = Labs::load(&file);
        let heard = std::sync::Arc::new(Mutex::new(Vec::new()));
        let h = heard.clone();
        let now = labs
            .subscribe(
                "frontend",
                "winter_mode",
                Box::new(move |v: &Value| {
                    h.lock().push(v["enabled"].clone());
                    true
                }),
            )
            .unwrap();
        assert_eq!(now["enabled"], false);
        // A listener that's gone is dropped at the next change.
        labs.subscribe("frontend", "winter_mode", Box::new(|_: &Value| false));
        assert!(labs.update("frontend", "winter_mode", true).await);
        assert!(labs.is_enabled("frontend", "winter_mode"));
        assert_eq!(*heard.lock(), [json!(true)]);
        assert_eq!(labs.listeners.lock().len(), 1);
        assert!(
            labs.subscribe("frontend", "nope", Box::new(|_: &Value| true))
                .is_none()
        );
        let _ = std::fs::remove_file(&file);
    }
}
