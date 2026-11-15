use super::*;
use crate::format::{EventRow, StateRow};
use crate::{Config, Row};
use serde_json::json;
use std::sync::Arc;

const T0: i64 = 1_768_464_000_000_000; // 2026-01-15T08:00:00Z
const S: i64 = 1_000_000;

fn secs(n: i64) -> f64 {
    seconds(T0 + n * S)
}

struct Dir(std::path::PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn state(
    entity: &str,
    at: i64,
    value: &str,
    attrs: &str,
    flags: u8,
    ctx: u8,
    aug: Option<&str>,
) -> Row {
    Row::State(StateRow {
        entity_id: entity.into(),
        ts: T0 + at * S,
        last_changed: T0 + at * S,
        state: value.into(),
        attributes: Arc::from(attrs),
        flags,
        context: [ctx; 16],
        aug: aug.map(Arc::from),
    })
}

fn event(kind: &str, at: i64, data: Option<&str>, ctx: u8, aug: Option<&str>) -> Row {
    Row::Event(EventRow {
        event_type: kind.into(),
        ts: T0 + at * S,
        data: data.map(Arc::from),
        context: [ctx; 16],
        aug: aug.map(Arc::from),
    })
}

async fn store(name: &str, rows: Vec<Row>) -> (Store, Dir) {
    let p = std::env::temp_dir().join(format!("hars-logbook-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    let store = Store::open(Config::new(p.clone())).unwrap();
    for r in rows {
        assert!(store.record(r));
    }
    assert!(store.flush().await);
    (store, Dir(p))
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// Run a query over `[start, end]` seconds from `T0`.
fn run(store: &Store, filter: &Filter, start: i64, end: i64) -> Answer {
    events(
        store,
        &Query {
            start: T0 + start * S,
            end: T0 + end * S,
            filter,
            continuous: &|_| false,
        },
    )
}

fn json(a: &Answer) -> Value {
    serde_json::from_str(&a.json).unwrap()
}

const AUTO: &str = r#"{"name":"Motion light","entity_id":"automation.motion_light","source":"state of binary_sensor.hall_motion"}"#;

/// A day in the house: lights, a continuous sensor, an automation run
/// with the light it turned on, a restart.
fn fixture() -> Vec<Row> {
    vec![
        // Not visible: the first row of an entity.
        state(
            "light.hall",
            0,
            "off",
            r#"{"friendly_name":"Hall"}"#,
            0,
            1,
            None,
        ),
        event("homeassistant_start", 1, None, 2, None),
        state(
            "light.hall",
            10,
            "on",
            r#"{"icon":"mdi:lamp","friendly_name":"Hall"}"#,
            VISIBLE,
            3,
            Some(
                r#"{"context_user_id":"0a0a","context_domain":"light","context_service":"turn_on","context_event_type":"call_service"}"#,
            ),
        ),
        event(
            "call_service",
            10,
            Some(r#"{"domain":"light","service":"turn_on"}"#),
            3,
            None,
        ),
        // A continuous sensor by its recorded unit, and one with none.
        state(
            "sensor.temp",
            11,
            "21",
            r#"{"unit_of_measurement":"C"}"#,
            VISIBLE | HAS_UNIT,
            4,
            None,
        ),
        state("sensor.washer", 12, "done", "{}", VISIBLE, 5, None),
        state("counter.visits", 13, "4", "{}", VISIBLE, 6, None),
        state("switch.fan", 14, "", "{}", REMOVED, 7, None),
        event(
            "automation_triggered",
            20,
            Some(AUTO),
            8,
            Some(r#"{"context_state":"on","context_entity_id":"binary_sensor.hall_motion"}"#),
        ),
        state(
            "light.hall",
            21,
            "off",
            r#"{"icon":""}"#,
            VISIBLE,
            8,
            Some(
                r#"{"context_event_type":"automation_triggered","context_domain":"automation","context_name":"Motion light","context_message":"triggered by state of binary_sensor.hall_motion","context_source":"state of binary_sensor.hall_motion","context_entity_id":"automation.motion_light"}"#,
            ),
        ),
        event(
            "automation_triggered",
            30,
            Some(r#"{"source":null,"device_id":"dev1"}"#),
            9,
            None,
        ),
        event("homeassistant_stop", 40, None, 10, None),
    ]
}

#[tokio::test]
async fn every_row_in_the_window_in_ha_s_shapes() {
    let (store, _dir) = store("all", fixture()).await;
    let got = run(&store, &Filter::new(&[], &[], None), -1, 100);
    assert_eq!(
        json(&got),
        json!([
            {"name": "Home Assistant", "message": "started", "icon": "mdi:home-assistant",
             "domain": "homeassistant", "when": secs(1)},
            {"state": "on", "entity_id": "light.hall", "icon": "mdi:lamp", "when": secs(10),
             "context_user_id": "0a0a", "context_domain": "light",
             "context_service": "turn_on", "context_event_type": "call_service"},
            {"state": "done", "entity_id": "sensor.washer", "when": secs(12)},
            {"name": "Motion light", "message": "triggered by state of binary_sensor.hall_motion",
             "source": "state of binary_sensor.hall_motion",
             "entity_id": "automation.motion_light",
             "context_id": ctx_text(8),
             "domain": "automation", "when": secs(20),
             "context_state": "on", "context_entity_id": "binary_sensor.hall_motion"},
            {"state": "off", "entity_id": "light.hall", "when": secs(21),
             "context_event_type": "automation_triggered", "context_domain": "automation",
             "context_name": "Motion light",
             "context_message": "triggered by state of binary_sensor.hall_motion",
             "context_source": "state of binary_sensor.hall_motion",
             "context_entity_id": "automation.motion_light"},
            {"name": null, "message": "triggered by None", "source": null, "entity_id": null,
             "context_id": ctx_text(9), "domain": "automation", "when": secs(30)},
            {"name": "Home Assistant", "message": "stopped", "icon": "mdi:home-assistant",
             "domain": "homeassistant", "when": secs(40)},
        ])
    );
    assert_eq!(got.last, Some(T0 + 40 * S));
}

fn ctx_text(n: u8) -> String {
    ContextId::from_bytes([n; 16]).to_string()
}

/// Keys come in the order HA's dict has them.
#[tokio::test]
async fn keys_are_in_ha_s_order() {
    let (store, _dir) = store("order", fixture()).await;
    let got = run(&store, &Filter::new(&[], &[], None), 9, 11);
    assert_eq!(
        got.json,
        format!(
            r#"[{{"state":"on","entity_id":"light.hall","icon":"mdi:lamp","when":{:?},"context_user_id":"0a0a","context_domain":"light","context_service":"turn_on","context_event_type":"call_service"}}]"#,
            secs(10)
        )
    );
    let got = run(&store, &Filter::new(&[], &[], None), 29, 31);
    assert_eq!(
        got.json,
        format!(
            r#"[{{"name":null,"message":"triggered by None","source":null,"entity_id":null,"context_id":"{}","domain":"automation","when":{:?}}}]"#,
            ctx_text(9),
            secs(30)
        )
    );
}

#[tokio::test]
async fn the_window_is_open_at_both_ends() {
    let (store, _dir) = store("window", fixture()).await;
    let f = Filter::new(&[], &[], None);
    // 10 s and 21 s are the edges.
    let got = json(&run(&store, &f, 10, 21));
    let whens: Vec<f64> = got
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["when"].as_f64().unwrap())
        .collect();
    assert_eq!(whens, [secs(12), secs(20)]);
    let none = run(&store, &f, 41, 50);
    assert_eq!((none.json.as_str(), none.last), ("[]", None));
}

#[tokio::test]
async fn entities_get_their_rows_and_the_events_that_name_them() {
    let (store, _dir) = store("entities", fixture()).await;
    let f = Filter::new(
        &strings(&["light.hall", "automation.motion_light"]),
        &[],
        None,
    );
    let got = json(&run(&store, &f, -1, 100));
    // The light's two changes and the automation run between them (its
    // `entity_id` names the automation); no start or stop, no other entity.
    let whens: Vec<f64> = got
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["when"].as_f64().unwrap())
        .collect();
    assert_eq!(whens, [secs(10), secs(20), secs(21)], "{got}");
}

#[tokio::test]
async fn devices_get_only_events_and_no_states() {
    let (store, _dir) = store("devices", fixture()).await;
    let f = Filter::new(&[], &strings(&["dev1"]), None);
    let got = json(&run(&store, &f, -1, 100));
    assert_eq!(got.as_array().unwrap().len(), 1, "{got}");
    assert_eq!(got[0]["when"], secs(30));
    // Entities and devices together: either matches.
    let f = Filter::new(&strings(&["sensor.washer"]), &strings(&["dev1"]), None);
    let got = json(&run(&store, &f, -1, 100));
    assert_eq!(
        got.as_array()
            .unwrap()
            .iter()
            .map(|r| r["when"].as_f64().unwrap())
            .collect::<Vec<_>>(),
        [secs(12), secs(30)]
    );
    let f = Filter::new(&[], &strings(&["other"]), None);
    assert_eq!(run(&store, &f, -1, 100).json, "[]");
}

#[tokio::test]
async fn a_context_id_picks_that_context_s_rows() {
    let (store, _dir) = store("context", fixture()).await;
    // The automation's context: its trigger and the light it switched.
    let f = Filter::new(&[], &[], Some([8; 16]));
    let got = json(&run(&store, &f, -1, 100));
    assert_eq!(got.as_array().unwrap().len(), 2, "{got}");
    assert_eq!(got[0]["domain"], "automation");
    assert_eq!(got[1]["entity_id"], "light.hall");
    // The start event is a row of its context too.
    let f = Filter::new(&[], &[], Some([2; 16]));
    assert_eq!(json(&run(&store, &f, -1, 100))[0]["message"], "started");
    // `call_service` is only a link; a context with nothing else is empty.
    let f = Filter::new(&[], &[], Some([3; 16]));
    assert_eq!(json(&run(&store, &f, -1, 100)).as_array().unwrap().len(), 1);
    let f = Filter::new(&[], &[], Some([99; 16]));
    assert_eq!(run(&store, &f, -1, 100).json, "[]");
}

/// A sensor with no unit in its recorded attributes is still left out when
/// its state or registry entry says it is continuous (`is_sensor_continuous`),
/// asked once per entity.
#[tokio::test]
async fn sensors_that_are_continuous_are_left_out() {
    let (store, _dir) = store("continuous", fixture()).await;
    let asked = std::cell::RefCell::new(Vec::new());
    let continuous = |e: &str| {
        asked.borrow_mut().push(e.to_owned());
        true
    };
    let got = events(
        &store,
        &Query {
            start: T0 - S,
            end: T0 + 100 * S,
            filter: &Filter::new(&[], &[], None),
            continuous: &continuous,
        },
    );
    assert!(!got.json.contains("sensor.washer"), "{}", got.json);
    assert_eq!(*asked.borrow(), ["sensor.washer"]);
}

#[test]
fn continuous_by_attributes_as_ha() {
    let attrs = |v: Value| v.as_object().unwrap().clone();
    assert!(sensor_attributes_continuous(&attrs(
        json!({"unit_of_measurement": "C"})
    )));
    assert!(sensor_attributes_continuous(&attrs(
        json!({"unit_of_measurement": null})
    )));
    assert!(sensor_attributes_continuous(&attrs(
        json!({"state_class": "measurement"})
    )));
    assert!(sensor_attributes_continuous(&attrs(
        json!({"device_class": "temperature"})
    )));
    // A class that isn't numeric, or no class: an event-like sensor.
    for dc in ["enum", "date", "timestamp", "uptime"] {
        assert!(
            !sensor_attributes_continuous(&attrs(json!({"device_class": dc}))),
            "{dc}"
        );
    }
    assert!(!sensor_attributes_continuous(&attrs(
        json!({"device_class": null})
    )));
    assert!(!sensor_attributes_continuous(&attrs(
        json!({"friendly_name": "x"})
    )));
    // Only sensors, and the always continuous domains.
    let unit = attrs(json!({"unit_of_measurement": "W"}));
    assert!(continuous("sensor", &unit));
    assert!(!continuous("number", &unit));
    assert!(continuous("counter", &attrs(json!({}))));
    assert!(continuous("proximity", &attrs(json!({}))));
    assert!(!continuous("light", &attrs(json!({}))));
}

#[test]
fn json_strings_are_escaped_as_serde_json_does() {
    for s in [
        "",
        "plain",
        "quote \" and \\ slash",
        "line\nbreak\r\ttab",
        "ctl \u{1} \u{8} \u{c} \u{1f} del \u{7f}",
        "é €  😀 \u{2028}",
        "mdi:home-assistant",
    ] {
        let mut out = String::new();
        push_json(&mut out, s);
        assert_eq!(out, serde_json::to_string(s).unwrap(), "{s:?}");
    }
}

/// The follower's picks (`event_forwarder_filtered` and the state
/// subscriptions): the same entities and devices, and `extract_attr`'s
/// lists and comma separated text for events.
#[test]
fn live_rows_are_followed_as_ha_subscribes() {
    let live_state = |e: &str| state(e, 0, "on", "{}", VISIBLE, 1, None);
    let auto = |data: &str| event("automation_triggered", 0, Some(data), 1, None);
    let start = event("homeassistant_start", 0, None, 1, None);
    let f = |e: &[&str], d: &[&str]| Filter::new(&strings(e), &strings(d), None);

    // Unfiltered: everything arriving.
    let all = f(&[], &[]);
    assert!(all.follows(&live_state("light.a").as_rec()));
    assert!(all.follows(&start.as_rec()));
    assert!(all.follows(&auto("{}").as_rec()));

    let ents = f(&["light.a"], &[]);
    assert!(ents.follows(&live_state("light.a").as_rec()));
    assert!(!ents.follows(&live_state("light.b").as_rec()));
    assert!(!ents.follows(&start.as_rec()));
    assert!(ents.follows(&auto(r#"{"entity_id":"light.a"}"#).as_rec()));
    assert!(ents.follows(&auto(r#"{"entity_id":["x","light.a"]}"#).as_rec()));
    assert!(ents.follows(&auto(r#"{"entity_id":"x,light.a"}"#).as_rec()));
    assert!(!ents.follows(&auto(r#"{"entity_id":"light.b"}"#).as_rec()));
    assert!(!ents.follows(&auto(r#"{"device_id":"d"}"#).as_rec()));
    assert!(!ents.follows(&auto("{}").as_rec()));

    // Devices alone: no state changes.
    let devs = f(&[], &["d"]);
    assert!(!devs.follows(&live_state("light.a").as_rec()));
    assert!(devs.follows(&auto(r#"{"device_id":"d"}"#).as_rec()));
    assert!(!devs.follows(&start.as_rec()));

    // The stored query compares the whole string.
    assert!(!ents.selects(&auto(r#"{"entity_id":"x,light.a"}"#).as_rec()));
    assert!(!ents.selects(&auto(r#"{"entity_id":["light.a"]}"#).as_rec()));
    assert!(ents.selects(&auto(r#"{"entity_id":"light.a"}"#).as_rec()));
}
