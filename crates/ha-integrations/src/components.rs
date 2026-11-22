//! The entity components, one per domain however many integrations feed
//! it: HA's `EntityComponent`s. A domain's services are registered the
//! first time a platform asks for it (registering twice would replace
//! the handler, dropping the first platform's entities from it).

use crate::button::Buttons;
use crate::climate::Climates;
use crate::fan::Fans;
use crate::humidifier::Humidifiers;
use crate::light::{Lights, Profiles};
use crate::number::Numbers;
use crate::scene::Scenes;
use crate::select::Selects;
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
    [
        crate::switchbot_cloud::DOMAIN,
        crate::hue::DOMAIN,
        crate::lifx::DOMAIN,
    ]
    .contains(&platform)
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
    humidifiers: OnceLock<Arc<Humidifiers>>,
    fans: OnceLock<Arc<Fans>>,
    numbers: OnceLock<Arc<Numbers>>,
    selects: OnceLock<Arc<Selects>>,
    /// What every Hue bridge shares, made with the first.
    hue: OnceLock<Arc<crate::hue::Platforms>>,
    /// The entries that follow the registry, by entry id, from their
    /// set-up until they are removed (so that one whose task was stopped
    /// can still move its entities for the removal to find). Empty, and
    /// unallocated, on an install with none.
    followers: Mutex<Vec<(String, Arc<dyn Follower>)>>,
    /// The devices an entry runs, as (unique id, entry id): one entry each.
    /// Empty, and unallocated, on an install with none.
    running: Mutex<Vec<(String, String)>>,
}

impl Components {
    /// Entry `entry_id` runs the device `unique_id`, unless another entry
    /// runs it already: false then. Released with [`Components::let_go`].
    pub(crate) fn run_device(&self, unique_id: &str, entry_id: &str) -> bool {
        let mut running = self.running.lock();
        match running.iter().find(|(u, _)| u == unique_id) {
            Some((_, owner)) => owner == entry_id,
            None => {
                running.push((unique_id.to_owned(), entry_id.to_owned()));
                true
            }
        }
    }

    /// Entry `entry_id` no longer runs `unique_id`.
    pub(crate) fn let_go(&self, unique_id: &str, entry_id: &str) {
        let mut running = self.running.lock();
        running.retain(|(u, e)| !(u == unique_id && e == entry_id));
        if running.is_empty() {
            *running = Vec::new();
        }
    }

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
            humidifiers: OnceLock::new(),
            fans: OnceLock::new(),
            numbers: OnceLock::new(),
            selects: OnceLock::new(),
            hue: OnceLock::new(),
            followers: Mutex::default(),
            running: Mutex::default(),
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
        #[cfg(test)]
        hooks::at("edited");
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

    /// The registries as they change (or the fixed set of a test).
    pub fn lookup(&self) -> Lookup {
        self.registries.clone()
    }

    /// Hue's entity services and bridges, made when first asked for.
    pub fn hue(&self) -> Arc<crate::hue::Platforms> {
        self.hue
            .get_or_init(|| crate::hue::Platforms::new(self))
            .clone()
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

    pub fn humidifiers(&self) -> Arc<Humidifiers> {
        self.humidifiers
            .get_or_init(|| {
                let c = Arc::new(Humidifiers::default());
                c.register_services(&self.hass, self.registries.clone());
                c
            })
            .clone()
    }

    pub fn fans(&self) -> Arc<Fans> {
        self.fans
            .get_or_init(|| {
                let c = Arc::new(Fans::default());
                c.register_services(&self.hass, self.registries.clone());
                c
            })
            .clone()
    }

    pub fn numbers(&self) -> Arc<Numbers> {
        self.numbers
            .get_or_init(|| {
                let c = Arc::new(Numbers::default());
                c.register_services(&self.hass, self.registries.clone());
                c
            })
            .clone()
    }

    pub fn selects(&self) -> Arc<Selects> {
        self.selects
            .get_or_init(|| {
                let c = Arc::new(Selects::default());
                c.register_services(&self.hass, self.registries.clone());
                c
            })
            .clone()
    }
}

/// Points in the code where a test runs something of its own, once, on
/// the thread that gets there: to put a change between two steps.
#[cfg(test)]
pub(crate) mod hooks {
    use std::cell::RefCell;

    type Hook = Box<dyn FnOnce()>;

    thread_local! {
        static HOOKS: RefCell<Vec<(&'static str, Hook)>> = const { RefCell::new(Vec::new()) };
    }

    /// Run `f` the next time this thread gets to `point`.
    pub(crate) fn set(point: &'static str, f: impl FnOnce() + 'static) {
        HOOKS.with(|h| h.borrow_mut().push((point, Box::new(f))));
    }

    /// `point` is reached: its hook, if one was set, runs.
    pub(crate) fn at(point: &'static str) {
        let hook = HOOKS.with(|h| {
            let mut h = h.borrow_mut();
            let i = h.iter().position(|(p, _)| *p == point)?;
            Some(h.remove(i).1)
        });
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Whether `point`'s hook is still waiting.
    pub(crate) fn pending(point: &'static str) -> bool {
        HOOKS.with(|h| h.borrow().iter().any(|(p, _)| *p == point))
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
