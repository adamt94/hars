//! Generic entity adapters and scoped device/entity tracking for plugins.
//!
//! Every entity a plugin registers is wired into `hars`'s existing entity
//! components (`light`, `switch`, `button`, `scene`) and `ha-registry` so
//! dashboards, automations, and services treat plugin entities identically to
//! built-in integrations.

use super::manifest::InstalledPlugin;
use super::protocol::{DeviceUpsertParams, EntityCallParams, EntityUpsertParams, StateSetParams};
use crate::button::Button;
use crate::climate::{self, Climate, SetTemperature, Temp};
use crate::components::Components;
use crate::cover::Cover;
use crate::fan::{self, Fan};
use crate::light::{
    BoxFuture, Capabilities as LightCaps, ColorMode, Flash, Light, LightState, Modes, Params,
    TurnOff, White,
};
use crate::lock::Lock;
use crate::media_player::{self, MediaPlayer, MediaPlayerState, PlayMedia};
use crate::notify::Notify;
use crate::number::{self, Number};
use crate::remote::{DeleteCommand, LearnCommand, Remote, SendCommand};
use crate::scene::Scene;
use crate::select::Select;
use crate::switch::Switch;
use crate::vacuum::Vacuum;
use ha_core::{Attributes, Context, EntityId, Hass, ServiceError};
use ha_registry::{DeviceInfo, EntityPatch};
use parking_lot::{Mutex, RwLock};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Callback used by entity adapters to forward service calls (`light.turn_on`,
/// `switch.turn_on`, `button.press`, `scene.turn_on`) to the running plugin.
pub type CallPluginFn = Arc<
    dyn Fn(EntityCallParams) -> Pin<Box<dyn Future<Output = Result<(), ServiceError>> + Send>>
        + Send
        + Sync,
>;

/// Per-entry registered devices and entities, strictly scoped to one plugin
/// domain.
pub(crate) struct PluginEntityStore {
    domain: String,
    plugin: Arc<InstalledPlugin>,
    components: Arc<Components>,
    call_plugin: CallPluginFn,
    /// Active config entries owned by this plugin domain (`entry_id -> EntryEntities`).
    entries: Mutex<HashMap<String, EntryEntities>>,
}

#[derive(Default)]
struct EntryEntities {
    /// Plugin `device_identifier` -> `ha-registry` device id.
    devices: HashMap<String, String>,
    /// Plugin `unique_id` -> registered entity.
    entities: HashMap<String, RegisteredEntity>,
}

struct RegisteredEntity {
    entity_id: EntityId,
    domain: String,
    friendly_name: Option<String>,
    /// Shared mutable state for interactive entities (`switch`, `light`,
    /// `button`, `scene`).
    handle: EntityHandle,
    /// Immutable registration-time attributes (`friendly_name`, `icon`,
    /// `device_class`, `unit_of_measurement`). Each `state.set` starts fresh
    /// from these plus the current frame's attributes so keys never accumulate.
    base_attrs: Attributes,
}

enum EntityHandle {
    Switch(Arc<PluginSwitch>),
    Light(Arc<PluginLight>),
    Button(Arc<PluginButton>),
    Scene(Arc<PluginScene>),
    Climate(Arc<PluginClimate>),
    Vacuum(Arc<PluginVacuum>),
    Select(Arc<PluginSelect>),
    Number(Arc<PluginNumber>),
    Cover(Arc<PluginCover>),
    Lock(Arc<PluginLock>),
    Fan(Arc<PluginFan>),
    MediaPlayer(Arc<PluginMediaPlayer>),
    Remote(Arc<PluginRemote>),
    Notify(Arc<PluginNotify>),
    Passive,
    /// Registry row has `disabled_by` set: no service adapter or state writes.
    Disabled,
}

impl PluginEntityStore {
    pub fn new(
        plugin: Arc<InstalledPlugin>,
        components: Arc<Components>,
        call_plugin: CallPluginFn,
    ) -> Self {
        Self {
            domain: plugin.domain().to_owned(),
            plugin,
            components,
            call_plugin,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Allow a plugin to manage entities under `entry_id`.
    pub fn activate_entry(&self, entry_id: &str) {
        self.entries.lock().entry(entry_id.to_owned()).or_default();
    }

    /// Create or update a device row strictly scoped to this plugin's domain
    /// and `entry_id`. Matches exclusively on the plugin-namespaced identifier
    /// `(domain, identifier)` (never `connections`) so a plugin cannot match,
    /// restore, or overwrite a live or deleted device owned by another
    /// integration under any concurrency. Holds `self.entries` across the
    /// registry write so deactivation strictly serializes with in-flight
    /// notifications.
    pub fn upsert_device(&self, params: DeviceUpsertParams) -> Result<String, String> {
        let mut entries = self.entries.lock();
        let entry = entries.get_mut(&params.entry_id).ok_or_else(|| {
            format!(
                "entry {:?} is not active for plugin {:?}",
                params.entry_id, self.domain
            )
        })?;
        let device_id = if let Some(registry) = self.components.registry() {
            let device = registry.get_or_create_device(DeviceInfo {
                config_entry_id: params.entry_id.clone(),
                identifiers: vec![json!([self.domain, params.identifier])],
                connections: Vec::new(),
                entry_type: None,
                manufacturer: params.manufacturer,
                model: params.model,
                model_id: params.model_id,
                name: params.name,
                sw_version: params.sw_version,
                via_device: None,
                suggested_area: params.suggested_area,
            });
            device.id.clone()
        } else {
            format!("{}-{}", self.domain, params.identifier)
        };
        entry.devices.insert(params.identifier, device_id.clone());
        Ok(device_id)
    }

    /// Register or update an entity row and wire it into its `Components`
    /// service registry. Rejects `(domain, unique_id)` collisions with another
    /// active entry so two entries never share one adapter or entity row, and
    /// holds `self.entries` across registry + adapter registration so
    /// deactivation strictly serializes with in-flight notifications.
    pub fn upsert_entity(&self, params: EntityUpsertParams) -> Result<String, String> {
        if !self.plugin.allows_entity_domain(&params.domain) {
            return Err(format!(
                "plugin {:?} is not permitted to create {:?} entities",
                self.domain, params.domain
            ));
        }
        let mut entries = self.entries.lock();
        if !entries.contains_key(&params.entry_id) {
            return Err(format!(
                "entry {:?} is not active for plugin {:?}",
                params.entry_id, self.domain
            ));
        }
        if let Some(existing) = entries
            .get(&params.entry_id)
            .and_then(|e| e.entities.get(&params.unique_id))
            && existing.domain != params.domain
        {
            return Err(format!(
                "unique_id {:?} is already registered in domain {:?} for entry {:?}",
                params.unique_id, existing.domain, params.entry_id
            ));
        }
        for (other_eid, other) in entries.iter() {
            if other_eid != &params.entry_id
                && let Some(existing) = other.entities.get(&params.unique_id)
                && existing.domain == params.domain
            {
                return Err(format!(
                    "unique_id {:?} in domain {:?} is already registered by entry {other_eid:?}",
                    params.unique_id, params.domain
                ));
            }
        }
        let prev_media_caps = entries
            .get(&params.entry_id)
            .and_then(|e| e.entities.get(&params.unique_id))
            .and_then(|reg| match &reg.handle {
                EntityHandle::MediaPlayer(old) => {
                    let s = old.state.read();
                    Some((s.source_list.clone(), s.sound_mode_list.clone()))
                }
                _ => None,
            });
        let resolved_media_source_list = if params.domain == "media_player" {
            match &params.source_list {
                Some(list) => normalize_string_list_vec(list),
                None => prev_media_caps.as_ref().and_then(|(s, _)| s.clone()),
            }
        } else {
            None
        };
        let resolved_media_sound_mode_list = if params.domain == "media_player" {
            match &params.sound_mode_list {
                Some(list) => normalize_string_list_vec(list),
                None => prev_media_caps.as_ref().and_then(|(_, m)| m.clone()),
            }
        } else {
            None
        };
        let prev_climate_caps = entries
            .get(&params.entry_id)
            .and_then(|e| e.entities.get(&params.unique_id))
            .and_then(|reg| match &reg.handle {
                EntityHandle::Climate(old) => Some(old.state.read().clone()),
                _ => None,
            });
        let resolved_climate = if params.domain == "climate" {
            let min_temp = params
                .min_temp
                .as_ref()
                .and_then(temp_from_number)
                .or_else(|| prev_climate_caps.as_ref().map(|c| c.min_temp))
                .unwrap_or(Temp::Int(climate::DEFAULT_MIN_TEMP));
            let max_temp = params
                .max_temp
                .as_ref()
                .and_then(temp_from_number)
                .or_else(|| prev_climate_caps.as_ref().map(|c| c.max_temp))
                .unwrap_or(Temp::Int(climate::DEFAULT_MAX_TEMP));
            let target_temp_step = params
                .target_temp_step
                .as_ref()
                .and_then(temp_from_number)
                .filter(|t| t.get() > 0.0)
                .or_else(|| prev_climate_caps.as_ref().and_then(|c| c.target_temp_step));
            let min_humidity = params
                .min_humidity
                .as_ref()
                .and_then(temp_from_number)
                .or_else(|| prev_climate_caps.as_ref().map(|c| c.min_humidity))
                .unwrap_or(Temp::Int(climate::DEFAULT_MIN_HUMIDITY));
            let max_humidity = params
                .max_humidity
                .as_ref()
                .and_then(temp_from_number)
                .or_else(|| prev_climate_caps.as_ref().map(|c| c.max_humidity))
                .unwrap_or(Temp::Int(climate::DEFAULT_MAX_HUMIDITY));
            let target_humidity_step = params
                .target_humidity_step
                .as_ref()
                .and_then(temp_from_number)
                .filter(|t| t.get() > 0.0)
                .or_else(|| {
                    prev_climate_caps
                        .as_ref()
                        .and_then(|c| c.target_humidity_step)
                });
            let hvac_modes = params
                .hvac_modes
                .clone()
                .or_else(|| prev_climate_caps.as_ref().map(|c| c.hvac_modes.clone()))
                .unwrap_or_default();
            let fan_modes = params
                .fan_modes
                .clone()
                .or_else(|| prev_climate_caps.as_ref().and_then(|c| c.fan_modes.clone()));
            let preset_modes = params.preset_modes.clone().or_else(|| {
                prev_climate_caps
                    .as_ref()
                    .and_then(|c| c.preset_modes.clone())
            });
            let swing_modes = params.swing_modes.clone().or_else(|| {
                prev_climate_caps
                    .as_ref()
                    .and_then(|c| c.swing_modes.clone())
            });
            let swing_horizontal_modes = params.swing_horizontal_modes.clone().or_else(|| {
                prev_climate_caps
                    .as_ref()
                    .and_then(|c| c.swing_horizontal_modes.clone())
            });
            let mut inner = ClimateInner {
                available: prev_climate_caps.as_ref().is_none_or(|c| c.available),
                hvac_mode: prev_climate_caps.as_ref().and_then(|c| c.hvac_mode.clone()),
                min_temp,
                max_temp,
                target_temp_step,
                min_humidity,
                max_humidity,
                target_humidity_step,
                hvac_modes,
                fan_modes,
                preset_modes,
                swing_modes,
                swing_horizontal_modes,
                cached_caps: Map::new(),
            };
            inner.cached_caps = climate_capabilities_map(params.supported_features, &inner);
            Some(inner)
        } else {
            None
        };
        let device_id = params.device_identifier.as_ref().and_then(|ident| {
            entries
                .get(&params.entry_id)
                .and_then(|e| e.devices.get(ident).cloned())
        });

        let (entity_id, disabled, lock_default_code) =
            if let Some(registry) = self.components.registry() {
                let mut patch = EntityPatch {
                    config_entry_id: Some(Some(params.entry_id.clone())),
                    device_id: Some(device_id.clone()),
                    has_entity_name: Some(true),
                    original_name: Some(params.name.clone()),
                    object_id_base: Some(
                        params
                            .object_id_base
                            .clone()
                            .or_else(|| params.name.clone()),
                    ),
                    original_icon: params.original_icon.clone().map(Some),
                    original_device_class: params.device_class.clone().map(Some),
                    unit_of_measurement: params.unit_of_measurement.clone().map(Some),
                    entity_category: params.entity_category.clone().map(Some),
                    supported_features: Some(i64::from(params.supported_features)),
                    ..Default::default()
                };
                if params.domain == "light" {
                    let caps = parse_light_caps(&params);
                    patch.capabilities = Some(Some(json!({
                        "supported_color_modes": caps
                            .modes
                            .iter()
                            .map(|m| m.as_str())
                            .collect::<Vec<_>>(),
                    })));
                } else if params.domain == "vacuum"
                    && let Some(fan_speeds) = &params.fan_speed_list
                {
                    patch.capabilities = Some(Some(json!({
                        "fan_speed_list": fan_speeds,
                    })));
                } else if params.domain == "select" {
                    let options = params.options.clone().unwrap_or_default();
                    patch.capabilities = Some(Some(json!({
                        "options": options,
                    })));
                } else if params.domain == "number" {
                    let min = params.min_value.unwrap_or(number::DEFAULT_MIN_VALUE);
                    let max = params.max_value.unwrap_or(number::DEFAULT_MAX_VALUE);
                    let step = number::calculate_step(min, max, params.step);
                    let mode = params.mode.as_deref().unwrap_or("auto");
                    patch.capabilities = Some(Some(json!({
                        "min": min,
                        "max": max,
                        "step": step,
                        "mode": mode,
                    })));
                } else if params.domain == "fan"
                    && (params.supported_features
                        & (fan::feature::SET_SPEED | fan::feature::PRESET_MODE)
                        != 0)
                {
                    patch.capabilities = Some(Some(json!({
                        "preset_modes": params.preset_modes,
                    })));
                } else if params.domain == "media_player" {
                    patch.capabilities = Some(Some(media_player_capabilities(
                        params.supported_features,
                        &resolved_media_source_list,
                        &resolved_media_sound_mode_list,
                    )));
                } else if let Some(clim) = &resolved_climate {
                    patch.capabilities = Some(Some(Value::Object(clim.cached_caps.clone())));
                }
                let row = registry.get_or_create_entity(
                    &params.domain,
                    &self.domain,
                    &params.unique_id,
                    None,
                    patch,
                );
                let id = EntityId::parse(&row.entity_id)
                    .map_err(|e| format!("invalid entity_id {:?}: {e}", row.entity_id))?;
                let default_code = (params.domain == "lock")
                    .then(|| {
                        row.options
                            .object()
                            .get("lock")
                            .and_then(|v| v.get("default_code"))
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                    })
                    .flatten();
                (id, row.disabled_by.is_some(), default_code)
            } else {
                let slug = ha_core::slugify(
                    params
                        .object_id_base
                        .as_deref()
                        .or(params.name.as_deref())
                        .unwrap_or(&params.unique_id),
                );
                let slug = if slug.is_empty() {
                    ha_core::slugify(&params.unique_id)
                } else {
                    slug
                };
                let id = EntityId::parse(&format!("{}.{slug}", params.domain))
                    .map_err(|e| format!("invalid entity_id: {e}"))?;
                (id, false, None)
            };

        let friendly_name = params.name.clone().or_else(|| {
            device_id.as_deref().and_then(|did| {
                self.components
                    .registries()
                    .devices
                    .iter()
                    .find(|d| d.id == did)
                    .and_then(|d| d.name_by_user.clone().or_else(|| d.name.clone()))
            })
        });
        let mut base_attrs = Attributes::new();
        if let Some(name) = &friendly_name {
            base_attrs.insert("friendly_name".into(), json!(name));
        }
        if let Some(icon) = &params.original_icon {
            base_attrs.insert("icon".into(), json!(icon));
        }
        if let Some(dc) = &params.device_class {
            base_attrs.insert("device_class".into(), json!(dc));
        }
        if let Some(unit) = &params.unit_of_measurement {
            base_attrs.insert("unit_of_measurement".into(), json!(unit));
        }
        if params.domain == "vacuum" {
            base_attrs.insert(
                "supported_features".into(),
                json!(params.supported_features),
            );
            if let Some(fan_speeds) = &params.fan_speed_list {
                base_attrs.insert("fan_speed_list".into(), json!(fan_speeds));
            }
        } else if params.domain == "select" {
            if let Some(options) = &params.options {
                base_attrs.insert("options".into(), json!(options));
            }
        } else if params.domain == "number" {
            let min = params.min_value.unwrap_or(number::DEFAULT_MIN_VALUE);
            let max = params.max_value.unwrap_or(number::DEFAULT_MAX_VALUE);
            let step = number::calculate_step(min, max, params.step);
            let mode = params.mode.as_deref().unwrap_or("auto");
            base_attrs.insert("min".into(), json!(min));
            base_attrs.insert("max".into(), json!(max));
            base_attrs.insert("step".into(), json!(step));
            base_attrs.insert("mode".into(), json!(mode));
        } else if params.domain == "cover" {
            base_attrs.insert(
                "supported_features".into(),
                json!(params.supported_features),
            );
        } else if params.domain == "lock" {
            base_attrs.insert(
                "supported_features".into(),
                json!(params.supported_features),
            );
            if let Some(fmt) = &params.code_format {
                base_attrs.insert("code_format".into(), json!(fmt));
            }
        } else if params.domain == "fan" {
            base_attrs.insert(
                "supported_features".into(),
                json!(params.supported_features),
            );
            if params.supported_features & (fan::feature::SET_SPEED | fan::feature::PRESET_MODE)
                != 0
            {
                base_attrs.insert("preset_modes".into(), json!(params.preset_modes));
            }
            if params.supported_features & fan::feature::SET_SPEED != 0 {
                let sc = params.speed_count.unwrap_or(100).max(1);
                base_attrs.insert("percentage_step".into(), json!(100.0 / f64::from(sc)));
            }
        } else if params.domain == "media_player" {
            base_attrs.insert(
                "supported_features".into(),
                json!(params.supported_features),
            );
            if params.supported_features & media_player::feature::SELECT_SOURCE != 0
                && let Some(sources) = &resolved_media_source_list
            {
                base_attrs.insert("source_list".into(), sources.clone());
            }
            if params.supported_features & media_player::feature::SELECT_SOUND_MODE != 0
                && let Some(modes) = &resolved_media_sound_mode_list
            {
                base_attrs.insert("sound_mode_list".into(), modes.clone());
            }
        } else if params.domain == "remote"
            || params.domain == "notify"
            || params.domain == "climate"
        {
            base_attrs.insert(
                "supported_features".into(),
                json!(params.supported_features),
            );
        }

        let entry = entries
            .get_mut(&params.entry_id)
            .expect("entry verified active above");
        // Remove any previous adapter if re-registered, keeping its live state
        // so metadata re-upserts do not reset availability/toggle/position
        // before the next `state.set` arrives.
        let prev_handle = entry.entities.remove(&params.unique_id).map(|old| {
            self.unregister_adapter(&old);
            old.handle
        });

        let handle = if disabled {
            if let Some(EntityHandle::Notify(old)) = &prev_handle {
                old.state.lock().active = false;
                let hass = self.components.hass();
                hass.restore().unregister(hass, old.entity_id.as_str());
            }
            EntityHandle::Disabled
        } else {
            match params.domain.as_str() {
                "switch" => {
                    let prev = match &prev_handle {
                        Some(EntityHandle::Switch(old)) => {
                            let s = old.state.read();
                            SwitchInner {
                                available: s.available,
                                is_on: s.is_on,
                            }
                        }
                        _ => SwitchInner {
                            available: true,
                            is_on: false,
                        },
                    };
                    let sw = Arc::new(PluginSwitch {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        state: RwLock::new(prev),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.switches().add(sw.clone());
                    EntityHandle::Switch(sw)
                }
                "light" => {
                    let caps = parse_light_caps(&params);
                    let min_k = params.min_color_temp_kelvin.unwrap_or(2000);
                    let max_k = params.max_color_temp_kelvin.unwrap_or(6535);
                    let prev = match &prev_handle {
                        Some(EntityHandle::Light(old)) => {
                            let s = old.state.read();
                            let mut light = s.light.clone();
                            light.min_color_temp_kelvin = min_k;
                            light.max_color_temp_kelvin = max_k;
                            if let Some(cm) = light.color_mode
                                && !is_valid_light_color_mode(cm, &caps, light.effect.as_deref())
                            {
                                light.color_mode = None;
                            }
                            LightInner {
                                available: s.available,
                                light,
                            }
                        }
                        _ => LightInner {
                            available: true,
                            light: LightState {
                                min_color_temp_kelvin: min_k,
                                max_color_temp_kelvin: max_k,
                                ..Default::default()
                            },
                        },
                    };
                    let light = Arc::new(PluginLight {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        caps,
                        state: RwLock::new(prev),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.lights().add(light.clone());
                    EntityHandle::Light(light)
                }
                "button" => {
                    let prev = match &prev_handle {
                        Some(EntityHandle::Button(old)) => {
                            let s = old.state.lock();
                            ButtonInner {
                                available: s.available,
                                last_pressed: s.last_pressed.clone(),
                            }
                        }
                        _ => ButtonInner {
                            available: true,
                            last_pressed: None,
                        },
                    };
                    let btn = Arc::new(PluginButton {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        hass: self.components.hass().clone(),
                        friendly_name: friendly_name.clone(),
                        state: Mutex::new(prev),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.buttons().add(btn.clone());
                    EntityHandle::Button(btn)
                }
                "scene" => {
                    let prev_avail = match &prev_handle {
                        Some(EntityHandle::Scene(old)) => *old.available.read(),
                        _ => true,
                    };
                    let scene = Arc::new(PluginScene {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        hass: self.components.hass().clone(),
                        friendly_name: friendly_name.clone(),
                        available: RwLock::new(prev_avail),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.scenes().add(scene.clone());
                    EntityHandle::Scene(scene)
                }
                "vacuum" => {
                    let prev_avail = match &prev_handle {
                        Some(EntityHandle::Vacuum(old)) => *old.available.read(),
                        _ => true,
                    };
                    let vac = Arc::new(PluginVacuum {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        supported_features: params.supported_features,
                        available: RwLock::new(prev_avail),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.vacuums().add(vac.clone());
                    EntityHandle::Vacuum(vac)
                }
                "select" => {
                    let (available, current_option) = match &prev_handle {
                        Some(EntityHandle::Select(old)) => {
                            let s = old.state.read();
                            (s.available, s.current_option.clone())
                        }
                        _ => (true, None),
                    };
                    let sel = Arc::new(PluginSelect {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        state: RwLock::new(SelectInner {
                            available,
                            options: params.options.clone().unwrap_or_default(),
                            current_option,
                        }),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.selects().add(sel.clone());
                    EntityHandle::Select(sel)
                }
                "number" => {
                    let prev_avail = match &prev_handle {
                        Some(EntityHandle::Number(old)) => old.state.read().available,
                        _ => true,
                    };
                    let min = params.min_value.unwrap_or(number::DEFAULT_MIN_VALUE);
                    let max = params.max_value.unwrap_or(number::DEFAULT_MAX_VALUE);
                    let step = number::calculate_step(min, max, params.step);
                    let mode = params.mode.clone().unwrap_or_else(|| "auto".into());
                    let num = Arc::new(PluginNumber {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        state: RwLock::new(NumberInner {
                            available: prev_avail,
                            min,
                            max,
                            step,
                            explicit_step: params.step,
                            mode,
                        }),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.numbers().add(num.clone());
                    EntityHandle::Number(num)
                }
                "cover" => {
                    let prev = match &prev_handle {
                        Some(EntityHandle::Cover(old)) => old.state.read().clone(),
                        _ => CoverInner {
                            available: true,
                            is_closed: None,
                            is_opening: None,
                            is_closing: None,
                            current_position: None,
                            current_tilt_position: None,
                            last_toggle_direction_open: true,
                        },
                    };
                    let cov = Arc::new(PluginCover {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        supported_features: params.supported_features,
                        state: RwLock::new(prev),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.covers().add(cov.clone());
                    EntityHandle::Cover(cov)
                }
                "lock" => {
                    let prev_avail = match &prev_handle {
                        Some(EntityHandle::Lock(old)) => old.state.read().available,
                        _ => true,
                    };
                    // Match HA's `LockEntity._async_read_entity_options`: accept
                    // `options.lock.default_code` when the lock's registration
                    // `code_format` matches it, then retain it even if a later
                    // state update clears `code_format`.
                    let default_code = lock_default_code.filter(|def| {
                        params
                            .code_format
                            .as_deref()
                            .is_some_and(|fmt| crate::lock::code_format_matches(fmt, def))
                    });
                    let lck = Arc::new(PluginLock {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        supported_features: params.supported_features,
                        state: RwLock::new(LockInner {
                            available: prev_avail,
                            code_format: params.code_format.clone(),
                            default_code,
                        }),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.locks().add(lck.clone());
                    EntityHandle::Lock(lck)
                }
                "fan" => {
                    let (available, is_on, percentage) = match &prev_handle {
                        Some(EntityHandle::Fan(old)) => {
                            let s = old.state.read();
                            (s.available, s.is_on, s.percentage)
                        }
                        _ => (true, false, None),
                    };
                    let f = Arc::new(PluginFan {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        supported_features: params.supported_features,
                        state: RwLock::new(FanInner {
                            available,
                            is_on,
                            percentage,
                            speed_count: params.speed_count.unwrap_or(100).max(1),
                            preset_modes: params.preset_modes.clone(),
                        }),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.fans().add(f.clone());
                    EntityHandle::Fan(f)
                }
                "media_player" => {
                    let (available, state, volume_level) = match &prev_handle {
                        Some(EntityHandle::MediaPlayer(old)) => {
                            let s = old.state.read();
                            (s.available, s.state, s.volume_level)
                        }
                        _ => (true, None, None),
                    };
                    let volume_step = params
                        .volume_step
                        .filter(|v| v.is_finite() && *v > 0.0)
                        .unwrap_or(0.1);
                    let mp = Arc::new(PluginMediaPlayer {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        supported_features: params.supported_features,
                        state: RwLock::new(MediaPlayerInner {
                            available,
                            state,
                            volume_level,
                            volume_step,
                            source_list: resolved_media_source_list,
                            sound_mode_list: resolved_media_sound_mode_list,
                        }),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.media_players().add(mp.clone());
                    EntityHandle::MediaPlayer(mp)
                }
                "remote" => {
                    let (available, is_on) = match &prev_handle {
                        Some(EntityHandle::Remote(old)) => {
                            let s = old.state.read();
                            (s.available, s.is_on)
                        }
                        _ => (true, false),
                    };
                    let rem = Arc::new(PluginRemote {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        supported_features: params.supported_features,
                        state: RwLock::new(RemoteInner { available, is_on }),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.remotes().add(rem.clone());
                    EntityHandle::Remote(rem)
                }
                "climate" => {
                    let inner = resolved_climate.expect("climate state built above");
                    let clim = Arc::new(PluginClimate {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        supported_features: params.supported_features,
                        state: RwLock::new(inner),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.climates().add(clim.clone());
                    EntityHandle::Climate(clim)
                }
                "notify" => {
                    let hass = self.components.hass();
                    let state = match &prev_handle {
                        Some(EntityHandle::Notify(old)) => {
                            let mut s = old.state.lock();
                            s.base_attrs = base_attrs.clone();
                            drop(s);
                            old.state.clone()
                        }
                        _ => {
                            let restore = hass.restore();
                            restore.register(entity_id.as_str(), None);
                            let restored = restore
                                .last_state(entity_id.as_str())
                                .and_then(|s| {
                                    s.get("state").and_then(Value::as_str).map(str::to_owned)
                                })
                                .filter(|s| s != "unavailable" && s != "unknown");
                            let initial_state =
                                restored.clone().unwrap_or_else(|| "unknown".into());
                            hass.set_state(entity_id.clone(), initial_state, base_attrs.clone());
                            Arc::new(Mutex::new(NotifyInner {
                                active: true,
                                available: true,
                                last_notified: restored,
                                base_attrs: base_attrs.clone(),
                            }))
                        }
                    };
                    let n = Arc::new(PluginNotify {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        supported_features: params.supported_features,
                        hass: hass.clone(),
                        state,
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.notifiers().add(n.clone());
                    EntityHandle::Notify(n)
                }
                _ => EntityHandle::Passive,
            }
        };

        let id_str = entity_id.as_str().to_owned();
        entry.entities.insert(
            params.unique_id,
            RegisteredEntity {
                entity_id,
                domain: params.domain,
                friendly_name,
                handle,
                base_attrs,
            },
        );
        Ok(id_str)
    }

    /// Write a state update reported by the plugin into `Hass` and sync the
    /// entity adapter's cached state. Starts fresh from registration-time
    /// `base_attrs` + current `params.attributes` so dropped keys never linger,
    /// and holds `self.entries` across `Hass::set_state` so deactivation
    /// cannot race with an in-flight update.
    pub fn set_state(&self, params: StateSetParams) -> Result<(), String> {
        let entries = self.entries.lock();
        let entry = entries.get(&params.entry_id).ok_or_else(|| {
            format!(
                "entry {:?} is not active for plugin {:?}",
                params.entry_id, self.domain
            )
        })?;
        let reg = entry.entities.get(&params.unique_id).ok_or_else(|| {
            format!(
                "unknown entity unique_id {:?} for entry {:?}",
                params.unique_id, params.entry_id
            )
        })?;
        let available = params.state != "unavailable";
        let mut merged = reg.base_attrs.clone();
        for (k, v) in &params.attributes {
            merged.insert(k.clone(), v.clone());
        }
        if let Some(name) = &reg.friendly_name {
            merged.entry("friendly_name").or_insert_with(|| json!(name));
        }
        let mut state_str = params.state;
        match &reg.handle {
            EntityHandle::Switch(sw) => {
                let mut inner = sw.state.write();
                inner.available = available;
                inner.is_on = state_str == "on";
            }
            EntityHandle::Light(light) => {
                let mut inner = light.state.write();
                inner.available = available;
                inner.light.on = state_str == "on";
                update_light_state(&mut inner.light, &light.caps, &params.attributes);
                let mut light_attrs =
                    crate::light::attributes(&light.caps, &inner.light, available);
                light_attrs.insert("supported_features".into(), json!(light.caps.features));
                for (k, v) in light_attrs {
                    merged.insert(k, v);
                }
            }
            EntityHandle::Button(btn) => {
                // Serialize availability/recovery writes with `Button::pressed`
                // so a recovery write can never overwrite a concurrent press.
                let mut inner = btn.state.lock();
                inner.available = available;
                if available
                    && state_str == "unknown"
                    && let Some(ts) = inner.last_pressed.clone()
                {
                    state_str = ts;
                }
                self.components
                    .hass()
                    .set_state(reg.entity_id.clone(), state_str, merged);
                return Ok(());
            }
            EntityHandle::Scene(scene) => {
                *scene.available.write() = available;
            }
            EntityHandle::Vacuum(vac) => {
                *vac.available.write() = available;
                merged.insert("supported_features".into(), json!(vac.supported_features));
            }
            EntityHandle::Select(sel) => {
                let mut inner = sel.state.write();
                inner.available = available;
                if let Some(arr) = params.attributes.get("options").and_then(Value::as_array) {
                    inner.options = arr
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                }
                inner.current_option = match state_str.as_str() {
                    "unknown" | "unavailable" => None,
                    other => Some(other.to_owned()),
                };
                merged.insert("options".into(), json!(inner.options));
            }
            EntityHandle::Number(num) => {
                let mut inner = num.state.write();
                inner.available = available;
                if let Some(min) = params.attributes.get("min").and_then(Value::as_f64) {
                    inner.min = min;
                }
                if let Some(max) = params.attributes.get("max").and_then(Value::as_f64) {
                    inner.max = max;
                }
                if let Some(step) = params.attributes.get("step").and_then(Value::as_f64) {
                    inner.explicit_step = Some(step);
                    inner.step = step;
                } else {
                    inner.step = number::calculate_step(inner.min, inner.max, inner.explicit_step);
                }
                if let Some(mode) = params.attributes.get("mode").and_then(Value::as_str) {
                    inner.mode = mode.to_owned();
                }
                merged.insert("min".into(), json!(inner.min));
                merged.insert("max".into(), json!(inner.max));
                merged.insert("step".into(), json!(inner.step));
                merged.insert("mode".into(), json!(inner.mode));
            }
            EntityHandle::Cover(cov) => {
                let mut inner = cov.state.write();
                inner.available = available;
                match state_str.as_str() {
                    "opening" => {
                        inner.is_opening = Some(true);
                        inner.is_closing = Some(false);
                        inner.is_closed = Some(false);
                        inner.last_toggle_direction_open = true;
                    }
                    "closing" => {
                        inner.is_opening = Some(false);
                        inner.is_closing = Some(true);
                        inner.is_closed = Some(false);
                        inner.last_toggle_direction_open = false;
                    }
                    "closed" => {
                        inner.is_opening = Some(false);
                        inner.is_closing = Some(false);
                        inner.is_closed = Some(true);
                    }
                    "open" => {
                        inner.is_opening = Some(false);
                        inner.is_closing = Some(false);
                        inner.is_closed = Some(false);
                    }
                    _ => {
                        inner.is_opening = None;
                        inner.is_closing = None;
                        inner.is_closed = None;
                    }
                }
                if let Some(c) = params.attributes.get("is_closed").and_then(Value::as_bool) {
                    inner.is_closed = Some(c);
                }
                inner.current_position = params
                    .attributes
                    .get("current_position")
                    .and_then(Value::as_u64)
                    .map(|p| p.min(100) as u8);
                inner.current_tilt_position = params
                    .attributes
                    .get("current_tilt_position")
                    .and_then(Value::as_u64)
                    .map(|p| p.min(100) as u8);
                merged.insert("supported_features".into(), json!(cov.supported_features));
            }
            EntityHandle::Lock(lck) => {
                let mut inner = lck.state.write();
                inner.available = available;
                if let Some(v) = params.attributes.get("code_format") {
                    inner.code_format = v.as_str().map(str::to_owned);
                }
                match &inner.code_format {
                    Some(fmt) => {
                        merged.insert("code_format".into(), json!(fmt));
                    }
                    None => {
                        merged.remove("code_format");
                    }
                }
                merged.insert("supported_features".into(), json!(lck.supported_features));
            }
            EntityHandle::Fan(f) => {
                let mut inner = f.state.write();
                inner.available = available;
                inner.is_on = state_str == "on";
                inner.percentage = params
                    .attributes
                    .get("percentage")
                    .and_then(Value::as_u64)
                    .map(|n| n.min(100) as u8);
                if let Some(v) = params.attributes.get("preset_modes") {
                    inner.preset_modes = v.as_array().map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    });
                }
                if f.supported_features & (fan::feature::SET_SPEED | fan::feature::PRESET_MODE) != 0
                {
                    merged.insert("preset_modes".into(), json!(inner.preset_modes));
                } else {
                    match &inner.preset_modes {
                        Some(modes) => {
                            merged.insert("preset_modes".into(), json!(modes));
                        }
                        None => {
                            merged.remove("preset_modes");
                        }
                    }
                }
                merged.insert("supported_features".into(), json!(f.supported_features));
            }
            EntityHandle::MediaPlayer(mp) => {
                let updated_caps = {
                    let mut inner = mp.state.write();
                    inner.available = available;
                    inner.state = available.then(|| MediaPlayerState::parse(&state_str));
                    inner.volume_level = available
                        .then(|| {
                            params
                                .attributes
                                .get("volume_level")
                                .and_then(Value::as_f64)
                        })
                        .flatten();
                    let mut caps_changed = false;
                    if let Some(v) = params.attributes.get("source_list")
                        && !same_string_list_val(&inner.source_list, v)
                    {
                        inner.source_list = normalize_string_list_val(v);
                        if mp.supported_features & media_player::feature::SELECT_SOURCE != 0 {
                            caps_changed = true;
                        }
                    }
                    if let Some(v) = params.attributes.get("sound_mode_list")
                        && !same_string_list_val(&inner.sound_mode_list, v)
                    {
                        inner.sound_mode_list = normalize_string_list_val(v);
                        if mp.supported_features & media_player::feature::SELECT_SOUND_MODE != 0 {
                            caps_changed = true;
                        }
                    }
                    if !available {
                        // HA omits state_attributes when unavailable, retaining
                        // base entity attributes + capability_attributes + supported_features.
                        merged = unavailable_base_attrs(reg, &params.attributes);
                    }
                    if mp.supported_features & media_player::feature::SELECT_SOURCE != 0 {
                        match &inner.source_list {
                            Some(sources) => {
                                if merged.get("source_list") != Some(sources) {
                                    merged.insert("source_list".into(), sources.clone());
                                }
                            }
                            None => {
                                merged.remove("source_list");
                            }
                        }
                    } else {
                        merged.remove("source_list");
                    }
                    if mp.supported_features & media_player::feature::SELECT_SOUND_MODE != 0 {
                        match &inner.sound_mode_list {
                            Some(modes) => {
                                if merged.get("sound_mode_list") != Some(modes) {
                                    merged.insert("sound_mode_list".into(), modes.clone());
                                }
                            }
                            None => {
                                merged.remove("sound_mode_list");
                            }
                        }
                    } else {
                        merged.remove("sound_mode_list");
                    }
                    merged.insert("supported_features".into(), json!(mp.supported_features));
                    caps_changed.then(|| {
                        media_player_capabilities(
                            mp.supported_features,
                            &inner.source_list,
                            &inner.sound_mode_list,
                        )
                    })
                };
                if let Some(new_caps) = updated_caps
                    && let Some(registry) = self.components.registry()
                {
                    registry.update_entity(
                        reg.entity_id.as_str(),
                        EntityPatch {
                            capabilities: Some(Some(new_caps)),
                            ..Default::default()
                        },
                    );
                }
            }
            EntityHandle::Remote(rem) => {
                let mut inner = rem.state.write();
                inner.available = available;
                inner.is_on = available && state_str == "on";
                if !available {
                    merged = unavailable_base_attrs(reg, &params.attributes);
                }
                merged.insert("supported_features".into(), json!(rem.supported_features));
            }
            EntityHandle::Climate(clim) => {
                let updated_caps = {
                    let mut inner = clim.state.write();
                    inner.available = available;
                    match state_str.as_str() {
                        "unknown" | "unavailable" => inner.hvac_mode = None,
                        other => {
                            if inner.hvac_mode.as_deref() != Some(other) {
                                inner.hvac_mode = Some(other.to_owned());
                            }
                        }
                    }
                    let mut caps_changed = false;
                    if let Some(t) = params.attributes.get("min_temp").and_then(temp_from_value)
                        && inner.min_temp != t
                    {
                        inner.min_temp = t;
                        caps_changed = true;
                    }
                    if let Some(t) = params.attributes.get("max_temp").and_then(temp_from_value)
                        && inner.max_temp != t
                    {
                        inner.max_temp = t;
                        caps_changed = true;
                    }
                    if let Some(raw) = params.attributes.get("target_temp_step") {
                        let next = match raw {
                            Value::Null => Some(None),
                            v => temp_from_value(v)
                                .filter(|t| t.get() >= 0.0)
                                .map(|t| (t.get() > 0.0).then_some(t)),
                        };
                        if let Some(t) = next
                            && inner.target_temp_step != t
                        {
                            inner.target_temp_step = t;
                            caps_changed = true;
                        }
                    }
                    let has_hum = clim.supported_features & climate::feature::TARGET_HUMIDITY != 0;
                    if let Some(t) = params
                        .attributes
                        .get("min_humidity")
                        .and_then(temp_from_value)
                        && inner.min_humidity != t
                    {
                        inner.min_humidity = t;
                        if has_hum {
                            caps_changed = true;
                        }
                    }
                    if let Some(t) = params
                        .attributes
                        .get("max_humidity")
                        .and_then(temp_from_value)
                        && inner.max_humidity != t
                    {
                        inner.max_humidity = t;
                        if has_hum {
                            caps_changed = true;
                        }
                    }
                    if let Some(raw) = params.attributes.get("target_humidity_step") {
                        let next = match raw {
                            Value::Null => Some(None),
                            v => temp_from_value(v).filter(|t| t.get() > 0.0).map(Some),
                        };
                        if let Some(t) = next
                            && inner.target_humidity_step != t
                        {
                            inner.target_humidity_step = t;
                            if has_hum {
                                caps_changed = true;
                            }
                        }
                    }
                    if let Some(arr) = params
                        .attributes
                        .get("hvac_modes")
                        .and_then(Value::as_array)
                        && !same_str_slice(&inner.hvac_modes, arr)
                    {
                        inner.hvac_modes = arr
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect();
                        caps_changed = true;
                    }
                    {
                        let ClimateInner {
                            fan_modes,
                            preset_modes,
                            swing_modes,
                            swing_horizontal_modes,
                            ..
                        } = &mut *inner;
                        for (key, slot, feat) in [
                            ("fan_modes", fan_modes, climate::feature::FAN_MODE),
                            ("preset_modes", preset_modes, climate::feature::PRESET_MODE),
                            ("swing_modes", swing_modes, climate::feature::SWING_MODE),
                            (
                                "swing_horizontal_modes",
                                swing_horizontal_modes,
                                climate::feature::SWING_HORIZONTAL_MODE,
                            ),
                        ] {
                            if let Some(v) = params.attributes.get(key)
                                && !same_opt_str_slice(slot, v)
                            {
                                *slot = v.as_array().map(|arr| {
                                    arr.iter()
                                        .filter_map(Value::as_str)
                                        .map(str::to_owned)
                                        .collect::<Vec<_>>()
                                });
                                if clim.supported_features & feat != 0 {
                                    caps_changed = true;
                                }
                            }
                        }
                    }
                    if caps_changed {
                        inner.cached_caps =
                            climate_capabilities_map(clim.supported_features, &inner);
                    }
                    if !available {
                        merged = unavailable_base_attrs(reg, &params.attributes);
                    } else {
                        if inner.target_temp_step.is_none() {
                            merged.remove("target_temp_step");
                        }
                        if !has_hum {
                            merged.remove("min_humidity");
                            merged.remove("max_humidity");
                            merged.remove("target_humidity_step");
                        } else if inner.target_humidity_step.is_none() {
                            merged.remove("target_humidity_step");
                        }
                        for (key, feat) in [
                            ("fan_modes", climate::feature::FAN_MODE),
                            ("preset_modes", climate::feature::PRESET_MODE),
                            ("swing_modes", climate::feature::SWING_MODE),
                            (
                                "swing_horizontal_modes",
                                climate::feature::SWING_HORIZONTAL_MODE,
                            ),
                        ] {
                            if clim.supported_features & feat == 0 {
                                merged.remove(key);
                            }
                        }
                    }
                    for (k, v) in &inner.cached_caps {
                        if merged.get(k) != Some(v) {
                            merged.insert(k.clone(), v.clone());
                        }
                    }
                    merged.insert("supported_features".into(), json!(clim.supported_features));
                    caps_changed.then(|| Value::Object(inner.cached_caps.clone()))
                };
                if let Some(new_caps) = updated_caps
                    && let Some(registry) = self.components.registry()
                {
                    registry.update_entity(
                        reg.entity_id.as_str(),
                        EntityPatch {
                            capabilities: Some(Some(new_caps)),
                            ..Default::default()
                        },
                    );
                }
            }
            EntityHandle::Notify(n) => {
                let mut inner = n.state.lock();
                inner.available = available;
                if !available {
                    merged = unavailable_base_attrs(reg, &params.attributes);
                } else if state_str == "unknown"
                    && let Some(ts) = inner.last_notified.clone()
                {
                    state_str = ts;
                } else if state_str != "unknown" {
                    inner.last_notified = Some(state_str.clone());
                }
                merged.insert("supported_features".into(), json!(n.supported_features));
                self.components
                    .hass()
                    .set_state(reg.entity_id.clone(), state_str, merged);
                return Ok(());
            }
            EntityHandle::Passive => {}
            EntityHandle::Disabled => return Ok(()),
        }
        self.components
            .hass()
            .set_state(reg.entity_id.clone(), state_str, merged);
        Ok(())
    }

    /// Deactivate every active entry owned by this plugin (used when a plugin
    /// is removed or replaced while entries are running). Holds `self.entries`
    /// across adapter and state removal so no concurrent notification can
    /// interleave.
    pub fn deactivate_all(&self) {
        let mut entries = self.entries.lock();
        let hass = self.components.hass();
        for (_, entry) in entries.drain() {
            for reg in entry.entities.values() {
                self.deactivate_entity(reg, hass);
            }
        }
    }

    /// Snapshot of `Components` bound to this store.
    pub fn components(&self) -> Arc<Components> {
        self.components.clone()
    }

    /// Mark every entity of every active entry `unavailable` (called when the
    /// plugin process crashes or disconnects). Preserves capability attributes,
    /// unit of measurement, icon, device class, and `supported_features`,
    /// matching HA's `Entity._async_calculate_state`. Holds `self.entries`
    /// across state writes so deactivation cannot interleave.
    pub fn mark_all_unavailable(&self) {
        let entries = self.entries.lock();
        let hass = self.components.hass();
        for entry in entries.values() {
            for reg in entry.entities.values() {
                if matches!(reg.handle, EntityHandle::Disabled) {
                    continue;
                }
                let mut attrs = reg.base_attrs.clone();
                if let Some(name) = &reg.friendly_name {
                    attrs.entry("friendly_name").or_insert_with(|| json!(name));
                }
                match &reg.handle {
                    EntityHandle::Switch(sw) => sw.state.write().available = false,
                    EntityHandle::Light(light) => {
                        let mut inner = light.state.write();
                        inner.available = false;
                        let mut light_attrs =
                            crate::light::attributes(&light.caps, &inner.light, false);
                        light_attrs.insert("supported_features".into(), json!(light.caps.features));
                        for (k, v) in light_attrs {
                            attrs.insert(k, v);
                        }
                    }
                    EntityHandle::Button(btn) => {
                        let mut inner = btn.state.lock();
                        inner.available = false;
                        hass.set_state(reg.entity_id.clone(), "unavailable", attrs);
                        continue;
                    }
                    EntityHandle::Scene(scene) => *scene.available.write() = false,
                    EntityHandle::Vacuum(vac) => *vac.available.write() = false,
                    EntityHandle::Select(sel) => {
                        let mut inner = sel.state.write();
                        inner.available = false;
                        attrs.insert("options".into(), json!(inner.options));
                    }
                    EntityHandle::Number(num) => {
                        let mut inner = num.state.write();
                        inner.available = false;
                        attrs.insert("min".into(), json!(inner.min));
                        attrs.insert("max".into(), json!(inner.max));
                        attrs.insert("step".into(), json!(inner.step));
                        attrs.insert("mode".into(), json!(inner.mode));
                    }
                    EntityHandle::Cover(cov) => cov.state.write().available = false,
                    EntityHandle::Lock(lck) => {
                        lck.state.write().available = false;
                        // `code_format` is a state_attribute in HA's LockEntity, omitted when unavailable.
                        attrs.remove("code_format");
                    }
                    EntityHandle::Fan(f) => {
                        let mut inner = f.state.write();
                        inner.available = false;
                        // `percentage_step` is a state_attribute in HA's FanEntity, omitted when unavailable;
                        // `preset_modes` is a capability_attribute retained when SET_SPEED or PRESET_MODE is set.
                        attrs.remove("percentage_step");
                        if f.supported_features
                            & (fan::feature::SET_SPEED | fan::feature::PRESET_MODE)
                            != 0
                        {
                            attrs.insert("preset_modes".into(), json!(inner.preset_modes));
                        } else {
                            attrs.remove("preset_modes");
                        }
                    }
                    EntityHandle::MediaPlayer(mp) => {
                        let mut inner = mp.state.write();
                        inner.available = false;
                        if mp.supported_features & media_player::feature::SELECT_SOURCE != 0
                            && let Some(sources) = &inner.source_list
                        {
                            attrs.insert("source_list".into(), sources.clone());
                        } else {
                            attrs.remove("source_list");
                        }
                        if mp.supported_features & media_player::feature::SELECT_SOUND_MODE != 0
                            && let Some(modes) = &inner.sound_mode_list
                        {
                            attrs.insert("sound_mode_list".into(), modes.clone());
                        } else {
                            attrs.remove("sound_mode_list");
                        }
                    }
                    EntityHandle::Remote(rem) => {
                        rem.state.write().available = false;
                    }
                    EntityHandle::Climate(clim) => {
                        let mut inner = clim.state.write();
                        inner.available = false;
                        for (k, v) in &inner.cached_caps {
                            attrs.insert(k.clone(), v.clone());
                        }
                    }
                    EntityHandle::Notify(n) => {
                        let mut inner = n.state.lock();
                        inner.available = false;
                        hass.set_state(reg.entity_id.clone(), "unavailable", attrs);
                        continue;
                    }
                    EntityHandle::Passive | EntityHandle::Disabled => {}
                }
                hass.set_state(reg.entity_id.clone(), "unavailable", attrs);
            }
        }
    }

    /// Remove all entities for `entry_id` from `Components` and `Hass`. Holds
    /// `self.entries` across removal so no accepted notification can publish
    /// after deactivation completes.
    pub fn deactivate_entry(&self, entry_id: &str) {
        let mut entries = self.entries.lock();
        let Some(entry) = entries.remove(entry_id) else {
            return;
        };
        let hass = self.components.hass();
        for reg in entry.entities.values() {
            self.deactivate_entity(reg, hass);
        }
    }

    fn deactivate_entity(&self, reg: &RegisteredEntity, hass: &ha_core::Hass) {
        if let EntityHandle::Notify(n) = &reg.handle {
            n.state.lock().active = false;
            hass.restore().unregister(hass, reg.entity_id.as_str());
        }
        self.unregister_adapter(reg);
        hass.remove_state(&reg.entity_id);
    }

    fn unregister_adapter(&self, reg: &RegisteredEntity) {
        if matches!(reg.handle, EntityHandle::Disabled) {
            return;
        }
        match reg.domain.as_str() {
            "switch" => self.components.switches().remove(&reg.entity_id),
            "light" => self.components.lights().remove(&reg.entity_id),
            "button" => self.components.buttons().remove(&reg.entity_id),
            "scene" => self.components.scenes().remove(&reg.entity_id),
            "vacuum" => self.components.vacuums().remove(&reg.entity_id),
            "select" => self.components.selects().remove(&reg.entity_id),
            "number" => self.components.numbers().remove(&reg.entity_id),
            "cover" => self.components.covers().remove(&reg.entity_id),
            "lock" => self.components.locks().remove(&reg.entity_id),
            "fan" => self.components.fans().remove(&reg.entity_id),
            "media_player" => self.components.media_players().remove(&reg.entity_id),
            "remote" => self.components.remotes().remove(&reg.entity_id),
            "climate" => self.components.climates().remove(&reg.entity_id),
            "notify" => self.components.notifiers().remove(&reg.entity_id),
            _ => {}
        }
    }
}

fn parse_color_mode(s: &str) -> Option<ColorMode> {
    match s {
        "onoff" => Some(ColorMode::Onoff),
        "brightness" => Some(ColorMode::Brightness),
        "color_temp" => Some(ColorMode::ColorTemp),
        "hs" => Some(ColorMode::Hs),
        "xy" => Some(ColorMode::Xy),
        "rgb" => Some(ColorMode::Rgb),
        "rgbw" => Some(ColorMode::Rgbw),
        "rgbww" => Some(ColorMode::Rgbww),
        "white" => Some(ColorMode::White),
        _ => None,
    }
}

fn parse_light_caps(params: &EntityUpsertParams) -> LightCaps {
    let parsed: Vec<ColorMode> = params
        .supported_color_modes
        .iter()
        .filter_map(|s| parse_color_mode(s))
        .collect();
    let modes = if parsed.is_empty() {
        Modes::new([ColorMode::Onoff])
    } else {
        Modes::filtered(parsed)
    };
    LightCaps {
        modes,
        features: params.supported_features,
        effect_list: params.effect_list.clone(),
    }
}

/// Mirror HA's `LightEntity.__validate_color_mode`: while an active effect
/// (not `None`/`""`/`"off"`) is running on a light with `FEATURE_EFFECT`,
/// `onoff` is always permitted and `brightness` is permitted whenever the
/// light supports brightness.
fn is_valid_light_color_mode(cm: ColorMode, caps: &LightCaps, effect: Option<&str>) -> bool {
    if caps.modes.contains(cm) {
        return true;
    }
    let effect_active = caps.features & crate::light::FEATURE_EFFECT != 0
        && effect.is_some_and(|e| !e.is_empty() && e != "off");
    if !effect_active {
        return false;
    }
    match cm {
        ColorMode::Onoff => true,
        ColorMode::Brightness => caps.modes.iter().any(|m| m != ColorMode::Onoff),
        _ => false,
    }
}

fn update_light_state(state: &mut LightState, caps: &LightCaps, attrs: &Map<String, Value>) {
    if let Some(m) = attrs.get("color_mode").and_then(Value::as_str) {
        state.color_mode = parse_color_mode(m);
    } else if state.on && state.color_mode.is_none() {
        state.color_mode = caps.modes.single().or_else(|| caps.modes.iter().next());
    }
    if let Some(b) = attrs.get("brightness").and_then(Value::as_i64) {
        state.brightness = Some(b);
    }
    if let Some(k) = attrs.get("color_temp_kelvin").and_then(Value::as_i64) {
        state.color_temp_kelvin = Some(k);
    }
    if let Some(arr) = attrs.get("hs_color").and_then(Value::as_array)
        && arr.len() == 2
        && let (Some(h), Some(s)) = (arr[0].as_f64(), arr[1].as_f64())
    {
        state.hs_color = Some((h, s));
    }
    if let Some(arr) = attrs.get("rgb_color").and_then(Value::as_array)
        && arr.len() == 3
        && let (Some(r), Some(g), Some(b)) = (arr[0].as_f64(), arr[1].as_f64(), arr[2].as_f64())
    {
        state.rgb_color = Some([r, g, b]);
    }
    if let Some(arr) = attrs.get("xy_color").and_then(Value::as_array)
        && arr.len() == 2
        && let (Some(x), Some(y)) = (arr[0].as_f64(), arr[1].as_f64())
    {
        state.xy_color = Some((x, y));
    }
    if let Some(e) = attrs.get("effect").and_then(Value::as_str) {
        state.effect = Some(e.to_owned());
    }
}

struct SwitchInner {
    available: bool,
    is_on: bool,
}

struct PluginSwitch {
    entity_id: EntityId,
    entry_id: String,
    unique_id: String,
    state: RwLock<SwitchInner>,
    call_plugin: CallPluginFn,
}

impl Switch for PluginSwitch {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn available(&self) -> bool {
        self.state.read().available
    }

    fn is_on(&self) -> bool {
        self.state.read().is_on
    }

    fn turn(&self, on: bool) -> BoxFuture<'_> {
        let params = EntityCallParams {
            entry_id: self.entry_id.clone(),
            unique_id: self.unique_id.clone(),
            domain: "switch".into(),
            service: if on { "turn_on" } else { "turn_off" }.into(),
            data: Map::new(),
        };
        let call = (self.call_plugin)(params);
        Box::pin(call)
    }
}

struct LightInner {
    available: bool,
    light: LightState,
}

struct PluginLight {
    entity_id: EntityId,
    entry_id: String,
    unique_id: String,
    caps: LightCaps,
    state: RwLock<LightInner>,
    call_plugin: CallPluginFn,
}

impl Light for PluginLight {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn capabilities(&self) -> LightCaps {
        self.caps.clone()
    }

    fn state(&self) -> LightState {
        self.state.read().light.clone()
    }

    fn available(&self) -> bool {
        self.state.read().available
    }

    fn turn_on(&self, params: Params) -> BoxFuture<'_> {
        let mut data = Map::new();
        if let Some(b) = params.brightness {
            data.insert("brightness".into(), json!(b));
        }
        if let Some(k) = params.color_temp_kelvin {
            data.insert("color_temp_kelvin".into(), json!(k));
        }
        if let Some((h, s)) = params.hs_color {
            data.insert("hs_color".into(), json!([h, s]));
        }
        if let Some(rgb) = params.rgb_color {
            data.insert("rgb_color".into(), json!(rgb));
        }
        if let Some(rgbw) = params.rgbw_color {
            data.insert("rgbw_color".into(), json!(rgbw));
        }
        if let Some(rgbww) = params.rgbww_color {
            data.insert("rgbww_color".into(), json!(rgbww));
        }
        if let Some((x, y)) = params.xy_color {
            data.insert("xy_color".into(), json!([x, y]));
        }
        if let Some(t) = params.transition {
            data.insert("transition".into(), json!(t));
        }
        if let Some(e) = params.effect {
            data.insert("effect".into(), json!(e));
        }
        if let Some(f) = params.flash {
            data.insert(
                "flash".into(),
                json!(match f {
                    Flash::Short => "short",
                    Flash::Long => "long",
                }),
            );
        }
        if let Some(w) = params.white {
            let v = match w {
                White::True => json!(true),
                White::Level(Some(l)) => json!(l),
                White::Level(None) => json!(true),
            };
            data.insert("white".into(), v);
        }
        let call = (self.call_plugin)(EntityCallParams {
            entry_id: self.entry_id.clone(),
            unique_id: self.unique_id.clone(),
            domain: "light".into(),
            service: "turn_on".into(),
            data,
        });
        Box::pin(call)
    }

    fn turn_off(&self, params: TurnOff) -> BoxFuture<'_> {
        let mut data = Map::new();
        if let Some(t) = params.transition {
            data.insert("transition".into(), json!(t));
        }
        if let Some(f) = params.flash {
            data.insert(
                "flash".into(),
                json!(match f {
                    Flash::Short => "short",
                    Flash::Long => "long",
                }),
            );
        }
        let call = (self.call_plugin)(EntityCallParams {
            entry_id: self.entry_id.clone(),
            unique_id: self.unique_id.clone(),
            domain: "light".into(),
            service: "turn_off".into(),
            data,
        });
        Box::pin(call)
    }
}

struct ButtonInner {
    available: bool,
    last_pressed: Option<String>,
}

struct PluginButton {
    entity_id: EntityId,
    entry_id: String,
    unique_id: String,
    hass: Hass,
    friendly_name: Option<String>,
    state: Mutex<ButtonInner>,
    call_plugin: CallPluginFn,
}

impl Button for PluginButton {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn available(&self) -> bool {
        self.state.lock().available
    }

    fn pressed(&self, at: String, context: &Context) {
        let mut inner = self.state.lock();
        inner.last_pressed = Some(at.clone());
        let mut attrs = Attributes::new();
        if let Some(name) = &self.friendly_name {
            attrs.insert("friendly_name".into(), json!(name));
        }
        self.hass
            .set_state_with_context(self.entity_id.clone(), at, attrs, Some(context.clone()));
    }

    fn press(&self) -> BoxFuture<'_> {
        let call = (self.call_plugin)(EntityCallParams {
            entry_id: self.entry_id.clone(),
            unique_id: self.unique_id.clone(),
            domain: "button".into(),
            service: "press".into(),
            data: Map::new(),
        });
        Box::pin(call)
    }
}

struct PluginScene {
    entity_id: EntityId,
    entry_id: String,
    unique_id: String,
    hass: Hass,
    friendly_name: Option<String>,
    available: RwLock<bool>,
    call_plugin: CallPluginFn,
}

impl Scene for PluginScene {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn available(&self) -> bool {
        *self.available.read()
    }

    fn activate(&self, transition: Option<f64>) -> BoxFuture<'_> {
        let now = ha_core::isoformat(self.hass.clock().now());
        let mut attrs = Attributes::new();
        if let Some(name) = &self.friendly_name {
            attrs.insert("friendly_name".into(), json!(name));
        }
        self.hass.set_state(self.entity_id.clone(), now, attrs);
        let mut data = Map::new();
        if let Some(t) = transition {
            data.insert("transition".into(), json!(t));
        }
        let call = (self.call_plugin)(EntityCallParams {
            entry_id: self.entry_id.clone(),
            unique_id: self.unique_id.clone(),
            domain: "scene".into(),
            service: "turn_on".into(),
            data,
        });
        Box::pin(call)
    }
}

struct PluginVacuum {
    entity_id: EntityId,
    entry_id: String,
    unique_id: String,
    supported_features: u32,
    available: RwLock<bool>,
    call_plugin: CallPluginFn,
}

impl PluginVacuum {
    fn invoke(&self, service: &str, data: Map<String, Value>) -> BoxFuture<'_> {
        let call = (self.call_plugin)(EntityCallParams {
            entry_id: self.entry_id.clone(),
            unique_id: self.unique_id.clone(),
            domain: "vacuum".into(),
            service: service.into(),
            data,
        });
        Box::pin(call)
    }
}

impl Vacuum for PluginVacuum {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn available(&self) -> bool {
        *self.available.read()
    }

    fn supported_features(&self) -> u32 {
        self.supported_features
    }

    fn start(&self) -> BoxFuture<'_> {
        self.invoke("start", Map::new())
    }

    fn pause(&self) -> BoxFuture<'_> {
        self.invoke("pause", Map::new())
    }

    fn stop(&self) -> BoxFuture<'_> {
        self.invoke("stop", Map::new())
    }

    fn return_to_base(&self) -> BoxFuture<'_> {
        self.invoke("return_to_base", Map::new())
    }

    fn clean_spot(&self) -> BoxFuture<'_> {
        self.invoke("clean_spot", Map::new())
    }

    fn locate(&self) -> BoxFuture<'_> {
        self.invoke("locate", Map::new())
    }

    fn set_fan_speed(&self, fan_speed: &str) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("fan_speed".into(), json!(fan_speed));
        self.invoke("set_fan_speed", data)
    }

    fn send_command(&self, command: &str, params: Option<Value>) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("command".into(), json!(command));
        if let Some(params) = params {
            data.insert("params".into(), params);
        }
        self.invoke("send_command", data)
    }
}

struct SelectInner {
    available: bool,
    options: Vec<String>,
    current_option: Option<String>,
}

struct PluginSelect {
    entity_id: EntityId,
    entry_id: String,
    unique_id: String,
    state: RwLock<SelectInner>,
    call_plugin: CallPluginFn,
}

impl Select for PluginSelect {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn available(&self) -> bool {
        self.state.read().available
    }

    fn options(&self) -> Vec<String> {
        self.state.read().options.clone()
    }

    fn current_option(&self) -> Option<String> {
        self.state.read().current_option.clone()
    }

    fn select_option(&self, option: &str) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("option".into(), json!(option));
        let call = (self.call_plugin)(EntityCallParams {
            entry_id: self.entry_id.clone(),
            unique_id: self.unique_id.clone(),
            domain: "select".into(),
            service: "select_option".into(),
            data,
        });
        Box::pin(call)
    }
}

struct NumberInner {
    available: bool,
    min: f64,
    max: f64,
    step: f64,
    explicit_step: Option<f64>,
    mode: String,
}

struct PluginNumber {
    entity_id: EntityId,
    entry_id: String,
    unique_id: String,
    state: RwLock<NumberInner>,
    call_plugin: CallPluginFn,
}

impl Number for PluginNumber {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn available(&self) -> bool {
        self.state.read().available
    }

    fn min_value(&self) -> f64 {
        self.state.read().min
    }

    fn max_value(&self) -> f64 {
        self.state.read().max
    }

    fn bounds(&self) -> (f64, f64) {
        let s = self.state.read();
        (s.min, s.max)
    }

    fn step(&self) -> f64 {
        self.state.read().step
    }

    fn set_value(&self, value: f64) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("value".into(), json!(value));
        let call = (self.call_plugin)(EntityCallParams {
            entry_id: self.entry_id.clone(),
            unique_id: self.unique_id.clone(),
            domain: "number".into(),
            service: "set_value".into(),
            data,
        });
        Box::pin(call)
    }
}

#[derive(Clone)]
struct CoverInner {
    available: bool,
    is_closed: Option<bool>,
    is_opening: Option<bool>,
    is_closing: Option<bool>,
    current_position: Option<u8>,
    current_tilt_position: Option<u8>,
    last_toggle_direction_open: bool,
}

struct PluginCover {
    entity_id: EntityId,
    entry_id: String,
    unique_id: String,
    supported_features: u32,
    state: RwLock<CoverInner>,
    call_plugin: CallPluginFn,
}

impl PluginCover {
    fn invoke(&self, service: &str, data: Map<String, Value>) -> BoxFuture<'_> {
        let call = (self.call_plugin)(EntityCallParams {
            entry_id: self.entry_id.clone(),
            unique_id: self.unique_id.clone(),
            domain: "cover".into(),
            service: service.into(),
            data,
        });
        Box::pin(call)
    }
}

impl Cover for PluginCover {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn available(&self) -> bool {
        self.state.read().available
    }

    fn supported_features(&self) -> u32 {
        self.supported_features
    }

    fn is_closed(&self) -> Option<bool> {
        self.state.read().is_closed
    }

    fn is_opening(&self) -> Option<bool> {
        self.state.read().is_opening
    }

    fn is_closing(&self) -> Option<bool> {
        self.state.read().is_closing
    }

    fn current_position(&self) -> Option<u8> {
        self.state.read().current_position
    }

    fn current_tilt_position(&self) -> Option<u8> {
        self.state.read().current_tilt_position
    }

    fn last_toggle_direction_open(&self) -> bool {
        self.state.read().last_toggle_direction_open
    }

    fn open_cover(&self) -> BoxFuture<'_> {
        self.invoke("open_cover", Map::new())
    }

    fn close_cover(&self) -> BoxFuture<'_> {
        self.invoke("close_cover", Map::new())
    }

    fn stop_cover(&self) -> BoxFuture<'_> {
        self.invoke("stop_cover", Map::new())
    }

    fn set_cover_position(&self, position: u8) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("position".into(), json!(position));
        self.invoke("set_cover_position", data)
    }

    fn open_cover_tilt(&self) -> BoxFuture<'_> {
        self.invoke("open_cover_tilt", Map::new())
    }

    fn close_cover_tilt(&self) -> BoxFuture<'_> {
        self.invoke("close_cover_tilt", Map::new())
    }

    fn stop_cover_tilt(&self) -> BoxFuture<'_> {
        self.invoke("stop_cover_tilt", Map::new())
    }

    fn set_cover_tilt_position(&self, tilt_position: u8) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("tilt_position".into(), json!(tilt_position));
        self.invoke("set_cover_tilt_position", data)
    }
}

struct LockInner {
    available: bool,
    code_format: Option<String>,
    default_code: Option<String>,
}

struct PluginLock {
    entity_id: EntityId,
    entry_id: String,
    unique_id: String,
    supported_features: u32,
    state: RwLock<LockInner>,
    call_plugin: CallPluginFn,
}

impl PluginLock {
    fn invoke(&self, service: &str, code: Option<&str>) -> BoxFuture<'_> {
        let mut data = Map::new();
        if let Some(code) = code {
            data.insert("code".into(), json!(code));
        }
        let call = (self.call_plugin)(EntityCallParams {
            entry_id: self.entry_id.clone(),
            unique_id: self.unique_id.clone(),
            domain: "lock".into(),
            service: service.into(),
            data,
        });
        Box::pin(call)
    }
}

impl Lock for PluginLock {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn available(&self) -> bool {
        self.state.read().available
    }

    fn supported_features(&self) -> u32 {
        self.supported_features
    }

    fn code_format(&self) -> Option<String> {
        self.state.read().code_format.clone()
    }

    fn default_code(&self) -> Option<String> {
        self.state.read().default_code.clone()
    }

    fn lock(&self, code: Option<&str>) -> BoxFuture<'_> {
        self.invoke("lock", code)
    }

    fn unlock(&self, code: Option<&str>) -> BoxFuture<'_> {
        self.invoke("unlock", code)
    }

    fn open(&self, code: Option<&str>) -> BoxFuture<'_> {
        self.invoke("open", code)
    }
}

