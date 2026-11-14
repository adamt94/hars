//! The `hue` integration for v2 (CLIP v2) bridges (`components/hue/v2`
//! with aiohue 4.8.1): the bridge's resources, kept current from its event
//! stream, as HA's lights and light groups.
//!
//! Where `hars` differs from HA, on purpose:
//! - It doesn't rename a geofence client on the bridge every minute to
//!   keep the stream busy: TCP keepalive notices a dead bridge instead.
//! - After every (re)connect it fetches the full state and applies the
//!   difference, removals included; aiohue does so only after a long
//!   outage and never applies removals.
//! - New resources get entity ids but no registry entries (the registries
//!   are read-only here); disabled registry entries get no state, as in HA.

mod api;
mod light;
mod store;

use crate::light::Lights;
use api::{Api, ApiError};
use ha_core::{EntityId, Hass};
use ha_registry::Registries;
use light::{GroupLight, HueLight};
use parking_lot::RwLock;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Duration;
use store::{Change, Store};

pub const DOMAIN: &str = "hue";

/// One bridge: its API, resources and entities.
pub struct Bridge {
    hass: Hass,
    registries: Arc<Registries>,
    api: Api,
    store: RwLock<Store>,
    /// Device ids whose lights are always available (`options`).
    ignore_availability: Vec<String>,
    lights: RwLock<Vec<Arc<HueLight>>>,
    groups: RwLock<Vec<Arc<GroupLight>>>,
    services: Arc<Lights>,
}

impl Bridge {
    /// `HueBaseEntity.available`: a resource without a device, or whose
    /// device has no zigbee connectivity, is always available; otherwise
    /// it's available while connected (or ignored, for lights).
    fn available(&self, store: &Store, rid: &str, has_on: bool) -> bool {
        let Some(device) = store.device_of(rid) else {
            return true;
        };
        if has_on
            && device["id"]
                .as_str()
                .is_some_and(|d| self.ignore_availability.iter().any(|i| i == d))
        {
            return true;
        }
        match store.service_of(device, "zigbee_connectivity") {
            Some(z) => z["status"] == "connected",
            None => true,
        }
    }

    fn registry_entity(&self, domain: &str, unique_id: &str) -> Option<&ha_registry::Entity> {
        self.registries.entities.iter().find(|e| {
            e.platform == DOMAIN
                && e.entity_id.split('.').next() == Some(domain)
                && e.unique_id.as_ref().and_then(Value::as_str) == Some(unique_id)
        })
    }

    /// The registered entity id for a resource (any state or none).
    fn registered_id(&self, domain: &str, unique_id: &str) -> Option<String> {
        self.registry_entity(domain, unique_id)
            .map(|e| e.entity_id.clone())
    }

    /// The registry device for a Hue resource id (`("hue", id)`).
    fn registry_device(&self, hue_id: &str) -> Option<&ha_registry::Device> {
        self.registries.devices.iter().find(|d| {
            d.identifiers
                .iter()
                .any(|i| i[0] == DOMAIN && i[1] == hue_id)
        })
    }

    /// A Hue device's (or room's) display name: the user's name for it,
    /// else the bridge's.
    fn device_name<'a>(&'a self, device: &'a Value) -> Option<&'a str> {
        let id = device["id"].as_str()?;
        self.registry_device(id)
            .and_then(|d| d.name_by_user.as_deref())
            .or_else(|| device["metadata"]["name"].as_str())
    }

    /// `friendly_name`: the registry's name if the user set one, else the
    /// device's name joined with the entity's.
    fn tail(
        &self,
        a: &mut Map<String, Value>,
        id: &EntityId,
        device: Option<&Value>,
        name: Option<&str>,
    ) {
        let user = self
            .registries
            .entities
            .iter()
            .find(|e| e.entity_id == id.as_str())
            .and_then(|e| e.name.clone());
        let friendly = user.or_else(|| {
            let device = device.and_then(|d| self.device_name(d));
            match (device, name) {
                (Some(d), Some(n)) => Some(format!("{d} {n}")),
                (Some(d), None) => Some(d.to_owned()),
                (None, n) => n.map(str::to_owned),
            }
        });
        if let Some(f) = friendly.filter(|f| !f.is_empty()) {
            a.insert("friendly_name".into(), json!(f));
        }
    }

