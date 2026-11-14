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
use ha_registry::Registries;
use parking_lot::Mutex;
use std::sync::{Arc, OnceLock};

pub struct Components {
    hass: Hass,
    registries: Arc<Registries>,
    profiles: Mutex<Option<Profiles>>,
    lights: OnceLock<Arc<Lights>>,
    switches: OnceLock<Arc<Switches>>,
    scenes: OnceLock<Arc<Scenes>>,
    buttons: OnceLock<Arc<Buttons>>,
    climates: OnceLock<Arc<Climates>>,
}

impl Components {
    pub fn new(hass: &Hass, registries: Arc<Registries>, profiles: Profiles) -> Arc<Self> {
        Arc::new(Self {
            hass: hass.clone(),
            registries,
            profiles: Mutex::new(Some(profiles)),
            lights: OnceLock::new(),
            switches: OnceLock::new(),
            scenes: OnceLock::new(),
            buttons: OnceLock::new(),
            climates: OnceLock::new(),
        })
    }

    pub fn hass(&self) -> &Hass {
        &self.hass
    }

    pub fn registries(&self) -> &Arc<Registries> {
        &self.registries
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
