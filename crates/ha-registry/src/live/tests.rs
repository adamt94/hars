//! Against HA 2026.6.1's registry-write runs (`spec-registry-writes.md`,
//! `vectors/`: sanitised fakes; times and ids are placeholders there).

use super::*;
use crate::{DEVICE_KEY, ENTITY_KEY};

const ENTRY: &str = "01JFAKEMOBILEAPPENTRY00000";
const WEBHOOK: &str = "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f";
const PHONE: &str = "d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0";

fn vector(name: &str) -> Value {
    let text = match name {
        "register_new" => include_str!("../vectors/register_new.json"),
        "update_registration" => include_str!("../vectors/update_registration.json"),
        "entity_ids" => include_str!("../vectors/entity_ids.json"),
        _ => unreachable!(),
    };
    serde_json::from_str(text).unwrap()
}

/// HA's files before the runs: the phone's 21 rows and its device.
fn before() -> (std::path::PathBuf, Registries) {
    let dir = std::env::temp_dir().join(format!(
        "hars-live-{}-{}",
        std::process::id(),
        rand::random::<u32>()
    ));
    std::fs::create_dir_all(dir.join(".storage")).unwrap();
    let files = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/vectors/files");
    for key in [ENTITY_KEY, DEVICE_KEY] {
        std::fs::copy(
            files.join(format!("before__{key}")),
            dir.join(".storage").join(key),
        )
        .unwrap();
    }
    let r = Registries::load_strict(&dir).unwrap();
    (dir, r)
}

fn events(rx: &mut tokio::sync::broadcast::Receiver<Arc<Event>>) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if let EventKind::Custom { name, data } = &e.kind {
            out.push((name.to_string(), data.clone()));
        }
    }
    out
}

/// A row as JSON with HA's placeholders (`<t:…>`, `<uuid#…>`) and ours
/// both blanked.
fn row_json(v: Value) -> Value {
    let mut v = v;
    for k in ["created_at", "modified_at", "id"] {
        if v.get(k).is_some() {
            v[k] = json!("…");
        }
    }
    v
}

/// The sensor patch `mobile_app` passes when a sensor is registered.
fn sensor(name: &str, icon: &str, unit: Option<&str>, state_class: Option<&str>) -> EntityPatch {
    EntityPatch {
        capabilities: Some(state_class.map(|s| json!({"state_class": s}))),
        config_entry_id: Some(Some(ENTRY.into())),
        device_id: Some(Some(PHONE.into())),
        entity_category: Some(None),
        has_entity_name: Some(false),
        object_id_base: Some(Some(format!(" Phone {name}"))),
        original_device_class: Some(None),
        original_icon: Some(Some(icon.into())),
        original_name: Some(Some(format!(" Phone {name}"))),
        supported_features: Some(0),
        translation_key: Some(None),
        unit_of_measurement: Some(unit.map(str::to_owned)),
        ..Default::default()
    }
}

#[test]
fn available_ids_as_ha() {
    let hass = Hass::new();
    let (_, r) = before();
    hass.set_state(
        ha_core::EntityId::parse("sensor.taken").unwrap(),
        "x",
        Default::default(),
    );
    let v = vector("entity_ids");
    let table = &v["steps"][1]["result"];
    let mut n = 0;
    for (input, want) in table.as_object().unwrap() {
        // Python repr of the input: '…'.
        let Some(text) = input.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) else {
            continue;
        };
        // hars's slugify has a fixed transliteration table (spec U6).
        if text == "\u{216b} \u{fb01}" {
            continue;
        }
        assert_eq!(
            available_entity_id(&r, &hass, "sensor", text),
            *want,
            "{input}"
        );
        n += 1;
    }
    assert!(n >= 10);
    // A state, no row.
    assert_eq!(
        available_entity_id(&r, &hass, "sensor", "taken"),
        "sensor.taken_2"
    );
}

