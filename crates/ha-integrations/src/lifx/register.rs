//! A bulb's registry rows, as HA's set-up makes them on every start
//! (`entity.py`'s `DeviceInfo` and the platforms): the device, the light,
//! the restart and identify buttons and the RSSI sensor (disabled by
//! default; `hars` doesn't run it). A row that exists is found by its
//! identifier or unique id and changed only where HA's values differ, so an
//! imported install's rows come out as they went in.
//!
//! Checked against the rows HA 2026.6.1 made (`vectors/fresh_setup.json`
//! from `scripts/vectors/lifx_flow.py`, and `vectors/imported/` from
//! `scripts/vectors/lifx_rows.py`).

use super::{DOMAIN, Found, features, light, model_name, real_mac, rssi_unit};
use ha_registry::{DeviceInfo, EntityPatch, Registry};
use serde_json::{Value, json};

/// Register the bulb's device, then its entities. `disable_new` is the
/// entry's `pref_disable_new_entities`. `duplicate`: another entry runs the
/// bulb; as HA's platform refuses an entity whose unique id a running
/// entity has, a row that is enabled (the other entry runs it) is left as
/// it is, and the rest (a disabled row) is registered to this entry, as in
/// HA.
///
/// Nothing is registered when the device or entity store can't be written
/// (an install imported from an older Home Assistant): rows made now would
/// be gone after a restart while rows that point at them were saved. The
/// entities then run on the rows there are.
pub(super) fn register(
    _hass: &ha_core::Hass,
    registry: &Registry,
    entry_id: &str,
    found: &Found,
    disable_new: bool,
    duplicate: bool,
) {
    if registry.read_only().is_some() {
        return;
    }
    let serial = found.serial();
    let label = found.cached.label.clone();
    // An entity the other entry runs (its row is enabled) is refused before
    // HA's platform gets to the device: a duplicate with nothing else to
    // take over leaves the device as it is.
    let runs_elsewhere = |domain: &str, unique_id: &str| {
        duplicate
            && registry
                .entity_by_unique_id(domain, DOMAIN, unique_id)
                .is_some_and(|row| row.disabled_by.is_none())
    };
    let rows = [
        ("button", format!("{serial}_restart")),
        ("button", format!("{serial}_identify")),
        ("light", serial.clone()),
        ("sensor", format!("{serial}_rssi")),
    ];
    if rows
        .iter()
        .all(|(domain, unique_id)| runs_elsewhere(domain, unique_id))
    {
        return;
    }
    let device = registry.get_or_create_device(DeviceInfo {
        config_entry_id: entry_id.to_owned(),
        identifiers: vec![json!([DOMAIN, serial])],
        connections: vec![json!(["mac", real_mac(&found.serial, &found.firmware)])],
        manufacturer: Some("LIFX".into()),
        model: Some(model_name(found.product).to_owned()),
        name: Some(label),
        sw_version: Some(found.firmware.clone()),
        suggested_area: Some(found.group.clone()).filter(|g| !g.is_empty()),
        ..Default::default()
    });
    let device_disabled = device.disabled_by.is_some();
    let patch = || EntityPatch {
        config_entry_id: Some(Some(entry_id.to_owned())),
        device_id: Some(Some(device.id.clone())),
        has_entity_name: Some(true),
        original_icon: Some(None),
        pref_disable_new: disable_new,
        ..Default::default()
    };
    // `EntityPlatform._async_add_entity`: a row that is enabled, made now
    // or not, on a device that is disabled is disabled with it.
    let settle = |made: &ha_registry::Entity| {
        if device_disabled && made.disabled_by.is_none() {
            let disabled = EntityPatch {
                disabled_by: Some(Some("device".into())),
                ..Default::default()
            };
            registry.update_entity(&made.entity_id, disabled);
        }
    };
    // A name HA takes from its translations (a button's from its device
    // class, the sensor's from `rssi`) is written only to a row made now,
    // in English: a row HA made keeps the one in its language.
    let named = |domain: &str, unique_id: &str, name: &str| match registry
        .entity_by_unique_id(domain, DOMAIN, unique_id)
    {
        Some(_) => (None, None),
        None => (Some(Some(name.to_owned())), Some(Some(name.to_owned()))),
    };

    // `button`: restart, then identify.
    for (key, name) in [("restart", "Restart"), ("identify", "Identify")] {
        let unique_id = format!("{serial}_{key}");
        if runs_elsewhere("button", &unique_id) {
            continue;
        }
        let (original_name, object_id_base) = named("button", &unique_id, name);
        let row = EntityPatch {
            capabilities: Some(None),
            entity_category: Some(Some("config".into())),
            object_id_base,
            original_device_class: Some(Some(key.into())),
            original_name,
            supported_features: Some(0),
            translation_key: Some(None),
            unit_of_measurement: Some(None),
            ..patch()
        };
        let made = registry.get_or_create_entity("button", DOMAIN, &unique_id, None, row);
        settle(&made);
    }

    // `light`: named after its device.
    let model = light::Model::of(features(found.product));
    let caps = light::capabilities(model);
    let state = light::state(model, 0, [0; 4], None);
    let row = EntityPatch {
        capabilities: Some(Some(Value::Object(crate::light::attributes(
            &caps, &state, false,
        )))),
        entity_category: Some(None),
        object_id_base: Some(None),
        original_device_class: Some(None),
        original_name: Some(None),
        supported_features: Some(i64::from(caps.features)),
        translation_key: Some(None),
        unit_of_measurement: Some(None),
        ..patch()
    };
    if !runs_elsewhere("light", &serial) {
        let made = registry.get_or_create_entity("light", DOMAIN, &serial, None, row);
        settle(&made);
    }

    // `sensor`: the RSSI, disabled by default.
    let unique_id = format!("{serial}_rssi");
    let (original_name, object_id_base) = named("sensor", &unique_id, "RSSI");
    let row = EntityPatch {
        capabilities: Some(Some(json!({"state_class": "measurement"}))),
        entity_category: Some(Some("diagnostic".into())),
        object_id_base,
        original_device_class: Some(Some("signal_strength".into())),
        original_name,
        supported_features: Some(0),
        translation_key: Some(Some("rssi".into())),
        unit_of_measurement: Some(Some(rssi_unit(&found.firmware).into())),
        ..patch()
    };
    let disabled = Some("integration".to_owned());
    if !runs_elsewhere("sensor", &unique_id) {
        let made = registry.get_or_create_entity("sensor", DOMAIN, &unique_id, disabled, row);
        settle(&made);
    }
}