struct FanInner {
    available: bool,
    is_on: bool,
    percentage: Option<u8>,
    speed_count: u32,
    preset_modes: Option<Vec<String>>,
}

struct PluginFan {
    entity_id: EntityId,
    entry_id: String,
    unique_id: String,
    supported_features: u32,
    state: RwLock<FanInner>,
    call_plugin: CallPluginFn,
}

impl PluginFan {
    fn invoke(&self, service: &str, data: Map<String, Value>) -> BoxFuture<'_> {
        let call = (self.call_plugin)(EntityCallParams {
            entry_id: self.entry_id.clone(),
            unique_id: self.unique_id.clone(),
            domain: "fan".into(),
            service: service.into(),
            data,
        });
        Box::pin(call)
    }
}

impl Fan for PluginFan {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn available(&self) -> bool {
        self.state.read().available
    }

    fn supported_features(&self) -> u32 {
        self.supported_features
    }

    fn is_on(&self) -> bool {
        self.state.read().is_on
    }

    fn percentage(&self) -> Option<u8> {
        self.state.read().percentage
    }

    fn speed_count(&self) -> u32 {
        self.state.read().speed_count
    }

    fn preset_modes(&self) -> Vec<String> {
        self.state.read().preset_modes.clone().unwrap_or_default()
    }

