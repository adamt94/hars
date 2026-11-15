//! The running recorder: the store, the intake task that feeds it, the
//! final-write handler and the nightly purge.

use crate::intake::{Intake, recorded};
use crate::store::{Config, Info, Store};
use ha_core::Hass;
use ha_core::yaml::Yaml;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// HA's nightly purge time, local (`recorder/core.py`).
const PURGE_AT: (i8, i8) = (4, 12);
const DAY_US: i64 = 86_400_000_000;

/// The `recorder:` keys hars reads from `configuration.yaml`.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Days of history kept (`purge_keep_days`).
    pub purge_keep_days: u32,
    /// The longest a row waits before it is written (`commit_interval`).
    pub commit_interval: Duration,
    /// Purge nightly at 04:12 (`auto_purge`).
    pub auto_purge: bool,
    /// hars only: the store's size cap, in bytes (`max_db_size`, MiB).
    pub max_bytes: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            purge_keep_days: 10,
            commit_interval: Duration::from_secs(5),
            auto_purge: true,
            max_bytes: 64 << 20,
        }
    }
}

impl Settings {
    /// The `recorder:` section (absent or empty: the defaults). A key hars
    /// doesn't support, or a value of the wrong type, is warned about and
    /// left at its default.
    pub fn from_yaml(section: Option<&Yaml>) -> Self {
        let mut s = Self::default();
        let Some(Yaml::Map(pairs)) = section else {
            return s;
        };
        let int = |k: &str, v: &Yaml, min: i128| match v {
            Yaml::Int(i) if *i >= min => Some(*i),
            _ => {
                tracing::warn!("recorder: {k} must be an integer of at least {min}; ignored");
                None
            }
        };
        for (k, v) in pairs {
            let Some(k) = k.as_str() else { continue };
            match k {
                "purge_keep_days" => {
                    if let Some(i) = int(k, v, 1) {
                        s.purge_keep_days = u32::try_from(i).unwrap_or(u32::MAX);
                    }
                }
                "commit_interval" => {
                    if let Some(i) = int(k, v, 0) {
                        s.commit_interval =
                            Duration::from_secs(u64::try_from(i).unwrap_or(u64::MAX));
                    }
                }
                "max_db_size" => {
                    if let Some(i) = int(k, v, 1) {
                        s.max_bytes = u64::try_from(i).unwrap_or(u64::MAX >> 20) << 20;
                    }
                }
                "auto_purge" => match v {
                    Yaml::Bool(b) => s.auto_purge = *b,
                    _ => tracing::warn!("recorder: auto_purge must be a boolean; ignored"),
                },
                // Nothing to repack: purge unlinks whole files.
                "auto_repack" => {}
                other => tracing::warn!(
                    "recorder: {other} is not supported by hars; ignored (history is \
                     kept in <config>/.hars/recorder, recording everything)"
                ),
            }
        }
        s
    }
}

/// The recorder, once started.
pub struct Recorder {
    store: Arc<Store>,
}

impl std::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder").finish_non_exhaustive()
    }
}

impl Recorder {
    /// Open the store in `dir` and start recording. Call before anything
    /// writes states (HA sets the recorder up first), so the boot rows and
    /// `homeassistant_start` are recorded. Registers the final-write
    /// handler: the intake drains up to `homeassistant_final_write`, then
    /// the store writes its last batch and closes.
    pub async fn start(hass: &Hass, dir: PathBuf, settings: &Settings) -> std::io::Result<Self> {
        let config = Config {
            commit_interval: settings.commit_interval,
            max_bytes: settings.max_bytes,
            ..Config::new(dir)
        };
        let store = tokio::task::spawn_blocking(move || Store::open(config))
            .await
            .map_err(std::io::Error::other)??;
        let store = Arc::new(store);
        let mut feed = hass.bus().subscribe_lossless(recorded);
        let (drained, drained_rx) = tokio::sync::oneshot::channel::<()>();
        let mut intake = Intake::new(hass.clone());
        {
            let store = store.clone();
            tokio::spawn(async move {
                while let Some(event) = feed.recv().await {
                    if matches!(event.kind, ha_core::EventKind::HomeassistantFinalWrite {}) {
                        break;
                    }
                    if let Some(row) = intake.row(&event) {
                        store.record(row);
                    }
                }
                let _ = drained.send(());
            });
        }
        {
            let store = store.clone();
            hass.on_final_write("recorder", move || async move {
                let _ = drained_rx.await;
                store.close().await;
            });
        }
        if settings.auto_purge {
            tokio::spawn(purge_nightly(
                hass.clone(),
                Arc::downgrade(&store),
                settings.purge_keep_days,
            ));
        }
        Ok(Self { store })
    }

