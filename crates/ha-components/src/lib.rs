//! Home Assistant's built-in `zone` and `person` components, from HA's
//! `.storage/zone` (1.1) and `.storage/person` (2.1).
//!
//! Zones (`zone.home` from the location settings, plus stored zones) count
//! the people in them; a person follows their device trackers from
//! `homeassistant_start` on, and restores their last state before that.

use ha_core::storage::{Store, StoreError};
use ha_core::{Attributes, EntityId, Hass};
use ha_registry::Registries;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

pub mod zone;

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
    /// The person store's items (changed at runtime when an app registers).
    persons: parking_lot::RwLock<Vec<Map<String, Value>>>,
    /// The entities [`Components::setup`] made, for tracking.
    entities: parking_lot::Mutex<Tracked>,
    /// `.storage/person`'s delayed save, once [`Components::save_to`] ran.
    person_save: std::sync::OnceLock<Arc<ha_core::storage::DelayedSave>>,
}

/// `StorageCollection`'s save delay.
const PERSON_SAVE_DELAY: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, Default)]
struct Tracked {
    persons: Vec<Arc<Person>>,
    zones: Vec<Zone>,
    /// Set at `homeassistant_start`: people follow their trackers from then.
    started: bool,
    /// Every person's trackers, to skip other state changes cheaply.
    trackers: HashSet<String>,
}

impl Tracked {
    /// Recompute a person, then the zones: HA's zones update inside the
    /// person's `state_changed`, so both happen under one lock, in order,
    /// before anything else reads them.
    fn update(&mut self, hass: &Hass, person: usize) {
        let state = self.persons[person].update(hass);
        let entity_id = &self.persons[person].entity_id;
        for zone in &mut self.zones {
            let before = zone.persons.len();
            if zone.holds(Some(&state)) {
                zone.persons.insert(entity_id.to_string());
            } else {
                zone.persons.remove(entity_id.as_str());
            }
            if zone.persons.len() != before {
                zone.write(hass);
            }
        }
    }
}

/// A person entity: what its state is made from.
#[derive(Debug)]
struct Person {
    entity_id: EntityId,
    /// The storage id (`attributes.id`).
    id: String,
    device_trackers: Vec<String>,
    user_id: Option<String>,
    /// `friendly_name`, `icon` and `entity_picture`.
    base: Attributes,
}

/// A zone entity and who is in it.
#[derive(Debug)]
struct Zone {
    entity_id: EntityId,
    /// The configured name, case-folded, which a person's state matches.
    name: String,
    /// Everything but `persons`.
    attributes: Attributes,
    persons: std::collections::BTreeSet<String>,
}

impl Zone {
    /// `_state_is_in_zone`.
    fn holds(&self, state: Option<&ha_core::State>) -> bool {
        let Some(s) = state else {
            return false;
        };
        let s = s.state.as_str();
        !matches!(s, "not_home" | "unknown" | "unavailable")
            && (ha_core::casefold::casefold(s) == self.name
                || (s == "home" && self.entity_id.as_str() == "zone.home"))
    }

    fn write(&self, hass: &Hass) {
        let mut attrs = self.attributes.clone();
        attrs.insert("persons".into(), json!(self.persons));
        hass.set_state(
            self.entity_id.clone(),
            self.persons.len().to_string(),
            attrs,
        );
    }
}

