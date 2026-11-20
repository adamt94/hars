use super::crypto::Key;
use super::*;
use serde_json::json;

const SECRET: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
pub(crate) const WEBHOOK: &str = "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f";

/// The sanitised fake of the user's entry (`spec-mobile-app.md` §1.1).
pub(crate) fn entry(flag: bool) -> Map<String, Value> {
    let mut data = json!({
        "app_data": {"push_token": "fake-push-token",
                     "push_url": "https://mobile-apps.home-assistant.io/api/sendPushNotification"},
        "app_id": "io.robbie.HomeAssistant",
        "app_name": "Home Assistant",
        "app_version": "2026.9.0 (2026.2874)",
        "device_id": "11111111-2222-4333-8444-555555555555",
        "device_name": " Phone",
        "manufacturer": "Apple",
        "model": "iPhone17,1",
        "os_name": "iOS",
        "os_version": "27.0",
        "secret": SECRET,
        "supports_encryption": true,
        "user_id": "00112233445566778899aabbccddeeff",
        "webhook_id": WEBHOOK,
    });
    if flag {
        data["no_legacy_encryption"] = json!(true);
    }
    json!({"domain": "mobile_app", "entry_id": "01JFAKEMOBILEAPPENTRY00000",
           "disabled_by": null, "data": data})
    .as_object()
    .unwrap()
    .clone()
}