#[test]
fn new_ids_take_device_and_area_names() {
    let hass = Hass::new();
    let (_, mut r) = before();
    r.areas.push(crate::Area {
        id: "bedroom".into(),
        name: "Bedroom".into(),
        ..Default::default()
    });
    r.devices.push(Arc::new(Device {
        id: "tablet".into(),
        name: Some(" Phone".into()),
        area_id: Some("bedroom".into()),
        ..Default::default()
    }));
    r.devices.push(Arc::new(Device {
        id: "renamed".into(),
        name: Some(" Phone".into()),
        name_by_user: Some("Adam's iPhone".into()),
        area_id: Some("bedroom".into()),
        ..Default::default()
    }));
    let reg = Registry::in_memory(&hass, r);
    let create = |unique: &str, device: &str, base: Option<&str>| {
        reg.get_or_create_entity(
            "sensor",
            "mobile_app",
            unique,
            None,
            EntityPatch {
                device_id: Some(Some(device.into())),
                object_id_base: Some(base.map(str::to_owned)),
                ..Default::default()
            },
        )
        .entity_id
        .clone()
    };
    let v = vector("entity_ids");
    let want = &v["steps"][2]["result"];
    assert_eq!(
        create("speed", PHONE, Some(" Phone Speed")),
        want["object_id_base ' Phone Speed' (device ' Phone', no area)"]
    );
    assert_eq!(
        create("speed2", PHONE, Some("Speed")),
        want["object_id_base 'Speed' (no device prefix)"]
    );
    assert_eq!(create("none", PHONE, None), want["object_id_base None"]);
    assert_eq!(
        create("heading", "tablet", Some(" Phone Heading")),
        want["object_id_base ' Phone Heading' (device in area 'Bedroom')"]
    );
    assert_eq!(
        create("altitude", "renamed", Some(" Phone Altitude")),
        want["object_id_base ' Phone Altitude' (device renamed by user, in area)"]
    );
}

/// `register_7_new_sensors`: the rows HA created (but `sensor.phone_pressure`'s
/// display-precision option, which `hars` doesn't model; spec R6).
#[tokio::test]
async fn new_sensor_rows_as_ha() {
    let hass = Hass::new();
    let (_, r) = before();
    hass.set_state(
        ha_core::EntityId::parse("sensor.phone_ghost").unwrap(),
        "x",
        Default::default(),
    );
    let reg = Registry::in_memory(&hass, r);
    let mut rx = hass.bus().subscribe();
    let uid = |u: &str| format!("{WEBHOOK}_{u}");
    let new = |domain: &str, u: &str, disabled: bool, patch: EntityPatch| {
        reg.get_or_create_entity(
            domain,
            "mobile_app",
            &uid(u),
            disabled.then(|| "integration".to_owned()),
            patch,
        )
    };
    new(
        "sensor",
        "steps",
        false,
        sensor("Steps", "mdi:walk", Some("steps"), Some("total_increasing")),
    );
    new(
        "sensor",
        "pressure",
        false,
        EntityPatch {
            original_device_class: Some(Some("pressure".into())),
            entity_category: Some(Some("diagnostic".into())),
            ..sensor("Pressure", "mdi:gauge", Some("kPa"), Some("measurement"))
        },
    );
    new(
        "binary_sensor",
        "focus",
        false,
        EntityPatch {
            capabilities: Some(None),
            ..sensor("Focus", "mdi:moon-waning-crescent", None, None)
        },
    );
    new(
        "sensor",
        "activity",
        true,
        sensor("Activity", "mdi:walk", None, None),
    );
    new(
        "sensor",
        "battery_level_2",
        false,
        EntityPatch {
            original_device_class: Some(Some("battery".into())),
            ..sensor("Battery Level", "mdi:cellphone", Some("%"), None)
        },
    );
    new(
        "sensor",
        "ghost",
        false,
        sensor("Ghost", "mdi:cellphone", None, None),
    );
    let mut noname = sensor("", "mdi:cellphone", None, None);
    noname.object_id_base = Some(Some(" Phone ".into()));
    noname.original_name = Some(Some(" Phone ".into()));
    new("sensor", "noname", false, noname);

    let v = vector("register_new");
    let step = &v["steps"][1];
    let snap = reg.snapshot();
    let mut n = 0;
    for (entity_id, want) in step["rows"].as_object().unwrap() {
        let got = snap
            .entities
            .iter()
            .find(|e| &e.entity_id == entity_id)
            .unwrap_or_else(|| panic!("no {entity_id}"));
        let mut want = row_json(want.clone());
        if entity_id == "sensor.phone_pressure" {
            want["options"] = json!({});
        }
        assert_eq!(
            row_json(serde_json::to_value(&**got).unwrap()),
            want,
            "{entity_id}"
        );
        n += 1;
    }
    assert_eq!(n, 7);
    let creates: Vec<Value> = events(&mut rx)
        .into_iter()
        .map(|(name, data)| {
            assert_eq!(name, "entity_registry_updated");
            data
        })
        .collect();
    assert_eq!(creates.len(), 7);
    assert_eq!(
        creates[0],
        json!({"action": "create", "entity_id": "sensor.phone_steps"})
    );
    // The display list gets the enabled new rows.
    let display = snap.entities_for_display();
    let ids: Vec<&str> = display["entities"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["ei"].as_str())
        .collect();
    assert!(ids.contains(&"sensor.phone_steps") && !ids.contains(&"sensor.phone_activity"));
}

