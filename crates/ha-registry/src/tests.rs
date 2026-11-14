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

fn write_minor(dir: &Path, key: &str, minor: u32, data: Value) {
    std::fs::create_dir_all(dir.join(".storage")).unwrap();
    write(dir, key, minor, data);
}

fn temp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("hars-registry-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// HA's migrations for files still at an older minor version.
#[test]
fn older_minor_versions_are_migrated_like_ha() {
    let dir = temp("migrate");
    write_minor(
        &dir,
        DEVICE_KEY,
        8,
        json!({"devices": [
            {"id": "dev1", "config_entries": ["entry1", "entry2"], "name": "Kitchen", "name_by_user": null}
        ]}),
    );
    write_minor(
        &dir,
        AREA_KEY,
        8,
        json!({"areas": [
            {"id": "z", "name": "Zebra"}, {"id": "a", "name": "attic"}, {"id": "m", "name": "Middle"}
        ]}),
    );
    write_minor(
        &dir,
        FLOOR_KEY,
        2,
        json!({"floors": [
            {"floor_id": "none", "name": "Garden", "level": null},
            {"floor_id": "g", "name": "Ground", "level": 0},
            {"floor_id": "b", "name": "Basement", "level": -1},
            {"floor_id": "u2", "name": "Upstairs B", "level": 1},
            {"floor_id": "u1", "name": "upstairs a", "level": 1}
        ]}),
    );
    write_minor(
        &dir,
        ENTITY_KEY,
        21,
        json!({"entities": [
            {"entity_id": "sensor.renamed", "platform": "p", "device_id": "dev1", "name": "Kitchen Power", "name_v2": "Renamed", "labels": []},
            {"entity_id": "sensor.same", "platform": "p", "device_id": "dev1", "name": "Same", "name_v2": "Same", "labels": []},
            {"entity_id": "sensor.cleared", "platform": "p", "device_id": "dev1", "name": "Old", "name_v2": null, "labels": []}
        ]}),
    );
    let r = Registries::load_strict(&dir).unwrap();
    assert_eq!(
        r.devices_json()[0]["config_entries_subentries"],
        json!({"entry1": [null], "entry2": [null]})
    );
    let areas: Vec<Value> = r
        .areas_json()
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["area_id"].clone())
        .collect();
    assert_eq!(areas, [json!("a"), json!("m"), json!("z")]);
    let floors: Vec<Value> = r
        .floors_json()
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["floor_id"].clone())
        .collect();
    assert_eq!(
        floors,
        [
            json!("u1"),
            json!("u2"),
            json!("g"),
            json!("b"),
            json!("none")
        ]
    );
    let names: Vec<Option<String>> = r.entities.iter().map(|e| e.name.clone()).collect();
    assert_eq!(
        names,
        [Some("Kitchen Renamed".into()), Some("Same".into()), None]
    );
    assert!(r.entities.iter().all(|e| !e.extra.contains_key("name_v2")));

    // Current minor versions are left alone (file order is display order).
    let dir2 = temp("current");
    write_minor(
        &dir2,
        AREA_KEY,
        9,
        json!({"areas": [{"id": "z", "name": "Zebra"}, {"id": "a", "name": "Attic"}]}),
    );
    assert_eq!(
        Registries::load_strict(&dir2).unwrap().areas_json()[0]["area_id"],
        "z"
    );
    for d in [dir, dir2] {
        std::fs::remove_dir_all(d).unwrap();
    }
}

#[test]
fn nulls_and_empty_names_read_like_ha() {
    let dir = temp("nulls");
    write_minor(
        &dir,
        DEVICE_KEY,
        12,
        json!({"devices": [
            {"id": "dev1", "name": "Kitchen", "name_by_user": "", "config_entries": null, "labels": null}
        ]}),
    );
    write_minor(
        &dir,
        ENTITY_KEY,
        22,
        json!({"entities": [
            {"entity_id": "sensor.kitchen_power", "platform": "p", "device_id": "dev1", "has_entity_name": false,
             "original_name": "Kitchen Power", "options": null, "labels": null, "name": null}
        ]}),
    );
    let r = Registries::load_strict(&dir).unwrap();
    // `name_by_user or name`: an empty user name falls back to the name.
    assert_eq!(r.entities_for_display()["entities"][0]["en"], "Power");
    assert_eq!(r.devices_json()[0]["labels"], json!([]));
    std::fs::remove_dir_all(dir).unwrap();
}

/// Linear, not entities × devices: 10k entities over 5k devices in well
/// under the ~95 ms the nested scan took (release); generous for debug.
#[test]
fn display_list_scales_linearly() {
    let devices: Vec<Device> = (0..5000)
        .map(|i| Device {
            id: format!("d{i}"),
            name: Some(format!("Device {i}")),
            ..Default::default()
        })
        .collect();
    let entities: Vec<Entity> = (0..10000)
        .map(|i| Entity {
            entity_id: format!("sensor.e{i}"),
            platform: "p".into(),
            device_id: Some(format!("d{}", i % 5000)),
            original_name: Some(format!("Device {} Power", i % 5000)),
            ..Default::default()
        })
        .collect();
    let r = Registries {
        devices,
        entities,
        ..Default::default()
    };
    let start = std::time::Instant::now();
    let v = r.entities_for_display();
    let took = start.elapsed();
    eprintln!("entities_for_display, 10k entities / 5k devices: {took:?}");
    assert_eq!(v["entities"][0]["en"], "Power");
    assert!(took < std::time::Duration::from_millis(500), "{took:?}");
}

#[test]
fn targets_expand_like_ha() {
    let entity = |id: &str, device: Option<&str>, area: Option<&str>| Entity {
        entity_id: id.into(),
        platform: "met".into(),
        device_id: device.map(Into::into),
        area_id: area.map(Into::into),
        ..Default::default()
    };
    let r = Registries {
        entities: vec![
            entity("weather.a", Some("d1"), None),
            // Its own area wins over its device's.
            entity("weather.b", Some("d1"), Some("garden")),
            Entity {
                entity_category: Some("diagnostic".into()),
                ..entity("sensor.diag", Some("d1"), None)
            },
            Entity {
                hidden_by: Some("user".into()),
                ..entity("sensor.hidden", Some("d1"), None)
            },
            Entity {
                labels: vec!["outside".into()],
                ..entity("weather.c", None, None)
            },
        ],
        devices: vec![Device {
            id: "d1".into(),
            area_id: Some("roof".into()),
            ..Default::default()
        }],
        areas: vec![
            Area {
                id: "roof".into(),
                floor_id: Some("top".into()),
                ..Default::default()
            },
            Area {
                id: "garden".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let t = |v: Value| r.target_entities(v.as_object().unwrap());
    assert_eq!(t(json!({"device_id": "d1"})), ["weather.a", "weather.b"]);
    assert_eq!(t(json!({"area_id": "roof"})), ["weather.a"]);
    assert_eq!(t(json!({"floor_id": ["top"]})), ["weather.a"]);
    assert_eq!(t(json!({"area_id": "garden"})), ["weather.b"]);
    assert_eq!(t(json!({"label_id": "outside"})), ["weather.c"]);
    assert!(t(json!({"device_id": "none", "entity_id": "weather.a"})).is_empty());
}
