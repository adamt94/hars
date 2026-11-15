//! `mobile_app`'s `device_tracker` (`device_tracker.py` and the
//! `TrackerEntity` base) fed by `update_location`, and its `notify` entity
//! (restored only: push notifications aren't sent).

use super::{DOMAIN, Registration};
use ha_components::zone;
use ha_core::{EntityId, Hass};
use ha_registry::Registry;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// `ATTR_KEYS`: extra attributes, written when set.
const ATTR_KEYS: [&str; 4] = ["altitude", "course", "speed", "vertical_accuracy"];

struct Tracker {
    entity_id: EntityId,
    reg: Arc<Registration>,
    /// The last validated `update_location` (replaced whole each time).
    data: Map<String, Value>,
}

/// The trackers of every registration, by config entry.
pub struct Trackers {
    hass: Hass,
    registry: Arc<Registry>,
    inner: Arc<Mutex<HashMap<String, Tracker>>>,
}

impl std::fmt::Debug for Trackers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Trackers").finish_non_exhaustive()
    }
}

/// `LOCATION_UPDATE_SCHEMA`, coerced as voluptuous does; `None` when it
/// fails.
pub fn location_schema(v: &Value) -> Option<Map<String, Value>> {
    let m = v.as_object()?;
    // `key_dependency(gps, gps_accuracy)`.
    if m.contains_key("gps") && !m.contains_key("gps_accuracy") {
        return None;
    }
    let mut out = Map::new();
    for (k, v) in m {
        let coerced = match k.as_str() {
            "location_name" => json!(cv_string(v)?),
            "gps" => {
                let a = v.as_array().filter(|a| a.len() == 2)?;
                let lat = coerce_float(&a[0]).filter(|l| (-90.0..=90.0).contains(l))?;
                let lon = coerce_float(&a[1]).filter(|l| (-180.0..=180.0).contains(l))?;
                json!([lat, lon])
            }
            "gps_accuracy" => json!(coerce_float(v).filter(|f| *f >= 0.0)?),
            "battery" | "speed" | "course" | "vertical_accuracy" => {
                coerce_int(v).filter(|i| *i >= 0).map(int_json)?
            }
            "altitude" => json!(coerce_float(v)?),
            "in_zones" => {
                let ids = entity_ids(v)?;
                if ids.iter().any(|id| !id.starts_with("zone.")) {
                    return None;
                }
                json!(ids)
            }
            _ => return None,
        };
        out.insert(k.clone(), coerced);
    }
    Some(out)
}

fn cv_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(if *b { "True" } else { "False" }.into()),
        _ => None,
    }
}

/// `vol.Coerce(float)`: numbers, bools and numeric strings (Python's
/// `float()`); finite only.
fn coerce_float(v: &Value) -> Option<f64> {
    let f = match v {
        Value::Number(n) => n.as_f64()?,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::String(s) => crate::num::py_float_str(s)?,
        _ => return None,
    };
    f.is_finite().then_some(f)
}

/// `vol.Coerce(int)`: Python's `int()`: floats truncate toward zero;
/// strings must be integers. Exact up to `i128`.
fn coerce_int(v: &Value) -> Option<i128> {
    match v {
        Value::Number(n) => n.as_i64().map(i128::from).or_else(|| {
            n.as_u64().map(i128::from).or_else(|| {
                n.as_f64()
                    .filter(|f| f.is_finite() && f.abs() < 1.7e38)
                    .map(|f| f.trunc() as i128)
            })
        }),
        Value::Bool(b) => Some(i128::from(*b)),
        Value::String(s) => crate::num::py_int_str(s),
        _ => None,
    }
}

/// An integer as JSON: exact within 64 bits, else the nearest float.
fn int_json(i: i128) -> Value {
    i64::try_from(i)
        .map(Value::from)
        .or_else(|_| u64::try_from(i).map(Value::from))
        .unwrap_or_else(|_| json!(i as f64))
}

/// `cv.entity_ids`: a comma-separated string or a list, lowercased.
fn entity_ids(v: &Value) -> Option<Vec<String>> {
    let items: Vec<String> = match v {
        Value::String(s) => s.split(',').map(|x| x.trim().to_owned()).collect(),
        Value::Array(a) => a.iter().map(cv_string).collect::<Option<_>>()?,
        // A dict iterates its keys.
        Value::Object(m) => m.keys().cloned().collect(),
        _ => return None,
    };
    items
        .into_iter()
        .map(|s| {
            let lower = s.to_lowercase();
            ha_core::valid_entity_id(&lower).then_some(lower)
        })
        .collect()
}

