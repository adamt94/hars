//! The `lifx` integration (`components/lifx`) for single-zone colour
//! bulbs on the LAN, as HA 2026.6.1 runs it with aiolifx 1.2.1: one UDP
//! socket per bulb, a `LightGet` every 10 s, HA's retry layer (5 tries;
//! 3 s apart for commands, 18 s for polls), unavailability after a poll's
//! 90 s of silence, HA's `set_state` with its settle and refreshes, the
//! identify and restart buttons, and broadcast rediscovery.
//!
//! Not yet: the `lifx.*` services (effects, `set_state`, themes); a
//! `light.turn_on` with an effect calls `lifx.<effect>` as HA does, which
//! fails until they exist. The RSSI sensor (disabled by default) isn't
//! made. A bulb found at a new address is reconnected to without
//! rewriting the stored entry (the registries are read-only here).

pub mod conn;
pub mod light;
pub mod proto;

use crate::button::Button;
use crate::components::Components;
use crate::light::{BoxFuture, Capabilities, LightState, Params, TurnOff};
use conn::{Conn, Op};
use ha_core::{EntityId, Hass, ServiceError};
use parking_lot::Mutex;
use proto::ty;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

pub const DOMAIN: &str = "lifx";

const ATTEMPTS: u32 = 5;

/// HA's delays (tests shrink them).
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub poll: Duration,
    /// Commands and setup's device info: 5 tries in 15 s.
    pub command_try: Duration,
    /// A poll: 5 tries in 90 s.
    pub poll_try: Duration,
    /// `LIFX_STATE_SETTLE_DELAY`.
    pub settle: Duration,
    /// The coordinator's request-refresh debounce.
    pub debounce: Duration,
    pub identify: Duration,
}

impl Timing {
    pub const HA: Timing = Timing {
        poll: Duration::from_secs(10),
        command_try: Duration::from_secs(3),
        poll_try: Duration::from_secs(18),
        settle: Duration::from_millis(300),
        debounce: Duration::from_millis(350),
        identify: Duration::from_secs(3),
    };
}
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(15 * 60);
const DISCOVERY_WINDOW: Duration = Duration::from_millis(8500);

/// What the last `LightGet` (or an ack) left.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Cached {
    power: u16,
    color: [u16; 4],
    label: String,
}

/// Timer requests to the coordinator.
#[derive(Debug)]
enum Ctl {
    /// `async_set_updated_data`: the next poll is 10 s from now.
    Updated,
    /// `async_request_refresh`: a refresh in 0.35 s (debounced).
    Refresh,
    /// A refresh at the end of a transition, replacing any earlier one.
    Postponed(Duration),
    CancelPostponed,
}

struct ButtonInfo {
    entity_id: EntityId,
    friendly: String,
    class: &'static str,
    last: Mutex<Option<String>>,
}

/// One bulb: its connection, cached state and entities.
pub struct Bulb {
    hass: Hass,
    /// `"{title} ({host})"`, for logs.
    name: String,
    conn: Conn,
    cached: Mutex<Cached>,
    /// `last_update_success`.
    ok: AtomicBool,
    ctl: mpsc::UnboundedSender<Ctl>,
    timing: Timing,
    light: Option<(EntityId, String)>,
    buttons: Vec<ButtonInfo>,
}

impl Bulb {
    fn light_state(&self) -> LightState {
        let c = self.cached.lock();
        light::state(c.power, c.color, None)
    }

    fn write_light(&self) {
        let Some((id, friendly)) = &self.light else {
            return;
        };
        let ok = self.ok.load(Ordering::Relaxed);
        let s = self.light_state();
        let mut a = crate::light::attributes(&light::capabilities(), &s, ok);
        a.insert("friendly_name".into(), json!(friendly));
        a.insert("supported_features".into(), json!(light::FEATURES));
        let state = match (ok, s.on) {
            (false, _) => "unavailable",
            (true, true) => "on",
            (true, false) => "off",
        };
        self.hass.set_state(id.clone(), state, a);
    }

    fn write_button(&self, b: &ButtonInfo, context: Option<ha_core::Context>) {
        let state = match self.ok.load(Ordering::Relaxed) {
            true => b.last.lock().clone().unwrap_or_else(|| "unknown".into()),
            false => "unavailable".into(),
        };
        let mut a = Map::new();
        a.insert("device_class".into(), json!(b.class));
        a.insert("friendly_name".into(), json!(b.friendly));
        self.hass
            .set_state_with_context(b.entity_id.clone(), state, a, context);
    }

