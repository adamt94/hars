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
mod entities;
mod light;
mod store;

use crate::light::Lights;
use crate::scene::Scenes;
use crate::switch::Switches;
use api::{Api, ApiError};
use entities::{HueEntity, Kind};
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
    /// Sensors, binary sensors, switches and scenes, in HA's platform order.
    others: RwLock<Vec<Arc<HueEntity>>>,
    platforms: Arc<Platforms>,
}

/// What a deleted device takes with it.
#[derive(Default, Clone)]
struct Gone {
    lights: Vec<Arc<HueLight>>,
    others: Vec<Arc<HueEntity>>,
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
        // Ids in the registry, in the state machine, or given to an entity
        // not yet written: each new entity gets its own.
        let taken = |id: &str| {
            self.registries.entities.iter().any(|e| e.entity_id == id)
                || EntityId::parse(id).is_ok_and(|i| self.hass.states().get(&i).is_some())
                || self
                    .lights
                    .read()
                    .iter()
                    .any(|l| l.entity_id.as_str() == id)
                || self
                    .groups
                    .read()
                    .iter()
                    .any(|g| g.entity_id.as_str() == id)
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
        let mut made: Vec<Arc<HueEntity>> = Vec::new();
        for r in store.order() {
            let (Some(rid), Some(ty)) = (r["id"].as_str(), r["type"].as_str()) else {
                continue;
            };
            for &kind in Kind::for_type(ty) {
                if !kind.wanted(r, &store)
                    || self
                        .others
                        .read()
                        .iter()
                        .any(|e| e.rid == rid && e.kind == kind)
                {
                    continue;
                }
                let device = HueEntity::device(kind, rid, r, &store);
                let Some(id) =
                    self.entity_id_for(kind.domain(), rid, device, kind.name(r).as_deref())
                else {
                    continue;
                };
                let e = Arc::new(HueEntity::new(Arc::downgrade(self), kind, rid, id));
                made.push(e.clone());
                let mut others = self.others.write();
                others.push(e);
                others.sort_by_key(|e| e.kind.rank());
            }
        }
        drop(store);
        for l in new {
            self.platforms.lights.add(l);
        }
        for e in made {
            match e.kind.domain() {
                "switch" => self.platforms.switches.add(e),
                "scene" => self.platforms.scenes.add(e),
                _ => {}
            }
        }
    }

    fn write_entity(&self, e: &HueEntity) {
        let rendered = e.render(self, &self.store.read());
        if let Some((state, attrs)) = rendered {
            self.hass.set_state(e.entity_id.clone(), state, attrs);
        }
    }

    /// The other entities of a resource, in platform order.
    fn others_of(&self, rid: &str) -> Vec<Arc<HueEntity>> {
        self.others
            .read()
            .iter()
            .filter(|e| e.rid == rid)
            .cloned()
            .collect()
    }

    /// Everything on a Hue device.
    fn on_device(&self, device: &str) -> Gone {
        let store = self.store.read();
        let on = |rid: &str| store.device_of(rid).is_some_and(|d| d["id"] == device);
        Gone {
            lights: self
                .lights
                .read()
                .iter()
                .filter(|l| on(&l.rid))
                .cloned()
                .collect(),
            others: self
                .others
                .read()
                .iter()
                .filter(|e| on(&e.rid))
                .cloned()
                .collect(),
        }
    }

    /// Write a device's entities in HA's order: binary sensors, lights,
    /// then scenes, sensors and switches.
    fn write_device(&self, device: &str) {
        let Gone { lights, others } = self.on_device(device);
        for e in others.iter().filter(|e| e.kind.rank() < 2) {
            self.write_entity(e);
        }
        for l in &lights {
            self.write_light(l);
        }
        for e in others.iter().filter(|e| e.kind.rank() > 2) {
            self.write_entity(e);
        }
    }