impl Trackers {
    /// Each registration's tracker (its registry row keyed by the device
    /// id), restored from its extra data and written.
    pub fn setup(
        hass: &Hass,
        registry: Arc<Registry>,
        registrations: &[Arc<Registration>],
    ) -> Self {
        let this = Self {
            hass: hass.clone(),
            registry,
            inner: Arc::default(),
        };
        for reg in registrations {
            this.add(reg);
        }
        this
    }

    /// A registration's tracker (its registry row keyed by the device id),
    /// restored from its extra data and written.
    pub fn add(&self, reg: &Arc<Registration>) {
        let restore = self.hass.restore();
        let Some(device_id) = reg.device_id() else {
            return;
        };
        let Some(row) = self
            .registry
            .entity_by_unique_id("device_tracker", DOMAIN, &device_id)
            .filter(|r| r.disabled_by.is_none())
        else {
            return;
        };
        let Ok(entity_id) = EntityId::parse(&row.entity_id) else {
            return;
        };
        // `MobileAppDeviceTrackerExtraStoredData.from_dict`.
        let data = restore
            .last_extra_data(entity_id.as_str())
            .and_then(|e| e.get("data").cloned())
            .and_then(|d| location_schema(&d))
            .unwrap_or_default();
        let inner = Arc::downgrade(&self.inner);
        let entry = reg.entry_id.clone();
        restore.register(
            entity_id.as_str(),
            Some(Arc::new(move || {
                let inner = inner.upgrade()?;
                let inner = inner.lock();
                Some(json!({"data": inner.get(&entry)?.data}))
            })),
        );
        let tracker = Tracker {
            entity_id,
            reg: reg.clone(),
            data,
        };
        self.write(&tracker, false);
        self.inner.lock().insert(reg.entry_id.clone(), tracker);
    }

    /// The entry unloads: the tracker goes (its last state kept for
    /// restore).
    pub fn unload(&self, entry_id: &str) {
        let entity_id = self.inner.lock().get(entry_id).map(|t| t.entity_id.clone());
        let Some(entity_id) = entity_id else {
            return;
        };
        // Unregistering reads the restore data, which takes the lock.
        self.hass
            .restore()
            .unregister(&self.hass, entity_id.as_str());
        self.inner.lock().remove(entry_id);
        self.hass.remove_state(&entity_id);
    }

    /// `update_location`: the data replaced whole and written (always:
    /// `force_update`). `false` when the payload fails the schema.
    pub fn update(&self, reg: &Registration, payload: &Value) -> bool {
        let Some(data) = location_schema(payload) else {
            return false;
        };
        let mut inner = self.inner.lock();
        let Some(tracker) = inner.get_mut(&reg.entry_id) else {
            return true;
        };
        tracker.data = data;
        self.write(tracker, true);
        true
    }

    /// `TrackerEntity`'s state and attributes.
    fn write(&self, t: &Tracker, force: bool) {
        let hass = &self.hass;
        let data = &t.data;
        let gps = data.get("gps").and_then(Value::as_array);
        let lat = gps.and_then(|g| g.first()).and_then(Value::as_f64);
        let lon = gps.and_then(|g| g.get(1)).and_then(Value::as_f64);
        let accuracy = data.get("gps_accuracy").cloned().unwrap_or(json!(0));
        let zone_state = |id: &str| {
            EntityId::parse(id)
                .ok()
                .and_then(|id| hass.states().get(&id))
        };

        // `_async_write_ha_state`: the active zone and the zones.
        let (active, in_zones): (Option<Arc<ha_core::State>>, Option<Vec<String>>) =
            match data.get("in_zones").and_then(Value::as_array) {
                Some(zones) => {
                    let mut states: Vec<Arc<ha_core::State>> = zones
                        .iter()
                        .filter_map(|z| z.as_str().and_then(zone_state))
                        .collect();
                    states.sort_by(|a, b| {
                        let r = |s: &ha_core::State| {
                            s.attributes
                                .get("radius")
                                .and_then(Value::as_f64)
                                .unwrap_or(0.0)
                        };
                        r(a).partial_cmp(&r(b)).unwrap_or(std::cmp::Ordering::Equal)
                    });
                    let active = states
                        .iter()
                        .find(|z| {
                            !z.attributes
                                .get("passive")
                                .and_then(Value::as_bool)
                                .unwrap_or(false)
                        })
                        .cloned();
                    (
                        active,
                        Some(states.iter().map(|z| z.entity_id.to_string()).collect()),
                    )
                }
                None => match (lat, lon) {
                    (Some(lat), Some(lon)) => {
                        let (active, zones) =
                            zone::in_zones(hass, lat, lon, accuracy.as_f64().unwrap_or(0.0));
                        (active, Some(zones))
                    }
                    _ => (None, None),
                },
            };

        // `location_name`: ignored once the app sends `in_zones`.
        let location_name = if data.contains_key("in_zones") {
            None
        } else {
            match data.get("location_name").and_then(Value::as_str) {
                Some("") | None => None,
                Some("home") => Some("home".to_owned()),
                Some(name) => Some(
                    zone_state(&format!("zone.{name}"))
                        .map(|z| state_name(&z))
                        .unwrap_or_else(|| name.to_owned()),
                ),
            }
        };
        let state = match location_name {
            Some(name) => name,
            None if (lat.is_some() && lon.is_some()) || in_zones.is_some() => match &active {
                None => "not_home".to_owned(),
                Some(z) if z.entity_id.as_str() == "zone.home" => "home".to_owned(),
                Some(z) => state_name(z),
            },
            None => "unknown".to_owned(),
        };

        let mut a = Map::new();
        a.insert("in_zones".into(), json!(in_zones.unwrap_or_default()));
        a.insert("source_type".into(), json!("gps"));
        if let Some(b) = data.get("battery") {
            a.insert("battery_level".into(), b.clone());
        }
        if let (Some(lat), Some(lon)) = (lat, lon) {
            a.insert("latitude".into(), json!(lat));
            a.insert("longitude".into(), json!(lon));
            a.insert("gps_accuracy".into(), accuracy);
        }
        for k in ATTR_KEYS {
            if let Some(v) = data.get(k).filter(|v| !v.is_null()) {
                a.insert(k.into(), v.clone());
            }
        }
        let snap = self.registry.snapshot();
        let row = snap
            .entities
            .iter()
            .find(|e| e.entity_id == t.entity_id.as_str());
        let name = row
            .and_then(|r| r.name.clone())
            .filter(|n| !n.is_empty())
            .or_else(|| t.reg.device_name());
        if let Some(name) = name {
            a.insert("friendly_name".into(), json!(name));
        }
        if let Some(icon) = row.and_then(|r| r.icon.clone()) {
            a.insert("icon".into(), json!(icon));
        }
        if force {
            hass.set_state_forced(t.entity_id.clone(), state, a);
        } else {
            hass.set_state(t.entity_id.clone(), state, a);
        }
    }
}

