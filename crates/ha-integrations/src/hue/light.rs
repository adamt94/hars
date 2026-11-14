//! `HueLight` and `GroupedHueLight` (`components/hue/v2/light.py`,
//! `group.py`): state from the stored resources, commands as aiohue sends
//! them.

use super::Bridge;
use super::store::Store;
use crate::light::{
    self, BoxFuture, Capabilities, ColorMode, FEATURE_EFFECT, FEATURE_FLASH, FEATURE_TRANSITION,
    Flash, LightState, Modes, Params, TurnOff, color,
};
use ha_core::{EntityId, ServiceError};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::sync::Weak;

const FALLBACK_MIN_MIREDS: i64 = 153;
const FALLBACK_MAX_MIREDS: i64 = 500;
const FALLBACK_KELVIN: i64 = 5800;

/// aiohue's `EffectStatus` (anything else reads as `unknown`).
const EFFECTS: &[&str] = &[
    "no_effect",
    "candle",
    "fire",
    "sparkle",
    "glisten",
    "opal",
    "prism",
    "underwater",
    "cosmos",
    "sunbeam",
    "enchant",
];
const TIMED_EFFECTS: &[&str] = &["no_effect", "sunrise", "sunset"];

fn effect_status(s: &str) -> &str {
    if EFFECTS.contains(&s) { s } else { "unknown" }
}

fn timed_status(s: &str) -> &str {
    if TIMED_EFFECTS.contains(&s) {
        s
    } else {
        "unknown"
    }
}

/// aiohue's `DynamicStatus`.
fn dynamic_status(s: &str) -> &str {
    match s {
        "none" | "dynamic_palette" => s,
        _ => "unknown",
    }
}

/// A present, non-empty optional feature (aiohue parses `{}` as `None`).
fn feature<'a>(r: &'a Value, key: &str) -> Option<&'a Value> {
    r.get(key)
        .filter(|v| v.as_object().is_some_and(|m| !m.is_empty()))
}

fn f64_at(v: &Value, path: &[&str]) -> Option<f64> {
    path.iter().try_fold(v, |v, k| v.get(*k))?.as_f64()
}

/// `normalize_hue_brightness`: 0..255 to Hue's 0..100, unrounded.
fn hue_brightness(b: i64) -> f64 {
    (b as f64 / 255.0) * 100.0
}

/// `normalize_hue_transition`: seconds to ms, rounded to 100 ms.
fn hue_transition(t: f64) -> i64 {
    (crate::num::round(t, 1) * 1000.0) as i64
}

/// `normalize_hue_colortemp`: kelvin to mirek, clamped to the light's range.
fn hue_mirek(k: i64, min: i64, max: i64) -> Result<i64, ServiceError> {
    let m = color::kelvin_to_mired(k as f64).map_err(|e| ServiceError::Failed(e.to_string()))?;
    Ok(m.max(min).min(max))
}

/// The bridge's answer to a command, as HA reports it.
fn result(r: Result<(), super::api::ApiError>) -> Result<(), ServiceError> {
    match r {
        Ok(()) => Ok(()),
        Err(e) => match e.service_message() {
            None => {
                tracing::debug!("{e}");
                Ok(())
            }
            Some(m) => Err(ServiceError::Failed(m)),
        },
    }
}

/// One `light` resource.
pub struct HueLight {
    pub(super) bridge: Weak<Bridge>,
    pub(super) rid: String,
    pub(super) entity_id: EntityId,
    /// Fixed when the entity is made, as HA's.
    caps: Capabilities,
    /// Brightness to restore after a fading turn-off (`_last_brightness`).
    last_brightness: Mutex<Option<f64>>,
    /// Whether HA's last turn-on set a colour temperature (uncertified
    /// lights only).
    color_temp_active: Mutex<bool>,
}