    fn turn_on(&self, percentage: Option<u8>, preset_mode: Option<&str>) -> BoxFuture<'_> {
        let mut data = Map::new();
        if let Some(p) = percentage {
            data.insert("percentage".into(), json!(p));
        }
        if let Some(pm) = preset_mode {
            data.insert("preset_mode".into(), json!(pm));
        }
        self.invoke("turn_on", data)
    }

    fn turn_off(&self) -> BoxFuture<'_> {
        self.invoke("turn_off", Map::new())
    }

    fn set_percentage(&self, percentage: u8) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("percentage".into(), json!(percentage));
        self.invoke("set_percentage", data)
    }

    fn set_preset_mode(&self, preset_mode: &str) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("preset_mode".into(), json!(preset_mode));
        self.invoke("set_preset_mode", data)
    }

    fn oscillate(&self, oscillating: bool) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("oscillating".into(), json!(oscillating));
        self.invoke("oscillate", data)
    }

    fn set_direction(&self, direction: &str) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("direction".into(), json!(direction));
        self.invoke("set_direction", data)
    }
}

fn normalize_string_list_vec(list: &[String]) -> Option<Value> {
    if list.is_empty() {
        None
    } else {
        Some(json!(list))
    }
}

fn normalize_string_list_val(v: &Value) -> Option<Value> {
    let arr = v.as_array()?;
    if arr.is_empty() {
        return None;
    }
    if arr.iter().all(Value::is_string) {
        return Some(v.clone());
    }
    let filtered: Vec<Value> = arr.iter().filter(|x| x.is_string()).cloned().collect();
    (!filtered.is_empty()).then_some(Value::Array(filtered))
}

