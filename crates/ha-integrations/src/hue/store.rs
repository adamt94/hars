//! The bridge's resources as aiohue keeps them: the full snapshot, then
//! each event-stream item merged in. Only the types an entity reads are
//! kept; the rest (scripts, matter, geolocation, ...) are dropped at load.

use serde_json::{Map, Value};
use std::collections::HashMap;

/// The resource types `hars` reads.
const KEPT: &[&str] = &[
    "behavior_instance",
    "bell_button",
    "bridge",
    "bridge_home",
    "button",
    "camera_motion",
    "contact",
    "device",
    "device_power",
    "entertainment_configuration",
    "grouped_light",
    "grouped_light_level",
    "grouped_motion",
    "light",
    "light_level",
    "motion",
    "motion_area_configuration",
    "relative_rotary",
    "room",
    "scene",
    "security_area_motion",
    "service_group",
    "smart_scene",
    "tamper",
    "temperature",
    "zigbee_connectivity",
    "zone",
];

/// What an event did to the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added,
    Updated,
    Deleted,
}

/// The resources, in the bridge's order.
#[derive(Debug, Default, Clone)]
pub struct Store {
    order: Vec<String>,
    by_id: HashMap<String, Value>,
}

/// aiohue's `update_dataclass`: an absent key, or one sent as `null`,
/// keeps its value; objects merge key by key; anything else (lists
/// included) replaces. Reports whether anything changed.
pub fn merge(cur: &mut Map<String, Value>, update: &Map<String, Value>) -> bool {
    let mut changed = false;
    for (k, v) in update {
        match (cur.get_mut(k), v) {
            (_, Value::Null) => {}
            (Some(Value::Object(c)), Value::Object(u)) => changed |= merge(c, u),
            (Some(c), v) if c == v => {}
            (_, v) => {
                cur.insert(k.clone(), v.clone());
                changed = true;
            }
        }
    }
    changed
}

impl Store {
    /// From `GET /clip/v2/resource`'s `data`.
    pub fn new(resources: Vec<Value>) -> Self {
        let mut s = Self::default();
        for r in resources {
            s.insert(r);
        }
        s
    }

    fn insert(&mut self, r: Value) {
        let (Some(id), Some(ty)) = (r["id"].as_str(), r["type"].as_str()) else {
            return;
        };
        if !KEPT.contains(&ty) {
            return;
        }
        let id = id.to_owned();
        if self.by_id.insert(id.clone(), r).is_none() {
            self.order.push(id);
        }
    }

    pub fn get(&self, id: &str) -> Option<&Value> {
        self.by_id.get(id)
    }