    /// `recorder/info`'s live fields.
    pub fn info(&self) -> Info {
        self.store.info()
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }
}

/// The next 04:12 local after `now`.
fn next_purge(now: Timestamp, tz: &TimeZone) -> Timestamp {
    let local = now.to_zoned(tz.clone());
    let at = |d: jiff::civil::Date| {
        d.at(PURGE_AT.0, PURGE_AT.1, 0, 0)
            .to_zoned(tz.clone())
            .map(|z| z.timestamp())
            .ok()
    };
    let today = local.date();
    match at(today) {
        Some(t) if t > now => t,
        _ => today
            .checked_add(1.day())
            .ok()
            .and_then(at)
            .unwrap_or_else(|| now + Duration::from_secs(86_400)),
    }
}

/// Every night at 04:12 local, unlink the days older than `keep_days`:
/// one timer a day. Ends when the store is gone.
async fn purge_nightly(hass: Hass, store: std::sync::Weak<Store>, keep_days: u32) {
    loop {
        let tz = TimeZone::get(&hass.config().time_zone).unwrap_or(TimeZone::UTC);
        let now = hass.clock().now();
        let wait = next_purge(now, &tz).duration_since(now);
        tokio::time::sleep(Duration::try_from(wait).unwrap_or(Duration::from_secs(60))).await;
        let Some(store) = store.upgrade() else { return };
        let cutoff = hass.clock().now().as_microsecond() - i64::from(keep_days) * DAY_US;
        store.purge(cutoff).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_read_has_keys_and_warn_on_others() {
        let yaml = |s: &str| ha_core::yaml::parse(s).unwrap();
        assert_eq!(Settings::from_yaml(None), Settings::default());
        let y = yaml(
            "purge_keep_days: 3\ncommit_interval: 1\nauto_purge: false\nauto_repack: true\nmax_db_size: 8\ndb_url: mysql://x\npurge_keep_days_bad: 1",
        );
        assert_eq!(
            Settings::from_yaml(Some(&y)),
            Settings {
                purge_keep_days: 3,
                commit_interval: Duration::from_secs(1),
                auto_purge: false,
                max_bytes: 8 << 20,
            }
        );
        let bad = yaml("purge_keep_days: 0\ncommit_interval: soon");
        assert_eq!(Settings::from_yaml(Some(&bad)), Settings::default());
    }

    #[test]
    fn purge_runs_at_0412_local_across_dst() {
        let tz = TimeZone::get("Europe/London").unwrap();
        let t = |s: &str| s.parse::<Timestamp>().unwrap();
        // Before 04:12 BST: today.
        assert_eq!(
            next_purge(t("2026-07-01T02:00:00Z"), &tz),
            t("2026-07-01T03:12:00Z")
        );
        // After: tomorrow.
        assert_eq!(
            next_purge(t("2026-07-01T03:12:00Z"), &tz),
            t("2026-07-02T03:12:00Z")
        );
        // The night the clocks go back (25 h day): 04:12 GMT.
        assert_eq!(
            next_purge(t("2026-10-25T00:30:00Z"), &tz),
            t("2026-10-25T04:12:00Z")
        );
    }

    /// End to end: rows written before and during stop are all on disk
    /// after `finish_stop`, with `homeassistant_start` and `_stop`; the
    /// next start reads them.
    #[tokio::test]
    async fn stop_writes_everything_recorded() {
        let dir = std::env::temp_dir().join(format!("hars-recorder-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let settings = Settings {
            // Long: only the final write commits.
            commit_interval: Duration::from_secs(3600),
            ..Settings::default()
        };
        let hass = Hass::new();
        let recorder = Recorder::start(&hass, dir.clone(), &settings)
            .await
            .unwrap();
        let id = ha_core::EntityId::parse("sensor.washer").unwrap();
        hass.set_state(id.clone(), "idle", Default::default());
        hass.start();
        hass.set_state(id.clone(), "running", Default::default());
        hass.begin_stop();
        hass.set_state(id.clone(), "done", Default::default());
        hass.finish_stop().await;
        assert!(!recorder.info().thread_running);
        assert_eq!(recorder.info().backlog, 0);
        drop(recorder);

        let store = Store::open(Config::new(dir.clone())).unwrap();
        let mut got = Vec::new();
        store.scan(i64::MIN, i64::MAX, |r| match r {
            crate::Rec::State(s) => got.push(format!("{}={}/{}", s.entity_id, s.state, s.flags)),
            crate::Rec::Event(e) => got.push(e.event_type.to_owned()),
        });
        assert_eq!(
            got,
            [
                "sensor.washer=idle/0",
                "homeassistant_start",
                "sensor.washer=running/1",
                "homeassistant_stop",
                "sensor.washer=done/1",
            ]
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
