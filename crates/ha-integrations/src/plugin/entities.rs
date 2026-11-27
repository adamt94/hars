//! Generic entity adapters and scoped device/entity tracking for plugins.
//!
//! Every entity a plugin registers is wired into `hars`'s existing entity
//! components (`light`, `switch`, `button`, `scene`) and `ha-registry` so
//! dashboards, automations, and services treat plugin entities identically to
//! built-in integrations.

use super::manifest::InstalledPlugin;
use super::protocol::{DeviceUpsertParams, EntityCallParams, EntityUpsertParams, StateSetParams};
use crate::button::Button;
use crate::components::Components;
use crate::cover::Cover;
use crate::fan::{self, Fan};
use crate::light::{
    BoxFuture, Capabilities as LightCaps, ColorMode, Flash, Light, LightState, Modes, Params,
    TurnOff, White,
};
use crate::lock::Lock;
use crate::number::{self, Number};
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
    Vacuum(Arc<PluginVacuum>),
    Select(Arc<PluginSelect>),
    Number(Arc<PluginNumber>),
    Cover(Arc<PluginCover>),
    Lock(Arc<PluginLock>),
    Fan(Arc<PluginFan>),
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
        let device_id = params.device_identifier.as_ref().and_then(|ident| {
            entries
                .get(&params.entry_id)
                .and_then(|e| e.devices.get(ident).cloned())
        });

        let (entity_id, disabled) = if let Some(registry) = self.components.registry() {
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
            (id, row.disabled_by.is_some())
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
            (id, false)
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
            if let Some(modes) = &params.preset_modes {
                base_attrs.insert("preset_modes".into(), json!(modes));
            }
            if params.supported_features & fan::feature::SET_SPEED != 0 {
                let sc = params.speed_count.unwrap_or(100).max(1);
                base_attrs.insert("percentage_step".into(), json!(100.0 / f64::from(sc)));
            }
        }

        let entry = entries
            .get_mut(&params.entry_id)
            .expect("entry verified active above");
        // Remove any previous adapter if re-registered.
        if let Some(old) = entry.entities.remove(&params.unique_id) {
            self.unregister_adapter(&old);
        }

        let handle = if disabled {
            EntityHandle::Disabled
        } else {
            match params.domain.as_str() {
                "switch" => {
                    let sw = Arc::new(PluginSwitch {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        state: RwLock::new(SwitchInner {
                            available: true,
                            is_on: false,
                        }),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.switches().add(sw.clone());
                    EntityHandle::Switch(sw)
                }
                "light" => {
                    let caps = parse_light_caps(&params);
                    let min_k = params.min_color_temp_kelvin.unwrap_or(2000);
                    let max_k = params.max_color_temp_kelvin.unwrap_or(6535);
                    let light = Arc::new(PluginLight {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        caps,
                        state: RwLock::new(LightInner {
                            available: true,
                            light: LightState {
                                min_color_temp_kelvin: min_k,
                                max_color_temp_kelvin: max_k,
                                ..Default::default()
                            },
                        }),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.lights().add(light.clone());
                    EntityHandle::Light(light)
                }
                "button" => {
                    let btn = Arc::new(PluginButton {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        hass: self.components.hass().clone(),
                        friendly_name: friendly_name.clone(),
                        state: Mutex::new(ButtonInner {
                            available: true,
                            last_pressed: None,
                        }),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.buttons().add(btn.clone());
                    EntityHandle::Button(btn)
                }
                "scene" => {
                    let scene = Arc::new(PluginScene {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        hass: self.components.hass().clone(),
                        friendly_name: friendly_name.clone(),
                        available: RwLock::new(true),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.scenes().add(scene.clone());
                    EntityHandle::Scene(scene)
                }
                "vacuum" => {
                    let vac = Arc::new(PluginVacuum {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        supported_features: params.supported_features,
                        available: RwLock::new(true),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.vacuums().add(vac.clone());
                    EntityHandle::Vacuum(vac)
                }
                "select" => {
                    let sel = Arc::new(PluginSelect {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        state: RwLock::new(SelectInner {
                            available: true,
                            options: params.options.clone().unwrap_or_default(),
                            current_option: None,
                        }),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.selects().add(sel.clone());
                    EntityHandle::Select(sel)
                }
                "number" => {
                    let min = params.min_value.unwrap_or(number::DEFAULT_MIN_VALUE);
                    let max = params.max_value.unwrap_or(number::DEFAULT_MAX_VALUE);
                    let step = number::calculate_step(min, max, params.step);
                    let num = Arc::new(PluginNumber {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        state: RwLock::new(NumberInner {
                            available: true,
                            min,
                            max,
                            step,
                            explicit_step: params.step,
                        }),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.numbers().add(num.clone());
                    EntityHandle::Number(num)
                }
                "cover" => {
                    let cov = Arc::new(PluginCover {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        supported_features: params.supported_features,
                        state: RwLock::new(CoverInner {
                            available: true,
                            is_closed: None,
                            is_opening: None,
                            is_closing: None,
                            current_position: None,
                            current_tilt_position: None,
                            last_toggle_direction_open: true,
                        }),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.covers().add(cov.clone());
                    EntityHandle::Cover(cov)
                }
                "lock" => {
                    let lck = Arc::new(PluginLock {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        supported_features: params.supported_features,
                        state: RwLock::new(LockInner {
                            available: true,
                            code_format: params.code_format.clone(),
                        }),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.locks().add(lck.clone());
                    EntityHandle::Lock(lck)
                }
                "fan" => {
                    let f = Arc::new(PluginFan {
                        entity_id: entity_id.clone(),
                        entry_id: params.entry_id.clone(),
                        unique_id: params.unique_id.clone(),
                        supported_features: params.supported_features,
                        state: RwLock::new(FanInner {
                            available: true,
                            is_on: false,
                            percentage: None,
                            speed_count: params.speed_count.unwrap_or(100).max(1),
                            preset_modes: params.preset_modes.clone().unwrap_or_default(),
                        }),
                        call_plugin: self.call_plugin.clone(),
                    });
                    self.components.fans().add(f.clone());
                    EntityHandle::Fan(f)
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
                merged.insert("min".into(), json!(inner.min));
                merged.insert("max".into(), json!(inner.max));
                merged.insert("step".into(), json!(inner.step));
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
                merged.insert("supported_features".into(), json!(lck.supported_features));
            }
            EntityHandle::Fan(f) => {
                let mut inner = f.state.write();
                inner.available = available;
                inner.is_on = state_str == "on";
                if let Some(p) = params.attributes.get("percentage") {
                    inner.percentage = p.as_u64().map(|n| n.min(100) as u8);
                }
                if let Some(arr) = params
                    .attributes
                    .get("preset_modes")
                    .and_then(Value::as_array)
                {
                    inner.preset_modes = arr
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                }
                merged.insert("supported_features".into(), json!(f.supported_features));
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
                self.unregister_adapter(reg);
                hass.remove_state(&reg.entity_id);
            }
        }
    }

    /// Snapshot of `Components` bound to this store.
    pub fn components(&self) -> Arc<Components> {
        self.components.clone()
    }

    /// Mark every entity of every active entry `unavailable` (called when the
    /// plugin process crashes or disconnects). Holds `self.entries` across
    /// state writes so deactivation cannot interleave.
    pub fn mark_all_unavailable(&self) {
        let entries = self.entries.lock();
        let hass = self.components.hass();
        for entry in entries.values() {
            for reg in entry.entities.values() {
                if matches!(reg.handle, EntityHandle::Disabled) {
                    continue;
                }
                match &reg.handle {
                    EntityHandle::Switch(sw) => sw.state.write().available = false,
                    EntityHandle::Light(light) => light.state.write().available = false,
                    EntityHandle::Button(btn) => {
                        let mut inner = btn.state.lock();
                        inner.available = false;
                        let mut attrs = Attributes::new();
                        if let Some(name) = &reg.friendly_name {
                            attrs.insert("friendly_name".into(), json!(name));
                        }
                        hass.set_state(reg.entity_id.clone(), "unavailable", attrs);
                        continue;
                    }
                    EntityHandle::Scene(scene) => *scene.available.write() = false,
                    EntityHandle::Vacuum(vac) => *vac.available.write() = false,
                    EntityHandle::Select(sel) => sel.state.write().available = false,
                    EntityHandle::Number(num) => num.state.write().available = false,
                    EntityHandle::Cover(cov) => cov.state.write().available = false,
                    EntityHandle::Lock(lck) => lck.state.write().available = false,
                    EntityHandle::Fan(f) => f.state.write().available = false,
                    EntityHandle::Passive | EntityHandle::Disabled => {}
                }
                let mut attrs = Attributes::new();
                if let Some(name) = &reg.friendly_name {
                    attrs.insert("friendly_name".into(), json!(name));
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
            self.unregister_adapter(reg);
            hass.remove_state(&reg.entity_id);
        }
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
    preset_modes: Vec<String>,
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
        self.state.read().preset_modes.clone()
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

    fn set_direction(&self, direction: Option<&str>) -> BoxFuture<'_> {
        let mut data = Map::new();
        if let Some(d) = direction {
            data.insert("direction".into(), json!(d));
        }
        self.invoke("set_direction", data)
    }
}
