//! Entity ids and friendly names from the imported registries, for
//! integrations whose entities have `has_entity_name`.

use ha_core::{EntityId, Hass};
use ha_registry::Registries;
use parking_lot::Mutex;
use serde_json::Value;

pub struct Names<'a> {
    hass: &'a Hass,
    registries: &'a Registries,
    /// The integration's domain (the registry's `platform`).
    platform: &'a str,
    /// Ids given out already, before their states are written.
    given: Mutex<Vec<String>>,
}

impl<'a> Names<'a> {
    pub fn new(hass: &'a Hass, registries: &'a Registries, platform: &'a str) -> Self {
        Self {
            hass,
            registries,
            platform,
            given: Mutex::default(),
        }
    }

    /// The registry device with identifier `(platform, id)`.
    pub fn registry_device(&self, id: &str) -> Option<&ha_registry::Device> {
        self.registries
            .devices
            .iter()
            .find(|d| {
                d.identifiers
                    .iter()
                    .any(|i| i[0] == self.platform && i[1] == id)
            })
            .map(|v| &**v)
    }

    /// A device's name: the user's, else the integration's.
    pub fn device_name<'d>(&'d self, id: &str, default: &'d str) -> &'d str {
        self.registry_device(id)
            .and_then(|r| r.name_by_user.as_deref())
            .unwrap_or(default)
    }

    /// An entity's id (the registry's, else HA's rule for a new one:
    /// `slugify(area device name)`, made unique) and friendly name (the
    /// user's, else the device's joined with the entity's), or `None` if
    /// it's disabled.
    pub fn entity(
        &self,
        domain: &str,
        unique_id: &str,
        device_id: &str,
        device_default: &str,
        name: Option<&str>,
    ) -> Option<(EntityId, String)> {
        let entry = self.registries.entities.iter().find(|e| {
            e.platform == self.platform
                && e.entity_id.split('.').next() == Some(domain)
                && e.unique_id.as_ref().and_then(Value::as_str) == Some(unique_id)
        });
        if entry.is_some_and(|e| e.disabled_by.is_some()) {
            return None;
        }
        let device = self.device_name(device_id, device_default);
        let friendly = match (entry.and_then(|e| e.name.clone()), name) {
            (Some(user), _) => user,
            (None, Some(n)) => format!("{device} {n}"),
            (None, None) => device.to_owned(),
        };
        let id = match entry {
            Some(e) => e.entity_id.clone(),
            None => {
                let area = self
                    .registry_device(device_id)
                    .and_then(|r| r.area_id.as_deref())
                    .and_then(|a| self.registries.areas.iter().find(|x| x.id == a))
                    .map(|a| a.name.as_str());
                let parts: Vec<&str> = [area, Some(device), name].into_iter().flatten().collect();
                let base = format!("{domain}.{}", ha_components::slugify(&parts.join(" ")));
                let mut given = self.given.lock();
                let taken = |id: &str| {
                    self.registries.entities.iter().any(|e| e.entity_id == id)
                        || given.iter().any(|g| g == id)
                        || EntityId::parse(id).is_ok_and(|i| self.hass.states().get(&i).is_some())
                };
                let id = (1..)
                    .map(|n| match n {
                        1 => base.clone(),
                        n => format!("{base}_{n}"),
                    })
                    .find(|id| !taken(id))?;
                given.push(id.clone());
                id
            }
        };
        Some((EntityId::parse(&id).ok()?, friendly))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two new devices of the same name get their own ids.
    #[test]
    fn new_ids_are_unique_within_a_setup() {
        let hass = Hass::new();
        let registries = Registries::default();
        let names = Names::new(&hass, &registries, "switchbot_cloud");
        let a = names
            .entity("button", "A", "A", "Towel switch", None)
            .unwrap();
        let b = names
            .entity("button", "B", "B", "Towel switch", None)
            .unwrap();
        assert_eq!(a.0.as_str(), "button.towel_switch");
        assert_eq!(b.0.as_str(), "button.towel_switch_2");
        assert_eq!(b.1, "Towel switch");
    }
}