fn same_string_list_val(cached: &Option<Value>, incoming: &Value) -> bool {
    match incoming {
        Value::Array(arr) if arr.iter().all(Value::is_string) => {
            if arr.is_empty() {
                cached.is_none()
            } else {
                cached.as_ref() == Some(incoming)
            }
        }
        Value::Null => cached.is_none(),
        _ => cached == &normalize_string_list_val(incoming),
    }
}

fn is_py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

/// Build the attribute map for an explicit `state.set("unavailable", ...)`
/// frame, mirroring HA's `Entity._async_calculate_state` when `not available`:
/// keep registration `base_attrs`, `friendly_name`, and any incoming base entity
/// attributes (`unit_of_measurement`, `assumed_state` when Python-truthy,
/// `attribution`, `device_class`, `entity_picture`, `icon`, `friendly_name`),
/// while dropping live `state_attributes` and `extra_state_attributes`.
fn unavailable_base_attrs(reg: &RegisteredEntity, incoming: &Map<String, Value>) -> Attributes {
    const BASE_ENTITY_KEYS: &[&str] = &[
        "unit_of_measurement",
        "assumed_state",
        "attribution",
        "device_class",
        "entity_picture",
        "icon",
        "friendly_name",
    ];
    let mut out = reg.base_attrs.clone();
    for &key in BASE_ENTITY_KEYS {
        if let Some(v) = incoming.get(key)
            && !v.is_null()
            && !(key == "assumed_state" && !is_py_truthy(v))
        {
            out.insert(key.into(), v.clone());
        }
    }
    if let Some(name) = &reg.friendly_name {
        out.entry("friendly_name").or_insert_with(|| json!(name));
    }
    out
}