impl HueLight {
    pub fn new(bridge: Weak<Bridge>, resource: &Value, entity_id: EntityId) -> Self {
        let mut features = 0;
        if resource["alert"]["action_values"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
        {
            features |= FEATURE_FLASH;
        }
        let mut modes = vec![ColorMode::Onoff];
        if feature(resource, "color").is_some() {
            modes.push(ColorMode::Xy);
        }
        if feature(resource, "color_temperature").is_some() {
            modes.push(ColorMode::ColorTemp);
        }
        if feature(resource, "dimming").is_some() {
            modes.push(ColorMode::Brightness);
            features |= FEATURE_TRANSITION;
        }
        let mut effects: Vec<String> = Vec::new();
        if let Some(e) = feature(resource, "effects") {
            for v in e["status_values"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                let s = effect_status(v);
                if s != "no_effect" && s != "unknown" {
                    effects.push(s.to_owned());
                }
            }
        }
        if let Some(e) = feature(resource, "timed_effects") {
            for v in e["status_values"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                let s = timed_status(v);
                if s != "no_effect" {
                    effects.push(s.to_owned());
                }
            }
        }
        let effect_list = if effects.is_empty() {
            None
        } else {
            effects.insert(0, "off".into());
            features |= FEATURE_EFFECT;
            Some(effects)
        };
        Self {
            bridge,
            rid: resource["id"].as_str().unwrap_or_default().to_owned(),
            entity_id,
            caps: Capabilities {
                modes: Modes::filtered(modes),
                features,
                effect_list,
            },
            last_brightness: Mutex::new(None),
            color_temp_active: Mutex::new(false),
        }
    }

    fn mireds(r: &Value) -> (i64, i64) {
        let schema = |k| {
            feature(r, "color_temperature")
                .and_then(|c| c["mirek_schema"][k].as_i64())
                .filter(|m| *m != 0)
        };
        (
            schema("mirek_minimum").unwrap_or(FALLBACK_MIN_MIREDS),
            schema("mirek_maximum").unwrap_or(FALLBACK_MAX_MIREDS),
        )
    }

    /// The light's state from its resource.
    pub(super) fn state_in(&self, store: &Store) -> Option<LightState> {
        let r = store.get(&self.rid)?;
        let certified = store
            .device_of(&self.rid)
            .and_then(|d| d["product_data"]["certified"].as_bool())
            .unwrap_or(false);
        let ct = feature(r, "color_temperature");
        let mirek = ct.and_then(|c| c["mirek"].as_i64());
        let ct_active = match mirek {
            None => false,
            Some(_) if certified => ct.and_then(|c| c["mirek_valid"].as_bool()).unwrap_or(false),
            Some(_) => *self.color_temp_active.lock(),
        };
        let mode = self.caps.modes.single().unwrap_or(if ct_active {
            ColorMode::ColorTemp
        } else {
            ColorMode::Xy
        });
        let (min_m, max_m) = Self::mireds(r);
        let k = |m: i64| color::mired_to_kelvin(m as f64).unwrap_or(0);
        let effect = {
            let status = |key: &str, map: fn(&str) -> &str| {
                feature(r, key)
                    .and_then(|e| e["status"].as_str())
                    .map(map)
                    .filter(|s| *s != "no_effect")
            };
            status("effects", effect_status)
                .or_else(|| status("timed_effects", timed_status))
                .unwrap_or("off")
                .to_owned()
        };
        Some(LightState {
            on: r["on"]["on"].as_bool().unwrap_or(false),
            color_mode: Some(mode),
            brightness: feature(r, "dimming")
                .and_then(|d| d["brightness"].as_f64())
                .map(|b| ((b / 100.0) * 255.0).round_ties_even() as i64),
            color_temp_kelvin: match ct {
                Some(_) => mirek.map(k),
                None => Some(FALLBACK_KELVIN),
            },
            min_color_temp_kelvin: k(max_m),
            max_color_temp_kelvin: k(min_m),
            xy_color: feature(r, "color")
                .and_then(|c| Some((f64_at(c, &["xy", "x"])?, f64_at(c, &["xy", "y"])?))),
            effect: Some(effect),
            ..Default::default()
        })
    }

    /// The state HA writes: `(state, attributes)`.
    pub(super) fn render(
        &self,
        bridge: &Bridge,
        store: &Store,
    ) -> Option<(String, Map<String, Value>)> {
        let r = store.get(&self.rid)?;
        let state = self.state_in(store)?;
        let available = bridge.available(store, &self.rid, true);
        let mut a = light::attributes(&self.caps, &state, available);
        if available {
            a.insert("mode".into(), json!(r["mode"].as_str().unwrap_or("normal")));
            a.insert(
                "dynamics".into(),
                json!(dynamic_status(
                    r["dynamics"]["status"].as_str().unwrap_or("unknown")
                )),
            );
        }
        bridge.tail(&mut a, &self.entity_id, store.device_of(&self.rid), None);
        a.insert("supported_features".into(), json!(self.caps.features));
        let s = if !available {
            "unavailable"
        } else if state.on {
            "on"
        } else {
            "off"
        };
        Some((s.to_owned(), a))
    }

    async fn put(&self, body: Value) -> Result<(), ServiceError> {
        let Some(bridge) = self.bridge.upgrade() else {
            return Ok(());
        };
        result(bridge.api.put("light", &self.rid, &body).await)
    }

    /// `set_flash`: short identifies the light's device, long breathes.
    pub(super) async fn flash(&self, short: bool) -> Result<(), ServiceError> {
        let Some(bridge) = self.bridge.upgrade() else {
            return Ok(());
        };
        if short {
            let device = bridge
                .store
                .read()
                .device_of(&self.rid)
                .and_then(|d| d["id"].as_str().map(str::to_owned));
            let Some(device) = device else {
                return Ok(());
            };
            return result(
                bridge
                    .api
                    .put(
                        "device",
                        &device,
                        &json!({"identify": {"action": "identify"}}),
                    )
                    .await,
            );
        }
        self.put(json!({"alert": {"action": "breathe"}})).await
    }

    fn do_turn_on(&self, p: Params) -> Result<Option<Value>, ServiceError> {
        let bridge = self
            .bridge
            .upgrade()
            .ok_or_else(|| ServiceError::Failed("bridge gone".into()))?;
        let store = bridge.store.read();
        let r = store.get(&self.rid).cloned().unwrap_or_default();
        let current_effect = self.state_in(&store).and_then(|s| s.effect);
        drop(store);
        let mut transition = p.transition.map(hue_transition);
        let mut xy = p.xy_color;
        let (min_m, max_m) = Self::mireds(&r);
        let mut mirek = p
            .color_temp_kelvin
            .map(|k| hue_mirek(k, min_m, max_m))
            .transpose()?;
        let mut brightness = p.brightness.map(hue_brightness);
        {
            // `if self._last_brightness and brightness is None`: only a
            // non-zero memory is used (and then forgotten).
            let mut last = self.last_brightness.lock();
            if brightness.is_none() && last.is_some_and(|b| b != 0.0) {
                brightness = last.take();
            }
        }
        *self.color_temp_active.lock() = mirek.is_some();
        // (effects, timed effect)
        let mut effect: Option<(bool, String)> = None;
        let mut effect_str = p.effect.clone();
        if effect_str.as_deref() == Some("None") {
            tracing::warn!(
                "Detected deprecated effect 'None' in {}, use 'off' instead. This will stop working in HA 2025.10",
                self.entity_id
            );
            effect_str = Some("off".into());
        }
        match effect_str.as_deref() {
            Some("off") => {
                if current_effect.as_deref() != Some("off") {
                    effect = Some((false, "no_effect".into()));
                }
            }
            Some(name) => {
                let s = effect_status(name);
                if s == "unknown" {
                    effect = Some((true, timed_status(name).to_owned()));
                    if transition.is_none() {
                        transition = Some(600_000);
                    }
                } else {
                    effect = Some((false, s.to_owned()));
                }
                mirek = None;
                xy = None;
            }
            None => {}
        }
        if p.flash.is_some() {
            return Ok(None);
        }
        let mut body = Map::new();
        body.insert("on".into(), json!({"on": true}));
        if let Some(b) = brightness {
            body.insert("dimming".into(), json!({"brightness": b}));
        }
        if let Some((x, y)) = xy {
            body.insert("color".into(), json!({"xy": {"x": x, "y": y}}));
        }
        if let Some(m) = mirek {
            body.insert("color_temperature".into(), json!({"mirek": m}));
        }
        // aiohue: `transition_time is not None and effect in (None, NO_EFFECT)`.
        let plain = match &effect {
            None => true,
            Some((false, e)) => e == "no_effect",
            Some((true, _)) => false,
        };
        if let Some(t) = transition.filter(|_| plain) {
            body.insert("dynamics".into(), json!({"duration": t}));
        }
        match effect {
            Some((true, name)) => {
                let mut timed = json!({"effect": name});
                if let Some(t) = transition {
                    timed["duration"] = json!(t);
                }
                body.insert("timed_effects".into(), timed);
            }
            Some((false, name)) => {
                body.insert("effects".into(), json!({"effect": name}));
            }
            None => {}
        }
        Ok(Some(Value::Object(body)))
    }
}

impl light::Light for HueLight {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn capabilities(&self) -> Capabilities {
        self.caps.clone()
    }

