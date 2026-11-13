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

impl Hass {
    /// Start up, as HA's `async_start`: `STARTING` + `homeassistant_start`,
    /// then `RUNNING` + `homeassistant_started`.
    pub fn start(&self) {
        self.enter(CoreState::Starting, EventKind::HomeassistantStart {});
        self.enter(CoreState::Running, EventKind::HomeassistantStarted {});
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
            assert_eq!(v["data"], json!({}), "{v}");
            fired.push(v["event_type"].as_str().unwrap().to_owned());
        }
        assert_eq!(
            fired,
            [
                "homeassistant_start",
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
