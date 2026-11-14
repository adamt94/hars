//! Home Assistant's built-in `zone` and `person` components, from HA's
//! `.storage/zone` (1.1) and `.storage/person` (2.1).
//!
//! What's here is their state before any device tracker reports: zones
//! (`zone.home` from the location settings, plus stored zones) with nobody
//! in them, and people whose state is `unknown`. Tracking comes with the
//! companion app.

use ha_core::storage::{Store, StoreError};
use ha_core::{Attributes, EntityId, Hass};
use ha_registry::Registries;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::path::Path;

pub const ZONE_KEY: &str = "zone";
pub const PERSON_KEY: &str = "person";

#[derive(Debug, Default, Deserialize)]
struct Collection {
    #[serde(default)]
    items: Vec<Map<String, Value>>,
}

/// The stored zones and people.
#[derive(Debug, Default)]
pub struct Components {
    pub zones: Vec<Map<String, Value>>,
    pub persons: Vec<Map<String, Value>>,
}

impl Components {
    /// Read the stores for the server (blocking): one that can't be read is
    /// logged and skipped, so it can't stop the server.
    pub fn load(config_dir: &Path) -> Self {
        let read = |store: Store| match store.load::<Collection>() {
            Ok(c) => c.unwrap_or_default().items,
            Err(e) => {
                tracing::error!("{e}; starting without it");
                Vec::new()
            }
        };
        Self {
            zones: read(zone_store(config_dir)),
            persons: read(person_store(config_dir)),
        }
    }

    /// Any problem is an error: for the importer.
    pub fn load_strict(config_dir: &Path) -> Result<Self, StoreError> {
        Ok(Self {
            zones: zone_store(config_dir)
                .load::<Collection>()?
                .unwrap_or_default()
                .items,
            persons: person_store(config_dir)
                .load::<Collection>()?
                .unwrap_or_default()
                .items,
        })
    }

    /// Whether a `.storage` key is one of these stores.
    pub fn is_store(key: &str) -> bool {
        key == ZONE_KEY || key == PERSON_KEY
    }

    /// Create the zone and person entities, as HA does at start-up.
    pub fn setup(&self, hass: &Hass, registries: &Registries) {
        let mut taken: HashSet<String> = hass
            .states()
            .all()
            .iter()
            .map(|s| s.entity_id.to_string())
            .collect();
        let config = hass.config();
        let zone_attrs =
            |lat: &Value, lon: &Value, radius: &Value, passive: &Value, editable: bool| {
                json!({
                    "latitude": lat, "longitude": lon, "radius": radius, "passive": passive,
                    "persons": [], "editable": editable,
                })
            };

        // `zone.home`, from the location settings.
        let home = Entity {
            entity_id: "zone.home".into(),
            name: config.location_name.clone(),
            icon: Some("mdi:home".into()),
            picture: None,
            attributes: zone_attrs(
                &json!(config.latitude),
                &json!(config.longitude),
                &json!(config.radius),
                &json!(false),
                true,
            ),
            state: "0".into(),
        };
        home.write(hass, None);
        taken.insert(home.entity_id);

        for zone in &self.zones {
            let (Some(id), Some(name)) = (str_of(zone, "id"), str_of(zone, "name")) else {
                continue;
            };
            let entry = registry_entry(registries, "zone", id);
            if entry.is_some_and(|e| e.disabled_by.is_some()) {
                continue;
            }
            let get = |k: &str| zone.get(k).cloned().unwrap_or(Value::Null);
            let entity = Entity {
                entity_id: entity_id("zone", name, entry, &mut taken),
                name: name.to_owned(),
                icon: str_of(zone, "icon").map(str::to_owned),
                picture: None,
                attributes: zone_attrs(
                    &get("latitude"),
                    &get("longitude"),
                    &zone.get("radius").cloned().unwrap_or(json!(100)),
                    &zone.get("passive").cloned().unwrap_or(json!(false)),
                    true,
                ),
                state: "0".into(),
            };
            entity.write(hass, entry);
        }

        for person in &self.persons {
            let (Some(id), Some(name)) = (str_of(person, "id"), str_of(person, "name")) else {
                continue;
            };
            let entry = registry_entry(registries, "person", id);
            if entry.is_some_and(|e| e.disabled_by.is_some()) {
                continue;
            }
            let mut attributes = json!({
                "editable": true,
                "id": id,
                "device_trackers": person.get("device_trackers").cloned().unwrap_or(json!([])),
                "in_zones": [],
            });
            if let Some(user) = str_of(person, "user_id") {
                attributes["user_id"] = json!(user);
            }
            let entity = Entity {
                entity_id: entity_id("person", name, entry, &mut taken),
                name: name.to_owned(),
                icon: None,
                picture: str_of(person, "picture").map(str::to_owned),
                attributes,
                state: "unknown".into(),
            };
            entity.write(hass, entry);
        }
    }

