use super::*;

#[test]
fn slugs_like_ha() {
    for (text, slug) in [
        ("Living Room", "living_room"),
        ("Adam's Phone", "adam_s_phone"),
        ("Café Ünïcode", "cafe_unicode"),
        ("  Straße  ", "strasse"),
        ("Zone #2!", "zone_2"),
        ("???", "unknown"),
        ("Đorđe", "dorde"),
    ] {
        assert_eq!(slugify(text), slug, "{text}");
    }
}

fn write(dir: &Path, key: &str, version: u32, data: Value) {
    std::fs::create_dir_all(dir.join(".storage")).unwrap();
    let v = json!({"version": version, "minor_version": 1, "key": key, "data": data});
    std::fs::write(dir.join(".storage").join(key), v.to_string()).unwrap();
}

#[test]
fn creates_has_zone_and_person_entities() {
    let dir = std::env::temp_dir().join(format!("hars-components-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(
        &dir,
        ZONE_KEY,
        1,
        json!({"items": [
            {"id": "work", "name": "Work", "latitude": 52.1, "longitude": 4.3, "radius": 250, "passive": false, "icon": "mdi:briefcase"},
            {"id": "gym", "name": "Gym", "latitude": 52.2, "longitude": 4.4, "radius": 100, "passive": true, "icon": null}
        ]}),
    );
    write(
        &dir,
        PERSON_KEY,
        2,
        json!({"items": [
            {"id": "p1", "name": "Adam", "user_id": "u1", "device_trackers": ["device_tracker.iphone"], "picture": "/api/image/serve/x/512x512"},
            {"id": "p2", "name": "Guest", "user_id": null, "device_trackers": []}
        ]}),
    );
    let components = Components::load_strict(&dir).unwrap();
    let registries = Registries {
        entities: ha_registry::rows![
            // Renamed in HA's UI: the registry's entity id and name win.
            ha_registry::Entity {
                entity_id: "person.me".into(),
                platform: "person".into(),
                unique_id: Some(json!("p1")),
                name: Some("Me".into()),
                ..Default::default()
            },
            ha_registry::Entity {
                entity_id: "zone.gym".into(),
                platform: "zone".into(),
                unique_id: Some(json!("gym")),
                disabled_by: Some("user".into()),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let hass = Hass::new();
    hass.set_config(ha_core::config::CoreConfig {
        location_name: "Home Sweet Home".into(),
        latitude: 52.37,
        ..Default::default()
    });
    components.setup(&hass, &registries);

    let state = |id: &str| hass.states().get(&EntityId::parse(id).unwrap());
    let home = state("zone.home").unwrap();
    assert_eq!(home.state, "0");
    assert_eq!(
        Value::Object(home.attributes.clone()),
        json!({"latitude": 52.37, "longitude": 0.0, "radius": 100, "passive": false, "persons": [],
               "editable": true, "icon": "mdi:home", "friendly_name": "Home Sweet Home"})
    );
    let work = state("zone.work").unwrap();
    assert_eq!(work.attributes["radius"], 250);
    assert_eq!(work.attributes["icon"], "mdi:briefcase");
    assert!(state("zone.gym").is_none(), "disabled in the registry");

    let me = state("person.me").unwrap();
    assert_eq!(me.state, "unknown");
    assert_eq!(
        Value::Object(me.attributes.clone()),
        json!({"editable": true, "id": "p1", "device_trackers": ["device_tracker.iphone"], "in_zones": [],
               "user_id": "u1", "entity_picture": "/api/image/serve/x/512x512", "friendly_name": "Me"})
    );
    let guest = state("person.guest").unwrap();
    assert!(guest.attributes.get("user_id").is_none());

    assert_eq!(components.zones_json().as_array().unwrap().len(), 2);
    assert_eq!(components.persons_json()["config"], json!([]));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn duplicate_names_get_suffixes_and_bad_stores_dont_stop_the_server() {
    let dir = std::env::temp_dir().join(format!("hars-components-dup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(
        &dir,
        ZONE_KEY,
        1,
        json!({"items": [
            {"id": "a", "name": "Shop", "latitude": 1, "longitude": 1},
            {"id": "b", "name": "Shop", "latitude": 2, "longitude": 2}
        ]}),
    );
    std::fs::write(dir.join(".storage").join(PERSON_KEY), "{bad").unwrap();
    assert!(Components::load_strict(&dir).is_err());
    let components = Components::load(&dir);
    assert!(components.persons().is_empty());
    let hass = Hass::new();
    components.setup(&hass, &Registries::default());
    assert!(
        hass.states()
            .get(&EntityId::parse("zone.shop").unwrap())
            .is_some()
    );
    assert!(
        hass.states()
            .get(&EntityId::parse("zone.shop_2").unwrap())
            .is_some()
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// A person registered as `person.alex` keeps that id even when an
/// unregistered "Alex" comes first.
#[test]
fn registered_ids_are_reserved() {
    let dir = std::env::temp_dir().join(format!("hars-components-reserved-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(
        &dir,
        PERSON_KEY,
        2,
        json!({"items": [
            {"id": "new", "name": "Alex", "device_trackers": []},
            {"id": "old", "name": "Alexander", "device_trackers": []}
        ]}),
    );
    let registries = Registries {
        entities: ha_registry::rows![ha_registry::Entity {
            entity_id: "person.alex".into(),
            platform: "person".into(),
            unique_id: Some(json!("old")),
            ..Default::default()
        }],
        ..Default::default()
    };
    let hass = Hass::new();
    Components::load_strict(&dir)
        .unwrap()
        .setup(&hass, &registries);
    let id = |s: &str| hass.states().get(&EntityId::parse(s).unwrap());
    assert_eq!(id("person.alex").unwrap().attributes["id"], "old");
    assert_eq!(id("person.alex_2").unwrap().attributes["id"], "new");
    std::fs::remove_dir_all(dir).unwrap();
}

/// HA's person store migration (version 1 `persons`) and its fix for
/// `device_trackers: null`.
#[test]
fn old_person_stores_load_like_ha() {
    let dir = std::env::temp_dir().join(format!("hars-components-v1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(
        &dir,
        PERSON_KEY,
        1,
        json!({"persons": [
            {"id": "p1", "name": "Adam", "user_id": "u1", "device_trackers": null}
        ]}),
    );
    for c in [
        Components::load_strict(&dir).unwrap(),
        Components::load(&dir),
    ] {
        assert_eq!(c.persons().len(), 1);
        assert_eq!(c.persons()[0]["device_trackers"], json!([]));
    }
    let hass = Hass::new();
    Components::load(&dir).setup(&hass, &Registries::default());
    let adam = hass
        .states()
        .get(&EntityId::parse("person.adam").unwrap())
        .unwrap();
    assert_eq!(adam.attributes["device_trackers"], json!([]));
    std::fs::remove_dir_all(dir).unwrap();
}

fn tracking_setup(dir: &Path) -> (Hass, Arc<Components>) {
    write(
        dir,
        PERSON_KEY,
        2,
        json!({"items": [{"id": "adam", "name": "adam",
                          "user_id": "00112233445566778899aabbccddeeff",
                          "device_trackers": ["device_tracker.phone"]}]}),
    );
    let hass = Hass::new();
    hass.set_config(ha_core::config::CoreConfig {
        latitude: 0.5,
        longitude: 0.5,
        radius: 100,
        location_name: "Home".into(),
        ..Default::default()
    });
    hass.set_restore(ha_core::restore::RestoreState::load(dir));
    let components = Arc::new(Components::load_strict(dir).unwrap());
    components.setup(&hass, &Registries::default());
    components.track(&hass);
    (hass, components)
}

/// Let the tracking task catch up.
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

fn state_of(hass: &Hass, id: &str) -> Value {
    let s = hass.states().get(&EntityId::parse(id).unwrap()).unwrap();
    json!({"state": s.state.as_str(), "attributes": s.attributes})
}

/// HA 2026.6.1's `location.json`: the phone's tracker state after each
/// `update_location`, and the person and home zone HA computed from it.
#[tokio::test]
async fn people_and_zones_follow_trackers_as_ha() {
    let dir = std::env::temp_dir().join(format!("hars-tracking-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (hass, components) = tracking_setup(&dir);
    let v: Value = serde_json::from_str(include_str!("vectors/location.json")).unwrap();
    let steps = v["exchanges"].as_array().unwrap();
    let initial = &steps[0]["initial"];
    let set = |s: &Value| {
        hass.set_state(
            EntityId::parse("device_tracker.phone").unwrap(),
            s["state"].as_str().unwrap(),
            s["attributes"].as_object().unwrap().clone(),
        );
    };
    set(&initial["device_tracker.phone"]);
    hass.begin_start();
    components.start(&hass);
    settle().await;
    for id in ["person.adam", "zone.home"] {
        assert_eq!(state_of(&hass, id), initial[id], "initial {id}");
    }
    let mut n = 0;
    for step in &steps[1..] {
        let Some(after) = step.get("after") else {
            continue;
        };
        set(&after["device_tracker.phone"]);
        settle().await;
        for id in ["person.adam", "zone.home"] {
            assert_eq!(state_of(&hass, id), after[id], "step {n} {id}");
        }
        n += 1;
    }
    assert_eq!(n, 20);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Before start, a person is their restored state, as its own source;
/// trackers are read only from `homeassistant_start`.
#[tokio::test]
async fn people_restore_then_follow_from_start() {
    let dir = std::env::temp_dir().join(format!("hars-person-restore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(
        &dir,
        ha_core::restore::KEY,
        1,
        json!([{
            "state": {"entity_id": "person.adam", "state": "home",
                      "attributes": {"in_zones": ["zone.home"], "latitude": 0.5,
                                     "longitude": 0.5, "gps_accuracy": 5.0,
                                     "source": "device_tracker.phone"},
                      "last_changed": "2026-11-14T08:00:00+00:00",
                      "last_reported": "2026-11-14T08:00:00+00:00",
                      "last_updated": "2026-11-14T08:00:00+00:00",
                      "context": {"id": "01JFAKECONTEXT000000000000", "parent_id": null, "user_id": null}},
            "extra_data": null,
            "last_seen": "2026-11-14T08:00:00+00:00"
        }]),
    );
    let (hass, components) = tracking_setup(&dir);
    let person = state_of(&hass, "person.adam");
    assert_eq!(person["state"], "home");
    assert_eq!(person["attributes"]["source"], "person.adam");
    assert_eq!(person["attributes"]["gps_accuracy"], 5.0);
    // The home zone counts the restored person.
    assert_eq!(state_of(&hass, "zone.home")["state"], "1");

    // A tracker change before start is ignored.
    hass.set_state(
        EntityId::parse("device_tracker.phone").unwrap(),
        "not_home",
        json!({"source_type": "gps"}).as_object().unwrap().clone(),
    );
    settle().await;
    assert_eq!(state_of(&hass, "person.adam")["state"], "home");
    components.start(&hass);
    settle().await;
    assert_eq!(state_of(&hass, "person.adam")["state"], "not_home");
    assert_eq!(state_of(&hass, "zone.home")["state"], "0");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A router (non-GPS) tracker at home wins over GPS, with the home zone's
/// coordinates; unknown trackers don't count.
#[tokio::test]
async fn a_non_gps_home_tracker_wins() {
    let dir = std::env::temp_dir().join(format!("hars-person-nongps-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(
        &dir,
        PERSON_KEY,
        2,
        json!({"items": [{"id": "p", "name": "P", "user_id": null,
                          "device_trackers": ["device_tracker.gps", "device_tracker.router", "device_tracker.off"]}]}),
    );
    let hass = Hass::new();
    hass.set_config(ha_core::config::CoreConfig {
        latitude: 1.0,
        longitude: 2.0,
        ..Default::default()
    });
    let components = Arc::new(Components::load_strict(&dir).unwrap());
    components.setup(&hass, &Registries::default());
    components.track(&hass);
    let set = |id: &str, state: &str, attrs: Value| {
        hass.set_state(
            EntityId::parse(id).unwrap(),
            state,
            attrs.as_object().unwrap().clone(),
        );
    };
    set(
        "device_tracker.off",
        "unknown",
        json!({"source_type": "router"}),
    );
    set(
        "device_tracker.gps",
        "not_home",
        json!({"source_type": "gps", "latitude": 9.0, "longitude": 9.0, "gps_accuracy": 3}),
    );
    components.start(&hass);
    settle().await;
    let p = state_of(&hass, "person.p");
    assert_eq!(p["state"], "not_home");
    assert_eq!(p["attributes"]["source"], "device_tracker.gps");
    assert!(p["attributes"].get("user_id").is_none());

    set(
        "device_tracker.router",
        "home",
        json!({"source_type": "router"}),
    );
    settle().await;
    let p = state_of(&hass, "person.p");
    assert_eq!(p["state"], "home");
    assert_eq!(p["attributes"]["source"], "device_tracker.router");
    assert_eq!(p["attributes"]["latitude"], 1.0);
    assert_eq!(p["attributes"]["longitude"], 2.0);
    assert_eq!(
        state_of(&hass, "zone.home")["attributes"]["persons"],
        json!(["person.p"])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Occupancy is settled when `start` returns: automations attached right
/// after see the home zone already at 1, not a later "arrival".
#[tokio::test]
async fn zones_are_settled_when_start_returns() {
    let dir = std::env::temp_dir().join(format!("hars-tracking-start-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (hass, components) = tracking_setup(&dir);
    hass.set_state(
        EntityId::parse("device_tracker.phone").unwrap(),
        "home",
        json!({"source_type": "gps"}).as_object().unwrap().clone(),
    );
    let mut feed = hass.subscribe_state_changes();
    components.start(&hass);
    // No yield: the task hasn't run.
    assert_eq!(state_of(&hass, "person.adam")["state"], "home");
    assert_eq!(state_of(&hass, "zone.home")["state"], "1");
    // Person, then zone, in HA's order.
    let order: Vec<String> = std::iter::from_fn(|| feed.try_recv())
        .filter_map(|e| match &e.kind {
            ha_core::EventKind::StateChanged { entity_id, .. } => Some(entity_id.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(order, ["person.adam", "zone.home"]);
    // The task doesn't write either again once it catches up.
    settle().await;
    assert!(feed.try_recv().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A new tracker joins the user's person, and the store is written (at
/// the latest when flushed at stop).
#[tokio::test]
async fn trackers_join_the_users_person_and_are_saved() {
    let dir = std::env::temp_dir().join(format!("hars-person-add-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (hass, components) = tracking_setup(&dir);
    components.save_to(&dir);
    components.start(&hass);
    components.add_user_device_tracker(
        &hass,
        "00112233445566778899aabbccddeeff",
        "device_tracker.new",
    );
    // Again: no duplicate.
    components.add_user_device_tracker(
        &hass,
        "00112233445566778899aabbccddeeff",
        "device_tracker.new",
    );
    // Another user's id changes nothing.
    components.add_user_device_tracker(&hass, "someone", "device_tracker.other");
    assert_eq!(
        components.persons()[0]["device_trackers"],
        json!(["device_tracker.phone", "device_tracker.new"])
    );
    assert_eq!(
        state_of(&hass, "person.adam")["attributes"]["device_trackers"],
        json!(["device_tracker.phone", "device_tracker.new"])
    );
    // The person follows it now.
    hass.set_state(
        EntityId::parse("device_tracker.new").unwrap(),
        "home",
        json!({"source_type": "router"})
            .as_object()
            .unwrap()
            .clone(),
    );
    settle().await;
    assert_eq!(state_of(&hass, "person.adam")["state"], "home");
    components.flush().await;
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join(".storage/person")).unwrap())
            .unwrap();
    assert_eq!(saved["version"], 2);
    assert_eq!(
        saved["data"]["items"][0]["device_trackers"],
        json!(["device_tracker.phone", "device_tracker.new"])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Onboarding's person (`async_create_person`), and `zone.home` following
/// the location settings (`core_config_updated`).
#[tokio::test]
async fn a_new_person_and_a_moved_home() {
    let dir = std::env::temp_dir().join(format!("hars-new-person-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let hass = Hass::new();
    hass.start();
    let registry = ha_registry::Registry::in_memory(&hass, Registries::default());
    let components = Arc::new(Components::load(&dir));
    components.setup(&hass, &registry.snapshot());
    components.track(&hass);
    components.save_to(&dir);
    assert!(
        components
            .create_person(&hass, Some(&registry), "", None)
            .is_none()
    );
    let id = components
        .create_person(&hass, Some(&registry), "Test Name", Some("u1"))
        .unwrap();
    assert_eq!(id.as_str(), "person.test_name");
    // A second "Test Name" gets the next id, as `IDManager` gives.
    let id2 = components
        .create_person(&hass, Some(&registry), "Test Name", None)
        .unwrap();
    assert_eq!(id2.as_str(), "person.test_name_2");
    assert_eq!(
        components.persons()[1],
        json!({"id": "test_name_2", "name": "Test Name", "user_id": null, "device_trackers": []})
            .as_object()
            .unwrap()
            .clone()
    );
    assert_eq!(state_of(&hass, "person.test_name")["state"], "unknown");
    let row = registry
        .entity_by_unique_id("person", "person", "test_name")
        .unwrap();
    assert_eq!(row.entity_id, "person.test_name");

    let home = state_of(&hass, "zone.home");
    let mut config = (*hass.config()).clone();
    config.latitude = 51.5;
    config.radius = 150;
    config.location_name = "Flat".into();
    hass.set_config(config);
    settle().await;
    let moved = state_of(&hass, "zone.home");
    assert_ne!(moved, home);
    assert_eq!(moved["attributes"]["latitude"], 51.5);
    assert_eq!(moved["attributes"]["radius"], 150);
    assert_eq!(moved["attributes"]["friendly_name"], "Flat");
    let _ = std::fs::remove_dir_all(dir);
}