fn media_player_capabilities(
    supported_features: u32,
    source_list: &Option<Value>,
    sound_mode_list: &Option<Value>,
) -> Value {
    let mut caps = Map::new();
    if supported_features & media_player::feature::SELECT_SOURCE != 0
        && let Some(sources) = source_list
    {
        caps.insert("source_list".into(), sources.clone());
    }
    if supported_features & media_player::feature::SELECT_SOUND_MODE != 0
        && let Some(modes) = sound_mode_list
    {
        caps.insert("sound_mode_list".into(), modes.clone());
    }
    Value::Object(caps)
}

struct MediaPlayerInner {
    available: bool,
    state: Option<MediaPlayerState>,
    volume_level: Option<f64>,
    volume_step: f64,
    source_list: Option<Value>,
    sound_mode_list: Option<Value>,
}

struct PluginMediaPlayer {
    entity_id: EntityId,
    entry_id: String,
    unique_id: String,
    supported_features: u32,
    state: RwLock<MediaPlayerInner>,
    call_plugin: CallPluginFn,
}

impl PluginMediaPlayer {
    fn invoke(&self, service: &str, data: Map<String, Value>) -> BoxFuture<'_> {
        let call = (self.call_plugin)(EntityCallParams {
            entry_id: self.entry_id.clone(),
            unique_id: self.unique_id.clone(),
            domain: "media_player".into(),
            service: service.into(),
            data,
        });
        Box::pin(call)
    }
}