impl Person {
    /// `_update_extra_state_attributes` and the write, from a source state
    /// (or none: `unknown`).
    fn write(
        &self,
        hass: &Hass,
        source: Option<(&ha_core::State, &ha_core::State)>,
    ) -> Arc<ha_core::State> {
        let mut a = self.base.clone();
        a.insert("editable".into(), json!(true));
        a.insert("id".into(), json!(self.id));
        a.insert("device_trackers".into(), json!(self.device_trackers));
        let mut state = "unknown".to_owned();
        a.insert("in_zones".into(), json!([]));
        if let Some((latest, coordinates)) = source {
            state = latest.state.to_string();
            let in_zones = latest
                .attributes
                .get("in_zones")
                .cloned()
                .unwrap_or(json!([]));
            a.insert("in_zones".into(), in_zones);
            for k in ["latitude", "longitude", "gps_accuracy"] {
                if let Some(v) = coordinates.attributes.get(k).filter(|v| !v.is_null()) {
                    a.insert(k.into(), v.clone());
                }
            }
            a.insert("source".into(), json!(latest.entity_id));
        }
        if let Some(user) = &self.user_id {
            a.insert("user_id".into(), json!(user));
        }
        hass.set_state(self.entity_id.clone(), state, a)
    }

    /// `_update_state`: the latest GPS tracker, unless a non-GPS tracker
    /// says `home`; trackers in `unknown`/`unavailable` don't count.
    fn update(&self, hass: &Hass) -> Arc<ha_core::State> {
        let latest = |prev: Option<Arc<ha_core::State>>, cur: Arc<ha_core::State>| match prev {
            Some(p) if cur.last_updated <= p.last_updated => Some(p),
            _ => Some(cur),
        };
        let (mut gps, mut non_gps_home, mut not_home) = (None, None, None);
        for id in &self.device_trackers {
            let Some(state) = EntityId::parse(id)
                .ok()
                .and_then(|id| hass.states().get(&id))
            else {
                continue;
            };
            if matches!(state.state.as_str(), "unknown" | "unavailable") {
                continue;
            }
            if state.attributes.get("source_type").and_then(Value::as_str) == Some("gps") {
                gps = latest(gps, state);
            } else if state.state == "home" {
                non_gps_home = latest(non_gps_home, state);
            } else {
                not_home = latest(not_home, state);
            }
        }
        let home_zone = || hass.states().get(&EntityId::parse("zone.home").ok()?);
        let chosen = match non_gps_home {
            Some(home) => {
                let no_coordinates = home.attributes.get("latitude").is_none_or(Value::is_null)
                    && home.attributes.get("longitude").is_none_or(Value::is_null);
                let coordinates = match home_zone().filter(|_| no_coordinates) {
                    Some(zone) => zone,
                    None => home.clone(),
                };
                Some((home, coordinates))
            }
            None => gps.or(not_home).map(|s| (s.clone(), s)),
        };
        self.write(hass, chosen.as_ref().map(|(l, c)| (&**l, &**c)))
    }
}

impl Components {
    /// Read the stores for the server (blocking): one that can't be read is
    /// logged and skipped, so it can't stop the server.
    pub fn load(config_dir: &Path) -> Self {
        let zones = zone_store(config_dir)
            .load::<Collection>()
            .unwrap_or_else(|e| {
                tracing::error!("{e}; starting without zones");
                None
            });
        let persons = load_persons(config_dir).unwrap_or_else(|e| {
            tracing::error!("{e}; starting without people");
            Vec::new()
        });
        Self {
            zones: zones.unwrap_or_default().items,
            persons: parking_lot::RwLock::new(persons),
            ..Default::default()
        }
    }

    /// Any problem is an error: for the importer.
    pub fn load_strict(config_dir: &Path) -> Result<Self, StoreError> {
        Ok(Self {
            zones: zone_store(config_dir)
                .load::<Collection>()?
                .unwrap_or_default()
                .items,
            persons: parking_lot::RwLock::new(load_persons(config_dir)?),
            ..Default::default()
        })
    }

    /// Whether a `.storage` key is one of these stores.
    pub fn is_store(key: &str) -> bool {
        key == ZONE_KEY || key == PERSON_KEY
    }

