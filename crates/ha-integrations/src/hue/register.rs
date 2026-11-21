//! The registry rows of a bridge, as HA's set-up makes them on every start
//! (`hue/__init__.py`, `v2/device.py` and the entity platforms): the bridge,
//! each device, each room, zone and service group as a "service" device,
//! and a row per entity. A row that exists is found by its identifier or
//! unique id and changed only where HA's values differ, so an imported
//! install's rows come out as they went in.
//!
//! Checked against the rows HA 2026.6.1 made for the recorded bridge
//! (`vectors/fresh_setup.json`, from `scripts/vectors/hue_flow.py`).

use super::DOMAIN;
use super::entities::{HueEntity, Kind};
use super::light::{GroupLight, HueLight};
use super::store::Store;
use crate::light::Light;
use ha_core::EntityId;
use ha_registry::{DeviceInfo, EntityPatch, Registry};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Weak;

fn text(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

fn identifier(id: &str) -> Value {
    json!([DOMAIN, id])
}

/// aiohue's `mac_from_bridge_id`: the bridge id without its middle `fffe`.
fn bridge_mac(bridge_id: &str) -> Option<String> {
    let part = |at: usize| bridge_id.get(at..at + 2);
    let parts: Option<Vec<&str>> = [0, 2, 4, 10, 12, 14].into_iter().map(part).collect();
    Some(parts?.join(":"))
}

/// Register the bridge's devices, then its entities. `disable_new` is the
/// entry's `pref_disable_new_entities`.
///
/// Nothing is registered when the device or entity store can't be written
/// (an install imported from an older Home Assistant): rows made now would
/// be gone after a restart while rows that point at them were saved. The
/// entities then run on the rows there are, as before `hars` made rows.
pub(super) fn register(
    hass: &ha_core::Hass,
    registry: &Registry,
    entry_id: &str,
    store: &Store,
    disable_new: bool,
) {
    if registry.read_only().is_some() {
        return;
    }
    let Some(bridge) = store.bridge_device() else {
        return;
    };
    let bridge_device = bridge["id"].as_str().unwrap_or_default();
    let bridge_id = store
        .of_type("bridge")
        .next()
        .and_then(|b| b["bridge_id"].as_str());
    let manufacturer = text(&bridge["product_data"]["manufacturer_name"]);
    // `async_setup_devices`: the bridge's device first, then the others in
    // the bridge's order, then rooms, zones and service groups. A Hue id
    // to its row's id, and whether that device is disabled.
    let mut rows: HashMap<&str, (String, bool)> = HashMap::new();
    let mut devices: Vec<&Value> = store.of_type("device").collect();
    devices.sort_by_key(|d| d["id"] != bridge_device);
    for d in devices {
        let Some(id) = d["id"].as_str() else { continue };
        let product = &d["product_data"];
        let room = store.of_type("room").find(|room| {
            let children = room["children"].as_array();
            children.is_some_and(|c| c.iter().any(|c| c["rid"] == id))
        });
        let zigbee = store
            .service_of(d, "zigbee_connectivity")
            .and_then(|z| z["mac_address"].as_str())
            .filter(|mac| !mac.is_empty());
        let mut identifiers = vec![identifier(id)];
        if let Some(bridge_id) = bridge_id.filter(|_| id == bridge_device) {
            identifiers.push(identifier(bridge_id));
        }
        let row = registry.get_or_create_device(DeviceInfo {
            config_entry_id: entry_id.to_owned(),
            connections: zigbee.map(|mac| json!(["mac", mac])).into_iter().collect(),
            identifiers,
            manufacturer: text(&product["manufacturer_name"]),
            model: text(&product["product_name"]),
            model_id: text(&product["model_id"]),
            name: text(&d["metadata"]["name"]),
            sw_version: text(&product["software_version"]),
            via_device: (id != bridge_device).then(|| identifier(bridge_device)),
            suggested_area: room.and_then(|r| text(&r["metadata"]["name"])),
            ..Default::default()
        });
        rows.insert(id, (row.id.clone(), row.disabled_by.is_some()));
    }
    for (ty, model) in [
        ("room", "Room"),
        ("zone", "Zone"),
        ("service_group", "Service Group"),
    ] {
        for group in store.of_type(ty) {
            let Some(id) = group["id"].as_str() else {
                continue;
            };
            let name = text(&group["metadata"]["name"]);
            let row = registry.get_or_create_device(DeviceInfo {
                config_entry_id: entry_id.to_owned(),
                entry_type: Some("service".into()),
                identifiers: vec![identifier(id)],
                manufacturer: manufacturer.clone(),
                model: Some(model.into()),
                name: name.clone(),
                via_device: Some(identifier(bridge_device)),
                suggested_area: name.filter(|_| ty == "room"),
                ..Default::default()
            });
            rows.insert(id, (row.id.clone(), row.disabled_by.is_some()));
        }
    }
    // Then `async_setup_entry`: the bridge once more, by its network
    // address and both its ids.
    if let Some(bridge_id) = bridge_id {
        registry.get_or_create_device(DeviceInfo {
            config_entry_id: entry_id.to_owned(),
            connections: bridge_mac(bridge_id)
                .map(|mac| json!(["mac", mac]))
                .into_iter()
                .collect(),
            identifiers: vec![identifier(bridge_id), identifier(bridge_device)],
            manufacturer: manufacturer.clone(),
            name: text(&bridge["metadata"]["name"]),
            model_id: text(&bridge["product_data"]["model_id"]),
            sw_version: text(&bridge["product_data"]["software_version"]),
            ..Default::default()
        });
    }
    let row_of = |hue: Option<&Value>| {
        let id = hue.and_then(|d| d["id"].as_str())?;
        rows.get(id).map(|(row, _)| row.clone())
    };
    // How a row made now starts: disabled by the integration if HA's
    // entity is (`entity_registry_enabled_default`). The entry's
    // `pref_disable_new_entities` is the registry's to apply, after a
    // deleted row's own setting has come back. A row that exists keeps
    // what it has.
    let starts = |enabled_default: bool| (!enabled_default).then(|| "integration".to_owned());
    // Then `EntityPlatform._async_add_entity`: a row that is enabled, made
    // now or not, on a device that is disabled is disabled with it.
    // Whether the entity then runs.
    let settle = |made: &ha_registry::Entity, hue: Option<&Value>| {
        let device_disabled = hue
            .and_then(|d| d["id"].as_str())
            .and_then(|id| rows.get(id))
            .is_some_and(|(_, disabled)| *disabled);
        if device_disabled && made.disabled_by.is_none() {
            let disabled = EntityPatch {
                disabled_by: Some(Some("device".into())),
                ..Default::default()
            };
            registry.update_entity(&made.entity_id, disabled);
        }
        made.disabled_by.is_none() && !device_disabled
    };
    let patch = |device: Option<&Value>| EntityPatch {
        config_entry_id: Some(Some(entry_id.to_owned())),
        device_id: Some(row_of(device)),
        entity_category: Some(None),
        has_entity_name: Some(true),
        object_id_base: Some(None),
        original_device_class: Some(None),
        original_icon: Some(None),
        original_name: Some(None),
        translation_key: Some(None),
        unit_of_measurement: Some(None),
        pref_disable_new: disable_new,
        ..Default::default()
    };

    // The platforms in HA's order (binary_sensor, light, scene, sensor,
    // switch): two entities whose names lead to one id get it, and `_2`,
    // in that order. Lights first is the same: no other is a `light`.
    // A light's capabilities are those its entity reports, so one is made
    // (with no bridge behind it) to ask.
    let unnamed = EntityId::parse("light.unnamed").expect("a valid id");
    for r in store.of_type("light") {
        let Some(rid) = r["id"].as_str() else {
            continue;
        };
        let light = HueLight::new(Weak::new(), r, unnamed.clone());
        let Some(state) = light.state_in(store) else {
            continue;
        };
        let caps = light.capabilities();
        let row = EntityPatch {
            capabilities: Some(Some(Value::Object(crate::light::attributes(
                &caps, &state, false,
            )))),
            supported_features: Some(i64::from(caps.features)),
            translation_key: Some(Some("hue_light".into())),
            ..patch(store.device_of(rid))
        };
        let made = registry.get_or_create_entity("light", DOMAIN, rid, starts(true), row);
        settle(&made, store.device_of(rid));
    }
    for r in store.of_type("grouped_light") {
        let Some(rid) = r["id"].as_str() else {
            continue;
        };
        if matches!(
            r["owner"]["rtype"].as_str(),
            Some("bridge_home" | "private_group")
        ) {
            continue;
        }
        let Some(group) = store.group_of(rid) else {
            continue;
        };
        let gid = group["id"].as_str().unwrap_or_default();
        let light = GroupLight::new(Weak::new(), rid, gid, unnamed.clone(), store);
        let caps = light.capabilities();
        let state = light.state_in(store);
        let row = EntityPatch {
            capabilities: Some(Some(Value::Object(crate::light::attributes(
                &caps, &state, false,
            )))),
            supported_features: Some(i64::from(caps.features)),
            ..patch(Some(group))
        };
        let made = registry.get_or_create_entity("light", DOMAIN, rid, starts(true), row);
        settle(&made, Some(group));
    }
    let mut others: Vec<(u8, &Value, Kind)> = Vec::new();
    for r in store.order() {
        let ty = r["type"].as_str().unwrap_or_default();
        for &kind in Kind::for_type(ty) {
            if kind.wanted(r, store) {
                others.push((kind.order(ty), r, kind));
            }
        }
    }
    others.sort_by_key(|(order, _, _)| *order);
    for (_, r, kind) in others {
        let Some(rid) = r["id"].as_str() else {
            continue;
        };
        let name = kind.name(r);
        let device = HueEntity::device(kind, rid, r, store);
        // What HA derives from the install's language (a name) is written
        // only to a row made now, in English: a row HA made keeps HA's. A
        // name that is the resource's own (a scene's) follows the
        // resource.
        let existing = registry.entity_by_unique_id(kind.domain(), DOMAIN, rid);
        let name = match existing.is_some() && !kind.named_by_resource() {
            true => None,
            false => Some(name),
        };
        // A temperature's unit is the sensor component's to settle
        // (`add_to_platform_start`), which may first note in the row's
        // options the unit the row had.
        let unit = match kind {
            Kind::Temperature => {
                let settled = crate::sensor::when_registered(hass, existing.as_deref());
                if let (Some(row), Some(options)) = (&existing, settled.options) {
                    let options = EntityPatch {
                        options: Some(options),
                        ..Default::default()
                    };
                    registry.update_entity(&row.entity_id, options);
                }
                Some(settled.unit)
            }
            _ => kind.unit(),
        };
        let row = EntityPatch {
            capabilities: Some(kind.capabilities()),
            entity_category: Some(kind.category().map(str::to_owned)),
            has_entity_name: Some(!kind.legacy()),
            object_id_base: name.clone(),
            original_device_class: Some(kind.device_class().map(str::to_owned)),
            original_name: name,
            supported_features: Some(0),
            translation_key: Some(kind.translation_key().map(str::to_owned)),
            unit_of_measurement: Some(unit.map(str::to_owned)),
            ..patch(device)
        };
        let disabled = starts(kind.enabled_default());
        let made = registry.get_or_create_entity(kind.domain(), DOMAIN, rid, disabled, row);
        let runs = settle(&made, device);
        // The sensor's suggested precision goes beside the options the
        // row has (`async_update_entity_options`), as a change of its
        // own, when the entity is added to HA: a disabled one never is,
        // and its row stays without.
        if let (true, Some(precision)) = (runs, kind.suggested_precision())
            && let Some(options) = crate::sensor::with_precision(&made, i64::from(precision))
        {
            let options = EntityPatch {
                options: Some(options),
                ..Default::default()
            };
            registry.update_entity(&made.entity_id, options);
        }
    }
}