    /// `zone/list`.
    pub fn zones_json(&self) -> Value {
        Value::Array(self.zones.iter().cloned().map(Value::Object).collect())
    }

    /// `person/list`: stored people, and (none yet) YAML ones.
    pub fn persons_json(&self) -> Value {
        json!({
            "storage": self.persons.iter().cloned().map(Value::Object).collect::<Vec<_>>(),
            "config": [],
        })
    }
}

struct Entity {
    entity_id: String,
    name: String,
    icon: Option<String>,
    picture: Option<String>,
    attributes: Value,
    state: String,
}

impl Entity {
    /// Write the state with HA's standard attributes; the entity registry's
    /// name and icon override the entity's own.
    fn write(&self, hass: &Hass, entry: Option<&ha_registry::Entity>) {
        let Value::Object(mut attrs) = self.attributes.clone() else {
            return;
        };
        let attrs_ref: &mut Attributes = &mut attrs;
        if let Some(picture) = &self.picture {
            attrs_ref.insert("entity_picture".into(), json!(picture));
        }
        let icon = entry
            .and_then(|e| e.icon.clone())
            .or_else(|| self.icon.clone());
        if let Some(icon) = icon {
            attrs_ref.insert("icon".into(), json!(icon));
        }
        let name = entry
            .and_then(|e| e.name.clone())
            .unwrap_or_else(|| self.name.clone());
        attrs_ref.insert("friendly_name".into(), json!(name));
        match EntityId::parse(&self.entity_id) {
            Ok(id) => {
                hass.set_state(id, self.state.as_str(), attrs);
            }
            Err(e) => tracing::error!("not creating {}: {e}", self.entity_id),
        }
    }
}

fn str_of<'a>(item: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    item.get(key).and_then(Value::as_str)
}

fn registry_entry<'a>(
    r: &'a Registries,
    platform: &str,
    unique_id: &str,
) -> Option<&'a ha_registry::Entity> {
    r.entities.iter().find(|e| {
        e.platform == platform && e.unique_id.as_ref().and_then(Value::as_str) == Some(unique_id)
    })
}

/// The registry's entity id if there is an entry, else `<domain>.<slug>`
/// with HA's `_2`, `_3`, ... for duplicates.
fn entity_id(
    domain: &str,
    name: &str,
    entry: Option<&ha_registry::Entity>,
    taken: &mut HashSet<String>,
) -> String {
    if let Some(e) = entry {
        taken.insert(e.entity_id.clone());
        return e.entity_id.clone();
    }
    let base = format!("{domain}.{}", slugify(name));
    let mut id = base.clone();
    let mut n = 2;
    while taken.contains(&id) {
        id = format!("{base}_{n}");
        n += 1;
    }
    taken.insert(id.clone());
    id
}

/// HA's `slugify`: lowercase ASCII, other runs become `_`, trimmed;
/// common Latin accents are transliterated, and nothing left is `unknown`.
pub fn slugify(text: &str) -> String {
    let mut out = String::new();
    let mut sep = false;
    for c in text.chars().flat_map(char::to_lowercase) {
        let ascii: &str = match c {
            'a'..='z' | '0'..='9' => {
                if sep && !out.is_empty() {
                    out.push('_');
                }
                sep = false;
                out.push(c);
                continue;
            }
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => "a",
            'æ' => "ae",
            'ç' | 'ć' | 'č' => "c",
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ę' | 'ě' => "e",
            'ì' | 'í' | 'î' | 'ï' => "i",
            'ñ' | 'ń' | 'ň' => "n",
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => "o",
            'œ' => "oe",
            'ß' => "ss",
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' => "u",
            'ý' | 'ÿ' => "y",
            'ž' | 'ź' | 'ż' => "z",
            'š' | 'ś' => "s",
            'ł' => "l",
            'ř' => "r",
            _ => {
                sep = true;
                continue;
            }
        };
        if sep && !out.is_empty() {
            out.push('_');
        }
        sep = false;
        out.push_str(ascii);
    }
    if out.is_empty() {
        "unknown".into()
    } else {
        out
    }
}

fn zone_store(dir: &Path) -> Store {
    Store::new(dir, ZONE_KEY, 1, 1, false)
}

fn person_store(dir: &Path) -> Store {
    Store::new(dir, PERSON_KEY, 2, 1, false)
}

#[cfg(test)]
mod tests;