    /// Create the zone and person entities, as HA does at start-up. People
    /// restore their last state (`RestoreEntity`); zones count them.
    pub fn setup(&self, hass: &Hass, registries: &Registries) {
        let mut taken: HashSet<String> = hass
            .states()
            .all()
            .iter()
            .map(|s| s.entity_id.to_string())
            .collect();
        // New ids avoid existing states and every id the registry has given
        // out, as HA's `async_generate_entity_id` does.
        taken.extend(registries.entities.iter().map(|e| e.entity_id.clone()));
        let config = hass.config();
        let zone_attrs = |lat: &Value, lon: &Value, radius: &Value, passive: &Value| {
            json!({
                "latitude": lat, "longitude": lon, "radius": radius, "passive": passive,
                "editable": true,
            })
        };

        // `zone.home`, from the location settings.
        let mut zones = vec![(
            "zone.home".to_owned(),
            config.location_name.clone(),
            standard_attributes(
                zone_attrs(
                    &json!(config.latitude),
                    &json!(config.longitude),
                    &json!(config.radius),
                    &json!(false),
                ),
                &config.location_name,
                Some("mdi:home"),
                None,
                None,
            ),
        )];
        taken.insert("zone.home".into());
        for zone in &self.zones {
            let (Some(id), Some(name)) = (str_of(zone, "id"), str_of(zone, "name")) else {
                continue;
            };
            let entry = registry_entry(registries, "zone", id);
            if entry.is_some_and(|e| e.disabled_by.is_some()) {
                continue;
            }
            let get = |k: &str| zone.get(k).cloned().unwrap_or(Value::Null);
            let attrs = standard_attributes(
                zone_attrs(
                    &get("latitude"),
                    &get("longitude"),
                    &zone.get("radius").cloned().unwrap_or(json!(100)),
                    &zone.get("passive").cloned().unwrap_or(json!(false)),
                ),
                name,
                str_of(zone, "icon"),
                None,
                entry,
            );
            zones.push((
                entity_id("zone", name, entry, &mut taken),
                name.to_owned(),
                attrs,
            ));
        }

        let restore = hass.restore();
        let mut persons = Vec::new();
        for person in self.persons.read().iter() {
            let (Some(id), Some(name)) = (str_of(person, "id"), str_of(person, "name")) else {
                continue;
            };
            let entry = registry_entry(registries, "person", id);
            if entry.is_some_and(|e| e.disabled_by.is_some()) {
                continue;
            }
            let entity = entity_id("person", name, entry, &mut taken);
            let Ok(entity_id) = EntityId::parse(&entity) else {
                tracing::error!("not creating {entity}: not a valid entity id");
                continue;
            };
            let device_trackers = person
                .get("device_trackers")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let p = Person {
                entity_id,
                id: id.to_owned(),
                device_trackers,
                user_id: str_of(person, "user_id").map(str::to_owned),
                base: standard_attributes(json!({}), name, None, str_of(person, "picture"), entry),
            };
            // `_parse_source_state(last, last)`: the restored person is its
            // own source until the trackers are read at start.
            restore.register(p.entity_id.as_str(), None);
            let last = restore
                .last_state(p.entity_id.as_str())
                .and_then(|v| serde_json::from_value::<RestoredState>(v).ok());
            match last {
                Some(last) => p.write_restored(hass, &last),
                None => {
                    p.write(hass, None);
                }
            }
            persons.push(Arc::new(p));
        }

        let mut tracked = Vec::new();
        for (entity, name, attributes) in zones {
            let Ok(entity_id) = EntityId::parse(&entity) else {
                tracing::error!("not creating {entity}: not a valid entity id");
                continue;
            };
            let mut zone = Zone {
                entity_id,
                name: ha_core::casefold::casefold(&name),
                attributes,
                persons: Default::default(),
            };
            for p in &persons {
                if zone.holds(hass.states().get(&p.entity_id).as_deref()) {
                    zone.persons.insert(p.entity_id.to_string());
                }
            }
            zone.write(hass);
            tracked.push(zone);
        }
        let mut t = self.entities.lock();
        t.trackers = persons
            .iter()
            .flat_map(|p| p.device_trackers.iter().cloned())
            .collect();
        t.persons = persons;
        t.zones = tracked;
    }