    fn state(&self) -> LightState {
        self.bridge
            .upgrade()
            .and_then(|b| self.state_in(&b.store.read()))
            .unwrap_or_default()
    }

    fn available(&self) -> bool {
        self.bridge
            .upgrade()
            .is_some_and(|b| b.available(&b.store.read(), &self.rid, true))
    }

    fn turn_on(&self, p: Params) -> BoxFuture<'_> {
        Box::pin(async move {
            let flash = p.flash;
            match self.do_turn_on(p)? {
                Some(body) => self.put(body).await,
                None => self.flash(flash == Some(Flash::Short)).await,
            }
        })
    }

    fn turn_off(&self, p: TurnOff) -> BoxFuture<'_> {
        Box::pin(async move {
            let transition = p.transition.map(hue_transition);
            if transition.is_some()
                && let Some(bridge) = self.bridge.upgrade()
            {
                let b = bridge
                    .store
                    .read()
                    .get(&self.rid)
                    .and_then(|r| feature(r, "dimming").and_then(|d| d["brightness"].as_f64()));
                if let Some(b) = b {
                    *self.last_brightness.lock() = Some(b);
                }
            }
            if let Some(f) = p.flash {
                return self.flash(f == Flash::Short).await;
            }
            let mut body = json!({"on": {"on": false}});
            if let Some(t) = transition {
                body["dynamics"] = json!({"duration": t});
            }
            self.put(body).await
        })
    }
}

