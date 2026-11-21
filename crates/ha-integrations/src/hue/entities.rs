//! The hue entities other than lights (`components/hue/v2/sensor.py`,
//! `binary_sensor.py`, `switch.py`, `scene.py`): one [`HueEntity`] per
//! resource and role, its state and attributes as HA writes them.

use super::Bridge;
use super::store::Store;
use crate::light::BoxFuture;
use ha_core::{EntityId, ServiceError};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::sync::Weak;

/// What an entity shows of its resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Motion,
    GroupedMotion,
    Entertainment,
    Contact,
    Tamper,
    Scene,
    SmartScene,
    Temperature,
    LightLevel,
    GroupedLightLevel,
    Battery,
    Zigbee,
    MotionEnabled,
    LightLevelEnabled,
    Behavior,
}

/// `HueZigbeeConnectivitySensor`'s options.
const ZIGBEE_OPTIONS: &[&str] = &[
    "connected",
    "disconnected",
    "connectivity_issue",
    "unidirectional_incoming",
];

impl Kind {
    pub fn domain(self) -> &'static str {
        match self {
            Self::Motion
            | Self::GroupedMotion
            | Self::Entertainment
            | Self::Contact
            | Self::Tamper => "binary_sensor",
            Self::Scene | Self::SmartScene => "scene",
            Self::Temperature
            | Self::LightLevel
            | Self::GroupedLightLevel
            | Self::Battery
            | Self::Zigbee => "sensor",
            Self::MotionEnabled | Self::LightLevelEnabled | Self::Behavior => "switch",
        }
    }

    /// When HA makes this entity relative to the others (and so when it
    /// hears an event): platforms in order (binary_sensor, event, light,
    /// scene, sensor, switch), then each platform's controllers in the
    /// order it registers them. Lights are 20.
    pub fn order(self, ty: &str) -> u8 {
        match self {
            Self::Motion if ty == "camera_motion" => 0,
            Self::Motion => 1,
            Self::Entertainment => 2,
            Self::Contact => 3,
            Self::Tamper => 4,
            Self::GroupedMotion => 5,
            Self::Scene => 30,
            Self::SmartScene => 31,
            Self::Temperature => 40,
            Self::LightLevel => 41,
            Self::Battery => 42,
            Self::Zigbee => 43,
            Self::GroupedLightLevel => 44,
            Self::MotionEnabled => 50,
            Self::LightLevelEnabled => 51,
            Self::Behavior => 52,
        }
    }

    /// Named the legacy way (`has_entity_name=False`): the device name is
    /// stripped from the front of the name, then put back.
    pub fn legacy(self) -> bool {
        matches!(self, Self::Behavior | Self::Entertainment)
    }

    /// HA's `entity_registry_enabled_default`.
    pub fn enabled_default(self) -> bool {
        self != Self::Zigbee
    }

    /// Named by its resource (a scene's or an automation's own name), not
    /// by a fixed word HA translates.
    pub fn named_by_resource(self) -> bool {
        matches!(
            self,
            Self::Scene | Self::SmartScene | Self::Behavior | Self::Entertainment
        )
    }

    /// The registry's `original_device_class`.
    pub fn device_class(self) -> Option<&'static str> {
        Some(match self {
            Self::Motion | Self::GroupedMotion => "motion",
            Self::Entertainment => "running",
            Self::Contact => "opening",
            Self::Tamper => "tamper",
            Self::Scene | Self::SmartScene => return None,
            Self::Temperature => "temperature",
            Self::LightLevel | Self::GroupedLightLevel => "illuminance",
            Self::Battery => "battery",
            Self::Zigbee => "enum",
            Self::MotionEnabled | Self::LightLevelEnabled | Self::Behavior => "switch",
        })
    }

    /// The registry's `entity_category`.
    pub fn category(self) -> Option<&'static str> {
        match self {
            Self::Tamper | Self::Battery | Self::Zigbee => Some("diagnostic"),
            Self::MotionEnabled | Self::LightLevelEnabled | Self::Behavior => Some("config"),
            _ => None,
        }
    }

    pub fn translation_key(self) -> Option<&'static str> {
        match self {
            Self::Zigbee => Some("zigbee_connectivity"),
            Self::MotionEnabled => Some("motion_sensor_enabled"),
            Self::LightLevelEnabled => Some("light_sensor_enabled"),
            _ => None,
        }
    }

    /// The registry's `unit_of_measurement` (a metric system's).
    pub fn unit(self) -> Option<&'static str> {
        match self {
            Self::Temperature => Some("°C"),
            Self::LightLevel | Self::GroupedLightLevel => Some("lx"),
            Self::Battery => Some("%"),
            _ => None,
        }
    }

    /// The registry's `capabilities` (a sensor's `capability_attributes`).
    pub fn capabilities(self) -> Option<Value> {
        match self {
            Self::Temperature | Self::LightLevel | Self::GroupedLightLevel | Self::Battery => {
                Some(json!({"state_class": "measurement"}))
            }
            Self::Zigbee => Some(json!({"options": ZIGBEE_OPTIONS})),
            _ => None,
        }
    }

    /// `suggested_display_precision`, which HA stores in the row's options.
    pub fn suggested_precision(self) -> Option<u8> {
        (self == Self::Temperature).then_some(1)
    }

    /// The entities a resource type gets, in platform order.
    pub fn for_type(ty: &str) -> &'static [Kind] {
        match ty {
            "motion" => &[Self::Motion, Self::MotionEnabled],
            "camera_motion" => &[Self::Motion],
            "grouped_motion" => &[Self::GroupedMotion],
            "entertainment_configuration" => &[Self::Entertainment],
            "contact" => &[Self::Contact],
            "tamper" => &[Self::Tamper],
            "scene" => &[Self::Scene],
            "smart_scene" => &[Self::SmartScene],
            "temperature" => &[Self::Temperature],
            "light_level" => &[Self::LightLevel, Self::LightLevelEnabled],
            "grouped_light_level" => &[Self::GroupedLightLevel],
            "device_power" => &[Self::Battery],
            "zigbee_connectivity" => &[Self::Zigbee],
            "behavior_instance" => &[Self::Behavior],
            _ => &[],
        }
    }

    /// The entity's own name (with `has_entity_name`), or for legacy-named
    /// entities their full name; `None` when only the device names it.
    pub fn name(self, r: &Value) -> Option<String> {
        Some(match self {
            Self::Motion | Self::GroupedMotion => "Motion".into(),
            Self::Contact => "Opening".into(),
            Self::Tamper => "Tamper".into(),
            Self::Temperature => "Temperature".into(),
            Self::LightLevel | Self::GroupedLightLevel => "Illuminance".into(),
            Self::Battery => "Battery".into(),
            Self::Zigbee => "Zigbee connectivity".into(),
            Self::MotionEnabled => "Motion sensor enabled".into(),
            Self::LightLevelEnabled => "Light sensor enabled".into(),
            Self::Behavior => format!("Automation: {}", r["metadata"]["name"].as_str()?),
            Self::Scene | Self::SmartScene | Self::Entertainment => {
                r["metadata"]["name"].as_str()?.to_owned()
            }
        })
    }

    /// Whether HA makes this entity for the resource (grouped sensors need
    /// a room, zone or service group with more than one child).
    pub fn wanted(self, r: &Value, store: &Store) -> bool {
        match self {
            Self::GroupedMotion | Self::GroupedLightLevel => {
                matches!(
                    r["owner"]["rtype"].as_str(),
                    Some("room" | "zone" | "service_group")
                ) && r["owner"]["rid"]
                    .as_str()
                    .and_then(|p| store.get(p))
                    .and_then(|p| p["children"].as_array())
                    .is_some_and(|c| c.len() > 1)
            }
            Self::Scene | Self::SmartScene => r["group"]["rid"]
                .as_str()
                .and_then(|g| store.get(g))
                .is_some(),
            _ => true,
        }
    }
}

