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
        "remove_entry" => include_str!("../vectors/remove_entry.json"),
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
        name_by_user: Some("Alex's iPhone".into()),
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
        ..Default::default()
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

/// Review regression: concurrent get-or-create of one unique id makes one
/// row, and each entity's events alternate create/remove in commit order.
#[test]
fn concurrent_writes_are_serialised() {
    let hass = Hass::new();
    let reg = Registry::in_memory(&hass, Registries::default());
    let barrier = std::sync::Barrier::new(32);
    std::thread::scope(|s| {
        for _ in 0..32 {
            s.spawn(|| {
                barrier.wait();
                reg.get_or_create_entity("sensor", "p", "same", None, EntityPatch::default());
            });
        }
    });
    assert_eq!(reg.snapshot().entities.len(), 1);

    let mut rx = hass.bus().subscribe();
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|s| {
        for t in 0..8 {
            let (reg, barrier) = (&reg, &barrier);
            s.spawn(move || {
                barrier.wait();
                for _ in 0..20 {
                    let e = reg.get_or_create_entity(
                        "sensor",
                        "p",
                        "flip",
                        None,
                        EntityPatch::default(),
                    );
                    if t % 2 == 0 {
                        reg.remove_entity(&e.entity_id);
                    }
                }
            });
        }
    });
    let mut last: Option<String> = None;
    for (_, data) in events(&mut rx) {
        if data["entity_id"] != "sensor.p_flip" {
            continue;
        }
        let action = data["action"].as_str().unwrap().to_owned();
        if let Some(prev) = &last {
            assert_ne!(prev, &action, "two {action}s in a row");
        }
        last = Some(action);
    }
    // The registry agrees with the last event.
    let exists = reg
        .snapshot()
        .entities
        .iter()
        .any(|e| e.entity_id == "sensor.p_flip");
    assert_eq!(exists, last.as_deref() == Some("create"));
}

/// HA's `device_info_type == "primary"`: the entry becomes primary when
/// the device has none, or its primary entry is gone or a low-priority
/// integration's.
#[test]
fn primary_config_entries_as_ha() {
    let hass = Hass::new();
    let device = |id: &str, primary: &str| {
        Arc::new(Device {
            id: id.into(),
            identifiers: vec![json!(["mobile_app", id])],
            config_entries: vec![primary.into()],
            primary_config_entry: Some(primary.into()),
            ..Default::default()
        })
    };
    let reg = Registry::in_memory(
        &hass,
        Registries {
            devices: crate::rows![],
            ..Default::default()
        },
    );
    reg.write(|tx| {
        tx.r.devices = vec![
            device("a", "mqtt-entry"),
            device("b", "hue-entry"),
            device("c", "gone-entry"),
        ];
    });
    let info = |id: &str| DeviceInfo {
        config_entry_id: ENTRY.into(),
        identifiers: vec![json!(["mobile_app", id])],
        ..Default::default()
    };
    // Without knowing the entries, an existing primary stays.
    assert_eq!(
        reg.get_or_create_device(info("a"))
            .primary_config_entry
            .as_deref(),
        Some("mqtt-entry")
    );
    let entry = |domain: &str| {
        Some(EntryInfo {
            domain: domain.into(),
            disabled: false,
        })
    };
    reg.set_entry_lookup(Arc::new(move |id: &str| match id {
        "mqtt-entry" => entry("mqtt"),
        "hue-entry" => entry("hue"),
        _ => None,
    }));
    let mut rx = hass.bus().subscribe();
    let a = reg.get_or_create_device(info("a"));
    assert_eq!(a.primary_config_entry.as_deref(), Some(ENTRY));
    assert_eq!(
        events(&mut rx)[0].1["changes"]["primary_config_entry"],
        "mqtt-entry"
    );
    assert_eq!(
        reg.get_or_create_device(info("b"))
            .primary_config_entry
            .as_deref(),
        Some("hue-entry")
    );
    assert_eq!(
        reg.get_or_create_device(info("c"))
            .primary_config_entry
            .as_deref(),
        Some(ENTRY)
    );
}

/// HA's placeholders (`<t:…>`, `<ts:…>`, `<ulid#…>`, `<uuid#…>`) and ours,
/// blanked in the given keys.
fn mask(mut v: Value, keys: &[&str]) -> Value {
    for k in keys {
        if v.get(*k).is_some() {
            v[*k] = json!("…");
        }
    }
    v
}

