//! Start/stop lifecycle, mirroring HA's `CoreState` and its
//! `homeassistant_*` events.

use crate::{Context, Event, EventKind, Hass};
use serde::Serialize;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// How long `homeassistant_final_write` handlers get (`core.py`'s
/// `FINAL_WRITE_STAGE_SHUTDOWN_TIMEOUT`).
pub const FINAL_WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `homeassistant_close` handlers get (`CLOSE_STAGE_SHUTDOWN_TIMEOUT`).
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(30);

type StopFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type StopHandler = (&'static str, Box<dyn FnOnce() -> StopFuture + Send>);

/// Work `finish_stop` waits for ([`Hass::on_final_write`], [`Hass::on_close`]).
#[derive(Default)]
pub(crate) struct StopHandlers {
    final_write: parking_lot::Mutex<Vec<StopHandler>>,
    close: parking_lot::Mutex<Vec<StopHandler>>,
}

/// Run one stop stage's handlers together, for at most `timeout`; the
/// ones still running then are named and abandoned.
async fn run_stage(handlers: Vec<StopHandler>, timeout: Duration, stage: &str) {
    if handlers.is_empty() {
        return;
    }
    let mut set = tokio::task::JoinSet::new();
    let mut running = std::collections::HashMap::new();
    for (name, start) in handlers {
        running.insert(set.spawn(start()).id(), name);
    }
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(deadline, set.join_next_with_id()).await {
            Ok(None) => return,
            Ok(Some(Ok((id, ())))) => {
                running.remove(&id);
            }
            Ok(Some(Err(e))) => {
                let name = running.remove(&e.id()).unwrap_or("?");
                tracing::error!("{stage} handler {name} failed: {e}");
            }
            Err(_) => {
                let mut names: Vec<_> = running.into_values().collect();
                names.sort_unstable();
                tracing::warn!(
                    "timed out after {timeout:?} waiting for {stage} handlers: {}; continuing",
                    names.join(", ")
                );
                // Dropping the set aborts them.
                return;
            }
        }
    }
}

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
    /// `homeassistant_final_write`, waiting up to 60 s for the
    /// [`Hass::on_final_write`] handlers (the recorder's last write), then
    /// `NOT_RUNNING` + `homeassistant_close` and up to 30 s for the
    /// [`Hass::on_close`] handlers, then `STOPPED`.
    pub async fn finish_stop(&self) {
        self.enter(CoreState::FinalWrite, EventKind::HomeassistantFinalWrite {});
        let handlers = std::mem::take(&mut *self.inner.stop.final_write.lock());
        run_stage(handlers, FINAL_WRITE_TIMEOUT, "homeassistant_final_write").await;
        self.enter(CoreState::NotRunning, EventKind::HomeassistantClose {});
        let handlers = std::mem::take(&mut *self.inner.stop.close.lock());
        run_stage(handlers, CLOSE_TIMEOUT, "homeassistant_close").await;
        self.set_core_state(CoreState::Stopped);
    }

    /// Run `start()` once `homeassistant_final_write` has fired, and have
    /// [`Hass::finish_stop`] wait for it (up to 60 s, with the others).
    pub fn on_final_write<F, Fut>(&self, name: &'static str, start: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.inner
            .stop
            .final_write
            .lock()
            .push((name, Box::new(move || Box::pin(start()) as StopFuture)));
    }

    /// The same at `homeassistant_close` (up to 30 s).
    pub fn on_close<F, Fut>(&self, name: &'static str, start: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.inner
            .stop
            .close
            .lock()
            .push((name, Box::new(move || Box::pin(start()) as StopFuture)));
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
        hass.finish_stop().await;
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

    fn lifecycle_events(
        rx: &mut tokio::sync::broadcast::Receiver<std::sync::Arc<Event>>,
    ) -> Vec<String> {
        let mut fired = Vec::new();
        while let Ok(e) = rx.try_recv() {
            let v = serde_json::to_value(&*e).unwrap();
            fired.push(v["event_type"].as_str().unwrap_or("custom").to_owned());
        }
        fired
    }

    /// A slow final-write handler holds `homeassistant_close` back until it
    /// is done; the handlers run together.
    #[tokio::test(start_paused = true)]
    async fn final_write_waits_for_its_handlers() {
        let hass = Hass::new();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for (name, secs) in [("a", 5), ("b", 7)] {
            let done = done.clone();
            hass.on_final_write(name, move || async move {
                tokio::time::sleep(Duration::from_secs(secs)).await;
                done.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
        let closed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let closed = closed.clone();
            hass.on_close("c", move || async move {
                closed.store(true, std::sync::atomic::Ordering::SeqCst);
            });
        }
        let mut rx = hass.bus().subscribe();
        let start = tokio::time::Instant::now();
        let stop = tokio::spawn({
            let hass = hass.clone();
            async move { hass.finish_stop().await }
        });
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(hass.core_state(), CoreState::FinalWrite);
        assert_eq!(lifecycle_events(&mut rx), ["homeassistant_final_write"]);
        stop.await.unwrap();
        assert_eq!(
            start.elapsed(),
            Duration::from_secs(7),
            "concurrent, not 12 s"
        );
        assert_eq!(done.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(closed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(lifecycle_events(&mut rx), ["homeassistant_close"]);
        assert_eq!(hass.core_state(), CoreState::Stopped);
    }

    /// A hung handler is cut at its stage's timeout, and stop goes on.
    #[tokio::test(start_paused = true)]
    async fn hung_handlers_are_cut_at_the_timeout() {
        let hass = Hass::new();
        hass.on_final_write("hung", std::future::pending::<()>);
        hass.on_close("hung too", std::future::pending::<()>);
        let start = tokio::time::Instant::now();
        hass.finish_stop().await;
        assert_eq!(start.elapsed(), FINAL_WRITE_TIMEOUT + CLOSE_TIMEOUT);
        assert_eq!(hass.core_state(), CoreState::Stopped);
        // Idle: nothing registered, no wait.
        let idle = Hass::new();
        let start = tokio::time::Instant::now();
        idle.finish_stop().await;
        assert_eq!(start.elapsed(), Duration::ZERO);
    }
}