    /// An entity's id: the registry's, else HA's rule for a new one
    /// (`slugify(area device name)`, made unique). `None` if disabled.
    fn entity_id_for(
        &self,
        domain: &str,
        unique_id: &str,
        device: Option<&Value>,
        name: Option<&str>,
    ) -> Option<EntityId> {
        if let Some(e) = self.registry_entity(domain, unique_id) {
            if e.disabled_by.is_some() {
                return None;
            }
            return EntityId::parse(&e.entity_id).ok();
        }
        let area = device
            .and_then(|d| d["id"].as_str())
            .and_then(|id| self.registry_device(id))
            .and_then(|d| d.area_id.as_deref())
            .and_then(|a| self.registries.areas.iter().find(|x| x.id == a))
            .map(|a| a.name.as_str());
        let parts: Vec<&str> = [area, device.and_then(|d| self.device_name(d)), name]
            .into_iter()
            .flatten()
            .collect();
        let base = format!("{domain}.{}", ha_components::slugify(&parts.join(" ")));
        let taken = |id: &str| {
            self.registries.entities.iter().any(|e| e.entity_id == id)
                || EntityId::parse(id).is_ok_and(|i| self.hass.states().get(&i).is_some())
        };
        let mut id = base.clone();
        let mut n = 2;
        while taken(&id) {
            id = format!("{base}_{n}");
            n += 1;
        }
        EntityId::parse(&id).ok()
    }

    /// Make entities for resources that have none yet.
    fn add_entities(self: &Arc<Self>) {
        let store = self.store.read();
        let mut new: Vec<Arc<dyn crate::light::Light>> = Vec::new();
        for r in store.of_type("light") {
            let rid = r["id"].as_str().unwrap_or_default();
            if self.lights.read().iter().any(|l| l.rid == rid) {
                continue;
            }
            let Some(id) = self.entity_id_for("light", rid, store.device_of(rid), None) else {
                continue;
            };
            let l = Arc::new(HueLight::new(Arc::downgrade(self), r, id));
            new.push(l.clone());
            self.lights.write().push(l);
        }
        for r in store.of_type("grouped_light") {
            let rid = r["id"].as_str().unwrap_or_default();
            if matches!(
                r["owner"]["rtype"].as_str(),
                Some("bridge_home" | "private_group")
            ) || self.groups.read().iter().any(|g| g.rid == rid)
            {
                continue;
            }
            let Some(group) = store.group_of(rid) else {
                continue;
            };
            let gid = group["id"].as_str().unwrap_or_default();
            let Some(id) = self.entity_id_for("light", rid, Some(group), None) else {
                continue;
            };
            let g = Arc::new(GroupLight::new(Arc::downgrade(self), rid, gid, id, &store));
            new.push(g.clone());
            self.groups.write().push(g);
        }
        drop(store);
        for l in new {
            self.services.add(l);
        }
    }

    fn write_light(&self, l: &HueLight) {
        let rendered = l.render(self, &self.store.read());
        if let Some((state, attrs)) = rendered {
            self.hass.set_state(l.entity_id.clone(), state, attrs);
        }
    }

    fn write_group(&self, g: &GroupLight) {
        let store = self.store.read();
        g.update_values(&store);
        let rendered = g.render(self, &store);
        drop(store);
        if let Some((state, attrs)) = rendered {
            self.hass.set_state(g.entity_id.clone(), state, attrs);
        }
    }

    /// Write every entity (at start).
    fn write_all(&self) {
        let lights = self.lights.read().clone();
        for l in &lights {
            self.write_light(l);
        }
        let groups = self.groups.read().clone();
        for g in &groups {
            self.write_group(g);
        }
    }

    /// The lights on a Hue device.
    fn lights_on_device(&self, device: &str) -> Vec<Arc<HueLight>> {
        let store = self.store.read();
        self.lights
            .read()
            .iter()
            .filter(|l| store.device_of(&l.rid).is_some_and(|d| d["id"] == device))
            .cloned()
            .collect()
    }