fn frames(step: &Value) -> Vec<String> {
    let mut out: Vec<String> = step["ws_event_frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            let f: Value = serde_json::from_str(f.as_str().unwrap()).unwrap();
            json!([f["event"]["event_type"], f["event"]["data"]]).to_string()
        })
        .collect();
    out.sort();
    out
}

/// `config_entries_async_remove`: the device and its 21 entities go to the
/// deleted lists, orphaned, as HA's files have them.
#[tokio::test]
async fn clearing_an_entry_as_ha() {
    let hass = Hass::new();
    let (_, r) = before();
    let reg = Registry::in_memory(&hass, r);
    let mut rx = hass.bus().subscribe();
    reg.clear_config_entry(ENTRY);
    let v = vector("remove_entry");
    let step = &v["steps"][1];
    let snap = reg.snapshot();
    assert!(snap.devices.is_empty());
    // Only rows of other entries stay (the fixture's `person.alex`).
    let left: Vec<&str> = snap.entities.iter().map(|e| e.entity_id.as_str()).collect();
    assert_eq!(left, ["person.alex"]);
    let deleted: Value = serde_json::from_str(snap.device_file.deleted[0].get()).unwrap();
    let stamps = ["modified_at", "orphaned_timestamp"];
    assert_eq!(
        mask(deleted, &stamps),
        mask(step["deleted_devices"][0].clone(), &stamps)
    );
    // The deleted device row's keys in HA's order (`identifiers` before `id`).
    let text = snap.device_file.deleted[0].get();
    assert!(text.find("\"identifiers\"").unwrap() < text.find("\"id\"").unwrap());

    // Every deleted entity row as HA's file has it.
    let file: Value = serde_json::from_str(include_str!(
        "../vectors/files/remove_entry__config_entries_async_remove__core.entity_registry"
    ))
    .unwrap();
    let want = file["data"]["deleted_entities"].as_array().unwrap();
    assert_eq!(snap.entity_file.deleted.len(), want.len());
    for w in want {
        let got = snap
            .entity_file
            .deleted
            .iter()
            .map(|raw| serde_json::from_str::<Value>(raw.get()).unwrap())
            .find(|g| g["entity_id"] == w["entity_id"])
            .unwrap();
        assert_eq!(
            mask(got, &stamps),
            mask(w.clone(), &stamps),
            "{}",
            w["entity_id"]
        );
    }
    let mut got: Vec<String> = events(&mut rx)
        .into_iter()
        .map(|(name, data)| json!([name, data]).to_string())
        .collect();
    got.sort();
    // But person's own update of its row (its storage hash), not ours.
    let want: Vec<String> = frames(step)
        .into_iter()
        .filter(|f| !f.contains("person.alex"))
        .collect();
    assert_eq!(got, want);
}

/// `post_registrations_same_device_again`: the tracker row and the device
/// come back from the deleted lists with their ids, then the tracker moves
/// to the new entry.
#[tokio::test]
async fn re_adding_restores_the_tracker_and_device() {
    let hass = Hass::new();
    let (_, r) = before();
    let reg = Registry::in_memory(&hass, r);
    reg.clear_config_entry(ENTRY);
    let mut rx = hass.bus().subscribe();
    const DEVICE: &str = "11111111-2222-4333-8444-555555555555";
    const NEW: &str = "01JNEWENTRY000000000000000";
    // The registration flow: no entry, no device yet.
    let tracker = reg.get_or_create_entity(
        "device_tracker",
        "mobile_app",
        DEVICE,
        None,
        EntityPatch {
            object_id_base: Some(Some(" Phone".into())),
            ..Default::default()
        },
    );
    assert_eq!(tracker.entity_id, "device_tracker.phone");
    let device = reg.get_or_create_device(DeviceInfo {
        config_entry_id: NEW.into(),
        identifiers: vec![json!(["mobile_app", DEVICE])],
        manufacturer: Some("Apple".into()),
        model: Some("iPhone17,1".into()),
        name: Some(" Phone".into()),
        sw_version: Some("27.0".into()),
        ..Default::default()
    });
    assert_eq!(device.id, PHONE);
    assert!(reg.snapshot().device_file.deleted.is_empty());
    // The tracker platform attaches the row.
    reg.get_or_create_entity(
        "device_tracker",
        "mobile_app",
        DEVICE,
        None,
        EntityPatch {
            config_entry_id: Some(Some(NEW.into())),
            device_id: Some(Some(PHONE.into())),
            entity_category: Some(Some("diagnostic".into())),
            original_name: Some(Some(" Phone".into())),
            ..Default::default()
        },
    );
    let v = vector("remove_entry");
    let step = &v["steps"][2];
    let mut got: Vec<String> = events(&mut rx)
        .into_iter()
        .map(|(name, data)| json!([name, data]).to_string())
        .collect();
    got.sort();
    assert_eq!(got, frames(step));
    let row = reg
        .entity_by_unique_id("device_tracker", "mobile_app", DEVICE)
        .unwrap();
    let mut want = step["tracker_row"]["device_tracker.phone"].clone();
    want["config_entry_id"] = json!(NEW);
    assert_eq!(
        mask(serde_json::to_value(&*row).unwrap(), &["modified_at"]),
        mask(want, &["modified_at"])
    );
}