/// What `GroupedHueLight._update_values` keeps between updates.
#[derive(Debug, Clone)]
struct GroupValues {
    modes: Modes,
    mode: ColorMode,
    brightness: Option<i64>,
    brightness_pct: f64,
    color_temp_kelvin: Option<i64>,
    min_kelvin: i64,
    max_kelvin: i64,
    xy: Option<(f64, f64)>,
    dynamics: bool,
}

/// A room's or zone's `grouped_light`.
pub struct GroupLight {
    pub(super) bridge: Weak<Bridge>,
    pub(super) rid: String,
    /// The room or zone.
    pub(super) group: String,
    pub(super) entity_id: EntityId,
    values: Mutex<GroupValues>,
    restore_brightness: Mutex<Option<f64>>,
}

const GROUP_FEATURES: u32 = FEATURE_FLASH | FEATURE_TRANSITION;

impl GroupLight {
    pub fn new(
        bridge: Weak<Bridge>,
        rid: &str,
        group: &str,
        entity_id: EntityId,
        store: &Store,
    ) -> Self {
        let g = Self {
            bridge,
            rid: rid.to_owned(),
            group: group.to_owned(),
            entity_id,
            values: Mutex::new(GroupValues {
                modes: Modes::new([ColorMode::Onoff]),
                mode: ColorMode::Onoff,
                brightness: None,
                brightness_pct: 0.0,
                color_temp_kelvin: None,
                min_kelvin: 2000,
                max_kelvin: 6535,
                xy: None,
                dynamics: false,
            }),
            restore_brightness: Mutex::new(None),
        };
        g.update_values(store);
        g
    }

