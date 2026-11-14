use super::crypto::Key;
use super::*;
use serde_json::json;

const SECRET: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const WEBHOOK: &str = "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f";

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