fn dev(id: &str, entries: &[&str]) -> Device {
    let mut subentries = Map::new();
    for e in entries {
        subentries.insert((*e).into(), json!([null]));
    }
    Device {
        id: id.into(),
        identifiers: vec![json!(["test", id])],
        config_entries: entries.iter().map(|e| (*e).into()).collect(),
        config_entries_subentries: subentries,
        primary_config_entry: entries.first().map(|e| (*e).into()),
        ..Default::default()
    }
}

fn ent(id: &str, entry: &str, device: Option<&str>) -> Entity {
    Entity {
        entity_id: id.into(),
        platform: "test".into(),
        unique_id: Some(json!(id)),
        config_entry_id: Some(entry.into()),
        device_id: device.map(str::to_owned),
        ..Default::default()
    }
}

/// Review regressions: removing a device unlinks other entries' entities
/// and child devices; a shared device left with only disabled entries is
/// disabled.
#[tokio::test]
async fn clearing_unlinks_and_disables_as_ha() {
    let hass = Hass::new();
    let mut child = dev("child", &["B"]);
    child.via_device_id = Some("hub".into());
    let reg = Registry::in_memory(
        &hass,
        Registries {
            devices: crate::rows![dev("hub", &["A"]), child, dev("shared", &["A", "C"])],
            entities: crate::rows![
                ent("sensor.hub", "A", Some("hub")),
                ent("sensor.foreign", "B", Some("hub")),
            ],
            ..Default::default()
        },
    );
    reg.set_entry_lookup(Arc::new(|id: &str| {
        Some(EntryInfo {
            domain: "test".into(),
            disabled: id == "C",
        })
    }));
    let mut rx = hass.bus().subscribe();
    reg.clear_config_entry("A");
    let snap = reg.snapshot();
    let find = |id: &str| snap.devices.iter().find(|d| d.id == id).cloned();
    assert!(find("hub").is_none());
    assert_eq!(find("child").unwrap().via_device_id, None);
    let shared = find("shared").unwrap();
    assert_eq!(shared.disabled_by.as_deref(), Some("config_entry"));
    assert_eq!(shared.primary_config_entry, None);
    let foreign = snap
        .entities
        .iter()
        .find(|e| e.entity_id == "sensor.foreign")
        .unwrap();
    assert_eq!(foreign.device_id, None);
    assert!(!snap.entities.iter().any(|e| e.entity_id == "sensor.hub"));
    let got: Vec<Value> = events(&mut rx).into_iter().map(|(_, d)| d).collect();
    // The child's update comes before the hub's removal.
    let child_at = got.iter().position(|d| d["device_id"] == "child").unwrap();
    let hub_at = got
        .iter()
        .position(|d| d["device_id"] == "hub" && d["action"] == "remove")
        .unwrap();
    assert!(child_at < hub_at);
    assert_eq!(got[child_at]["changes"], json!({"via_device_id": "hub"}));
    assert!(
        got.contains(&json!({"action": "update", "entity_id": "sensor.foreign",
                                 "changes": {"device_id": "hub"}}))
    );
    let shared_ev = got.iter().find(|d| d["device_id"] == "shared").unwrap();
    assert_eq!(shared_ev["changes"]["disabled_by"], Value::Null);
}