    /// The coordinator's listeners: every entity.
    fn write_all(&self) {
        self.write_light();
        for b in &self.buttons {
            self.write_button(b, None);
        }
    }

    /// One refresh: a `LightGet` (5 tries, 18 s apart). The entities are
    /// written unless it failed after a failure.
    async fn refresh(&self) {
        let r = self
            .conn
            .execute(
                &[Op::get(ty::LIGHT_GET, ty::LIGHT_STATE)],
                ATTEMPTS,
                self.timing.poll_try,
            )
            .await;
        let was_ok = self.ok.load(Ordering::Relaxed);
        match r.ok().and_then(|r| proto::LightState::parse(&r[0].1)) {
            Some(s) => {
                *self.cached.lock() = Cached {
                    power: s.power,
                    color: s.color,
                    label: s.label,
                };
                if !was_ok {
                    tracing::info!("Fetching {} data recovered", self.name);
                }
                self.ok.store(true, Ordering::Relaxed);
                self.write_all();
            }
            None => {
                if was_ok {
                    tracing::error!("Timeout fetching {} data", self.name);
                    self.ok.store(false, Ordering::Relaxed);
                    self.write_all();
                }
            }
        }
    }

    fn send_ctl(&self, c: Ctl) {
        let _ = self.ctl.send(c);
    }

    /// `async_set_power`, its ack setting the cached power.
    async fn set_power(&self, on: bool, duration_ms: u32) -> Result<(), ()> {
        let level = if on { 65535 } else { 0 };
        let op = Op::set(ty::LIGHT_SET_POWER, proto::set_power(level, duration_ms));
        self.conn
            .execute(&[op], ATTEMPTS, self.timing.command_try)
            .await
            .map_err(drop)?;
        self.cached.lock().power = level;
        Ok(())
    }

    /// `async_set_waveform_optional` (the ack changes nothing).
    async fn waveform(&self, w: proto::Waveform) -> Result<(), ()> {
        let op = Op::set(ty::LIGHT_SET_WAVEFORM_OPTIONAL, w.encode());
        self.conn
            .execute(&[op], ATTEMPTS, self.timing.command_try)
            .await
            .map(drop)
            .map_err(drop)
    }

    /// `LIFXLight.set_state`.
    async fn set_state(&self, power: Option<bool>, p: Params) -> Result<(), ServiceError> {
        // `async_set_updated_data(None)`: listeners written from the cache.
        self.ok.store(true, Ordering::Relaxed);
        self.send_ctl(Ctl::Updated);
        self.write_all();
        if let Some(effect) = &p.effect {
            let (id, _) = self
                .light
                .as_ref()
                .ok_or(ServiceError::Unknown(String::new()))?;
            let mut data = Map::new();
            data.insert("entity_id".into(), json!(id.as_str()));
            return self
                .hass
                .call_service(DOMAIN, effect, data, None, false, None)
                .await
                .map(drop);
        }
        let fade = p.transition.map_or(0, |t| (t * 1000.0) as u32);
        let hsbk = light::find_hsbk(&p);
        let power_err = || ServiceError::Failed("Timeout setting power for None".into());
        let color_err = || ServiceError::Failed("Timeout setting color for None".into());
        let transform = |hsbk: light::Hsbk, period: u32| {
            let base = self.cached.lock().color;
            proto::Waveform {
                transient: false,
                color: light::merge(base, hsbk),
                period_ms: period,
                cycles: 1.0,
                skew_ratio: 0,
                waveform: 0,
                set: hsbk.map(|c| c.is_some()),
            }
        };
        let is_on = self.cached.lock().power != 0;
        if !is_on {
            if power == Some(false) {
                self.set_power(false, 0).await.map_err(|()| power_err())?;
            }
            match (hsbk, power) {
                (Some(h), Some(true)) => {
                    self.waveform(transform(h, 0))
                        .await
                        .map_err(|()| color_err())?;
                    self.set_power(true, fade).await.map_err(|()| power_err())?;
                }
                (Some(h), None) => {
                    self.waveform(transform(h, fade))
                        .await
                        .map_err(|()| color_err())?;
                }
                (None, Some(true)) => {
                    self.set_power(true, fade).await.map_err(|()| power_err())?;
                }
                _ => {}
            }
        } else {
            if power == Some(true) {
                self.set_power(true, 0).await.map_err(|()| power_err())?;
            }
            if let Some(h) = hsbk {
                self.waveform(transform(h, fade))
                    .await
                    .map_err(|()| color_err())?;
            }
            if power == Some(false) {
                self.set_power(false, fade)
                    .await
                    .map_err(|()| power_err())?;
            }
        }
        tokio::time::sleep(self.timing.settle).await;
        // `update_during_transition`.
        self.send_ctl(Ctl::CancelPostponed);
        self.write_light();
        self.send_ctl(Ctl::Refresh);
        if fade > 0 {
            self.send_ctl(Ctl::Postponed(Duration::from_millis(fade.into())));
        }
        Ok(())
    }

