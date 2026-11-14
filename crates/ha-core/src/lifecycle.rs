//! Start/stop lifecycle, mirroring HA's `CoreState` and its
//! `homeassistant_*` events.

use crate::{Context, Event, EventKind, Hass};
use serde::Serialize;

/// Where the instance is in its lifecycle. Serializes as HA's
/// `CoreState` value (`"RUNNING"`, …), as `/api/config` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[repr(u8)]
pub enum CoreState {
    NotRunning = 0,
    Starting,
    Running,
    Stopping,
    FinalWrite,
    Stopped,
}

impl CoreState {
    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Starting,
            2 => Self::Running,
            3 => Self::Stopping,
            4 => Self::FinalWrite,
            5 => Self::Stopped,
            _ => Self::NotRunning,
        }
    }
}

/// Integrations still on their first setup attempt (HA's bootstrap waits
/// for these before `homeassistant_start`).
#[derive(Default)]
pub(crate) struct Setups {
    pending: std::sync::atomic::AtomicUsize,
    notify: tokio::sync::Notify,
    /// Woken at `homeassistant_started`.
    started: tokio::sync::Notify,
}

/// One integration's first setup attempt. Done when it has made its first
/// writes or failed (`ConfigEntryNotReady` counts: HA retries in the
/// background); dropping it counts as done too.
#[must_use = "dropping the guard marks the setup done"]
pub struct SetupGuard {
    hass: Option<Hass>,
}

impl SetupGuard {
    pub fn done(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        if let Some(hass) = self.hass.take() {
            let s = &hass.inner.setups;
            s.pending.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            s.notify.notify_waiters();
        }
    }
}

impl Drop for SetupGuard {
    fn drop(&mut self) {
        self.finish();
    }
}

impl Hass {
    /// Start up, as HA's `async_start`: `STARTING` + `homeassistant_start`,
    /// then `RUNNING` + `homeassistant_started`.
    pub fn start(&self) {
        self.begin_start();
        self.finish_start();
    }

    /// `STARTING`, `core_config_updated` (frontends re-read the state)
    /// and `homeassistant_start` (HA writes the placeholders for missing
    /// entities here).
    pub fn begin_start(&self) {
        self.set_core_state(CoreState::Starting);
        self.fire_config_updated();
        self.enter(CoreState::Starting, EventKind::HomeassistantStart {});
    }

    /// `RUNNING`, `core_config_updated` and `homeassistant_started`
    /// (automations attach here; integrations whose first setup failed
    /// retry now).
    pub fn finish_start(&self) {
        self.set_core_state(CoreState::Running);
        self.fire_config_updated();
        self.enter(CoreState::Running, EventKind::HomeassistantStarted {});
        self.inner.setups.started.notify_waiters();
    }

    fn fire_config_updated(&self) {
        let now = self.clock().now();
        self.bus().fire(Event::new(
            EventKind::Custom {
                name: "core_config_updated".into(),
                data: serde_json::json!({}),
            },
            now,
            Context::at(now),
        ));
    }

    /// Until `homeassistant_started` (at once if running). A config entry
    /// whose first setup failed waits for this before retrying, as HA's
    /// does during bootstrap.
    pub async fn wait_started(&self) {
        loop {
            let notified = self.inner.setups.started.notified();
            if self.core_state() == CoreState::Running {
                return;
            }
            notified.await;
        }
    }

    /// A failed setup's wait before retrying: until `homeassistant_started`
    /// while starting (HA retries then), else the backoff.
    pub async fn setup_retry_wait(&self, backoff: std::time::Duration) {
        if self.core_state() == CoreState::Running {
            tokio::time::sleep(backoff).await;
        } else {
            self.wait_started().await;
        }
    }

    /// Count an integration's first setup attempt, for [`Hass::setups_done`].
    pub fn setup_guard(&self) -> SetupGuard {
        self.inner
            .setups
            .pending
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        SetupGuard {
            hass: Some(self.clone()),
        }
    }

    /// Until every [`SetupGuard`] is done.
    pub async fn setups_done(&self) {
        let s = &self.inner.setups;
        loop {
            let notified = s.notify.notified();
            if s.pending.load(std::sync::atomic::Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }

    /// First stop stage: `STOPPING` + `homeassistant_stop`. Integrations
    /// disconnect and the HTTP server stops accepting work.
    pub fn begin_stop(&self) {
        self.enter(CoreState::Stopping, EventKind::HomeassistantStop {});
    }

    /// Last stop stages, as the end of HA's `async_stop`: `FINAL_WRITE` +
    /// `homeassistant_final_write` (the recorder flushes), `NOT_RUNNING` +
    /// `homeassistant_close`, then `STOPPED`.
    pub fn finish_stop(&self) {
        self.enter(CoreState::FinalWrite, EventKind::HomeassistantFinalWrite {});
        self.enter(CoreState::NotRunning, EventKind::HomeassistantClose {});
        self.set_core_state(CoreState::Stopped);
    }

    fn enter(&self, state: CoreState, kind: EventKind) {
        self.set_core_state(state);
        let now = self.clock().now();
        self.bus().fire(Event::new(kind, now, Context::at(now)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn setups_done_waits_for_every_guard() {
        let hass = Hass::new();
        hass.setups_done().await;
        let a = hass.setup_guard();
        let b = hass.setup_guard();
        let waiting = tokio::spawn({
            let hass = hass.clone();
            async move { hass.setups_done().await }
        });
        a.done();
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        // Dropped (an integration that gave up) counts as done.
        drop(b);
        waiting.await.unwrap();
        // A retry during startup waits for `started`.
        let retry = tokio::spawn({
            let hass = hass.clone();
            async move {
                hass.setup_retry_wait(std::time::Duration::from_secs(3600))
                    .await
            }
        });
        tokio::task::yield_now().await;
        assert!(!retry.is_finished());
        hass.begin_start();
        hass.finish_start();
        retry.await.unwrap();
    }

    #[tokio::test]
    async fn lifecycle_fires_ha_events_in_order() {
        let hass = Hass::new();
        assert_eq!(hass.core_state(), CoreState::NotRunning);
        let mut rx = hass.bus().subscribe();

        hass.start();
        assert_eq!(hass.core_state(), CoreState::Running);
        hass.begin_stop();
        assert_eq!(hass.core_state(), CoreState::Stopping);
        hass.finish_stop();
        assert_eq!(hass.core_state(), CoreState::Stopped);

        let mut fired = Vec::new();
        while let Ok(event) = rx.try_recv() {
            let v = serde_json::to_value(&*event).unwrap();
            let v = match &event.kind {
                EventKind::Custom { name, .. } => json!({"event_type": name, "data": {}}),
                _ => v,
            };
            assert_eq!(v["data"], json!({}), "{v}");
            fired.push(v["event_type"].as_str().unwrap().to_owned());
        }
        assert_eq!(
            fired,
            [
                "core_config_updated",
                "homeassistant_start",
                "core_config_updated",
                "homeassistant_started",
                "homeassistant_stop",
                "homeassistant_final_write",
                "homeassistant_close",
            ]
        );
        assert_eq!(
            serde_json::to_value(CoreState::FinalWrite).unwrap(),
            "FINAL_WRITE"
        );
    }
}
