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
        entities: vec![
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
    assert!(components.persons.is_empty());
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
        entities: vec![ha_registry::Entity {
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
        assert_eq!(c.persons.len(), 1);
        assert_eq!(c.persons[0]["device_trackers"], json!([]));
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