/// The value of a sensor-like resource: its `*_report`, else the flat field.
/// A number as aiohue parses one: a JSON number, or a string Python's
/// `float()` / `int()` takes.
fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn integer(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn reading<'a>(r: &'a Value, feature: &str, field: &str) -> Option<&'a Value> {
    let f = &r[feature];
    f[format!("{field}_report")]
        .get(field)
        .filter(|v| !v.is_null())
        .or_else(|| f.get(field).filter(|v| !v.is_null()))
}

fn on_off(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "on",
        Some(false) => "off",
        None => "unknown",
    }
}

/// `int(10 ** ((value - 1) / 10000))`.
fn lux(level: f64) -> i64 {
    10f64.powf((level - 1.0) / 10000.0) as i64
}

pub struct HueEntity {
    pub(super) bridge: Weak<Bridge>,
    pub(super) kind: Kind,
    /// [`Kind::order`].
    pub(super) order: u8,
    pub(super) rid: String,
    pub(super) entity_id: EntityId,
    /// A scene's last activation by HA (its state).
    activated: Mutex<Option<String>>,
    /// The bridge no longer has this entity (its resource was deleted):
    /// set and read under the bridge's write guard ([`Bridge::remove`]),
    /// so a state rendered before the removal isn't written after it.
    pub(super) gone: std::sync::atomic::AtomicBool,
}