    /// `async_identify_bulb`: flash three times, turning on (and back off
    /// after 3 s) if it's off.
    async fn identify(&self) -> Result<(), ServiceError> {
        // HA raises the bare `TimeoutError` naming the partial; the
        // object's address (process-specific) is shown as 0x0.
        let partial = |method: &str, args: &str| {
            ServiceError::Unknown(format!(
                "functools.partial(<bound method Light.{method} of <aiolifx.aiolifx.Light object at 0x0>>, {args}) timed out after 5 attempts"
            ))
        };
        let flash = proto::Waveform {
            transient: true,
            color: [0, 0, 1, 3500],
            period_ms: 1000,
            cycles: 3.0,
            skew_ratio: 0,
            waveform: 1,
            set: [true; 4],
        };
        let waveform_err = || {
            partial(
                "set_waveform_optional",
                "value={'transient': True, 'color': [0, 0, 1, 3500], 'skew_ratio': 0, 'period': 1000, 'cycles': 3, 'waveform': 1, 'set_hue': True, 'set_saturation': True, 'set_brightness': True, 'set_kelvin': True}, rapid=False",
            )
        };
        if self.cached.lock().power != 0 {
            return self.waveform(flash).await.map_err(|()| waveform_err());
        }
        let power_err = |on: &str| partial("set_power", &format!("{on}, duration=1"));
        self.set_power(true, 1)
            .await
            .map_err(|()| power_err("True"))?;
        self.waveform(flash).await.map_err(|()| waveform_err())?;
        tokio::time::sleep(self.timing.identify).await;
        self.set_power(false, 1)
            .await
            .map_err(|()| power_err("False"))
    }
}

/// The coordinator's timers: the poll at `floor(now) + μ + 10` after each
/// refresh, the debounced refresh, and the end-of-transition refresh.
async fn coordinate(bulb: Arc<Bulb>, mut rx: mpsc::UnboundedReceiver<Ctl>, polling: bool) {
    let t = bulb.timing;
    let base = Instant::now();
    let mu = Duration::from_micros(rand::random_range(50_000..500_001));
    let next_poll = || {
        let secs = Instant::now().duration_since(base).as_secs();
        base + Duration::from_secs(secs) + mu + t.poll
    };
    let far = || Instant::now() + Duration::from_secs(86_400 * 365);
    let mut poll_at = if polling { next_poll() } else { far() };
    let mut debounce_at: Option<Instant> = None;
    let mut postponed_at: Option<Instant> = None;
    loop {
        let sleep = |at: Option<Instant>| async move {
            match at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = tokio::time::sleep_until(poll_at) => {}
            () = sleep(debounce_at) => {}
            () = sleep(postponed_at) => postponed_at = None,
            msg = rx.recv() => {
                match msg {
                    None => return,
                    // `async_set_updated_data` also drops a pending
                    // debounced refresh.
                    Some(Ctl::Updated) => {
                        debounce_at = None;
                        poll_at = if polling { next_poll() } else { far() };
                    }
                    Some(Ctl::Refresh) => {
                        debounce_at.get_or_insert(Instant::now() + t.debounce);
                    }
                    Some(Ctl::Postponed(d)) => postponed_at = Some(Instant::now() + d),
                    Some(Ctl::CancelPostponed) => postponed_at = None,
                }
                continue;
            }
        }
        // A refresh cancels the debounced one and reschedules the poll.
        debounce_at = None;
        bulb.refresh().await;
        poll_at = if polling { next_poll() } else { far() };
    }
}

/// Why setup stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum SetupError {
    Retry(String),
}

/// What setup learnt.
pub struct Found {
    conn: Conn,
    serial: [u8; 6],
    cached: Cached,
}

