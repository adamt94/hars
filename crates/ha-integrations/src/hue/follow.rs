//! A bridge's entities follow the registry when the user names a device or
//! gives an entity another id ([`crate::components::Follower`]), as HA's
//! entities do (`Entity._async_device_registry_updated` and
//! `_async_registry_updated`): an entity whose device was named, or whose
//! row changed, writes its state again with the names it has now; one
//! whose id changed is removed (its state too, a scene's last activation
//! kept for a restore) and added under the new id, where it writes what it
//! had.
//!
//! Checked against `config_entries/vectors/device_names.json`
//! (`scripts/vectors/device_names.py`) in `ha-api`.

use super::entities::HueEntity;
use super::light::{GroupLight, HueLight};
use super::{Bridge, registry_device, registry_entity};
use crate::components::{Components, Follower};
use ha_core::EntityId;
use ha_registry::Registries;
use serde_json::Value;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

/// What `Components` holds for a bridge: not the bridge, which goes with
/// its entry's task.
pub(super) struct Follow(pub(super) Weak<Bridge>);

impl Follower for Follow {
    fn registry_changed(&self, components: &Components) {
        if let Some(bridge) = self.0.upgrade() {
            bridge.follow(components.registries());
        }
    }
}

/// An entity the follow writes again.
enum Write {
    Light(Arc<HueLight>),
    Group(Arc<GroupLight>),
    Other(Arc<HueEntity>),
}

impl Bridge {
    /// Take `fresh`, the registries as they are now, and have each entity
    /// follow what changed since they were taken. All under the write
    /// guard: no state made with the old names is written after.
    pub(super) fn follow(&self, fresh: Arc<Registries>) {
        let alive = self.alive.lock();
        let old = std::mem::replace(&mut *self.registries.write(), fresh.clone());
        self.names.fetch_add(1, Ordering::Release);
        if !*alive {
            return;
        }
        let changes = Changes {
            old: &old,
            new: &fresh,
        };
        let store = self.store.read();
        let mut writes = Vec::new();
        for l in self.lights.write().iter_mut() {
            let device = store.device_of(&l.rid);
            match changes.of("light", &l.rid, device, &l.entity_id) {
                Change::Same => continue,
                Change::Write => {}
                Change::Move(id) => {
                    let moved = Arc::new(l.moved(id));
                    l.gone.store(true, Ordering::Relaxed);
                    self.platforms.lights.remove(&l.entity_id);
                    self.hass.remove_state(&l.entity_id);
                    *l = moved;
                    self.platforms.lights.add(l.clone());
                }
            }
            writes.push(Write::Light(l.clone()));
        }
        for g in self.groups.write().iter_mut() {
            let device = store.get(&g.group);
            match changes.of("light", &g.rid, device, &g.entity_id) {
                Change::Same => continue,
                Change::Write => {}
                Change::Move(id) => {
                    let moved = Arc::new(g.moved(id));
                    g.gone.store(true, Ordering::Relaxed);
                    self.platforms.lights.remove(&g.entity_id);
                    self.hass.remove_state(&g.entity_id);
                    *g = moved;
                    self.platforms.lights.add(g.clone());
                }
            }
            writes.push(Write::Group(g.clone()));
        }
        for e in self.others.write().iter_mut() {
            let device = store
                .get(&e.rid)
                .and_then(|r| HueEntity::device(e.kind, &e.rid, r, &store));
            match changes.of(e.kind.domain(), &e.rid, device, &e.entity_id) {
                Change::Same => continue,
                Change::Write => {}
                Change::Move(id) => {
                    let moved = Arc::new(e.moved(id));
                    e.gone.store(true, Ordering::Relaxed);
                    // `async_remove`: a scene's last activation is kept
                    // for a restore, then its state goes.
                    self.hass
                        .restore()
                        .unregister(&self.hass, e.entity_id.as_str());
                    self.platforms.switches.remove(&e.entity_id);
                    self.platforms.scenes.remove(&e.entity_id);
                    self.hass.remove_state(&e.entity_id);
                    // `async_added_to_hass` again, for the new id.
                    moved.restore(&self.hass);
                    *e = moved;
                    match e.kind.domain() {
                        "switch" => self.platforms.switches.add(e.clone()),
                        "scene" => self.platforms.scenes.add(e.clone()),
                        _ => {}
                    }
                }
            }
            writes.push(Write::Other(e.clone()));
        }
        // The guard is held: the states are made and written here.
        for write in writes {
            let (id, rendered) = match &write {
                Write::Light(l) => (&l.entity_id, l.render(self, &store)),
                Write::Group(g) => {
                    g.update_values(&store);
                    (&g.entity_id, g.render(self, &store))
                }
                Write::Other(e) => (&e.entity_id, e.render(self, &store)),
            };
            if let Some((state, attrs)) = rendered {
                self.hass.set_state(id.clone(), state, attrs);
            }
        }
    }
}

/// What became of an entity.
enum Change {
    Same,
    /// Its device was named or its row changed: it writes its state again.
    Write,
    /// Its row has another id.
    Move(EntityId),
}

/// The registries before and after.
struct Changes<'a> {
    old: &'a Registries,
    new: &'a Registries,
}

impl Changes<'_> {
    /// For a resource's entity in `domain`, on the Hue `device` (or room),
    /// running under `entity_id`. A row that is gone or disabled changes
    /// nothing: its entity runs on until its entry is set up again.
    fn of(
        &self,
        domain: &str,
        unique_id: &str,
        device: Option<&Value>,
        entity_id: &EntityId,
    ) -> Change {
        let Some(row) = registry_entity(self.new, domain, unique_id) else {
            return Change::Same;
        };
        if row.disabled_by.is_some() {
            return Change::Same;
        }
        if row.entity_id != entity_id.as_str() {
            return match EntityId::parse(&row.entity_id) {
                Ok(id) => Change::Move(id),
                Err(_) => Change::Same,
            };
        }
        // Rows are shared between snapshots: one that wasn't touched is
        // the same row.
        let was = registry_entity(self.old, domain, unique_id);
        let row_changed = !was.is_some_and(|was| Arc::ptr_eq(was, row));
        // `_async_device_registry_updated`: only a new name counts.
        let hue_id = device.and_then(|d| d["id"].as_str());
        let named = hue_id.is_some_and(|id| {
            let (was, now) = (registry_device(self.old, id), registry_device(self.new, id));
            match (was, now) {
                (Some(was), Some(now)) => {
                    was.name != now.name || was.name_by_user != now.name_by_user
                }
                (None, None) => false,
                _ => true,
            }
        });
        match row_changed || named {
            true => Change::Write,
            false => Change::Same,
        }
    }
}