/// `State.name`: the friendly name, else the object id with spaces.
fn state_name(s: &ha_core::State) -> String {
    s.attributes
        .get("friendly_name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| s.entity_id.object_id().replace('_', " "))
}

/// The registrations' `notify` entities (`MobileAppNotifyEntity`), made
/// when the app can receive push. Their state is the last message's time,
/// restored; `hars` sends no push, so it never changes.
pub fn setup_notify(hass: &Hass, registry: &Registry, registrations: &[Arc<Registration>]) {
    let restore = hass.restore();
    for reg in registrations {
        let Some(entity_id) = notify_entity(registry, reg) else {
            continue;
        };
        restore.register(entity_id.as_str(), None);
        let state = restore
            .last_state(entity_id.as_str())
            .and_then(|s| s.get("state").and_then(Value::as_str).map(str::to_owned))
            .filter(|s| s != "unavailable")
            .unwrap_or_else(|| "unknown".into());
        let mut a = Map::new();
        // `has_entity_name` with no name: the device's name.
        let row =
            registry.entity_by_unique_id("notify", DOMAIN, &reg.device_id().unwrap_or_default());
        let name = row
            .and_then(|r| r.name.clone())
            .or_else(|| reg.device_name());
        if let Some(name) = name {
            a.insert("friendly_name".into(), json!(name));
        }
        a.insert("supported_features".into(), json!(1));
        hass.set_state(entity_id, state, a);
    }
}

/// Whether the app can receive push (`supports_push`): cloud push, or the
/// local push channel.
pub fn supports_push(reg: &Registration) -> bool {
    reg.with_data(|data| {
        data.get("app_data")
            .and_then(Value::as_object)
            .is_some_and(|a| {
                (a.contains_key("push_token") && a.contains_key("push_url"))
                    || a.contains_key("push_websocket_channel")
            })
    })
}

/// The registration's enabled notify row's entity, when it supports push.
fn notify_entity(registry: &Registry, reg: &Registration) -> Option<EntityId> {
    if !supports_push(reg) {
        return None;
    }
    let device_id = reg.device_id()?;
    let row = registry
        .entity_by_unique_id("notify", DOMAIN, &device_id)
        .filter(|r| r.disabled_by.is_none())?;
    EntityId::parse(&row.entity_id).ok()
}

/// The entry unloads: its notify entity goes (its state kept for restore).
pub fn unload_notify(hass: &Hass, registry: &Registry, reg: &Registration) {
    if let Some(entity_id) = reg
        .device_id()
        .and_then(|d| registry.entity_by_unique_id("notify", DOMAIN, &d))
        .and_then(|r| EntityId::parse(&r.entity_id).ok())
    {
        hass.restore().unregister(hass, entity_id.as_str());
        hass.remove_state(&entity_id);
    }
}
