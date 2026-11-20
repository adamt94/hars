//! The entity components, one per domain however many integrations feed
//! it: HA's `EntityComponent`s. A domain's services are registered the
//! first time a platform asks for it (registering twice would replace
//! the handler, dropping the first platform's entities from it).

use crate::button::Buttons;
use crate::climate::Climates;
use crate::light::{Lights, Profiles};
use crate::scene::Scenes;
use crate::switch::Switches;
use ha_core::Hass;
use ha_registry::{Lookup, Registries};
use parking_lot::Mutex;
use std::sync::{Arc, OnceLock};

/// An entry whose entities take what the registry says of them when it
/// changes: a device's new name (their friendly names), an entity's new
/// id. HA's entities each listen for this; here the entry is told, by
/// whoever makes the change ([`Components::registry_edit`]).
pub trait Follower: Send + Sync {
    /// Called with `switchbot_cloud::SETUP` held: no entry is set up or
    /// removed meanwhile.
    fn registry_changed(&self, components: &Components);
}

/// The integrations whose entries are [`Follower`]s. The entities of
/// another keep the name and id they started with until it is set up
/// again, so a change that would reach them is refused.
pub fn follows(platform: &str) -> bool {
    platform == crate::switchbot_cloud::DOMAIN
}

pub struct Components {
    hass: Hass,
    /// The live registry in `hars`; a fixed set in tests.
    registries: Lookup,
    profiles: Mutex<Option<Profiles>>,
    lights: OnceLock<Arc<Lights>>,
    switches: OnceLock<Arc<Switches>>,
    scenes: OnceLock<Arc<Scenes>>,
    buttons: OnceLock<Arc<Buttons>>,
    climates: OnceLock<Arc<Climates>>,
    /// The entries that follow the registry, by entry id, from their
    /// set-up until they are removed (so that one whose task was stopped
    /// can still move its entities for the removal to find). Empty, and
    /// unallocated, on an install with none.
    followers: Mutex<Vec<(String, Arc<dyn Follower>)>>,
}

impl Components {
    pub fn new(hass: &Hass, registries: impl Into<Lookup>, profiles: Profiles) -> Arc<Self> {
        Arc::new(Self {
            hass: hass.clone(),
            registries: registries.into(),
            profiles: Mutex::new(Some(profiles)),
            lights: OnceLock::new(),
            switches: OnceLock::new(),
            scenes: OnceLock::new(),
            buttons: OnceLock::new(),
            climates: OnceLock::new(),
            followers: Mutex::default(),
        })
    }

    /// Tell `follower` of registry changes until its entry is removed
    /// ([`Components::unfollow`]). An entry set up again replaces its
    /// earlier follower.
    pub fn follow(&self, entry_id: &str, follower: Arc<dyn Follower>) {
        let mut all = self.followers.lock();
        all.retain(|(id, _)| id != entry_id);
        all.push((entry_id.to_owned(), follower));
    }

    /// The entry is gone: nothing of it is left to follow.
    pub fn unfollow(&self, entry_id: &str) {
        let mut all = self.followers.lock();
        all.retain(|(id, _)| id != entry_id);
        if all.is_empty() {
            *all = Vec::new();
        }
    }

    /// Change a device's name or an entity's id in the registry with
    /// `edit`, and have every entry that follows take its entities' ids
    /// and names again before anything else sees the change: no entry is
    /// set up or removed in between (it would find rows the entities have
    /// not caught up with), and two such changes take turns.
    pub fn registry_edit<T>(&self, edit: impl FnOnce() -> T) -> T {
        let _one = crate::switchbot_cloud::SETUP.lock();
        let out = edit();
        let followers: Vec<Arc<dyn Follower>> = self
            .followers
            .lock()
            .iter()
            .map(|(_, f)| f.clone())
            .collect();
        for follower in followers {
            follower.registry_changed(self);
        }
        out
    }

    pub fn hass(&self) -> &Hass {
        &self.hass
    }

    /// The registries as they are now: what an entry reads at setup.
    pub fn registries(&self) -> Arc<Registries> {
        self.registries.get()
    }

    /// Where an entry makes its device and entity rows (`None` in tests
    /// that run on a fixed set).
    pub fn registry(&self) -> Option<&Arc<ha_registry::Registry>> {
        self.registries.live()
    }

    pub fn lights(&self) -> Arc<Lights> {
        self.lights
            .get_or_init(|| {
                let profiles = self.profiles.lock().take().unwrap_or_default();
                let c = Arc::new(Lights::new(profiles));
                c.register_services(&self.hass, self.registries.clone());
                c
            })
            .clone()
    }

    pub fn switches(&self) -> Arc<Switches> {
        self.switches
            .get_or_init(|| {
                let c = Arc::new(Switches::default());
                c.register_services(&self.hass, self.registries.clone());
                c
            })
            .clone()
    }

    pub fn scenes(&self) -> Arc<Scenes> {
        self.scenes
            .get_or_init(|| {
                let c = Arc::new(Scenes::default());
                c.register_services(&self.hass, self.registries.clone());
                c
            })
            .clone()
    }

    pub fn buttons(&self) -> Arc<Buttons> {
        self.buttons
            .get_or_init(|| {
                let c = Arc::new(Buttons::default());
                c.register_services(&self.hass, self.registries.clone());
                c
            })
            .clone()
    }

    pub fn climates(&self) -> Arc<Climates> {
        self.climates
            .get_or_init(|| {
                let c = Arc::new(Climates::default());
                c.register_services(&self.hass, self.registries.clone());
                c
            })
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two platforms asking for the same domain share one registry, so
    /// the services reach both platforms' entities.
    #[tokio::test]
    async fn one_registry_per_domain() {
        let hass = Hass::new();
        let registries = Arc::new(Registries::default());
        let c = Components::new(&hass, registries, Profiles::default());
        assert!(!hass.services().has_service("switch", "turn_on"));
        assert!(Arc::ptr_eq(&c.switches(), &c.switches()));
        assert!(hass.services().has_service("switch", "turn_on"));
        assert!(Arc::ptr_eq(&c.climates(), &c.climates()));
        assert!(hass.services().has_service("climate", "set_hvac_mode"));
        assert!(!hass.services().has_service("button", "press"));
        let _ = c.buttons();
        assert!(hass.services().has_service("button", "press"));
    }
}