/// Review regressions: a restore follows the target entry's disabled
/// state, and a taken id falls back to the restored area + name.
#[tokio::test]
async fn restores_follow_the_entry_and_area() {
    let hass = Hass::new();
    let reg = Registry::in_memory(
        &hass,
        Registries {
            areas: vec![crate::Area {
                id: "kitchen".into(),
                name: "Kitchen".into(),
                ..Default::default()
            }],
            entities: crate::rows![
                Entity {
                    area_id: Some("kitchen".into()),
                    name: Some("Temperature".into()),
                    ..ent("sensor.taken", "A", None)
                },
                Entity {
                    disabled_by: Some("config_entry".into()),
                    ..ent("sensor.kept", "A", None)
                },
            ],
            ..Default::default()
        },
    );
    reg.remove_entity("sensor.taken");
    reg.remove_entity("sensor.kept");
    hass.set_state(
        ha_core::EntityId::parse("sensor.taken").unwrap(),
        "x",
        Default::default(),
    );
    let patch = |entry: &str| EntityPatch {
        config_entry_id: Some(Some(entry.into())),
        ..Default::default()
    };
    // Into an enabled entry: the id is taken, so area + name.
    let row = reg.get_or_create_entity("sensor", "test", "sensor.taken", None, patch("A"));
    assert_eq!(row.entity_id, "sensor.kitchen_temperature");
    assert_eq!(row.disabled_by, None);
    // A `config_entry` disable lifts for an enabled entry; a disabled entry
    // disables the restored row.
    reg.set_entry_lookup(Arc::new(|id: &str| {
        Some(EntryInfo {
            domain: "test".into(),
            disabled: id == "D",
        })
    }));
    let row = reg.get_or_create_entity("sensor", "test", "sensor.kept", None, patch("A"));
    assert_eq!(row.disabled_by, None);
    reg.remove_entity("sensor.kept");
    let row = reg.get_or_create_entity("sensor", "test", "sensor.kept", None, patch("D"));
    assert_eq!(row.disabled_by.as_deref(), Some("config_entry"));
}

/// Onboarding's default areas (`async_create`, guarded by name): ids from
/// the name with `_2` when taken, names compared case-folded without
/// spaces, `area_registry_updated` per area, and HA's file layout.
#[tokio::test]
async fn areas_are_created_once_by_name() {
    let dir = std::env::temp_dir().join(format!("hars-areas-{}", rand::random::<u32>()));
    let hass = Hass::new();
    hass.start();
    let mut events = hass.bus().subscribe();
    let registry = Registry::new(&hass, &dir, Registries::default());
    let area = |name: &str, icon: Option<&str>| {
        registry.create_area(NewArea {
            name: name.to_owned(),
            icon: icon.map(str::to_owned),
            ..Default::default()
        })
    };
    let a = area("Living Room", Some("mdi:sofa")).unwrap();
    assert_eq!(
        (a.id.as_str(), a.icon.as_deref()),
        ("living_room", Some("mdi:sofa"))
    );
    assert!(area("living room", None).is_err());
    assert!(area("LivingRoom", None).is_err());
    let b = area("Living_Room", None).unwrap();
    assert_eq!(b.id, "living_room_2");
    let e = events.recv().await.unwrap();
    let EventKind::Custom { name, data } = &e.kind else {
        panic!("{e:?}")
    };
    assert_eq!(name, "area_registry_updated");
    assert_eq!(*data, json!({"action": "create", "area_id": "living_room"}));
    registry.flush().await;
    let file: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join(".storage/core.area_registry")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        (&file["version"], &file["minor_version"]),
        (&json!(1), &json!(9))
    );
    let keys: Vec<&String> = file["data"]["areas"][0]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(keys.len(), 11);
    assert_eq!(file["data"]["areas"][1]["id"], "living_room_2");
    let _ = std::fs::remove_dir_all(dir);
}

/// Start-up's placeholder pass takes the writer's lock for a batch of rows
/// at a time: a change to the registry made while it runs gets its turn
/// after the batch in hand, long before the last row, and a row that is
/// gone by the time the pass reaches it gets no placeholder.
#[test]
fn a_change_waits_for_a_batch_of_the_placeholder_pass() {
    const ROWS: usize = 20_000;
    let last = format!("sensor.row_{}", ROWS - 1);
    // A thread that is given no time between seeing the pass begin and
    // asking for its change proves nothing either way: such a run is tried
    // again. With the lock held for the whole pass, every run is one.
    for _ in 0..10 {
        let hass = Hass::new();
        let entities = (0..ROWS)
            .map(|i| {
                Arc::new(Entity {
                    entity_id: format!("sensor.row_{i}"),
                    ..Default::default()
                })
            })
            .collect();
        let reg = Registry::in_memory(
            &hass,
            Registries {
                entities,
                ..Default::default()
            },
        );
        let written = std::thread::scope(|s| {
            s.spawn(|| reg.write_unavailable_states(&hass));
            // The pass has begun: it holds the lock for its first batch.
            while hass.states().is_empty() {
                std::hint::spin_loop();
            }
            reg.remove_entity(&last);
            hass.states().len()
        });
        if written == ROWS {
            continue;
        }
        assert!(
            hass.states()
                .get(&ha_core::EntityId::parse(&last).unwrap())
                .is_none(),
            "a placeholder was written for a row that was gone"
        );
        assert_eq!(hass.states().len(), ROWS - 1);
        return;
    }
    panic!("the change waited for the whole pass each time");
}