#[tokio::test]
async fn updates_fire_old_values_and_no_ops_nothing() {
    let hass = Hass::new();
    let (_, r) = before();
    let reg = Registry::in_memory(&hass, r);
    let mut rx = hass.bus().subscribe();
    let id = "sensor.phone_ssid";
    let before_row = reg
        .snapshot()
        .entities
        .iter()
        .find(|e| e.entity_id == id)
        .unwrap()
        .clone();
    // HA's disable step: `{"disabled_by": null, "original_icon": "mdi:wifi-off"}`.
    let changes = reg
        .update_entity(
            id,
            EntityPatch {
                disabled_by: Some(Some("integration".into())),
                original_icon: Some(Some("mdi:wifi".into())),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        Value::Object(changes),
        json!({"disabled_by": null, "original_icon": before_row.original_icon})
    );
    let after = reg.snapshot();
    let row = after.entities.iter().find(|e| e.entity_id == id).unwrap();
    assert_eq!(row.disabled_by.as_deref(), Some("integration"));
    assert_ne!(row.modified_at, before_row.modified_at);
    assert_eq!(row.created_at, before_row.created_at);
    assert_eq!(events(&mut rx).len(), 1);
    // The same again: nothing.
    assert!(
        reg.update_entity(
            id,
            EntityPatch {
                disabled_by: Some(Some("integration".into())),
                ..Default::default()
            },
        )
        .is_none()
    );
    assert!(events(&mut rx).is_empty());
    // get_or_create on an existing row is an update.
    let uid = format!("{WEBHOOK}_connectivity_ssid");
    let got = reg.get_or_create_entity(
        "sensor",
        "mobile_app",
        &uid,
        None,
        EntityPatch {
            disabled_by: Some(None),
            ..Default::default()
        },
    );
    assert_eq!(got.entity_id, id);
    assert!(got.disabled_by.is_none());
}

/// A removed row goes to `deleted_entities` in HA's shape.
#[tokio::test]
async fn removed_rows_are_kept_as_deleted() {
    let hass = Hass::new();
    let (_, r) = before();
    let reg = Registry::in_memory(&hass, r);
    let mut rx = hass.bus().subscribe();
    reg.remove_entity("sensor.phone_storage");
    reg.remove_entity("sensor.nope");
    assert_eq!(
        events(&mut rx),
        [(
            "entity_registry_updated".to_owned(),
            json!({"action": "remove", "entity_id": "sensor.phone_storage"})
        )]
    );
    let snap = reg.snapshot();
    assert!(
        !snap
            .entities
            .iter()
            .any(|e| e.entity_id == "sensor.phone_storage")
    );
    let deleted: Value = serde_json::from_str(snap.entity_file.deleted[0].get()).unwrap();
    // The keys in HA's order (the vector's deleted `speed` row).
    let v = vector("entity_ids");
    let want = &v["steps"][2]["result"]["deleted row for 'speed' (as stored)"];
    let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    let raw_keys: Vec<String> = {
        // Key order as written (a Map would sort it).
        let text = snap.entity_file.deleted[0].get();
        want.as_object()
            .unwrap()
            .keys()
            .filter(|k| text.contains(&format!("\"{k}\":")))
            .cloned()
            .collect()
    };
    assert_eq!(raw_keys, keys(want));
    let text = snap.entity_file.deleted[0].get();
    let positions: Vec<usize> = keys(want)
        .iter()
        .map(|k| text.find(&format!("\"{k}\":")).unwrap())
        .collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{text}");
    assert_eq!(deleted["orphaned_timestamp"], Value::Null);
    assert_eq!(deleted["config_entry_id"], ENTRY);
    // Removing it again after re-adding replaces the deleted row.
    reg.get_or_create_entity(
        "sensor",
        "mobile_app",
        &format!("{WEBHOOK}_storage"),
        None,
        EntityPatch::default(),
    );
    reg.remove_entity("sensor.phone_storage");
    assert_eq!(reg.snapshot().entity_file.deleted.len(), 1);
}

/// `update_registration`: the OS version, then the model and name.
#[tokio::test]
async fn device_updates_as_ha() {
    let hass = Hass::new();
    let (_, r) = before();
    let reg = Registry::in_memory(&hass, r);
    let mut rx = hass.bus().subscribe();
    let info = |model: &str, name: &str, sw: &str| DeviceInfo {
        config_entry_id: ENTRY.into(),
        identifiers: vec![json!([
            "mobile_app",
            "11111111-2222-4333-8444-555555555555"
        ])],
        manufacturer: Some("Apple".into()),
        model: Some(model.into()),
        name: Some(name.into()),
        sw_version: Some(sw.into()),
    };
    let v = vector("update_registration");
    let frames = |step: usize| -> Vec<(String, Value)> {
        v["steps"][step]["ws_event_frames"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                let f: Value = serde_json::from_str(f.as_str().unwrap()).unwrap();
                (
                    f["event"]["event_type"].as_str().unwrap().to_owned(),
                    f["event"]["data"].clone(),
                )
            })
            .collect()
    };
    // Nothing new: nothing at all.
    reg.get_or_create_device(info("iPhone17,1", " Phone", "27.0"));
    assert!(events(&mut rx).is_empty());

    let d = reg.get_or_create_device(info("iPhone17,1", " Phone", "27.0.1"));
    assert_eq!(d.id, PHONE);
    assert_eq!(d.sw_version.as_deref(), Some("27.0.1"));
    assert_eq!(events(&mut rx), frames(1));

    reg.get_or_create_device(info("iPhone18,1", " Phone Pro", "27.0.1"));
    let mut got = events(&mut rx);
    let mut want = frames(2);
    // HA delivers in nested order; compare as sets.
    got.sort_by_key(|(_, d)| d.to_string());
    want.sort_by_key(|(_, d)| d.to_string());
    assert_eq!(got, want);
    assert_eq!(want.len(), 21);
}

