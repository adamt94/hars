use super::*;
use serde_json::json;

/// From HA's own `_async_strip_prefix_from_entity_name`, run in Python.
#[test]
fn strips_device_name_prefixes_like_ha() {
    let cases: Vec<(String, String, Option<String>)> = serde_json::from_str(
        r#"[["Kitchen Light Power", "Kitchen Light", "Power"], ["kitchen light power", "Kitchen Light", "Power"], ["Kitchen Light", "Kitchen Light", ""], ["Kitchen Lightbulb", "Kitchen Light", null], ["Kitchen Light: battery level", "Kitchen Light", "Battery level"], ["Kitchen Light - iPhone mode", "Kitchen Light", "iPhone mode"], ["Straße Sensor temp", "STRASSE Sensor", "Temp"], ["Other name", "Kitchen", null], ["Kitchen", "", null], ["", "Kitchen", null], ["ÉCLAIRAGE salon", "éclairage", "Salon"], ["Hall ǅx", "hall", "ǅx"]]"#,
    )
    .unwrap();
    for (name, prefix, expected) in cases {
        assert_eq!(
            strip_prefix(&name, &prefix),
            expected,
            "{name:?} / {prefix:?}"
        );
    }
}

fn write(dir: &Path, key: &str, minor: u32, data: Value) {
    std::fs::write(
        dir.join(".storage").join(key),
        json!({"version": 1, "minor_version": minor, "key": key, "data": data}).to_string(),
    )
    .unwrap();
}

/// HA 2026.6.1 files, as HA writes them.
fn ha_files(dir: &Path) {
    std::fs::create_dir_all(dir.join(".storage")).unwrap();
    write(
        dir,
        AREA_KEY,
        9,
        json!({"areas": [
            {"aliases": ["Cooking"], "floor_id": "ground", "humidity_entity_id": null, "icon": "mdi:stove", "id": "kitchen",
             "labels": [], "name": "Kitchen", "picture": null, "temperature_entity_id": "sensor.kitchen_temp",
             "created_at": "2026-06-01T10:00:00+00:00", "modified_at": "2026-06-02T10:00:00.500000+00:00"},
            {"aliases": [], "floor_id": null, "humidity_entity_id": null, "icon": null, "id": "hall", "labels": ["x"],
             "name": "Hall", "picture": null, "temperature_entity_id": null,
             "created_at": "1970-01-01T00:00:00+00:00", "modified_at": "1970-01-01T00:00:00+00:00"}
        ]}),
    );
    write(
        dir,
        FLOOR_KEY,
        3,
        json!({"floors": [
            {"aliases": [], "floor_id": "ground", "icon": null, "level": 0, "name": "Ground floor",
             "created_at": "2026-06-01T10:00:00+00:00", "modified_at": "2026-06-01T10:00:00+00:00"}
        ]}),
    );
    write(
        dir,
        LABEL_KEY,
        2,
        json!({"labels": [
            {"color": "red", "description": null, "icon": null, "label_id": "x", "name": "X",
             "created_at": "2026-06-01T10:00:00+00:00", "modified_at": "2026-06-01T10:00:00+00:00"}
        ]}),
    );
    write(
        dir,
        DEVICE_KEY,
        12,
        json!({"devices": [
        {"area_id": "kitchen", "config_entries": ["e1"], "config_entries_subentries": {"e1": [null]},
         "configuration_url": null, "connections": [["mac", "00:11"]], "created_at": "2026-06-01T10:00:00+00:00",
         "disabled_by": null, "entry_type": null, "hw_version": null, "id": "dev1", "identifiers": [["hue", "abc"]],
         "labels": [], "manufacturer": "Signify", "model": "LCT", "model_id": "LCT015", "modified_at": "2026-06-01T10:00:00+00:00",
         "name_by_user": null, "name": "Kitchen Light", "primary_config_entry": "e1", "serial_number": null,
         "sw_version": "1.2", "via_device_id": null, "insertion_order": 7}
    ], "deleted_devices": [{"id": "gone"}]}),
    );
    write(
        dir,
        ENTITY_KEY,
        22,
        json!({"entities": [
        {"entity_id": "light.kitchen", "platform": "hue", "id": "u1", "unique_id": "abc", "device_id": "dev1",
         "area_id": null, "labels": ["x"], "icon": null, "translation_key": null, "entity_category": null,
         "hidden_by": null, "disabled_by": null, "has_entity_name": true, "name": null, "original_name": null,
         "options": {}, "aliases": [], "capabilities": null},
        {"entity_id": "sensor.kitchen_light_power", "platform": "legacy", "device_id": "dev1",
         "has_entity_name": false, "name": null, "original_name": "Kitchen Light Power",
         "entity_category": "diagnostic", "hidden_by": "user", "labels": [],
         "options": {"sensor": {"display_precision": null, "suggested_display_precision": 1}}},
        {"entity_id": "switch.off", "platform": "x", "disabled_by": "user", "labels": []}
    ], "deleted_entities": []}),
    );
}