impl MediaPlayer for PluginMediaPlayer {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn available(&self) -> bool {
        self.state.read().available
    }

    fn supported_features(&self) -> u32 {
        self.supported_features
    }

    fn state(&self) -> Option<MediaPlayerState> {
        self.state.read().state
    }

    fn volume_info(&self) -> (Option<f64>, f64) {
        let s = self.state.read();
        (s.volume_level, s.volume_step)
    }

    fn turn_on(&self) -> BoxFuture<'_> {
        self.invoke("turn_on", Map::new())
    }

    fn turn_off(&self) -> BoxFuture<'_> {
        self.invoke("turn_off", Map::new())
    }

    fn volume_up(&self) -> BoxFuture<'_> {
        if self.supported_features & media_player::feature::VOLUME_STEP != 0 {
            return self.invoke("volume_up", Map::new());
        }
        let (level, step) = self.volume_info();
        if let Some(v) = level
            && v < 1.0
            && (self.supported_features & media_player::feature::VOLUME_SET) != 0
        {
            return self.set_volume_level((v + step).min(1.0));
        }
        Box::pin(async { Ok(()) })
    }

    fn volume_down(&self) -> BoxFuture<'_> {
        if self.supported_features & media_player::feature::VOLUME_STEP != 0 {
            return self.invoke("volume_down", Map::new());
        }
        let (level, step) = self.volume_info();
        if let Some(v) = level
            && v > 0.0
            && (self.supported_features & media_player::feature::VOLUME_SET) != 0
        {
            return self.set_volume_level((v - step).max(0.0));
        }
        Box::pin(async { Ok(()) })
    }

    fn set_volume_level(&self, volume: f64) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("volume_level".into(), json!(volume));
        self.invoke("volume_set", data)
    }

    fn mute_volume(&self, mute: bool) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("is_volume_muted".into(), json!(mute));
        self.invoke("volume_mute", data)
    }

    fn media_play(&self) -> BoxFuture<'_> {
        self.invoke("media_play", Map::new())
    }

    fn media_pause(&self) -> BoxFuture<'_> {
        self.invoke("media_pause", Map::new())
    }

    fn media_stop(&self) -> BoxFuture<'_> {
        self.invoke("media_stop", Map::new())
    }

    fn media_next_track(&self) -> BoxFuture<'_> {
        self.invoke("media_next_track", Map::new())
    }

    fn media_previous_track(&self) -> BoxFuture<'_> {
        self.invoke("media_previous_track", Map::new())
    }

    fn media_seek(&self, position: f64) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("seek_position".into(), json!(position));
        self.invoke("media_seek", data)
    }

    fn play_media(&self, params: PlayMedia) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("media_content_type".into(), json!(params.media_type));
        data.insert("media_content_id".into(), json!(params.media_id));
        if let Some(enqueue) = params.enqueue {
            data.insert("enqueue".into(), json!(enqueue));
        }
        if let Some(announce) = params.announce {
            data.insert("announce".into(), json!(announce));
        }
        data.insert("extra".into(), Value::Object(params.extra));
        self.invoke("play_media", data)
    }

    fn select_source(&self, source: &str) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("source".into(), json!(source));
        self.invoke("select_source", data)
    }

    fn select_sound_mode(&self, sound_mode: &str) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("sound_mode".into(), json!(sound_mode));
        self.invoke("select_sound_mode", data)
    }

    fn set_shuffle(&self, shuffle: bool) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("shuffle".into(), json!(shuffle));
        self.invoke("shuffle_set", data)
    }

    fn set_repeat(&self, repeat: &str) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("repeat".into(), json!(repeat));
        self.invoke("repeat_set", data)
    }

    fn join_players(&self, group_members: &[String]) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("group_members".into(), json!(group_members));
        self.invoke("join", data)
    }

    fn unjoin_player(&self) -> BoxFuture<'_> {
        self.invoke("unjoin", Map::new())
    }

    fn clear_playlist(&self) -> BoxFuture<'_> {
        self.invoke("clear_playlist", Map::new())
    }
}