    /// Every resource of a type, in the bridge's order.
    pub fn of_type<'a>(&'a self, ty: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
        self.order
            .iter()
            .filter_map(|id| self.by_id.get(id))
            .filter(move |r| r["type"] == ty)
    }

    /// One event item (`add`, `update` or `delete`). An update for an
    /// unknown id is dropped, as aiohue does; `None` when nothing applies.
    pub fn apply(&mut self, kind: &str, item: &Value) -> Option<Change> {
        let id = item.get("rid").or_else(|| item.get("id"))?.as_str()?;
        match kind {
            "add" => {
                if !KEPT.contains(&item["type"].as_str()?) {
                    return None;
                }
                self.insert(item.clone());
                Some(Change::Added)
            }
            "update" => {
                let Value::Object(cur) = self.by_id.get_mut(id)? else {
                    return None;
                };
                let Value::Object(update) = item else {
                    return None;
                };
                merge(cur, update);
                Some(Change::Updated)
            }
            "delete" => {
                self.by_id.remove(id)?;
                self.order.retain(|i| i != id);
                Some(Change::Deleted)
            }
            _ => None,
        }
    }

    /// Reconcile with a fresh snapshot: what was added, changed (merged as
    /// an update would be) and removed, in snapshot order then removals.
    pub fn reconcile(&mut self, fresh: Vec<Value>) -> Vec<(Change, String)> {
        let fresh = Store::new(fresh);
        let mut out = Vec::new();
        for id in &fresh.order {
            let new = &fresh.by_id[id];
            match self.by_id.get_mut(id) {
                Some(Value::Object(cur)) => {
                    if let Value::Object(n) = new
                        && merge(cur, n)
                    {
                        out.push((Change::Updated, id.clone()));
                    }
                }
                _ => {
                    self.insert(new.clone());
                    out.push((Change::Added, id.clone()));
                }
            }
        }
        let gone: Vec<String> = self
            .order
            .iter()
            .filter(|id| !fresh.by_id.contains_key(*id))
            .cloned()
            .collect();
        for id in gone {
            self.by_id.remove(&id);
            self.order.retain(|i| *i != id);
            out.push((Change::Deleted, id));
        }
        out
    }

    /// Every resource, in the bridge's order.
    pub fn order(&self) -> impl Iterator<Item = &Value> {
        self.order.iter().filter_map(|id| self.by_id.get(id))
    }

    /// The bridge's own device (the one with a `bridge` service).
    pub fn bridge_device(&self) -> Option<&Value> {
        self.of_type("device")
            .find(|d| services(d).any(|s| s["rtype"] == "bridge"))
    }

    /// The device whose services include `id` (aiohue's `get_device`).
    pub fn device_of(&self, id: &str) -> Option<&Value> {
        self.of_type("device").find(|d| has_service(d, id))
    }

    /// A device's service of a type.
    pub fn service_of<'a>(&'a self, device: &'a Value, ty: &str) -> Option<&'a Value> {
        services(device)
            .filter(|s| s["rtype"] == ty)
            .find_map(|s| self.get(s["rid"].as_str()?))
    }

    /// The room or zone a `grouped_light` belongs to.
    pub fn group_of(&self, grouped_light: &str) -> Option<&Value> {
        ["room", "zone"]
            .into_iter()
            .flat_map(|t| self.of_type(t))
            .find(|g| has_service(g, grouped_light))
    }

    /// A room's or zone's lights: a room's devices' light services, a
    /// zone's light children (in the bridge's order of lights).
    pub fn lights_of(&self, group: &Value) -> Vec<&Value> {
        let children = group["children"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        if group["type"] == "zone" {
            return self
                .of_type("light")
                .filter(|l| children.iter().any(|c| c["rid"] == l["id"]))
                .collect();
        }
        let mut out = Vec::new();
        for child in children.iter().filter(|c| c["rtype"] == "device") {
            let Some(device) = child["rid"].as_str().and_then(|id| self.get(id)) else {
                continue;
            };
            for s in services(device).filter(|s| s["rtype"] == "light") {
                if let Some(l) = s["rid"].as_str().and_then(|id| self.get(id))
                    && !out.iter().any(|o: &&Value| o["id"] == l["id"])
                {
                    out.push(l);
                }
            }
        }
        out
    }
}

pub fn services(r: &Value) -> impl Iterator<Item = &Value> {
    r["services"].as_array().into_iter().flatten()
}

fn has_service(r: &Value, id: &str) -> bool {
    services(r).any(|s| s["rid"] == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merges_as_aiohue() {
        let mut cur =
            json!({"on": {"on": false}, "color_temperature": {"mirek": 416, "mirek_valid": true},
            "effects": {"status_values": ["a", "b"]}})
            .as_object()
            .unwrap()
            .clone();
        let update = json!({"on": {"on": true}, "color_temperature": {"mirek": null, "mirek_valid": false},
            "effects": {"status_values": ["c"]}, "new": 1});
        assert!(merge(&mut cur, update.as_object().unwrap()));
        assert_eq!(
            Value::Object(cur.clone()),
            json!({"on": {"on": true}, "color_temperature": {"mirek": 416, "mirek_valid": false},
                "effects": {"status_values": ["c"]}, "new": 1})
        );
        // The same again changes nothing.
        assert!(!merge(&mut cur, update.as_object().unwrap()));
    }

    #[test]
    fn applies_events_and_reconciles() {
        let mut s = Store::new(vec![
            json!({"id": "a", "type": "light", "on": {"on": false}}),
            json!({"id": "x", "type": "behavior_script"}),
        ]);
        assert!(s.get("x").is_none());
        assert_eq!(
            s.apply(
                "update",
                &json!({"id": "a", "type": "light", "on": {"on": true}})
            ),
            Some(Change::Updated)
        );
        assert_eq!(
            s.apply("update", &json!({"id": "zz", "type": "light"})),
            None
        );
        assert_eq!(
            s.apply(
                "add",
                &json!({"id": "b", "type": "light", "on": {"on": true}})
            ),
            Some(Change::Added)
        );
        let changes = s.reconcile(vec![
            json!({"id": "b", "type": "light", "on": {"on": true}}),
            json!({"id": "c", "type": "light", "on": {"on": false}}),
        ]);
        assert_eq!(
            changes,
            [(Change::Added, "c".into()), (Change::Deleted, "a".into())]
        );
        assert_eq!(
            s.apply("delete", &json!({"id": "b", "type": "light"})),
            Some(Change::Deleted)
        );
        assert_eq!(s.of_type("light").count(), 1);
    }
}
