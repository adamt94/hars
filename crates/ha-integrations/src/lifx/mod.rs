//! The `lifx` integration (`components/lifx`) for single-zone bulbs on
//! the LAN, colour and white, as HA 2026.6.1 runs it with aiolifx 1.2.1:
//! one UDP socket per bulb, a `LightGet` every 10 s, HA's retry layer (5
//! tries; 3 s apart for commands, 18 s for polls), unavailability after a
//! poll's 90 s of silence, HA's `set_state` with its settle and refreshes,
//! the identify and restart buttons, and broadcast rediscovery. Set-up
//! makes the registry rows HA's makes ([`register`]), and reports where it
//! is to the entry; a removed entry stops ([`Bulb::stop`]).
//!
//! Not yet: the `lifx.*` services (effects, `set_state`, themes); a
//! `light.turn_on` with an effect calls `lifx.<effect>` as HA does, which
//! fails until they exist. The RSSI sensor (disabled by default) has its
//! row but doesn't run. Multi-zone, matrix, HEV (Clean) and infrared
//! (Night Vision) products aren't run as HA runs them: the config flow
//! refuses them ([`config_flow`]); an imported one is run as a single-zone
//! bulb, and its rows are left as HA made them. A bulb found at a new
//! address is reconnected to without rewriting the stored entry.

pub mod config_flow;
pub mod conn;
pub mod light;
pub mod products;
pub mod proto;
mod register;

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

/// What a running entry adds to `get_config`'s `components`.
pub const COMPONENTS: &[&str] = &["lifx", "light", "button"];

/// `lifx_features`: a product's features, those of the first product for
/// one aiolifx doesn't know.
pub fn features(product: Option<u32>) -> &'static products::Product {
    product
        .and_then(|id| products::PRODUCTS.iter().find(|p| p.id == id))
        .unwrap_or(&products::PRODUCTS[0])
}

/// The model HA gives the device: the product's name, or "LIFX Bulb".
pub fn model_name(product: Option<u32>) -> &'static str {
    product
        .and_then(|id| products::PRODUCTS.iter().find(|p| p.id == id))
        .map_or("LIFX Bulb", |p| p.name)
}

/// Whether `hars` runs a product as HA does: a single-zone bulb, colour or
/// white, with no infrared, HEV, zones, tiles, relays or buttons.
pub fn runs(product: &products::Product) -> bool {
    use products::*;
    product.flags
        & (MULTIZONE | EXTENDED_MULTIZONE | MATRIX | CHAIN | HEV | INFRARED | RELAYS | BUTTONS)
        == 0
}

/// A version aiolifx writes (`"3.70"`) as AwesomeVersion compares it.
fn version(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// `get_real_mac_addr`: the serial, its last byte one up from firmware
/// 3.70 on.
pub fn real_mac(serial: &[u8; 6], firmware: &str) -> String {
    let mut mac = *serial;
    if version(firmware) >= version("3.70") {
        mac[5] = mac[5].wrapping_add(1);
    }
    proto::mac(&mac)
}

/// The RSSI sensor's unit (`rssi_uom`): dB up to firmware 2.77, dBm after.
pub fn rssi_unit(firmware: &str) -> &'static str {
    match version(firmware) <= version("2.77") {
        true => "dB",
        false => "dBm",
    }
}

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
    /// The icon the user gave it.
    icon: Option<String>,
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
    model: light::Model,
    light: Option<(EntityId, String)>,
    /// The icon the user gave the light.
    light_icon: Option<String>,
    buttons: Vec<ButtonInfo>,
    /// False once the entry is stopped: nothing of it writes a state after.
    /// Held for every state write.
    alive: Mutex<bool>,
    components: Arc<Components>,
}

impl Bulb {
    fn light_state(&self) -> LightState {
        let c = self.cached.lock();
        light::state(self.model, c.power, c.color, None)
    }

    /// Write a state, unless the entry was stopped.
    fn write(
        &self,
        id: &EntityId,
        state: &str,
        a: Map<String, Value>,
        context: Option<ha_core::Context>,
    ) {
        let alive = self.alive.lock();
        if *alive {
            self.hass
                .set_state_with_context(id.clone(), state, a, context);
        }
    }

    /// The entry is stopped (HA's unload): its commands end, nothing is
    /// sent to the bulb or written after, and its entities go with their
    /// states and their place as service targets.
    pub fn stop(&self) {
        self.conn.stop();
        let mut alive = self.alive.lock();
        *alive = false;
        if let Some((id, _)) = &self.light {
            self.components.lights().remove(id);
            self.hass.remove_state(id);
        }
        for b in &self.buttons {
            self.components.buttons().remove(&b.entity_id);
            self.hass
                .restore()
                .unregister(&self.hass, b.entity_id.as_str());
            self.hass.remove_state(&b.entity_id);
        }
    }