/// Saves land 180 s after the last change before start, 10 s once
/// running, in HA's layout.
#[tokio::test(start_paused = true)]
async fn saves_are_delayed_as_ha() {
    let hass = Hass::new();
    let (dir, r) = before();
    let reg = Registry::new(&hass, &dir, r);
    let file = dir.join(".storage").join(ENTITY_KEY);
    let original = std::fs::read(&file).unwrap();
    let advance = |s: u64| async move {
        tokio::time::advance(Duration::from_secs(s)).await;
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    reg.update_entity(
        "sensor.phone_ssid",
        EntityPatch {
            original_icon: Some(Some("mdi:test-one".into())),
            ..Default::default()
        },
    );
    advance(170).await;
    assert_eq!(std::fs::read(&file).unwrap(), original);
    advance(20).await;
    let saved = std::fs::read_to_string(&file).unwrap();
    assert!(saved.contains("mdi:test-one"));
    // Reloads to the same snapshot.
    let again = Registries::load_strict(&dir).unwrap();
    assert_eq!(again.entities.len(), reg.snapshot().entities.len());

    hass.begin_start();
    hass.finish_start();
    reg.update_entity(
        "sensor.phone_ssid",
        EntityPatch {
            original_icon: Some(Some("mdi:test-two".into())),
            ..Default::default()
        },
    );
    advance(9).await;
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("mdi:test-one")
    );
    advance(2).await;
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("mdi:test-two")
    );
    // A flush with nothing pending writes nothing.
    std::fs::write(&file, b"untouched").unwrap();
    reg.flush().await;
    assert_eq!(std::fs::read(&file).unwrap(), b"untouched");
    let _ = std::fs::remove_dir_all(&dir);
}