    /// The member lights' ids.
    pub(super) fn members(&self, store: &Store) -> Vec<String> {
        store
            .get(&self.group)
            .map(|g| {
                store
                    .lights_of(g)
                    .iter()
                    .filter_map(|l| l["id"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `_update_values`, run on each event the group hears.
    pub(super) fn update_values(&self, store: &Store) {
        let Some(group) = store.get(&self.group) else {
            return;
        };
        let mut v = self.values.lock();
        let (mut with_color, mut with_ct, mut with_dim, mut on_dim) = (0, 0, 0, 0);
        let (mut in_ct, mut in_xy, mut dynamic) = (0, 0, 0);
        let (mut total_b, mut xs, mut ys, mut xy_n, mut temps) = (0.0, 0.0, 0.0, 0, 0.0);
        let k = |m: i64| color::mired_to_kelvin(m as f64).ok();
        for light in store.lights_of(group) {
            let on = light["on"]["on"].as_bool().unwrap_or(false);
            let mut in_ct_mode = false;
            if let Some(ct) = feature(light, "color_temperature") {
                with_ct += 1;
                let mirek = ct["mirek"].as_i64();
                v.color_temp_kelvin = mirek.filter(|m| *m != 0).and_then(k);
                if let Some(kv) = ct["mirek_schema"]["mirek_maximum"].as_i64().and_then(k) {
                    v.min_kelvin = kv;
                }
                if let Some(kv) = ct["mirek_schema"]["mirek_minimum"].as_i64().and_then(k) {
                    v.max_kelvin = kv;
                }
                if on
                    && ct["mirek_valid"].as_bool().unwrap_or(false)
                    && let Some(kv) = mirek.and_then(k)
                {
                    in_ct += 1;
                    in_ct_mode = true;
                    temps += kv as f64;
                }
            }
            if let Some(c) = feature(light, "color")
                && let (Some(x), Some(y)) = (f64_at(c, &["xy", "x"]), f64_at(c, &["xy", "y"]))
            {
                with_color += 1;
                v.xy = Some((x, y));
                if on {
                    xs += x;
                    ys += y;
                    xy_n += 1;
                    if !in_ct_mode {
                        in_xy += 1;
                    }
                }
            }
            if let Some(d) = feature(light, "dimming") {
                with_dim += 1;
                if on {
                    total_b += d["brightness"].as_f64().unwrap_or(0.0);
                    on_dim += 1;
                }
            }
            if light["dynamics"]["status"] == "dynamic_palette" {
                dynamic += 1;
            }
        }
        let mut modes = Vec::new();
        if with_color > 0 {
            modes.push(ColorMode::Xy);
        }
        if with_ct > 0 {
            modes.push(ColorMode::ColorTemp);
        }
        if with_dim > 0 {
            if modes.is_empty() {
                modes.push(ColorMode::Brightness);
            }
            if on_dim > 0 {
                let mean = total_b / f64::from(on_dim);
                v.brightness_pct = mean;
                v.brightness = Some(((mean / 100.0) * 255.0).round_ties_even() as i64);
            }
        } else {
            modes.push(ColorMode::Onoff);
        }
        v.dynamics = dynamic > 0;
        v.modes = Modes::new(modes);
        if xy_n > 0 {
            let n = f64::from(xy_n);
            v.xy = Some((crate::num::round(xs / n, 5), crate::num::round(ys / n, 5)));
        }
        if in_ct > 0 {
            v.color_temp_kelvin = Some((temps / f64::from(in_ct)).round_ties_even() as i64);
        }
        v.mode = if in_xy > 0 && in_xy >= in_ct {
            ColorMode::Xy
        } else if in_ct > 0 && in_ct > in_xy {
            ColorMode::ColorTemp
        } else if with_color > 0 {
            ColorMode::Xy
        } else if with_ct > 0 {
            ColorMode::ColorTemp
        } else if with_dim > 0 {
            ColorMode::Brightness
        } else {
            ColorMode::Onoff
        };
    }

    fn caps(&self) -> Capabilities {
        Capabilities {
            modes: self.values.lock().modes.clone(),
            features: GROUP_FEATURES,
            effect_list: None,
        }
    }

    fn state_in(&self, store: &Store) -> LightState {
        let v = self.values.lock().clone();
        LightState {
            on: store
                .get(&self.rid)
                .and_then(|r| r["on"]["on"].as_bool())
                .unwrap_or(false),
            color_mode: Some(v.mode),
            brightness: v.brightness,
            color_temp_kelvin: v.color_temp_kelvin,
            min_color_temp_kelvin: v.min_kelvin,
            max_color_temp_kelvin: v.max_kelvin,
            xy_color: v.xy,
            ..Default::default()
        }
    }

    /// The state HA writes (a group is always available).
    pub(super) fn render(
        &self,
        bridge: &Bridge,
        store: &Store,
    ) -> Option<(String, Map<String, Value>)> {
        store.get(&self.rid)?;
        let group = store.get(&self.group)?;
        let state = self.state_in(store);
        let caps = self.caps();
        let mut a = light::attributes(&caps, &state, true);
        let scenes: Vec<&str> = store
            .of_type("scene")
            .chain(store.of_type("smart_scene"))
            .filter(|s| s["group"]["rid"] == self.group.as_str())
            .filter_map(|s| s["metadata"]["name"].as_str())
            .fold(Vec::new(), |mut acc, n| {
                if !acc.contains(&n) {
                    acc.push(n);
                }
                acc
            });
        let (mut names, mut ids) = (Vec::new(), Vec::new());
        for light in store.lights_of(group) {
            let id = light["id"].as_str().unwrap_or_default();
            if let Some(n) = store
                .device_of(id)
                .and_then(|d| d["metadata"]["name"].as_str())
                && !names.contains(&n)
            {
                names.push(n);
            }
            if let Some(e) = bridge.registered_id("light", id)
                && !ids.contains(&e)
            {
                ids.push(e);
            }
        }
        a.insert("is_hue_group".into(), json!(true));
        a.insert("hue_scenes".into(), json!(scenes));
        a.insert("hue_type".into(), json!(group["type"]));
        a.insert("lights".into(), json!(names));
        a.insert("entity_id".into(), json!(ids));
        a.insert("dynamics".into(), json!(self.values.lock().dynamics));
        bridge.tail(&mut a, &self.entity_id, Some(group), None);
        a.insert("supported_features".into(), json!(GROUP_FEATURES));
        Some((if state.on { "on" } else { "off" }.to_owned(), a))
    }

    async fn put(&self, body: Value) -> Result<(), ServiceError> {
        let Some(bridge) = self.bridge.upgrade() else {
            return Ok(());
        };
        result(bridge.api.put("grouped_light", &self.rid, &body).await)
    }

    async fn flash(&self, short: bool) -> Result<(), ServiceError> {
        let Some(bridge) = self.bridge.upgrade() else {
            return Ok(());
        };
        if !short {
            return self.put(json!({"alert": {"action": "breathe"}})).await;
        }
        // Each member's device identifies, concurrently.
        let devices: Vec<String> = {
            let store = bridge.store.read();
            self.members(&store)
                .iter()
                .filter_map(|l| {
                    store
                        .device_of(l)
                        .and_then(|d| d["id"].as_str().map(str::to_owned))
                })
                .collect()
        };
        let body = json!({"identify": {"action": "identify"}});
        let calls = devices.iter().map(|d| bridge.api.put("device", d, &body));
        for r in light::join_all(calls).await {
            result(r)?;
        }
        Ok(())
    }
}

impl light::Light for GroupLight {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn capabilities(&self) -> Capabilities {
        self.caps()
    }

    fn state(&self) -> LightState {
        self.bridge
            .upgrade()
            .map(|b| self.state_in(&b.store.read()))
            .unwrap_or_default()
    }

    fn available(&self) -> bool {
        true
    }

    fn turn_on(&self, p: Params) -> BoxFuture<'_> {
        Box::pin(async move {
            let (min_k, max_k) = {
                let v = self.values.lock();
                (v.min_kelvin, v.max_kelvin)
            };
            let transition = p.transition.map(hue_transition);
            let to_m = |k: i64| {
                color::kelvin_to_mired(k as f64).map_err(|e| ServiceError::Failed(e.to_string()))
            };
            let mirek = match p.color_temp_kelvin {
                Some(k) => Some(hue_mirek(k, to_m(max_k)?, to_m(min_k)?)?),
                None => None,
            };
            let mut brightness = p.brightness.map(hue_brightness);
            {
                let mut restore = self.restore_brightness.lock();
                if brightness.is_none() && restore.is_some_and(|b| b != 0.0) {
                    brightness = restore.take();
                }
            }
            if let Some(f) = p.flash {
                return self.flash(f == Flash::Short).await;
            }
            let mut body = Map::new();
            body.insert("on".into(), json!({"on": true}));
            if let Some(b) = brightness {
                body.insert("dimming".into(), json!({"brightness": b}));
            }
            if let Some((x, y)) = p.xy_color {
                body.insert("color".into(), json!({"xy": {"x": x, "y": y}}));
            }
            if let Some(m) = mirek {
                body.insert("color_temperature".into(), json!({"mirek": m}));
            }
            if let Some(t) = transition {
                body.insert("dynamics".into(), json!({"duration": t}));
            }
            self.put(Value::Object(body)).await
        })
    }

    fn turn_off(&self, p: TurnOff) -> BoxFuture<'_> {
        Box::pin(async move {
            let transition = p.transition.map(hue_transition);
            if transition.is_some() {
                *self.restore_brightness.lock() = Some(self.values.lock().brightness_pct);
            }
            if let Some(f) = p.flash {
                return self.flash(f == Flash::Short).await;
            }
            let mut body = json!({"on": {"on": false}});
            if let Some(t) = transition {
                body["dynamics"] = json!({"duration": t});
            }
            self.put(body).await
        })
    }
}