/// Whether the stores can be written is told from what was loaded, so
/// asking is no file access (start-up asks on an async thread): the answer
/// is the same with the files gone.
#[test]
fn what_cant_be_written_is_known_without_the_disk() {
    let (dir, loaded) = before();
    assert_eq!(loaded.unwritable(), None);
    assert_eq!(Registries::default().unwritable(), None);
    // The device file as an older Home Assistant left it.
    let file = dir.join(".storage").join(DEVICE_KEY);
    let text = std::fs::read_to_string(&file).unwrap();
    let current = format!("\"minor_version\": {}", loaded.device_file.minor);
    assert!(text.contains(&current));
    let older = format!("\"minor_version\": {}", loaded.device_file.minor - 1);
    std::fs::write(&file, text.replace(&current, &older)).unwrap();
    let loaded = Registries::load_strict(&dir).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(loaded.unwritable(), Some(DEVICE_KEY));
}

/// The deleted row for a unique id is found however a file spells the
/// id: with an escape, or a character written as `\u….` (valid JSON that
/// another writer may leave), and not confused with a row that has the
/// same text in another field. It comes back with what it had, and a
/// removal replaces it rather than adding a second.
#[test]
fn a_deleted_row_is_found_by_its_unique_id() {
    let raw = |text: String| serde_json::value::RawValue::from_string(text).unwrap();
    for (unique, spelled) in [
        ("plain-1", r#""plain-1""#),
        ("plain-1", r#""\u0070lain-1""#),
        ("quoted \"1\"", r#""quoted \"1\"""#),
        ("caf\u{e9}/1", r#""caf\u00e9/1""#),
    ] {
        let seeded = || Registries {
            entity_file: crate::FileRest {
                deleted: Arc::new(vec![
                    // Another entity's row, with the id's text as its name.
                    raw(format!(
                        r#"{{"entity_id":"sensor.other","platform":"p","unique_id":"other","name":{spelled}}}"#
                    )),
                    raw(format!(
                        r#"{{"entity_id":"sensor.kept","platform":"p","unique_id":{spelled},"icon":"mdi:star"}}"#
                    )),
                ]),
                ..Default::default()
            },
            ..Default::default()
        };
        let ids = |reg: &Registry| -> Vec<Value> {
            let deleted = reg.snapshot().entity_file.deleted.clone();
            deleted
                .iter()
                .map(|d| serde_json::from_str::<Value>(d.get()).unwrap()["unique_id"].clone())
                .collect()
        };
        // It comes back, with its id and what the user had set.
        let hass = Hass::new();
        let reg = Registry::in_memory(&hass, seeded());
        let back = reg.get_or_create_entity("sensor", "p", unique, None, EntityPatch::default());
        assert_eq!(
            (back.entity_id.as_str(), back.icon.as_deref()),
            ("sensor.kept", Some("mdi:star")),
            "{spelled}"
        );
        assert_eq!(ids(&reg), [json!("other")], "{spelled}");
        // Removed again, there is one row for it.
        reg.remove_entity("sensor.kept");
        assert_eq!(ids(&reg), [json!("other"), json!(unique)], "{spelled}");
        // An entity removed while an older row for its id is kept
        // replaces that row.
        let mut with_row = seeded();
        with_row.entities.push(Arc::new(Entity {
            entity_id: "sensor.now".into(),
            platform: "p".into(),
            unique_id: Some(json!(unique)),
            ..Default::default()
        }));
        let reg = Registry::in_memory(&hass, with_row);
        reg.remove_entity("sensor.now");
        assert_eq!(ids(&reg), [json!("other"), json!(unique)], "{spelled}");
    }
}

/// A placeholder state start-up wrote for an entity nothing provides
/// moves with its row to a new id (`cleanup_restored_states`); a state an
/// entity wrote is its entity's to move, and stays.
#[test]
fn a_placeholder_moves_with_its_row() {
    let hass = Hass::new();
    let row = |id: &str| {
        Arc::new(Entity {
            entity_id: id.to_owned(),
            platform: "switchbot_cloud".into(),
            original_name: Some("Rail".into()),
            ..Default::default()
        })
    };
    let reg = Registry::in_memory(
        &hass,
        Registries {
            entities: vec![row("button.rail"), row("button.live")],
            ..Default::default()
        },
    );
    let id = |id: &str| ha_core::EntityId::parse(id).unwrap();
    hass.set_state(id("button.live"), "unknown", Default::default());
    reg.write_unavailable_states(&hass);
    let follows = |_: &str| true;
    reg.rename_entity(
        "button.rail",
        "button.towel_rail",
        &Default::default(),
        &follows,
    )
    .unwrap();
    assert!(hass.states().get(&id("button.rail")).is_none());
    let moved = hass.states().get(&id("button.towel_rail")).unwrap();
    assert_eq!(moved.state.as_str(), "unavailable");
    assert_eq!(moved.attributes["restored"], true);
    // And on, leaving nothing at the id in between.
    reg.rename_entity(
        "button.towel_rail",
        "button.bath",
        &Default::default(),
        &follows,
    )
    .unwrap();
    assert!(hass.states().get(&id("button.towel_rail")).is_none());
    assert_eq!(
        hass.states().get(&id("button.bath")).unwrap().attributes["restored"],
        true
    );
    reg.rename_entity("button.live", "button.other", &Default::default(), &follows)
        .unwrap();
    assert!(hass.states().get(&id("button.live")).is_some());
    assert!(hass.states().get(&id("button.other")).is_none());
}

/// A store read from a file that says minor version 0 is one its save
/// would refuse (`check_minor`), so it is known as unwritable and a change
/// to it is refused where it is made, whoever makes it (onboarding makes
/// areas without going through the WebSocket). Registries made in memory
/// with no version given have no file to spare.
#[tokio::test]
async fn a_file_at_minor_version_0_is_not_written() {
    let dir = std::env::temp_dir().join(format!("hars-minor-0-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".storage")).unwrap();
    let file = dir.join(".storage").join(crate::AREA_KEY);
    let text = r#"{"version":1,"minor_version":0,"key":"core.area_registry","data":{"areas":[]}}"#;
    std::fs::write(&file, text).unwrap();
    let loaded = Registries::load_strict(&dir).unwrap();
    assert_eq!(
        loaded.unwritable_stores().collect::<Vec<_>>(),
        [crate::AREA_KEY]
    );
    assert_eq!(Registries::default().unwritable_stores().count(), 0);
    let hass = Hass::new();
    let reg = Registry::new(&hass, &dir, loaded);
    let new = NewArea {
        name: "Bath".into(),
        ..Default::default()
    };
    assert_eq!(
        reg.create_area(new).err(),
        Some(crate::AreaRefusal::Unwritable)
    );
    assert!(reg.snapshot().areas.is_empty());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
    let _ = std::fs::remove_dir_all(&dir);
}

/// HA's `async_device_modified`: whatever changes of an enabled device, an
/// entity its disable had disabled is enabled again, with its own event.
/// And `_async_get_full_entity_name`: an empty `suggested_object_id` is
/// taken on its own, so the id falls back to the platform and unique id.
#[test]
fn a_device_change_enables_what_its_disable_disabled() {
    let hass = Hass::new();
    let entity = |id: &str, disabled_by: Option<&str>| {
        Arc::new(Entity {
            entity_id: id.to_owned(),
            platform: "switchbot_cloud".into(),
            unique_id: Some(json!("u1")),
            has_entity_name: true,
            device_id: Some("dev".into()),
            disabled_by: disabled_by.map(str::to_owned),
            suggested_object_id: Some(String::new()),
            ..Default::default()
        })
    };
    let reg = Registry::in_memory(
        &hass,
        Registries {
            devices: vec![Arc::new(Device {
                id: "dev".into(),
                name: Some("Rail".into()),
                ..Default::default()
            })],
            entities: vec![
                entity("sensor.stale", Some("device")),
                entity("sensor.users", Some("user")),
            ],
            ..Default::default()
        },
    );
    let mut rx = hass.bus().subscribe();
    let patch = DevicePatch {
        labels: Some(vec!["bath".into()]),
        ..Default::default()
    };
    reg.update_device("dev", &patch, &|_| true).unwrap();
    let r = reg.snapshot();
    assert_eq!(r.entities[0].disabled_by, None);
    assert_eq!(r.entities[1].disabled_by.as_deref(), Some("user"));
    let fired = events(&mut rx);
    assert_eq!(fired.len(), 2, "{fired:?}");
    assert_eq!(
        fired[1].1,
        json!({"action": "update", "entity_id": "sensor.stale",
               "changes": {"disabled_by": "device"}})
    );
    let ids = reg.automatic_entity_ids(&["sensor.stale".to_owned()]);
    assert_eq!(ids[0].1.as_deref(), Some("sensor.switchbot_cloud_u1"));
}

/// `async_get_device`: a device is found by its identifier before any
/// device that only shares an address with it (another integration's row
/// for the same hardware, listed first, is not taken over); with no
/// identifier in common it is the one with the address, spelled as HA
/// spells MAC addresses. A deleted device comes back by its address too,
/// with the id and the user's settings it had.
#[test]
fn devices_are_matched_by_identifier_then_by_address() {
    let hass = Hass::new();
    let device = |id: &str, identifier: Value, mac: &str| {
        Arc::new(Device {
            id: id.to_owned(),
            identifiers: vec![identifier],
            connections: vec![json!(["mac", mac])],
            config_entries: vec!["other".into()],
            name: Some(id.to_owned()),
            ..Default::default()
        })
    };
    let deleted = serde_json::value::RawValue::from_string(
        json!({"id": "old", "identifiers": [["hue", "gone-uuid"]],
               "connections": [["mac", "aa:bb:cc:00:00:09"]], "config_entries": ["e"],
               "config_entries_subentries": {"e": [null]}, "name_by_user": "Mine",
               "area_id": "den", "created_at": "2026-01-01T00:00:00+00:00",
               "modified_at": "2026-01-01T00:00:00+00:00", "orphaned_timestamp": null,
               "labels": [], "disabled_by": null})
        .to_string(),
    )
    .unwrap();
    let reg = Registry::in_memory(
        &hass,
        Registries {
            devices: vec![
                device("theirs", json!(["zha", "x"]), "aa:bb:cc:00:00:01"),
                device("ours", json!(["hue", "uuid-1"]), "aa:bb:cc:00:00:01"),
                device("lone", json!(["zha", "y"]), "aa:bb:cc:00:00:02"),
            ],
            device_file: crate::FileRest {
                deleted: Arc::new(vec![deleted]),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let info = |identifier: &str, mac: &str| DeviceInfo {
        config_entry_id: "e".into(),
        identifiers: vec![json!(["hue", identifier])],
        connections: vec![json!(["mac", mac])],
        ..Default::default()
    };
    // Its own row, though another with the address comes first.
    assert_eq!(
        reg.get_or_create_device(info("uuid-1", "AA-BB-CC-00-00-01"))
            .id,
        "ours"
    );
    let theirs = reg.snapshot().devices[0].clone();
    assert_eq!(theirs.config_entries, ["other"]);
    assert_eq!(theirs.identifiers, [json!(["zha", "x"])]);
    // No identifier in common: the device with the address.
    let merged = reg.get_or_create_device(info("uuid-2", "AABBCC000002"));
    assert_eq!(merged.id, "lone");
    assert!(merged.identifiers.contains(&json!(["hue", "uuid-2"])));
    // A deleted one, by its address: back with what it had.
    let back = reg.get_or_create_device(DeviceInfo {
        suggested_area: Some("Bath".into()),
        ..info("new-uuid", "aa:bb:cc:00:00:09")
    });
    assert_eq!(
        (
            back.id.as_str(),
            back.name_by_user.as_deref(),
            back.area_id.as_deref()
        ),
        ("old", Some("Mine"), Some("den"))
    );
    assert!(
        reg.snapshot().areas.is_empty(),
        "an area was suggested to a device that came back"
    );
}

/// A new device's suggested area is not made when the areas' file can't
/// be written: the device's row (which is saved) would point at an area
/// gone after a restart. An area of that name that exists is used.
#[tokio::test]
async fn an_area_that_cant_be_saved_is_not_suggested() {
    let older = crate::STORES
        .iter()
        .find(|s| s.0 == crate::AREA_KEY)
        .unwrap()
        .2
        - 1;
    let dir = std::env::temp_dir().join(format!("hars-no-area-{}", std::process::id()));
    let hass = Hass::new();
    let registries = Registries {
        areas: vec![crate::Area {
            id: "hall".into(),
            name: "Hall".into(),
            ..Default::default()
        }],
        area_minor: older,
        ..Default::default()
    };
    let reg = Registry::new(&hass, &dir, registries);
    let add = |identifier: &str, area: &str| {
        reg.get_or_create_device(DeviceInfo {
            config_entry_id: "e".into(),
            identifiers: vec![json!(["hue", identifier])],
            suggested_area: Some(area.into()),
            ..Default::default()
        })
    };
    assert_eq!(add("a", "Bath").area_id, None);
    assert_eq!(add("b", "hall").area_id.as_deref(), Some("hall"));
    assert_eq!(reg.snapshot().areas.len(), 1);
}

/// `_async_get_full_entity_name`: only an entity named the old way loses
/// its device's name from the front of its own when its id is made. One
/// with `has_entity_name` keeps it: a scene "Bedroom Wake" of the room
/// "Bedroom" is `scene.bedroom_bedroom_wake`.
#[test]
fn only_old_names_lose_the_device_prefix_in_a_new_id() {
    let hass = Hass::new();
    let reg = Registry::in_memory(&hass, Registries::default());
    let device = reg.get_or_create_device(DeviceInfo {
        config_entry_id: "e".into(),
        identifiers: vec![json!(["hue", "room"])],
        name: Some("Bedroom".into()),
        ..Default::default()
    });
    let make = |unique: &str, has_entity_name: bool| {
        let patch = EntityPatch {
            device_id: Some(Some(device.id.clone())),
            has_entity_name: Some(has_entity_name),
            object_id_base: Some(Some("Bedroom Wake".into())),
            ..Default::default()
        };
        reg.get_or_create_entity("scene", "hue", unique, None, patch)
            .entity_id
            .clone()
    };
    assert_eq!(make("new", true), "scene.bedroom_bedroom_wake");
    assert_eq!(make("old", false), "scene.bedroom_wake");
}

/// What an integration asks at every start, when the rows are there and
/// match, changes nothing: no copy of the registries is made or swapped
/// in (the snapshot stays the very same one), nothing is fired, nothing
/// is saved. A row that differs is still changed.
#[test]
fn asking_for_rows_that_match_copies_and_publishes_nothing() {
    let hass = Hass::new();
    let reg = Registry::in_memory(&hass, Registries::default());
    let device = || DeviceInfo {
        config_entry_id: "e".into(),
        identifiers: vec![json!(["hue", "uuid-1"])],
        connections: vec![json!(["mac", "AA:BB:CC:00:00:01"])],
        manufacturer: Some("Signify".into()),
        model: Some("Lamp".into()),
        name: Some("Lamp".into()),
        suggested_area: Some("Den".into()),
        ..Default::default()
    };
    let row = |name: &str| EntityPatch {
        capabilities: Some(Some(json!({"supported_color_modes": ["xy"]}))),
        config_entry_id: Some(Some("e".into())),
        has_entity_name: Some(true),
        original_name: Some(Some(name.to_owned())),
        unit_of_measurement: Some(None),
        pref_disable_new: true,
        ..Default::default()
    };
    let made = reg.get_or_create_device(device());
    let via = DeviceInfo {
        identifiers: vec![json!(["hue", "uuid-2"])],
        connections: Vec::new(),
        via_device: Some(json!(["hue", "uuid-1"])),
        ..device()
    };
    reg.get_or_create_device(via.clone());
    let entity = reg.get_or_create_entity("light", "hue", "u1", None, row("Lamp"));
    let before = reg.snapshot();
    let mut events = hass.bus().subscribe();
    assert_eq!(reg.get_or_create_device(device()).id, made.id);
    reg.get_or_create_device(via);
    let again = reg.get_or_create_entity("light", "hue", "u1", None, row("Lamp"));
    assert!(Arc::ptr_eq(&again, &entity));
    let same = EntityPatch {
        options: Some(json!({})),
        ..Default::default()
    };
    assert_eq!(reg.update_entity(&entity.entity_id, same), None);
    assert!(
        Arc::ptr_eq(&before, &reg.snapshot()),
        "the registries were copied for a call that changed nothing"
    );
    assert!(events.try_recv().is_err(), "something was fired");
    // One that differs still is.
    reg.get_or_create_entity("light", "hue", "u1", None, row("Reading lamp"));
    let after = reg.snapshot();
    assert!(!Arc::ptr_eq(&before, &after));
    assert_eq!(
        after.entities[0].original_name.as_deref(),
        Some("Reading lamp")
    );
    assert!(events.try_recv().is_ok());
}