struct RemoteInner {
    available: bool,
    is_on: bool,
}

struct PluginRemote {
    entity_id: EntityId,
    entry_id: String,
    unique_id: String,
    supported_features: u32,
    state: RwLock<RemoteInner>,
    call_plugin: CallPluginFn,
}

impl PluginRemote {
    fn invoke(&self, service: &str, data: Map<String, Value>) -> BoxFuture<'_> {
        let call = (self.call_plugin)(EntityCallParams {
            entry_id: self.entry_id.clone(),
            unique_id: self.unique_id.clone(),
            domain: "remote".into(),
            service: service.into(),
            data,
        });
        Box::pin(call)
    }
}

impl Remote for PluginRemote {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn available(&self) -> bool {
        self.state.read().available
    }

    fn is_on(&self) -> bool {
        self.state.read().is_on
    }

    fn turn_on(&self, activity: Option<&str>) -> BoxFuture<'_> {
        let mut data = Map::new();
        if let Some(a) = activity {
            data.insert("activity".into(), json!(a));
        }
        self.invoke("turn_on", data)
    }

    fn turn_off(&self, activity: Option<&str>) -> BoxFuture<'_> {
        let mut data = Map::new();
        if let Some(a) = activity {
            data.insert("activity".into(), json!(a));
        }
        self.invoke("turn_off", data)
    }

    fn send_command(&self, cmd: SendCommand) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("command".into(), json!(cmd.command));
        if let Some(device) = cmd.device {
            data.insert("device".into(), json!(device));
        }
        data.insert("num_repeats".into(), json!(cmd.num_repeats));
        if let Some(delay) = cmd.delay_secs {
            data.insert("delay_secs".into(), json!(delay));
        }
        data.insert("hold_secs".into(), json!(cmd.hold_secs));
        self.invoke("send_command", data)
    }

    fn learn_command(&self, cmd: LearnCommand) -> BoxFuture<'_> {
        let mut data = Map::new();
        if let Some(device) = cmd.device {
            data.insert("device".into(), json!(device));
        }
        if let Some(command) = cmd.command {
            data.insert("command".into(), json!(command));
        }
        if let Some(ct) = cmd.command_type {
            data.insert("command_type".into(), json!(ct));
        }
        if let Some(alt) = cmd.alternative {
            data.insert("alternative".into(), json!(alt));
        }
        if let Some(timeout) = cmd.timeout {
            data.insert("timeout".into(), json!(timeout));
        }
        self.invoke("learn_command", data)
    }

    fn delete_command(&self, cmd: DeleteCommand) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("command".into(), json!(cmd.command));
        if let Some(device) = cmd.device {
            data.insert("device".into(), json!(device));
        }
        self.invoke("delete_command", data)
    }
}