    /// Drop entities: their states, and their place as service targets.
    fn remove(&self, gone: &Gone) {
        self.lights
            .write()
            .retain(|l| !gone.lights.iter().any(|g| g.rid == l.rid));
        self.others.write().retain(|e| {
            !gone
                .others
                .iter()
                .any(|g| g.rid == e.rid && g.kind == e.kind)
        });
        for l in &gone.lights {
            self.platforms.lights.remove(&l.entity_id);
            self.hass.remove_state(&l.entity_id);
        }
        for e in &gone.others {
            self.platforms.switches.remove(&e.entity_id);
            self.platforms.scenes.remove(&e.entity_id);
            self.hass.remove_state(&e.entity_id);
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

    /// Write every entity (at start), in HA's platform order.
    fn write_all(&self) {
        let others = self.others.read().clone();
        for e in others.iter().filter(|e| e.kind.rank() < 2) {
            self.write_entity(e);
        }
        let lights = self.lights.read().clone();
        for l in &lights {
            self.write_light(l);
        }
        let groups = self.groups.read().clone();
        for g in &groups {
            self.write_group(g);
        }
        for e in others.iter().filter(|e| e.kind.rank() > 2) {
            self.write_entity(e);
        }
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
        // What a deleted device takes with it, found while it's known.
        let gone = if kind == "delete" && ty == "device" {
            self.on_device(&id)
        } else {
            Gone::default()
        };
        let change = self.store.write().apply(kind, item);
        let Some(change) = change else {
            return;
        };
        self.after(change, &ty, &id, gone);
    }

    /// The writes that follow a change to resource `id` of type `ty`.
    fn after(self: &Arc<Self>, change: Change, ty: &str, id: &str, gone: Gone) {
        match change {
            Change::Added => {
                // A room or zone can arrive after its `grouped_light` (the
                // race HA's `group.py` retries for): it makes that group.
                self.add_entities();
                for e in self.others_of(id).iter().filter(|e| e.kind.rank() < 2) {
                    self.write_entity(e);
                }
                let lights = self.lights.read().clone();
                for l in lights.iter().filter(|l| l.rid == id) {
                    self.write_light(l);
                }
                let groups = self.groups.read().clone();
                for g in groups.iter().filter(|g| g.rid == id || g.group == id) {
                    self.write_group(g);
                }
                for e in self.others_of(id).iter().filter(|e| e.kind.rank() > 2) {
                    self.write_entity(e);
                }
            }
            Change::Deleted => {
                let mut gone = gone;
                // Device-less entities go with their resource.
                gone.others
                    .extend(self.others_of(id).into_iter().filter(|e| {
                        matches!(
                            e.kind,
                            Kind::Scene
                                | Kind::SmartScene
                                | Kind::GroupedMotion
                                | Kind::GroupedLightLevel
                                | Kind::Behavior
                                | Kind::Entertainment
                        )
                    }));
                if ty == "grouped_light" {
                    let groups: Vec<_> = self
                        .groups
                        .read()
                        .iter()
                        .filter(|g| g.rid == id)
                        .cloned()
                        .collect();
                    self.groups.write().retain(|g| g.rid != id);
                    for g in groups {
                        self.platforms.lights.remove(&g.entity_id);
                        self.hass.remove_state(&g.entity_id);
                    }
                }
                self.remove(&gone);
            }
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
                    // Scenes show their group's name.
                    let scenes: Vec<_> = {
                        let store = self.store.read();
                        self.others
                            .read()
                            .iter()
                            .filter(|e| matches!(e.kind, Kind::Scene | Kind::SmartScene))
                            .filter(|e| store.get(&e.rid).is_some_and(|r| r["group"]["rid"] == id))
                            .cloned()
                            .collect()
                    };
                    for e in &scenes {
                        self.write_entity(e);
                    }
                }
                "device" => self.write_device(id),
                "zigbee_connectivity" => {
                    let owner = self
                        .store
                        .read()
                        .get(id)
                        .and_then(|z| z["owner"]["rid"].as_str().map(str::to_owned));
                    // Its own sensor, then everything on its device.
                    for e in self.others_of(id) {
                        self.write_entity(&e);
                    }
                    if let Some(device) = owner {
                        self.write_device(&device);
                    }
                }
                _ => {
                    for e in self.others_of(id) {
                        self.write_entity(&e);
                    }
                }
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
            Ok(fresh) => self.apply_snapshot(fresh),
            Err(e) => tracing::warn!("hue: refreshing after reconnect failed: {e}"),
        }
    }

    /// Apply a fresh snapshot: additions, changes and removals, each with
    /// the writes an event for it would make.
    fn apply_snapshot(self: &Arc<Self>, fresh: Vec<Value>) {
        // What may be removed, known before the store forgets it: each
        // resource's type, and what each device takes with it.
        let (types, devices): (Vec<(String, String)>, Vec<String>) = {
            let store = self.store.read();
            let types = store
                .order()
                .filter_map(|r| {
                    Some((r["id"].as_str()?.to_owned(), r["type"].as_str()?.to_owned()))
                })
                .collect();
            let devices = store
                .of_type("device")
                .filter_map(|d| d["id"].as_str().map(str::to_owned))
                .collect();
            (types, devices)
        };
        let gone: Vec<(String, Gone)> = devices
            .into_iter()
            .map(|d| {
                let g = self.on_device(&d);
                (d, g)
            })
            .collect();
        let changes = self.store.write().reconcile(fresh);
        for (change, id) in changes {
            let ty = match change {
                Change::Deleted => types
                    .iter()
                    .find(|(i, _)| *i == id)
                    .map(|(_, t)| t.clone())
                    .unwrap_or_default(),
                _ => self
                    .store
                    .read()
                    .get(&id)
                    .and_then(|r| r["type"].as_str().map(str::to_owned))
                    .unwrap_or_default(),
            };
            let g = gone
                .iter()
                .find(|(d, _)| *d == id)
                .map(|(_, g)| g.clone())
                .unwrap_or_default();
            self.after(change, &ty, &id, g);
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

/// What every hue bridge shares: the entity services (`light`, `switch`,
/// `scene`, `hue.*`) and the bridges, for `hue.hue_activate_scene`.
pub struct Platforms {
    hass: Hass,
    registries: Arc<Registries>,
    pub lights: Arc<Lights>,
    pub switches: Arc<Switches>,
    pub scenes: Arc<Scenes>,
    bridges: RwLock<Vec<std::sync::Weak<Bridge>>>,
}

/// Python's `bool(x)` of a JSON value (`vol.Coerce(bool)`).
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

impl Platforms {
    /// Register the services the hue platforms provide.
    pub fn new(
        hass: &Hass,
        registries: Arc<Registries>,
        profiles: crate::light::Profiles,
    ) -> Arc<Self> {
        let p = Arc::new(Self {
            hass: hass.clone(),
            registries: registries.clone(),
            lights: Arc::new(Lights::new(profiles)),
            switches: Arc::new(Switches::default()),
            scenes: Arc::new(Scenes::default()),
            bridges: RwLock::default(),
        });
        p.lights.register_services(hass, registries.clone());
        p.switches.register_services(hass, registries.clone());
        p.scenes.register_services(hass, registries.clone());
        p.register_hue_services();
        p
    }

    fn bridges(&self) -> Vec<Arc<Bridge>> {
        self.bridges
            .read()
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect()
    }

    fn scene_entities(&self) -> Vec<Arc<HueEntity>> {
        self.bridges()
            .iter()
            .flat_map(|b| b.others.read().clone())
            .filter(|e| matches!(e.kind, Kind::Scene | Kind::SmartScene))
            .collect()
    }

    fn register_hue_services(self: &Arc<Self>) {
        use ha_core::{Kind as K, Range, Schema, ServiceError, SupportsResponse};
        let p = self.clone();
        let schema = Schema::entity_service()
            .optional("dynamic", K::Any)
            .optional("speed", K::Int(Range::new(0.0, 100.0)))
            .optional("transition", K::Float(Range::new(0.0, 3600.0)))
            .optional("brightness", K::Int(Range::new(1.0, 255.0)));
        self.hass.services().register_with_schema(
            DOMAIN,
            "activate_scene",
            Some(schema),
            SupportsResponse::None,
            move |call| {
                let p = p.clone();
                async move {
                    let target = call.entity_ids()?;
                    let indirect = p.registries.target_entities(&call.data);
                    let d = &call.data;
                    let dynamic = d.get("dynamic").is_some_and(truthy);
                    let (speed, brightness) = (
                        d.get("speed").and_then(Value::as_i64),
                        d.get("brightness").and_then(Value::as_i64),
                    );
                    let transition = d.get("transition").and_then(Value::as_f64);
                    let scenes = p.scene_entities();
                    let chosen = crate::switch::select(
                        &scenes,
                        |e| &e.entity_id,
                        |_| true,
                        &target,
                        &indirect,
                    );
                    let calls = chosen
                        .iter()
                        .map(|e| e.activate(transition, dynamic, speed, brightness));
                    crate::light::join_all(calls)
                        .await
                        .into_iter()
                        .collect::<Result<Vec<()>, _>>()?;
                    Ok(None)
                }
            },
        );
        let p = self.clone();
        let schema = Schema::new()
            .required("group_name", K::String)
            .required("scene_name", K::String)
            .optional(
                "transition",
                K::Int(Range {
                    min: Some(0.0),
                    max: None,
                }),
            )
            .optional("dynamic", K::Boolean);
        self.hass.services().register_with_schema(
            DOMAIN,
            "hue_activate_scene",
            Some(schema),
            SupportsResponse::None,
            move |call| {
                let p = p.clone();
                async move {
                    tracing::warn!(
                        "Use of service_call 'hue_activate_scene' is deprecated and will be removed in a future release. Please use scene entities instead"
                    );
                    let d = &call.data;
                    let text = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or_default().to_lowercase();
                    let (group, scene) = (text("group_name"), text("scene_name"));
                    let transition = d.get("transition").and_then(Value::as_i64).filter(|t| *t != 0);
                    let dynamic = d.get("dynamic").and_then(Value::as_bool).unwrap_or(false);
                    for bridge in p.bridges() {
                        // Regular scenes first, then smart ones; names match
                        // case-insensitively.
                        let found = {
                            let store = bridge.store.read();
                            ["scene", "smart_scene"].into_iter().find_map(|ty| {
                                store.of_type(ty).find_map(|s| {
                                    let name = s["metadata"]["name"].as_str()?.to_lowercase();
                                    let g = store.get(s["group"]["rid"].as_str()?)?;
                                    let gname = g["metadata"]["name"].as_str()?.to_lowercase();
                                    (name == scene && gname == group)
                                        .then(|| (ty, s["id"].as_str().unwrap_or_default().to_owned()))
                                })
                            })
                        };
                        let Some((ty, id)) = found else {
                            continue;
                        };
                        if ty == "smart_scene" {
                            // HA hands a smart scene arguments it doesn't take.
                            return Err(ServiceError::Failed(
                                "SmartScenesController.recall() got an unexpected keyword argument 'dynamic'".into(),
                            ));
                        }
                        let mut recall = json!({"action": if dynamic { "dynamic_palette" } else { "active" }});
                        if let Some(t) = transition {
                            recall["duration"] = json!(t * 1000);
                        }
                        light::result(bridge.api.put("scene", &id, &json!({"recall": recall})).await)?;
                        return Ok(None);
                    }
                    tracing::warn!("No bridge was able to activate scene {scene} in group {group}");
                    Ok(None)
                }
            },
        );
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
pub async fn run(platforms: Arc<Platforms>, entry: Map<String, Value>) {
    let (hass, registries) = (platforms.hass.clone(), platforms.registries.clone());
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
        others: RwLock::default(),
        platforms: platforms.clone(),
    });
    platforms.bridges.write().push(Arc::downgrade(&bridge));
    bridge.add_entities();
    bridge.write_all();
    bridge.listen().await;
}

#[cfg(test)]
mod tests;