    /// The person store's items.
    pub fn persons(&self) -> Vec<Map<String, Value>> {
        self.persons.read().clone()
    }

    /// Save changes to the person store to `config_dir` (10 s after the
    /// last, as HA's collections).
    pub fn save_to(self: &Arc<Self>, config_dir: &Path) {
        let (weak, dir) = (Arc::downgrade(self), config_dir.to_owned());
        let save = ha_core::storage::DelayedSave::spawn(PERSON_KEY, move || {
            let Some(this) = weak.upgrade() else {
                return Ok(());
            };
            let items = this.persons();
            person_store(&dir).save(&json!({ "items": items }))
        });
        let _ = self.person_save.set(save);
    }

    /// `person.async_add_user_device_tracker`: the tracker joins the
    /// person linked to `user_id` (if not already theirs), who follows it
    /// from now on; the store is saved.
    pub fn add_user_device_tracker(&self, hass: &Hass, user_id: &str, tracker: &str) {
        let person_id = {
            let mut persons = self.persons.write();
            let Some(p) = persons
                .iter_mut()
                .find(|p| p.get("user_id").and_then(Value::as_str) == Some(user_id))
            else {
                return;
            };
            let trackers = p.entry("device_trackers").or_insert_with(|| json!([]));
            let Some(list) = trackers.as_array_mut() else {
                return;
            };
            if list.iter().any(|t| t == tracker) {
                return;
            }
            list.push(json!(tracker));
            p.get("id").and_then(Value::as_str).map(str::to_owned)
        };
        if let Some(save) = self.person_save.get() {
            save.schedule(PERSON_SAVE_DELAY);
        }
        let Some(person_id) = person_id else {
            return;
        };
        // `async_update_config`: the entity follows the new list now.
        let mut t = self.entities.lock();
        let Some(i) = t.persons.iter().position(|p| p.id == person_id) else {
            return;
        };
        let mut p = Person {
            entity_id: t.persons[i].entity_id.clone(),
            id: t.persons[i].id.clone(),
            device_trackers: t.persons[i].device_trackers.clone(),
            user_id: t.persons[i].user_id.clone(),
            base: t.persons[i].base.clone(),
        };
        p.device_trackers.push(tracker.to_owned());
        t.persons[i] = Arc::new(p);
        t.trackers.insert(tracker.to_owned());
        if t.started {
            t.update(hass, i);
        }
    }

    /// `homeassistant_start`: people read their trackers, and follow them
    /// from now on.
    pub fn start(&self, hass: &Hass) {
        // One lock for the whole pass: the task can't interleave a newer
        // tracker reading with it.
        let mut t = self.entities.lock();
        t.started = true;
        for i in 0..t.persons.len() {
            t.update(hass, i);
        }
    }

    /// Follow tracker changes (one task; it sleeps until a state changes).
    pub fn track(self: &Arc<Self>, hass: &Hass) -> tokio::task::JoinHandle<()> {
        let mut feed = hass.subscribe_state_changes();
        feed.snapshot.clear();
        let (this, hass) = (self.clone(), hass.clone());
        tokio::spawn(async move {
            while let Some(event) = feed.recv().await {
                let ha_core::EventKind::StateChanged { entity_id, .. } = &event.kind else {
                    continue;
                };
                this.changed(&hass, entity_id);
            }
        })
    }

    fn changed(&self, hass: &Hass, entity_id: &EntityId) {
        let mut t = self.entities.lock();
        if !t.started || !t.trackers.contains(entity_id.as_str()) {
            return;
        }
        for i in 0..t.persons.len() {
            if t.persons[i]
                .device_trackers
                .iter()
                .any(|d| d == entity_id.as_str())
            {
                t.update(hass, i);
            }
        }
    }