/// HA's setup: device info (5 × 3 s), then the first refresh (5 × 18 s),
/// both tagged to "any", then the serial check.
pub async fn connect(
    host: &str,
    port: u16,
    unique_id: &str,
    t: Timing,
) -> Result<Found, SetupError> {
    let conn = Conn::open(host, port)
        .await
        .map_err(|e| SetupError::Retry(format!("Could not resolve {host}: {e}")))?;
    let info = [
        Op::get(ty::GET_HOST_FIRMWARE, ty::STATE_HOST_FIRMWARE),
        Op::get(ty::GET_VERSION, ty::STATE_VERSION),
        Op::get(ty::GET_GROUP, ty::STATE_GROUP),
    ];
    if let Err(missing) = conn.execute(&info, ATTEMPTS, t.command_try).await {
        let names: Vec<&str> = missing
            .iter()
            .map(|i| ["get_hostfirmware", "get_version", "get_group"][*i])
            .collect();
        return Err(SetupError::Retry(format!(
            "{} timed out after 5 attempts",
            names.join(", ")
        )));
    }
    let r = conn
        .execute(
            &[Op::get(ty::LIGHT_GET, ty::LIGHT_STATE)],
            ATTEMPTS,
            t.poll_try,
        )
        .await
        .map_err(|_| SetupError::Retry("get_color timed out after 5 attempts".into()))?;
    let (h, payload) = &r[0];
    let s = proto::LightState::parse(payload)
        .ok_or_else(|| SetupError::Retry("get_color timed out after 5 attempts".into()))?;
    let found = proto::mac(&h.target);
    if found != unique_id {
        return Err(SetupError::Retry(format!(
            "Unexpected device found at {host}; expected {unique_id}, found {found}"
        )));
    }
    conn.set_target(h.target);
    Ok(Found {
        conn,
        serial: h.target,
        cached: Cached {
            power: s.power,
            color: s.color,
            label: s.label,
        },
    })
}

/// The bulb and its entities, from a setup, with its coordinator (stop
/// it to let the bulb go).
pub fn bulb(
    components: &Components,
    title: &str,
    host: &str,
    found: Found,
    polling: bool,
    timing: Timing,
) -> (Arc<Bulb>, tokio::task::JoinHandle<()>) {
    let hass = components.hass();
    let names = crate::naming::Names::new(hass, components.registries(), DOMAIN);
    let serial = proto::mac(&found.serial);
    let label = found.cached.label.clone();
    let light = names.entity("light", &serial, &serial, &label, None);
    let buttons = [("restart", "Restart"), ("identify", "Identify")]
        .into_iter()
        .filter_map(|(class, name)| {
            let (entity_id, friendly) = names.entity(
                "button",
                &format!("{serial}_{class}"),
                &serial,
                &label,
                Some(name),
            )?;
            let last = crate::button::restore(hass, &entity_id);
            Some(ButtonInfo {
                entity_id,
                friendly,
                class,
                last: Mutex::new(last),
            })
        })
        .collect();
    let (tx, rx) = mpsc::unbounded_channel();
    let b = Arc::new(Bulb {
        hass: hass.clone(),
        name: format!("{title} ({host})"),
        conn: found.conn,
        cached: Mutex::new(found.cached),
        ok: AtomicBool::new(true),
        ctl: tx,
        timing,
        light,
        buttons,
    });
    let coordinator = tokio::spawn(coordinate(b.clone(), rx, polling));
    if b.light.is_some() {
        components.lights().add(Arc::new(LifxLight(b.clone())));
    }
    for i in 0..b.buttons.len() {
        components.buttons().add(Arc::new(LifxButton(b.clone(), i)));
    }
    b.write_all();
    (b, coordinator)
}