    fn write_light(&self) {
        let Some((id, friendly)) = &self.light else {
            return;
        };
        let ok = self.ok.load(Ordering::Relaxed);
        let s = self.light_state();
        let mut a = crate::light::attributes(&light::capabilities(self.model), &s, ok);
        if let Some(icon) = &self.light_icon {
            a.insert("icon".into(), json!(icon));
        }
        a.insert("friendly_name".into(), json!(friendly));
        a.insert("supported_features".into(), json!(light::FEATURES));
        let state = match (ok, s.on) {
            (false, _) => "unavailable",
            (true, true) => "on",
            (true, false) => "off",
        };
        self.write(id, state, a, None);
    }

    fn write_button(&self, b: &ButtonInfo, context: Option<ha_core::Context>) {
        let state = match self.ok.load(Ordering::Relaxed) {
            true => b.last.lock().clone().unwrap_or_else(|| "unknown".into()),
            false => "unavailable".into(),
        };
        let mut a = Map::new();
        a.insert("device_class".into(), json!(b.class));
        if let Some(icon) = &b.icon {
            a.insert("icon".into(), json!(icon));
        }
        a.insert("friendly_name".into(), json!(b.friendly));
        self.write(&b.entity_id, &state, a, context);
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

/// Why setup stopped: HA's `ConfigEntryNotReady`, with its message (none
/// for a refresh that timed out) and what is logged.
#[derive(Debug, PartialEq, Eq)]
pub enum SetupError {
    Retry { reason: Option<String>, log: String },
}

/// What setup learnt.
pub struct Found {
    conn: Conn,
    serial: [u8; 6],
    cached: Cached,
    /// `host_firmware_version`, `product` and `group`.
    pub firmware: String,
    pub product: Option<u32>,
    pub group: String,
}

impl Found {
    pub fn serial(&self) -> String {
        proto::mac(&self.serial)
    }
}

/// HA's setup: device info (5 × 3 s), then the first refresh (5 × 18 s),
/// both tagged to "any", then the serial check. The bulb at `host` is
/// reached at `at` (the host and port, or a stand-in).
pub async fn connect(
    host: &str,
    at: (&str, u16),
    unique_id: &str,
    t: Timing,
) -> Result<Found, SetupError> {
    let conn = Conn::open(at.0, at.1).await.map_err(|e| {
        let why = format!("Could not resolve {host}: {e}");
        SetupError::Retry {
            reason: Some(why.clone()),
            log: why,
        }
    })?;
    let info = [
        Op::get(ty::GET_HOST_FIRMWARE, ty::STATE_HOST_FIRMWARE),
        Op::get(ty::GET_VERSION, ty::STATE_VERSION),
        Op::get(ty::GET_GROUP, ty::STATE_GROUP),
    ];
    // A refresh that times out is a `ConfigEntryNotReady` with no message.
    let timed_out = |log: String| SetupError::Retry { reason: None, log };
    let info = match conn.execute(&info, ATTEMPTS, t.command_try).await {
        Ok(info) => info,
        Err(missing) => {
            let names: Vec<&str> = missing
                .iter()
                .map(|i| ["get_hostfirmware", "get_version", "get_group"][*i])
                .collect();
            return Err(timed_out(format!(
                "{} timed out after 5 attempts",
                names.join(", ")
            )));
        }
    };
    let r = conn
        .execute(
            &[Op::get(ty::LIGHT_GET, ty::LIGHT_STATE)],
            ATTEMPTS,
            t.poll_try,
        )
        .await
        .map_err(|_| timed_out("get_color timed out after 5 attempts".into()))?;
    let (h, payload) = &r[0];
    let s = proto::LightState::parse(payload)
        .ok_or_else(|| timed_out("get_color timed out after 5 attempts".into()))?;
    let found = proto::mac(&h.target);
    if found != unique_id {
        let why = format!("Unexpected device found at {host}; expected {unique_id}, found {found}");
        return Err(SetupError::Retry {
            reason: Some(why.clone()),
            log: why,
        });
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
        firmware: proto::host_firmware(&info[0].1).unwrap_or_default(),
        product: proto::product(&info[1].1),
        group: proto::group(&info[2].1).unwrap_or_default(),
    })
}

/// The bulb and its entities, from a setup, with its coordinator (stop
/// it to let the bulb go).
pub fn bulb(
    components: &Arc<Components>,
    title: &str,
    host: &str,
    found: Found,
    polling: bool,
    timing: Timing,
) -> (Arc<Bulb>, tokio::task::JoinHandle<()>) {
    let hass = components.hass();
    let registries = components.registries();
    let names = crate::naming::Names::new(hass, &registries, DOMAIN);
    let serial = proto::mac(&found.serial);
    let label = found.cached.label.clone();
    let model = light::Model::of(features(found.product));
    let light = names.entity("light", &serial, &serial, &label, None);
    // The icon the user gave an entity is in its state.
    let icon = |domain: &str, unique_id: &str| {
        registries
            .entities
            .iter()
            .find(|e| {
                e.platform == DOMAIN
                    && e.entity_id.split('.').next() == Some(domain)
                    && e.unique_id.as_ref().and_then(Value::as_str) == Some(unique_id)
            })
            .and_then(|e| e.icon.clone())
    };
    let light_icon = icon("light", &serial);
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
                icon: icon("button", &format!("{serial}_{class}")),
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
        model,
        light,
        light_icon,
        buttons,
        alive: Mutex::new(true),
        components: components.clone(),
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

/// Where bulbs are reached: at their own address, or at stand-ins on
/// 127.0.0.1 (tests, and a debug build's browser run).
#[derive(Debug, Clone)]
pub struct Where {
    /// Where a discovery's `GetService` goes (`255.255.255.255:56700`).
    pub broadcast: std::net::SocketAddr,
    /// A bulb's address and the stand-in that answers for it.
    pub stand_ins: Vec<(String, std::net::SocketAddr)>,
    /// The config flow's waits.
    pub flow: config_flow::Timing,
    /// An entry's waits.
    pub timing: Timing,
}

impl Default for Where {
    fn default() -> Self {
        Self {
            broadcast: std::net::SocketAddr::from(([255, 255, 255, 255], conn::PORT)),
            stand_ins: Vec::new(),
            flow: config_flow::Timing::HA,
            timing: Timing::HA,
        }
    }
}

impl Where {
    /// Where the bulb at `host` is reached.
    pub fn addr(&self, host: &str) -> (String, u16) {
        match self.stand_ins.iter().find(|(h, _)| h == host) {
            Some((_, at)) => (at.ip().to_string(), at.port()),
            None => (host.to_owned(), conn::PORT),
        }
    }

    /// The bulb an answer came from.
    pub fn host_of(&self, from: std::net::SocketAddr) -> String {
        match self.stand_ins.iter().find(|(_, at)| *at == from) {
            Some((host, _)) => host.clone(),
            None => from.ip().to_string(),
        }
    }
}

/// When its task ends (its entry was removed, or the bulb moved), a
/// bulb stops: whoever stops the task waits for that.
struct Stopped(Arc<Bulb>, tokio::task::JoinHandle<()>);

impl Drop for Stopped {
    fn drop(&mut self) {
        self.1.abort();
        self.0.stop();
    }
}

/// A task that ends with whoever holds this.
struct Owned(tokio::task::JoinHandle<()>);

impl Drop for Owned {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The entry: set up on HA's ladder (retrying as `ConfigEntryNotReady`
/// does), make its registry rows, then run; rediscovery moves it. `ready`
/// is done after the first connection attempt; `status` hears where the
/// set-up is.
pub async fn run(
    components: Arc<Components>,
    entry: Map<String, Value>,
    at: Where,
    ready: ha_core::SetupGuard,
    status: crate::config_entries::Status,
) {
    use crate::config_entries::EntryState;
    let mut ready = Some(ready);
    let s = |k: &str| entry.get(k).and_then(Value::as_str).map(str::to_owned);
    let host = entry
        .get("data")
        .and_then(|d| d.get("host"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    // HA's legacy entry (no unique id, or `lifx`) sets nothing up.
    let unique_id = s("unique_id").filter(|u| u != DOMAIN);
    let (Some(mut host), Some(unique_id)) = (host, unique_id) else {
        tracing::warn!("lifx: the entry has no host or serial; nothing is set up for it");
        status.set(EntryState::Loaded, None);
        return;
    };
    // Two entries for one bulb would both run it: the second isn't started.
    if let Some((entries, entry_id)) = status.entry()
        && !entries.claim_unique_id(DOMAIN, entry_id, &unique_id)
    {
        tracing::error!(
            "lifx: another entry is already set up for {unique_id}; this one isn't started"
        );
        status.set(EntryState::SetupError, None);
        return;
    }
    let title = s("title").unwrap_or_default();
    let polling = entry.get("pref_disable_polling").and_then(Value::as_bool) != Some(true);
    let disable_new = entry
        .get("pref_disable_new_entities")
        .and_then(Value::as_bool)
        == Some(true);
    let (found_tx, mut found_rx) = mpsc::unbounded_channel::<String>();
    // Rediscovery ends with this task.
    let _discovery = Owned(tokio::spawn(discover(
        unique_id.clone(),
        at.clone(),
        found_tx,
    )));
    let mut tries = 0u32;
    let mut loaded = false;
    loop {
        let found = loop {
            let (ip, port) = at.addr(&host);
            match connect(&host, (&ip, port), &unique_id, at.timing).await {
                Ok(f) => break f,
                Err(SetupError::Retry { reason, log }) => {
                    status.set(EntryState::SetupRetry, reason);
                    if let Some(r) = ready.take() {
                        r.done();
                    }
                    let wait = (5u64 << tries.min(7)).min(600);
                    tracing::info!(
                        "Config entry '{title}' for lifx integration not ready yet: {log}; Retrying in {wait} seconds"
                    );
                    tries += 1;
                    let jitter = Duration::from_micros(rand::random_range(50_000..500_001));
                    // Discovery of the bulb retries at once.
                    tokio::select! {
                        () = components.hass().setup_retry_wait(Duration::from_secs(wait) + jitter) => {}
                        Some(found) = found_rx.recv() => host = found,
                    }
                    status.set(EntryState::SetupInProgress, None);
                }
            }
        };
        let stopped = {
            // No other entry is set up or removed, and no registry edit
            // made, while this one's rows and entities are.
            let _one = crate::switchbot_cloud::SETUP.lock();
            let product = features(found.product);
            if !runs(product) {
                tracing::warn!(
                    "lifx: {title} is a {}, which hars runs as a single-zone bulb; its registry rows are left as they are",
                    model_name(found.product)
                );
            } else if let (Some(registry), Some(entry_id)) = (
                components.registry(),
                entry.get("entry_id").and_then(Value::as_str),
            ) {
                register::register(components.hass(), registry, entry_id, &found, disable_new);
            }
            let (b, coordinator) = bulb(&components, &title, &host, found, polling, at.timing);
            Stopped(b, coordinator)
        };
        if !loaded {
            loaded = true;
            status.set(EntryState::Loaded, None);
        }
        drop(ready.take());
        // Running until the bulb turns up elsewhere.
        loop {
            match found_rx.recv().await {
                Some(found) if found != host => {
                    tracing::info!("lifx: {title} moved from {host} to {found}; reconnecting");
                    host = found;
                    break;
                }
                Some(_) => {}
                None => return std::future::pending().await,
            }
        }
        drop(stopped);
    }
}

/// Broadcast `GetService` every 15 min (and 5 s after start), listening
/// 8.5 s each time; report where the bulb answered from. Only to
/// 255.255.255.255: what HA sends with its default network settings (only
/// the default interface enabled), which is how the imported install runs.
async fn discover(unique_id: String, at: Where, found: mpsc::UnboundedSender<String>) {
    tokio::time::sleep(Duration::from_secs(5)).await;
    loop {
        let bulbs = scan(&at, DISCOVERY_WINDOW).await;
        if let Some((_, ip)) = bulbs.into_iter().find(|(serial, _)| *serial == unique_id)
            && found.send(ip).is_err()
        {
            return;
        }
        tokio::time::sleep(DISCOVERY_INTERVAL).await;
    }
}

/// aiolifx's `LifxDiscovery` for `window`: one `GetService` broadcast, then
/// every bulb that answers (as UDP, or with a `LightState`), by serial, in
/// the order they first answered, with the address each answered from.
/// Its socket is gone when this returns.
pub(crate) async fn scan(at: &Where, window: Duration) -> Vec<(String, String)> {
    let mut bulbs: Vec<(String, String)> = Vec::new();
    let Ok(sock) = tokio::net::UdpSocket::bind("0.0.0.0:0").await else {
        return bulbs;
    };
    if sock.set_broadcast(true).is_err() {
        return bulbs;
    }
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
    if sock.send_to(&frame, at.broadcast).await.is_err() {
        return bulbs;
    }
    let mut buf = [0u8; 256];
    let deadline = Instant::now() + window;
    while let Ok(Ok((n, from))) = tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await
    {
        let Some((h, p)) = proto::decode(&buf[..n]) else {
            continue;
        };
        let udp = h.ty == ty::STATE_SERVICE && proto::service(p).is_some_and(|s| s.0 == 1);
        if (udp || h.ty == ty::LIGHT_STATE) && h.target != [0; 6] {
            let serial = proto::mac(&h.target);
            if !bulbs.iter().any(|(s, _)| *s == serial) {
                bulbs.push((serial, at.host_of(from)));
            }
        }
    }
    bulbs
}

struct LifxLight(Arc<Bulb>);

impl crate::light::Light for LifxLight {
    fn entity_id(&self) -> &EntityId {
        &self.0.light.as_ref().unwrap_or_else(|| unreachable!()).0
    }

    fn capabilities(&self) -> Capabilities {
        light::capabilities(self.0.model)
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