#[test]
fn loads_ha_files_and_serves_has_shapes() {
    let dir = std::env::temp_dir().join(format!("hars-registry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    ha_files(&dir);
    let r = Registries::load(&dir).unwrap();

    assert_eq!(
        r.areas_json()[0],
        json!({"aliases": ["Cooking"], "area_id": "kitchen", "floor_id": "ground", "humidity_entity_id": null,
               "icon": "mdi:stove", "labels": [], "name": "Kitchen", "picture": null,
               "temperature_entity_id": "sensor.kitchen_temp", "created_at": 1780308000.0, "modified_at": 1780394400.5})
    );
    // File order is display order.
    assert_eq!(r.areas_json()[1]["area_id"], "hall");
    assert_eq!(r.floors_json()[0]["level"], 0);
    assert_eq!(r.labels_json()[0]["color"], "red");

    let device = &r.devices_json()[0];
    assert_eq!(device["identifiers"], json!([["hue", "abc"]]));
    assert_eq!(device["config_entries_subentries"], json!({"e1": [null]}));
    assert!(
        device.get("insertion_order").is_none(),
        "storage-only fields stay out of the API"
    );
    assert_eq!(device.as_object().unwrap().len(), 22);

    let display = r.entities_for_display();
    assert_eq!(
        display["entity_categories"],
        json!({"0": "config", "1": "diagnostic"})
    );
    assert_eq!(
        display["entities"],
        json!([
            {"ei": "light.kitchen", "pl": "hue", "lb": ["x"], "di": "dev1", "hn": true},
            // Legacy naming: the device name prefix is stripped.
            {"ei": "sensor.kitchen_light_power", "pl": "legacy", "lb": [], "di": "dev1", "ec": 1, "hb": true,
             "en": "Power", "dp": 1}
        ]),
        "disabled entities are left out"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_missing_registry_is_empty() {
    let dir = std::env::temp_dir().join(format!("hars-registry-empty-{}", std::process::id()));
    let r = Registries::load(&dir).unwrap();
    assert_eq!(r.entities_for_display()["entities"], json!([]));
    assert_eq!(r.areas_json(), json!([]));
}

#[test]
fn a_corrupt_registry_is_set_aside_for_the_server_but_an_error_for_the_importer() {
    let dir = std::env::temp_dir().join(format!("hars-registry-corrupt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    ha_files(&dir);
    std::fs::write(dir.join(".storage").join(AREA_KEY), "{oops").unwrap();
    assert!(Registries::load_strict(&dir).is_err());
    assert!(
        dir.join(".storage").join(AREA_KEY).exists(),
        "strict loading moves nothing"
    );
    let r = Registries::load(&dir).unwrap();
    assert_eq!(r.areas_json(), json!([]));
    assert_eq!(r.devices.len(), 1, "the other registries still load");
    assert!(!dir.join(".storage").join(AREA_KEY).exists());
    std::fs::remove_dir_all(dir).unwrap();
}