    /// One event-stream item: merge it, then write what HA writes, in its
    /// order (no coalescing: every item writes).
    fn handle(self: &Arc<Self>, kind: &str, item: &Value) {
        let ty = item["type"].as_str().unwrap_or_default().to_owned();
        let id = item
            .get("rid")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        // Who hears of a deletion (and so must be found first).
        let device_lights = if kind == "delete" && ty == "device" {
            self.lights_on_device(&id)
        } else {
            Vec::new()
        };
        let change = self.store.write().apply(kind, item);
        let Some(change) = change else {
            return;
        };
        self.after(change, &ty, &id, device_lights);
    }

    /// The writes that follow a change to resource `id` of type `ty`.
    fn after(
        self: &Arc<Self>,
        change: Change,
        ty: &str,
        id: &str,
        device_lights: Vec<Arc<HueLight>>,
    ) {
        match change {
            Change::Added => {
                if matches!(ty, "light" | "grouped_light") {
                    self.add_entities();
                    let lights = self.lights.read().clone();
                    for l in lights.iter().filter(|l| l.rid == id) {
                        self.write_light(l);
                    }
                    let groups = self.groups.read().clone();
                    for g in groups.iter().filter(|g| g.rid == id) {
                        self.write_group(g);
                    }
                }
            }
            Change::Deleted => match ty {
                "grouped_light" => {
                    let gone: Vec<_> = self
                        .groups
                        .read()
                        .iter()
                        .filter(|g| g.rid == id)
                        .cloned()
                        .collect();
                    self.groups.write().retain(|g| g.rid != id);
                    for g in gone {
                        self.hass.remove_state(&g.entity_id);
                    }
                }
                "device" => {
                    self.lights
                        .write()
                        .retain(|l| !device_lights.iter().any(|d| d.rid == l.rid));
                    for l in device_lights {
                        self.hass.remove_state(&l.entity_id);
                    }
                }
                _ => {}
            },
            Change::Updated => match ty {
                "light" => {
                    let lights = self.lights.read().clone();
                    for l in lights.iter().filter(|l| l.rid == id) {
                        self.write_light(l);
                    }
                    let groups: Vec<_> = {
                        let store = self.store.read();
                        let groups = self.groups.read();
                        groups
                            .iter()
                            .filter(|g| g.members(&store).iter().any(|m| m == id))
                            .cloned()
                            .collect()
                    };
                    for g in &groups {
                        self.write_group(g);
                    }
                }
                "grouped_light" => {
                    let groups = self.groups.read().clone();
                    for g in groups.iter().filter(|g| g.rid == id) {
                        self.write_group(g);
                    }
                }
                "room" | "zone" => {
                    let groups = self.groups.read().clone();
                    for g in groups.iter().filter(|g| g.group == id) {
                        self.write_group(g);
                    }
                }
                "device" => {
                    for l in self.lights_on_device(id) {
                        self.write_light(&l);
                    }
                }
                "zigbee_connectivity" => {
                    let owner = self
                        .store
                        .read()
                        .get(id)
                        .and_then(|z| z["owner"]["rid"].as_str().map(str::to_owned));
                    if let Some(device) = owner {
                        for l in self.lights_on_device(&device) {
                            self.write_light(&l);
                        }
                    }
                }
                _ => {}
            },
        }
    }

    /// A `data:` line: an array of events, each with its items.
    fn handle_data(self: &Arc<Self>, data: &str) {
        let Ok(Value::Array(events)) = serde_json::from_str::<Value>(data) else {
            tracing::debug!("hue: unparsable event data");
            return;
        };
        for event in &events {
            let kind = event["type"].as_str().unwrap_or_default();
            if !matches!(kind, "add" | "update" | "delete") {
                tracing::debug!("Unable to parse Event message");
                continue;
            }
            for item in event["data"].as_array().into_iter().flatten() {
                self.handle(kind, item);
            }
        }
    }