    /// `zone/list`.
    pub fn zones_json(&self) -> Value {
        Value::Array(self.zones.iter().cloned().map(Value::Object).collect())
    }

    /// `person/list`: stored people, and (none yet) YAML ones.
    pub fn persons_json(&self) -> Value {
        json!({
            "storage": self.persons.read().iter().cloned().map(Value::Object).collect::<Vec<_>>(),
            "config": [],
        })
    }
}

/// A last state, as the restore store keeps it.
#[derive(Debug, Deserialize)]
struct RestoredState {
    state: String,
    #[serde(default)]
    attributes: Attributes,
}

impl Person {
    fn write_restored(&self, hass: &Hass, last: &RestoredState) {
        let state = ha_core::State {
            entity_id: self.entity_id.clone(),
            state: last.state.as_str().into(),
            attributes: last.attributes.clone(),
            last_changed: hass.clock().now(),
            last_reported: ha_core::Reported::new(hass.clock().now()),
            last_updated: hass.clock().now(),
            context: ha_core::Context::at(hass.clock().now()),
        };
        self.write(hass, Some((&state, &state)));
    }
}

/// An entity's attributes plus HA's standard ones: `entity_picture`,
/// `icon` and `friendly_name`, the registry's name and icon overriding the
/// entity's own.
fn standard_attributes(
    base: Value,
    name: &str,
    icon: Option<&str>,
    picture: Option<&str>,
    entry: Option<&ha_registry::Entity>,
) -> Attributes {
    let Value::Object(mut attrs) = base else {
        return Attributes::new();
    };
    if let Some(picture) = picture {
        attrs.insert("entity_picture".into(), json!(picture));
    }
    let icon = entry
        .and_then(|e| e.icon.clone())
        .or_else(|| icon.map(str::to_owned));
    if let Some(icon) = icon {
        attrs.insert("icon".into(), json!(icon));
    }
    let name = entry
        .and_then(|e| e.name.clone())
        .unwrap_or_else(|| name.to_owned());
    attrs.insert("friendly_name".into(), json!(name));
    attrs
}

fn str_of<'a>(item: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    item.get(key).and_then(Value::as_str)
}

fn registry_entry<'a>(
    r: &'a Registries,
    platform: &str,
    unique_id: &str,
) -> Option<&'a ha_registry::Entity> {
    r.entities
        .iter()
        .find(|e| {
            e.platform == platform
                && e.unique_id.as_ref().and_then(Value::as_str) == Some(unique_id)
        })
        .map(|e| &**e)
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

pub use ha_core::slugify;

fn zone_store(dir: &Path) -> Store {
    Store::new(dir, ZONE_KEY, 1, 1, false)
}

fn person_store(dir: &Path) -> Store {
    Store::new(dir, PERSON_KEY, 2, 1, false)
}

/// `.storage/person`: version 2 (`items`), or version 1 (`persons`), which
/// HA migrates to `items`. Then HA's fix for an old onboarding bug: a person
/// with `device_trackers: null` gets `[]`.
fn load_persons(dir: &Path) -> Result<Vec<Map<String, Value>>, StoreError> {
    #[derive(Deserialize)]
    struct V1 {
        #[serde(default)]
        persons: Vec<Map<String, Value>>,
    }
    let mut persons = match person_store(dir).load::<Collection>() {
        Ok(c) => c.unwrap_or_default().items,
        Err(StoreError::TooOld { found: 1, .. }) => Store::new(dir, PERSON_KEY, 1, 1, false)
            .load::<V1>()?
            .map(|v| v.persons)
            .unwrap_or_default(),
        Err(e) => return Err(e),
    };
    for p in &mut persons {
        if p.get("device_trackers").is_none_or(Value::is_null) {
            p.insert("device_trackers".into(), json!([]));
        }
    }
    Ok(persons)
}

#[cfg(test)]
mod tests;