#[test]
fn hex_key_only_once_flagged() {
    let r = Registration::from_entry(&entry(true)).unwrap();
    let hex = Key::hex(SECRET).unwrap().seal(br#"{"a":1}"#);
    assert_eq!(r.open(&hex), (Opened::Json(json!({"a": 1})), false));
    let legacy = Key::legacy(SECRET).seal(br#"{"a":1}"#);
    assert_eq!(r.open(&legacy), (Opened::Failed, false));
    // Not JSON, bad base64, a scalar.
    let not_json = Key::hex(SECRET).unwrap().seal(b"nope");
    assert_eq!(r.open(&not_json).0, Opened::Failed);
    assert_eq!(r.open("@@@").0, Opened::Failed);
    let scalar = Key::hex(SECRET).unwrap().seal(b"5");
    assert_eq!(r.open(&scalar).0, Opened::Json(json!(5)));
}

#[test]
fn a_hex_success_sets_the_flag() {
    let r = Registration::from_entry(&entry(false)).unwrap();
    // Unflagged: the legacy key works, and replies use it.
    let legacy = Key::legacy(SECRET).seal(b"{}");
    assert_eq!(r.open(&legacy), (Opened::Json(json!({})), false));
    let reply: Value = serde_json::from_str(&r.response_body("{}".into())).unwrap();
    let sealed = reply["encrypted_data"].as_str().unwrap();
    assert!(Key::legacy(SECRET).open(sealed).is_some());
    // A hex-key plaintext that isn't JSON doesn't set it.
    let bad = Key::hex(SECRET).unwrap().seal(b"x");
    assert_eq!(r.open(&bad), (Opened::Failed, false));
    assert!(!r.data().contains_key("no_legacy_encryption"));
    // The first good hex-key request does, once.
    let hex = Key::hex(SECRET).unwrap().seal(b"{}");
    assert_eq!(r.open(&hex), (Opened::Json(json!({})), true));
    assert_eq!(r.open(&hex), (Opened::Json(json!({})), false));
    assert_eq!(r.data()["no_legacy_encryption"], true);
    // Then the legacy key is refused, and replies use the hex key.
    assert_eq!(r.open(&legacy).0, Opened::Failed);
    let reply: Value = serde_json::from_str(&r.response_body("{}".into())).unwrap();
    assert_eq!(
        reply.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["encrypted", "encrypted_data"]
    );
    let sealed = reply["encrypted_data"].as_str().unwrap();
    assert_eq!(Key::hex(SECRET).unwrap().open(sealed).unwrap(), b"{}");
}

#[test]
fn plaintext_registrations_reply_in_clear() {
    let mut e = entry(false);
    e["data"]["supports_encryption"] = json!(false);
    let r = Registration::from_entry(&e).unwrap();
    assert_eq!(r.response_body(r#"{"a":1}"#.into()), r#"{"a":1}"#);
}

#[test]
fn webhooks_by_id() {
    let mut disabled = entry(true);
    disabled["disabled_by"] = json!("user");
    disabled["data"]["webhook_id"] = json!("disabled");
    let mut other = entry(true);
    other["domain"] = json!("hue");
    other["data"]["webhook_id"] = json!("hue");
    let app = MobileApp::new(&[entry(true), disabled, other], vec!["gone".into()]);
    assert!(matches!(app.webhook(WEBHOOK), Webhook::Registered(_)));
    assert!(matches!(app.webhook("gone"), Webhook::Deleted));
    assert!(matches!(app.webhook("disabled"), Webhook::Unknown));
    assert!(matches!(app.webhook("hue"), Webhook::Unknown));
}

fn sensors_fixture() -> (
    ha_core::Hass,
    Arc<ha_registry::Registry>,
    Arc<Registration>,
    sensor::Sensors,
) {
    let hass = ha_core::Hass::new();
    let registry = ha_registry::Registry::in_memory(&hass, Default::default());
    let reg = Arc::new(Registration::from_entry(&entry(true)).unwrap());
    let sensors = sensor::Sensors::setup(&hass, registry.clone(), std::slice::from_ref(&reg));
    (hass, registry, reg, sensors)
}

fn state_of(hass: &ha_core::Hass, id: &str) -> Option<String> {
    hass.states()
        .get(&ha_core::EntityId::parse(id).unwrap())
        .map(|s| s.state.to_string())
}

/// Review regression: re-enabled without a state, a sensor restores the
/// value it had when it was disabled (HA adds it, then restores).
#[test]
fn re_enabled_sensors_restore() {
    let (hass, _, reg, sensors) = sensors_fixture();
    let register = |extra: Value| {
        let mut p = json!({"name": "Steps", "type": "sensor", "unique_id": "steps"});
        p.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(sensors.register(&reg, None, &p));
    };
    register(json!({"state": 42}));
    assert_eq!(state_of(&hass, "sensor.phone_steps").as_deref(), Some("42"));
    register(json!({"disabled": true}));
    assert_eq!(state_of(&hass, "sensor.phone_steps"), None);
    register(json!({"disabled": false}));
    assert_eq!(state_of(&hass, "sensor.phone_steps").as_deref(), Some("42"));
}

/// Review regression: concurrent updates can't publish out of order: the
/// state and what would be restored always agree.
#[test]
fn concurrent_updates_stay_consistent() {
    let (hass, _, reg, sensors) = sensors_fixture();
    assert!(sensors.register(
        &reg,
        None,
        &json!({"name": "Steps", "type": "sensor", "unique_id": "steps", "state": 0})
    ));
    std::thread::scope(|s| {
        for t in 0..8 {
            let (sensors, reg) = (&sensors, &reg);
            s.spawn(move || {
                for i in 0..50 {
                    let v = t * 1000 + i;
                    sensors.update_states(
                        reg,
                        &json!([{"type": "sensor", "unique_id": "steps", "state": v}]),
                    );
                }
            });
        }
    });
    let state = state_of(&hass, "sensor.phone_steps").unwrap();
    let dumped = hass
        .restore()
        .extra_data_now("sensor.phone_steps")
        .expect("registered");
    assert_eq!(dumped["native_value"].to_string(), state);
}

/// Review regressions: voluptuous's coercions as Python does them.
#[test]
fn location_schema_coerces_as_voluptuous() {
    use tracker::location_schema;
    let one = |v: Value| location_schema(&v);
    assert_eq!(one(json!({"battery": "1_0"})).unwrap()["battery"], 10);
    assert_eq!(
        one(json!({"battery": "\u{661}\u{662}"})).unwrap()["battery"],
        12
    );
    assert_eq!(
        one(json!({"altitude": "1_000.5"})).unwrap()["altitude"],
        1000.5
    );
    assert_eq!(one(json!({"speed": 1.9})).unwrap()["speed"], 1);
    assert_eq!(one(json!({"speed": -0.5})).unwrap()["speed"], 0);
    assert!(one(json!({"speed": -1.5})).is_none());
    assert_eq!(
        one(json!({"battery": 1e30})).unwrap()["battery"],
        json!(1e30),
        "beyond 64 bits: the nearest float, not a saturated i64"
    );
    assert_eq!(
        one(json!({"in_zones": {"zone.home": true}})).unwrap()["in_zones"],
        json!(["zone.home"])
    );
    assert_eq!(one(json!({"in_zones": {}})).unwrap()["in_zones"], json!([]));
    assert!(one(json!({"in_zones": ["light.x"]})).is_none());
    assert!(
        one(json!({"gps": [1, 2]})).is_none(),
        "gps needs gps_accuracy"
    );
    assert!(one(json!({"battery": "1__0"})).is_none());
}

/// Review regression: concurrent changes save the newest data, never an
/// older snapshot.
#[test]
fn concurrent_persists_save_the_newest_data() {
    let hass = ha_core::Hass::new();
    let entries = crate::config_entries::ConfigEntries::in_memory(&hass, vec![entry(true)]);
    let app = MobileApp::new(&[entry(true)], Vec::new());
    let registry = ha_registry::Registry::in_memory(&hass, Default::default());
    app.setup(&hass, registry, entries.clone(), None);
    let Webhook::Registered(reg) = app.webhook(WEBHOOK) else {
        panic!("registered");
    };
    std::thread::scope(|s| {
        for t in 0..4 {
            let (app, reg) = (&app, &reg);
            s.spawn(move || {
                for i in 0..100 {
                    let _turn = reg.lock_ops();
                    let mut update = Map::new();
                    update.insert("app_version".into(), json!(format!("{t}.{i}")));
                    reg.merge(update);
                    app.persist(reg);
                }
            });
        }
    });
    let saved = entries.list().unwrap()[0]["data"].clone();
    assert_eq!(Value::Object(reg.data()), saved);
}

/// A store written by a newer Home Assistant (1.2, from 2026.7, with
/// Live Activity tokens) is read, and the next change writes what Home
/// Assistant 2026.6.1 writes after loading the same file: version 1.1,
/// the deleted ids, and none of what 2026.6.1 doesn't know. The vectors
/// are 2026.6.1's `mobile_app` on stores made by 2026.7.0 and 2026.7.4
/// (`scripts/vectors/import_releases.py downgrade`).
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn store_downgrade_keeps_what_ha_keeps() {
    let vectors: Value =
        serde_json::from_str(include_str!("vectors/store_downgrade.json")).unwrap();
    let cases = vectors["cases"].as_array().unwrap();
    assert!(!cases.is_empty());
    for case in cases {
        let from = case["from"].as_str().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "hars-mobile-app-downgrade-{from}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".storage")).unwrap();
        let file = dir.join(".storage/mobile_app");
        let input = case["input"].as_str().unwrap();
        std::fs::write(&file, input).unwrap();
        let read = |text: &str| serde_json::from_str::<Value>(text).unwrap();
        assert_eq!(read(input)["minor_version"], 2, "{from}");
        assert!(read(input)["data"]["live_activity_tokens"].is_object());

        let app = MobileApp::load(&dir, &[]);
        let deleted = read(input)["data"]["deleted_ids"][0]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(matches!(app.webhook(&deleted), Webhook::Deleted), "{from}");
        // Home Assistant writes the file back at once (`after_load`: the
        // same data at 1.1); hars leaves it until something changes.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), input, "{from}");
        assert_eq!(
            read(case["after_load"].as_str().unwrap())["data"],
            read(input)["data"]
        );

        app.save_deleted(case["removed_webhook_id"].as_str().unwrap())
            .await;
        assert_eq!(
            read(&std::fs::read_to_string(&file).unwrap()),
            read(case["after_change"].as_str().unwrap()),
            "{from}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// Removing an entry from the UI (`async_remove_entry`): the webhook id is
/// kept as deleted whether or not the registration was loaded (a disabled
/// entry's isn't), so after a restart the app is told it is gone.
#[tokio::test]
async fn a_removed_entry_leaves_its_webhook_id_as_deleted() {
    let hass = ha_core::Hass::new();
    let mut disabled = entry(true);
    disabled.insert("disabled_by".into(), json!("user"));
    let entries = crate::config_entries::ConfigEntries::in_memory(&hass, vec![disabled.clone()]);
    let app = MobileApp::new(std::slice::from_ref(&disabled), Vec::new());
    let registry = ha_registry::Registry::in_memory(&hass, Default::default());
    app.setup(&hass, registry.clone(), entries.clone(), None);
    assert!(
        app.is_empty(),
        "a disabled entry's registration isn't loaded"
    );
    app.stop_entry(&disabled).await;
    assert_eq!(*app.stored_deleted.lock(), [WEBHOOK]);

    // A loaded one stops answering too.
    let app = MobileApp::new(&[entry(true)], Vec::new());
    app.setup(&hass, registry, entries, None);
    assert!(matches!(app.webhook(WEBHOOK), Webhook::Registered(_)));
    app.stop_entry(&entry(true)).await;
    assert!(!matches!(app.webhook(WEBHOOK), Webhook::Registered(_)));
    assert_eq!(*app.stored_deleted.lock(), [WEBHOOK]);
}