    /// Fetch everything and apply the difference (after a reconnect).
    async fn reconcile(self: &Arc<Self>) {
        match self.api.fetch_all().await {
            Ok(fresh) => {
                let changes = self.store.write().reconcile(fresh);
                for (change, id) in changes {
                    let ty = match change {
                        Change::Deleted => {
                            // Gone from the store: find it by what we hold.
                            if self.groups.read().iter().any(|g| g.rid == id) {
                                "grouped_light".to_owned()
                            } else {
                                String::new()
                            }
                        }
                        _ => self
                            .store
                            .read()
                            .get(&id)
                            .and_then(|r| r["type"].as_str().map(str::to_owned))
                            .unwrap_or_default(),
                    };
                    self.after(change, &ty, &id, Vec::new());
                }
            }
            Err(e) => tracing::warn!("hue: refreshing after reconnect failed: {e}"),
        }
    }

    /// Read the event stream forever, reconnecting as aiohue does (2 s per
    /// failed attempt, capped at 600 s) and reconciling each time.
    async fn listen(self: Arc<Self>) {
        let mut attempts: u64 = 0;
        let mut last_id: Option<String> = None;
        loop {
            attempts += 1;
            match self.api.events(last_id.as_deref()).await {
                Ok(mut stream) => {
                    attempts = 1;
                    self.reconcile().await;
                    loop {
                        match stream.next_line().await {
                            Ok(Some(line)) => match line.key.as_str() {
                                "id" => last_id = Some(line.value),
                                "data" => self.handle_data(&line.value),
                                _ => {}
                            },
                            Ok(None) => break,
                            Err(e) => {
                                tracing::debug!("hue: event stream: {e}");
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    if attempts % 10 == 0 {
                        tracing::warn!(
                            "hue: event stream unreachable after {attempts} attempts: {e}"
                        );
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs((2 * attempts).min(600))).await;
        }
    }
}

/// The parts of a `hue` config entry `hars` uses, or `None` (logged) for a
/// v1 bridge or a malformed entry.
fn entry_settings(entry: &Map<String, Value>) -> Option<(String, String, Vec<String>)> {
    let data = entry.get("data")?;
    if data["api_version"].as_i64() != Some(2) {
        tracing::warn!("hue: only v2 bridges are supported; this entry isn't started");
        return None;
    }
    let host = data["host"].as_str()?.to_owned();
    let key = data["api_key"]
        .as_str()
        .or_else(|| data["username"].as_str())?
        .to_owned();
    let ignore = entry
        .get("options")
        .and_then(|o| o["ignore_availability"].as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    Some((host, key, ignore))
}

/// Run a `hue` config entry until the task is dropped: set up (retrying as
/// HA's `ConfigEntryNotReady` does), make the entities, then follow the
/// event stream.
pub async fn run(
    hass: Hass,
    registries: Arc<Registries>,
    services: Arc<Lights>,
    entry: Map<String, Value>,
) {
    let Some((host, key, ignore)) = entry_settings(&entry) else {
        return;
    };
    let api = match Api::new(&host, &key) {
        Ok(api) => api,
        Err(e) => {
            tracing::error!("hue: {e}");
            return;
        }
    };
    let mut tries = 0u32;
    let resources = loop {
        let err = match tokio::time::timeout(Duration::from_secs(10), api.fetch_all()).await {
            Ok(Ok(r)) => break r,
            Ok(Err(e)) => e,
            Err(_) => ApiError::Connection("timed out".into()),
        };
        match err {
            ApiError::Unauthorized => {
                tracing::error!(
                    "hue: the bridge at {host} refused the app key; it needs pairing again"
                );
                return;
            }
            ApiError::Connection(_) | ApiError::Busy => {
                let wait = (5u64 << tries.min(7)).min(600);
                tracing::warn!(
                    "Error connecting to the Hue bridge at {host}; retrying in {wait} s"
                );
                tries += 1;
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
            other => {
                tracing::error!("Unknown error connecting to Hue bridge: {other}");
                return;
            }
        }
    };
    let bridge = Arc::new(Bridge {
        hass,
        registries,
        api,
        store: RwLock::new(Store::new(resources)),
        ignore_availability: ignore,
        lights: RwLock::default(),
        groups: RwLock::default(),
        services,
    });
    bridge.add_entities();
    bridge.write_all();
    bridge.listen().await;
}

#[cfg(test)]
mod tests;