impl HueEntity {
    pub fn new(bridge: Weak<Bridge>, kind: Kind, ty: &str, rid: &str, entity_id: EntityId) -> Self {
        Self {
            bridge,
            kind,
            order: kind.order(ty),
            rid: rid.to_owned(),
            entity_id,
            activated: Mutex::new(None),
            gone: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// A scene restores its last activation (`BaseScene`, a RestoreEntity):
    /// any stored state but `unavailable`, and it's dumped from now on.
    pub(super) fn restore(&self, hass: &ha_core::Hass) {
        if !matches!(self.kind, Kind::Scene | Kind::SmartScene) {
            return;
        }
        let restore = hass.restore();
        if let Some(state) = restore
            .last_state(self.entity_id.as_str())
            .and_then(|s| s["state"].as_str().map(str::to_owned))
            .filter(|s| s != "unavailable")
        {
            *self.activated.lock() = Some(state);
        }
        restore.register(self.entity_id.as_str(), None);
    }

    /// The Hue resource naming the entity's device: its own device, the
    /// owning room/zone/service group, the scene's group, or the bridge.
    pub fn device<'a>(kind: Kind, rid: &str, r: &'a Value, store: &'a Store) -> Option<&'a Value> {
        match kind {
            Kind::GroupedMotion | Kind::GroupedLightLevel => store.get(r["owner"]["rid"].as_str()?),
            Kind::Scene | Kind::SmartScene => store.get(r["group"]["rid"].as_str()?),
            _ => store.device_of(rid).or_else(|| store.bridge_device()),
        }
    }

    /// `(state, attributes)` as HA writes them, or `None` if HA wouldn't
    /// write (a connectivity status outside the options).
    pub(super) fn render(
        &self,
        bridge: &Bridge,
        store: &Store,
    ) -> Option<(String, Map<String, Value>)> {
        let r = store.get(&self.rid)?;
        let available = self.kind == Kind::Zigbee || bridge.available(store, &self.rid, false);
        let mut a = Map::new();
        let enabled = r["enabled"].as_bool().unwrap_or(true);
        let measurement = |a: &mut Map<String, Value>| {
            a.insert("state_class".into(), json!("measurement"));
        };
        let (state, unit, device_class): (String, Option<&str>, &str) = match self.kind {
            Kind::Motion | Kind::GroupedMotion => {
                let motion = enabled
                    .then(|| reading(r, "motion", "motion").and_then(Value::as_bool))
                    .flatten();
                (on_off(motion).into(), None, "motion")
            }
            Kind::Entertainment => (
                on_off(Some(r["status"] == "active")).into(),
                None,
                "running",
            ),
            Kind::Contact => {
                let open = enabled
                    .then(|| {
                        r["contact_report"]["state"]
                            .as_str()
                            .map(|s| s != "contact")
                    })
                    .flatten();
                (on_off(open).into(), None, "opening")
            }
            Kind::Tamper => {
                let tampered = r["tamper_reports"]
                    .as_array()
                    .and_then(|t| t.first())
                    .is_some_and(|t| t["state"] == "tampered");
                (on_off(Some(tampered)).into(), None, "tamper")
            }
            Kind::Temperature => {
                measurement(&mut a);
                // In the unit the sensor component settles on (the
                // user's, the one kept for the row, or the unit
                // system's), converted from the bridge's °C.
                let mut rows = bridge.registries.entities.iter();
                let row = rows.find(|e| e.entity_id == self.entity_id.as_str());
                let unit = crate::sensor::state_unit(&bridge.hass, row.map(|row| &**row));
                let t = reading(r, "temperature", "temperature").and_then(number);
                let s = t.map_or("unknown".into(), |t| {
                    // `_stringify_state` of a float: 15 significant digits.
                    let shown = crate::sensor::from_celsius(crate::num::round(t, 1), unit);
                    crate::num::py_format(shown, 15)
                });
                (s, Some(unit), "temperature")
            }
            Kind::LightLevel | Kind::GroupedLightLevel => {
                measurement(&mut a);
                let level = reading(r, "light", "light_level").and_then(number);
                if available && let Some(l) = level {
                    a.insert("light_level".into(), json!(l as i64));
                }
                (
                    level.map_or("unknown".into(), |l| lux(l).to_string()),
                    Some("lx"),
                    "illuminance",
                )
            }
            Kind::Battery => {
                measurement(&mut a);
                let p = &r["power_state"];
                // An unknown state is no state, as aiohue's enum makes it.
                if available
                    && let Some(s) = p["battery_state"]
                        .as_str()
                        .filter(|s| matches!(*s, "normal" | "low" | "critical"))
                {
                    a.insert("battery_state".into(), json!(s));
                }
                let level = integer(&p["battery_level"]);
                (
                    level.map_or("unknown".into(), |l| l.to_string()),
                    Some("%"),
                    "battery",
                )
            }
            Kind::Zigbee => {
                a.insert("options".into(), json!(ZIGBEE_OPTIONS));
                let status = r["status"].as_str().unwrap_or_default();
                if !ZIGBEE_OPTIONS.contains(&status) {
                    return None;
                }
                if let Some(mac) = r["mac_address"].as_str() {
                    a.insert("mac_address".into(), json!(mac));
                }
                (status.to_owned(), None, "enum")
            }
            Kind::MotionEnabled | Kind::LightLevelEnabled | Kind::Behavior => {
                (on_off(Some(enabled)).into(), None, "switch")
            }
            Kind::Scene | Kind::SmartScene => {
                let group = store.get(r["group"]["rid"].as_str()?)?;
                a.insert("group_name".into(), group["metadata"]["name"].clone());
                a.insert("group_type".into(), group["type"].clone());
                a.insert("name".into(), r["metadata"]["name"].clone());
                if self.kind == Kind::Scene {
                    scene_attributes(r, &mut a);
                } else {
                    smart_scene_attributes(r, store, &mut a);
                }
                let state = self
                    .activated
                    .lock()
                    .clone()
                    .unwrap_or_else(|| "unknown".into());
                let device = Self::device(self.kind, &self.rid, r, store);
                bridge.tail(
                    &mut a,
                    &self.entity_id,
                    device,
                    self.kind.name(r).as_deref(),
                );
                return Some((state, a));
            }
        };
        if let Some(u) = unit {
            a.insert("unit_of_measurement".into(), json!(u));
        }
        a.insert("device_class".into(), json!(device_class));
        let device = Self::device(self.kind, &self.rid, r, store);
        let name = self.kind.name(r);
        if self.kind.legacy() {
            bridge.tail_legacy(&mut a, &self.entity_id, device, name.as_deref());
        } else {
            bridge.tail(&mut a, &self.entity_id, device, name.as_deref());
        }
        let state = if available {
            state
        } else {
            "unavailable".into()
        };
        Some((state, a))
    }

    fn put_target(&self) -> &'static str {
        match self.kind {
            Kind::MotionEnabled => "motion",
            Kind::LightLevelEnabled => "light_level",
            Kind::Behavior => "behavior_instance",
            Kind::SmartScene => "smart_scene",
            _ => "scene",
        }
    }

    async fn put(&self, body: Value) -> Result<(), ServiceError> {
        let Some(bridge) = self.bridge.upgrade() else {
            return Ok(());
        };
        super::light::result(bridge.api.put(self.put_target(), &self.rid, &body).await)
    }

    /// HA's `_async_activate`: record the time and write it, then recall.
    /// `options` are `hue.activate_scene`'s (dynamic, speed 0..100,
    /// brightness 0..255).
    pub(super) async fn activate(
        &self,
        transition: Option<f64>,
        dynamic: bool,
        speed: Option<i64>,
        brightness: Option<i64>,
    ) -> Result<(), ServiceError> {
        let Some(bridge) = self.bridge.upgrade() else {
            return Ok(());
        };
        *self.activated.lock() = Some(ha_core::isoformat(bridge.hass.clock().now()));
        bridge.write_entity(self);
        if self.kind == Kind::SmartScene {
            return self.put(json!({"recall": {"action": "activate"}})).await;
        }
        if let Some(s) = speed {
            self.put(json!({"speed": s as f64 / 100.0})).await?;
        }
        let mut recall = json!({"action": if dynamic { "dynamic_palette" } else { "active" }});
        if let Some(t) = transition {
            recall["duration"] = json!((crate::num::round(t, 1) * 1000.0) as i64);
        }
        if let Some(b) = brightness {
            recall["dimming"] = json!({"brightness": (b as f64 / 255.0) * 100.0});
        }
        self.put(json!({"recall": recall})).await
    }
}

/// A regular scene's extra attributes.
fn scene_attributes(r: &Value, a: &mut Map<String, Value>) {
    let palette = &r["palette"];
    let brightness = palette["dimming"]
        .as_array()
        .and_then(|d| d.first())
        .and_then(|d| d["brightness"].as_f64())
        .or_else(|| {
            r["actions"].as_array()?.iter().find_map(|a| {
                let d = &a["action"]["dimming"];
                d.as_object().filter(|m| !m.is_empty())?;
                d["brightness"].as_f64()
            })
        })
        .map(|b| ((b / 100.0) * 255.0).round_ties_even() as i64);
    let many = |k: &str| palette[k].as_array().is_some_and(|l| l.len() > 1);
    a.insert(
        "speed".into(),
        r["speed"].as_f64().map_or(Value::Null, |s| json!(s)),
    );
    a.insert("brightness".into(), json!(brightness));
    a.insert(
        "is_dynamic".into(),
        json!(many("color") || many("color_temperature")),
    );
}

/// A smart scene's extra attributes.
fn smart_scene_attributes(r: &Value, store: &Store, a: &mut Map<String, Value>) {
    let active = r["state"] == "active";
    a.insert("is_active".into(), json!(active));
    let slot = &r["active_timeslot"];
    if !active || slot.is_null() {
        return;
    }
    let (Some(id), Some(day)) = (slot["timeslot_id"].as_i64(), slot["weekday"].as_str()) else {
        return;
    };
    a.insert("active_timeslot_id".into(), json!(id));
    a.insert("active_timeslot_name".into(), json!(day));
    // The `id`-th timeslot counted across all days, as HA counts it.
    let target = r["week_timeslots"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|d| d["timeslots"].as_array().into_iter().flatten())
        .nth(id as usize)
        .and_then(|t| t["target"]["rid"].as_str());
    if let Some(name) = target
        .and_then(|t| store.get(t))
        .and_then(|s| s["metadata"]["name"].as_str())
    {
        a.insert("active_scene".into(), json!(name));
    }
}

impl crate::switch::Switch for HueEntity {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn available(&self) -> bool {
        self.bridge
            .upgrade()
            .is_some_and(|b| b.available(&b.store.read(), &self.rid, false))
    }

    fn is_on(&self) -> bool {
        self.bridge
            .upgrade()
            .and_then(|b| {
                b.store
                    .read()
                    .get(&self.rid)
                    .and_then(|r| r["enabled"].as_bool())
            })
            .unwrap_or(false)
    }

    fn turn(&self, on: bool) -> BoxFuture<'_> {
        Box::pin(self.put(json!({"enabled": on})))
    }
}

impl crate::scene::Scene for HueEntity {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn available(&self) -> bool {
        true
    }

    fn activate(&self, transition: Option<f64>) -> BoxFuture<'_> {
        Box::pin(HueEntity::activate(self, transition, false, None, None))
    }
}