fn same_str_slice(existing: &[String], arr: &[Value]) -> bool {
    let mut iter = arr.iter().filter_map(Value::as_str);
    existing
        .iter()
        .all(|s| iter.next().is_some_and(|v| v == s.as_str()))
        && iter.next().is_none()
}

fn same_opt_str_slice(existing: &Option<Vec<String>>, incoming: &Value) -> bool {
    match (existing, incoming.as_array()) {
        (Some(list), Some(arr)) => same_str_slice(list, arr),
        (None, None) => true,
        _ => false,
    }
}

fn temp_from_number(n: &serde_json::Number) -> Option<Temp> {
    if let Some(i) = n.as_i64() {
        Some(Temp::Int(i))
    } else if let Some(u) = n.as_u64() {
        i64::try_from(u)
            .map(Temp::Int)
            .ok()
            .or(Some(Temp::Float(u as f64)))
    } else {
        n.as_f64().filter(|f| f.is_finite()).map(Temp::Float)
    }
}

fn temp_from_value(v: &Value) -> Option<Temp> {
    v.as_number().and_then(temp_from_number)
}

fn temp_json(t: Temp) -> Value {
    match t {
        Temp::Int(i) => json!(i),
        Temp::Float(f) => json!(f),
    }
}

fn climate_capabilities_map(supported_features: u32, c: &ClimateInner) -> Map<String, Value> {
    let mut caps = Map::new();
    caps.insert("hvac_modes".into(), json!(c.hvac_modes));
    caps.insert(
        "min_temp".into(),
        climate::display_temp(Some(c.min_temp), climate::Precision::Tenths),
    );
    caps.insert(
        "max_temp".into(),
        climate::display_temp(Some(c.max_temp), climate::Precision::Tenths),
    );
    if let Some(step) = c.target_temp_step.filter(|s| s.get() != 0.0) {
        caps.insert("target_temp_step".into(), temp_json(step));
    }
    if supported_features & climate::feature::TARGET_HUMIDITY != 0 {
        caps.insert("min_humidity".into(), temp_json(c.min_humidity));
        caps.insert("max_humidity".into(), temp_json(c.max_humidity));
        if let Some(step) = c.target_humidity_step {
            caps.insert("target_humidity_step".into(), temp_json(step));
        }
    }
    if supported_features & climate::feature::FAN_MODE != 0 {
        caps.insert("fan_modes".into(), json!(c.fan_modes));
    }
    if supported_features & climate::feature::PRESET_MODE != 0 {
        caps.insert("preset_modes".into(), json!(c.preset_modes));
    }
    if supported_features & climate::feature::SWING_MODE != 0 {
        caps.insert("swing_modes".into(), json!(c.swing_modes));
    }
    if supported_features & climate::feature::SWING_HORIZONTAL_MODE != 0 {
        caps.insert(
            "swing_horizontal_modes".into(),
            json!(c.swing_horizontal_modes),
        );
    }
    caps
}

#[derive(Clone)]
struct ClimateInner {
    available: bool,
    hvac_mode: Option<String>,
    min_temp: Temp,
    max_temp: Temp,
    target_temp_step: Option<Temp>,
    min_humidity: Temp,
    max_humidity: Temp,
    target_humidity_step: Option<Temp>,
    hvac_modes: Vec<String>,
    fan_modes: Option<Vec<String>>,
    preset_modes: Option<Vec<String>>,
    swing_modes: Option<Vec<String>>,
    swing_horizontal_modes: Option<Vec<String>>,
    cached_caps: Map<String, Value>,
}

struct PluginClimate {
    entity_id: EntityId,
    entry_id: String,
    unique_id: String,
    supported_features: u32,
    state: RwLock<ClimateInner>,
    call_plugin: CallPluginFn,
}

impl PluginClimate {
    fn invoke(&self, service: &str, data: Map<String, Value>) -> BoxFuture<'_> {
        let call = (self.call_plugin)(EntityCallParams {
            entry_id: self.entry_id.clone(),
            unique_id: self.unique_id.clone(),
            domain: "climate".into(),
            service: service.into(),
            data,
        });
        Box::pin(call)
    }
}

impl Climate for PluginClimate {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn available(&self) -> bool {
        self.state.read().available
    }

    fn supported_features(&self) -> u32 {
        self.supported_features
    }

    fn hvac_mode(&self) -> Option<String> {
        self.state.read().hvac_mode.clone()
    }

    fn hvac_modes(&self) -> &[&'static str] {
        &[]
    }

    fn hvac_modes_vec(&self) -> Vec<String> {
        self.state.read().hvac_modes.clone()
    }

    fn fan_modes_vec(&self) -> Vec<String> {
        self.state.read().fan_modes.clone().unwrap_or_default()
    }

    fn preset_modes_vec(&self) -> Vec<String> {
        self.state.read().preset_modes.clone().unwrap_or_default()
    }

    fn swing_modes_vec(&self) -> Vec<String> {
        self.state.read().swing_modes.clone().unwrap_or_default()
    }

    fn swing_horizontal_modes_vec(&self) -> Vec<String> {
        self.state
            .read()
            .swing_horizontal_modes
            .clone()
            .unwrap_or_default()
    }

    fn temp_bounds(&self) -> (Temp, Temp) {
        let s = self.state.read();
        (s.min_temp, s.max_temp)
    }

    fn humidity_bounds(&self) -> (Temp, Temp) {
        let s = self.state.read();
        (s.min_humidity, s.max_humidity)
    }

    fn turn_on(&self) -> BoxFuture<'_> {
        self.invoke("turn_on", Map::new())
    }

    fn turn_off(&self) -> BoxFuture<'_> {
        self.invoke("turn_off", Map::new())
    }

    fn toggle(&self) -> BoxFuture<'_> {
        self.invoke("toggle", Map::new())
    }

    fn set_hvac_mode(&self, mode: &str) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("hvac_mode".into(), json!(mode));
        self.invoke("set_hvac_mode", data)
    }

    fn set_fan_mode(&self, mode: &str) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("fan_mode".into(), json!(mode));
        self.invoke("set_fan_mode", data)
    }

    fn set_preset_mode(&self, mode: &str) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("preset_mode".into(), json!(mode));
        self.invoke("set_preset_mode", data)
    }

    fn set_humidity(&self, humidity: i64) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("humidity".into(), json!(humidity));
        self.invoke("set_humidity", data)
    }

    fn set_swing_mode(&self, mode: &str) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("swing_mode".into(), json!(mode));
        self.invoke("set_swing_mode", data)
    }

    fn set_swing_horizontal_mode(&self, mode: &str) -> BoxFuture<'_> {
        let mut data = Map::new();
        data.insert("swing_horizontal_mode".into(), json!(mode));
        self.invoke("set_swing_horizontal_mode", data)
    }

    fn set_temperature(&self, args: SetTemperature) -> BoxFuture<'_> {
        let mut data = Map::new();
        if let Some(t) = args.temperature {
            data.insert("temperature".into(), json!(t));
        }
        if let Some(low) = args.target_temp_low {
            data.insert("target_temp_low".into(), json!(low));
        }
        if let Some(high) = args.target_temp_high {
            data.insert("target_temp_high".into(), json!(high));
        }
        if let Some(mode) = args.hvac_mode {
            data.insert("hvac_mode".into(), json!(mode));
        }
        self.invoke("set_temperature", data)
    }
}

struct NotifyInner {
    active: bool,
    available: bool,
    last_notified: Option<String>,
    base_attrs: Attributes,
}

struct PluginNotify {
    entity_id: EntityId,
    entry_id: String,
    unique_id: String,
    supported_features: u32,
    hass: Hass,
    state: Arc<Mutex<NotifyInner>>,
    call_plugin: CallPluginFn,
}

impl Notify for PluginNotify {
    fn entity_id(&self) -> Option<EntityId> {
        Some(self.entity_id.clone())
    }

    fn available(&self) -> bool {
        let s = self.state.lock();
        s.active && s.available
    }

    fn supported_features(&self) -> u32 {
        self.supported_features
    }

    fn send_message<'a>(
        &'a self,
        message: &'a str,
        title: Option<&'a str>,
        context: Context,
    ) -> BoxFuture<'a> {
        if !self.state.lock().active {
            return Box::pin(async { Ok(()) });
        }
        let context_set = self.hass.clock().instant();
        let mut data = Map::new();
        data.insert("message".into(), json!(message));
        if let Some(t) = title {
            data.insert("title".into(), json!(t));
        }
        let call = (self.call_plugin)(EntityCallParams {
            entry_id: self.entry_id.clone(),
            unique_id: self.unique_id.clone(),
            domain: "notify".into(),
            service: "send_message".into(),
            data,
        });
        Box::pin(async move {
            call.await?;
            // Record last notification timestamp (`NotifyEntity._async_record_notification`),
            // serialized with availability and deactivation under `self.state`.
            let ts = ha_core::isoformat(self.hass.clock().now());
            let elapsed = self
                .hass
                .clock()
                .instant()
                .saturating_duration_since(context_set);
            let ctx = (elapsed <= std::time::Duration::from_secs(5)).then_some(context);
            let mut inner = self.state.lock();
            if !inner.active {
                return Ok(());
            }
            inner.last_notified = Some(ts.clone());
            if inner.available {
                let mut attrs = inner.base_attrs.clone();
                attrs.insert("supported_features".into(), json!(self.supported_features));
                self.hass
                    .set_state_with_context(self.entity_id.clone(), ts, attrs, ctx);
            }
            Ok(())
        })
    }
}