/// The entry: set up on HA's ladder, then run; rediscovery moves it.
/// `ready` is done after the first connection attempt.
pub async fn run(
    components: Arc<Components>,
    entry: Map<String, Value>,
    port: u16,
    ready: ha_core::SetupGuard,
) {
    let mut ready = Some(ready);
    let s = |k: &str| entry.get(k).and_then(Value::as_str).map(str::to_owned);
    let (Some(mut host), Some(unique_id)) = (
        entry
            .get("data")
            .and_then(|d| d.get("host"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        s("unique_id"),
    ) else {
        tracing::error!("lifx: the entry has no host or unique id");
        return;
    };
    let title = s("title").unwrap_or_default();
    let polling = entry.get("pref_disable_polling").and_then(Value::as_bool) != Some(true);
    let (found_tx, mut found_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(discover(unique_id.clone(), port, found_tx));
    loop {
        let mut tries = 0u32;
        let found = loop {
            match connect(&host, port, &unique_id, Timing::HA).await {
                Ok(f) => break f,
                Err(SetupError::Retry(why)) => {
                    if let Some(r) = ready.take() {
                        r.done();
                    }
                    let wait = (5u64 << tries.min(7)).min(600);
                    tracing::info!(
                        "Config entry '{title}' for lifx integration not ready yet: {why}; Retrying in {wait} seconds"
                    );
                    tries += 1;
                    let jitter = Duration::from_micros(rand::random_range(50_000..500_001));
                    // Discovery of the bulb retries at once.
                    tokio::select! {
                        () = components.hass().setup_retry_wait(Duration::from_secs(wait) + jitter) => {}
                        Some(at) = found_rx.recv() => host = at,
                    }
                }
            }
        };
        let (b, coordinator) = bulb(&components, &title, &host, found, polling, Timing::HA);
        if let Some(r) = ready.take() {
            r.done();
        }
        // Running until the bulb turns up elsewhere.
        loop {
            match found_rx.recv().await {
                Some(at) if at != host => {
                    tracing::info!("lifx: {title} moved from {host} to {at}; reconnecting");
                    host = at;
                    break;
                }
                Some(_) => {}
                None => return std::future::pending().await,
            }
        }
        coordinator.abort();
        if let Some((id, _)) = &b.light {
            components.lights().remove(id);
        }
        for btn in &b.buttons {
            components.buttons().remove(&btn.entity_id);
        }
    }
}

/// Broadcast `GetService` every 15 min (and 5 s after start), listening
/// 8.5 s each time; report where the bulb answered from. Only to
/// 255.255.255.255: what HA sends with its default network settings (only
/// the default interface enabled), which is how the imported install runs.
async fn discover(unique_id: String, port: u16, found: mpsc::UnboundedSender<String>) {
    tokio::time::sleep(Duration::from_secs(5)).await;
    loop {
        if let Some(ip) = discover_once(&unique_id, port).await
            && found.send(ip).is_err()
        {
            return;
        }
        tokio::time::sleep(DISCOVERY_INTERVAL).await;
    }
}

async fn discover_once(unique_id: &str, port: u16) -> Option<String> {
    let sock = tokio::net::UdpSocket::bind("0.0.0.0:0").await.ok()?;
    sock.set_broadcast(true).ok()?;
    let frame = proto::Out {
        source: rand::random_range(1..=u32::MAX),
        target: [0; 6],
        seq: 0,
        ack: false,
        res: true,
        ty: ty::GET_SERVICE,
        payload: &[],
    }
    .encode();
    sock.send_to(&frame, ("255.255.255.255", port)).await.ok()?;
    let mut buf = [0u8; 256];
    let deadline = Instant::now() + DISCOVERY_WINDOW;
    let mut at = None;
    while let Ok(Ok((n, from))) = tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await
    {
        let Some((h, p)) = proto::decode(&buf[..n]) else {
            continue;
        };
        let udp = h.ty == ty::STATE_SERVICE && proto::service(p).is_some_and(|s| s.0 == 1);
        if (udp || h.ty == ty::LIGHT_STATE)
            && h.target != [0; 6]
            && proto::mac(&h.target) == unique_id
        {
            at.get_or_insert(from.ip().to_string());
        }
    }
    at
}

struct LifxLight(Arc<Bulb>);

impl crate::light::Light for LifxLight {
    fn entity_id(&self) -> &EntityId {
        &self.0.light.as_ref().unwrap_or_else(|| unreachable!()).0
    }

    fn capabilities(&self) -> Capabilities {
        light::capabilities()
    }

    fn state(&self) -> LightState {
        self.0.light_state()
    }

    fn available(&self) -> bool {
        self.0.ok.load(Ordering::Relaxed)
    }

    fn turn_on(&self, params: Params) -> BoxFuture<'_> {
        Box::pin(self.0.set_state(Some(true), params))
    }

    fn turn_off(&self, params: TurnOff) -> BoxFuture<'_> {
        let p = Params {
            transition: params.transition,
            ..Params::default()
        };
        Box::pin(self.0.set_state(Some(false), p))
    }
}

/// The restart (`0`) or identify (`1`) button.
struct LifxButton(Arc<Bulb>, usize);

impl Button for LifxButton {
    fn entity_id(&self) -> &EntityId {
        &self.0.buttons[self.1].entity_id
    }

    fn available(&self) -> bool {
        self.0.ok.load(Ordering::Relaxed)
    }

    fn pressed(&self, at: String, context: &ha_core::Context) {
        let b = &self.0.buttons[self.1];
        *b.last.lock() = Some(at);
        self.0.write_button(b, Some(context.clone()));
    }

    fn press(&self) -> BoxFuture<'_> {
        Box::pin(async move {
            match self.0.buttons[self.1].class {
                "restart" => {
                    self.0.conn.fire_and_forget(ty::SET_REBOOT).await;
                    Ok(())
                }
                _ => self.0.identify().await,
            }
        })
    }
}

#[cfg(test)]
mod tests;
