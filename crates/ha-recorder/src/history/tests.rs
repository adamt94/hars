use super::*;
use crate::{Config, Row};
use serde_json::{Value, json};
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

fn row(entity: &str, at: i64, changed: i64, state: &str, attrs: &str, flags: u8) -> Row {
    Row::State(StateRow {
        entity_id: entity.into(),
        ts: T0 + at * S,
        last_changed: T0 + changed * S,
        state: state.into(),
        attributes: Arc::from(attrs),
        flags,
        context: [1; 16],
        aug: None,
    })
}

async fn store(name: &str, rows: Vec<Row>) -> (Store, Dir) {
    let p = std::env::temp_dir().join(format!("hars-hist-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    let store = Store::open(Config::new(p.clone())).unwrap();
    for r in rows {
        assert!(store.record(r));
    }
    assert!(store.flush().await);
    (store, Dir(p))
}

fn query<'a>(ids: &'a [String], start: i64) -> Query<'a> {
    Query {
        start: T0 + start * S,
        end: None,
        entity_ids: ids,
        include_start_state: true,
        significant_only: true,
        minimal: false,
        no_attributes: false,
    }
}

fn ids(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn answer(store: &Store, q: &Query<'_>) -> Value {
    serde_json::from_str(&significant_states(store, q).json).unwrap()
}

/// A light turns off, then changes colour; a sensor ticks.
fn fixture() -> Vec<Row> {
    vec![
        row("light.a", 0, 0, "on", r#"{"b":1}"#, 0),
        row("light.a", 10, 10, "off", r#"{"b":0}"#, 0),
        row("light.a", 20, 10, "off", r#"{"b":0,"c":1}"#, 0),
        row("sensor.t", 5, 5, "20.5", r#"{"u":"C"}"#, 0),
        row("sensor.t", 15, 15, "20.5", r#"{"u":"C"}"#, 0),
        row("sensor.t", 25, 25, "21", r#"{"u":"C"}"#, 0),
    ]
}

#[tokio::test]
async fn significant_changes_with_the_start_state() {
    let (store, _dir) = store("significant", fixture()).await;
    let ids = ids(&["light.a", "sensor.t"]);
    let q = query(&ids, 7);
    let got = significant_states(&store, &q);
    // The start state is stamped with the start; the attribute-only row
    // at 20 s is not a change.
    assert_eq!(
        got.json,
        format!(
            r#"{{"light.a":[{{"s":"on","a":{{"b":1}},"lu":{:?}}},{{"s":"off","a":{{"b":0}},"lu":{:?}}}],"sensor.t":[{{"s":"20.5","a":{{"u":"C"}},"lu":{:?}}},{{"s":"20.5","a":{{"u":"C"}},"lu":{:?}}},{{"s":"21","a":{{"u":"C"}},"lu":{:?}}}]}}"#,
            secs(7),
            secs(10),
            secs(7),
            secs(15),
            secs(25),
        )
    );
    assert_eq!(got.last_updated, Some(T0 + 25 * S));

    // Everything, with `lc` where it differs.
    let mut q = query(&ids, 7);
    q.significant_only = false;
    let all = answer(&store, &q);
    assert_eq!(
        all["light.a"],
        json!([
            {"s": "on", "a": {"b": 1}, "lu": secs(7)},
            {"s": "off", "a": {"b": 0}, "lu": secs(10)},
            {"s": "off", "a": {"b": 0, "c": 1}, "lu": secs(20), "lc": secs(10)},
        ])
    );
}

#[tokio::test]
async fn the_window_is_open_at_both_ends_and_the_start_state_is_optional() {
    let (store, _dir) = store("window", fixture()).await;
    let ids = ids(&["light.a"]);
    let mut q = query(&ids, 0);
    q.end = Some(T0 + 10 * S);
    // Nothing before 0 s, so no start state; the row at 0 s is excluded
    // (`> start`), the one at 10 s too (`< end`).
    assert_eq!(answer(&store, &q), json!({}));

    let mut q = query(&ids, 10);
    q.include_start_state = false;
    assert_eq!(
        answer(&store, &q),
        json!({"light.a": []})
            .as_object()
            .map(|_| json!({}))
            .unwrap()
    );
    q.significant_only = false;
    assert_eq!(
        answer(&store, &q)["light.a"],
        json!([{"s": "off", "a": {"b": 0, "c": 1}, "lu": secs(20), "lc": secs(10)}])
    );
}

#[tokio::test]
async fn minimal_responses_keep_the_first_state_whole() {
    let (store, _dir) = store("minimal", fixture()).await;
    let ids = ids(&["sensor.t"]);
    let mut q = query(&ids, 0);
    q.minimal = true;
    q.significant_only = false;
    // Repeated states are skipped; later rows carry only `s` and `lu`.
    assert_eq!(
        answer(&store, &q),
        json!({"sensor.t": [
            {"s": "20.5", "a": {"u": "C"}, "lu": secs(5)},
            {"s": "21", "lu": secs(25)},
        ]})
    );
    q.no_attributes = true;
    assert_eq!(
        answer(&store, &q)["sensor.t"][0],
        json!({"s": "20.5", "lu": secs(5)})
    );
    // Not minimal: attributes are `{}` when left out, never absent.
    q.minimal = false;
    assert_eq!(
        answer(&store, &q)["sensor.t"][0],
        json!({"s": "20.5", "a": {}, "lu": secs(5)})
    );
}

#[tokio::test]
async fn climate_rows_are_always_significant_and_never_minimal() {
    let rows = vec![
        row("climate.ac", 0, 0, "cool", r#"{"t":20}"#, 0),
        row("climate.ac", 10, 0, "cool", r#"{"t":21}"#, 0),
    ];
    let (store, _dir) = store("climate", rows).await;
    let ids = ids(&["climate.ac"]);
    let mut q = query(&ids, 1);
    q.minimal = true;
    assert_eq!(
        answer(&store, &q),
        json!({"climate.ac": [
            {"s": "cool", "a": {"t": 20}, "lu": secs(1)},
            {"s": "cool", "a": {"t": 21}, "lu": secs(10)},
        ]})
    );
}

#[tokio::test]
async fn a_removal_ends_the_line_with_a_null_state() {
    let rows = vec![
        row("light.a", 0, 0, "on", r#"{"b":1}"#, 0),
        row("light.a", 10, 10, "", "{}", REMOVED),
        row("light.a", 20, 20, "off", "{}", 0),
    ];
    let (store, _dir) = store("removed", rows).await;
    let ids = ids(&["light.a"]);
    assert_eq!(
        answer(&store, &query(&ids, 5))["light.a"],
        json!([
            {"s": "on", "a": {"b": 1}, "lu": secs(5)},
            {"s": null, "a": {}, "lu": secs(10)},
            {"s": "off", "a": {}, "lu": secs(20)},
        ])
    );
}

#[tokio::test]
async fn entities_come_back_in_the_order_asked_and_empty_ones_are_left_out() {
    let (store, _dir) = store("order", fixture()).await;
    let ids = ids(&["sensor.t", "light.missing", "light.a", "sensor.t"]);
    let got = significant_states(&store, &query(&ids, 0)).json;
    let t = got.find("sensor.t").unwrap();
    let l = got.find("light.a").unwrap();
    assert!(t < l && !got.contains("missing"));
    assert_eq!(got.matches("sensor.t").count(), 1);
    // A query before anything was recorded has no start state either.
    let got = significant_states(&store, &query(&ids, -100));
    assert!(got.json.contains("sensor.t"));
    assert_eq!(
        significant_states(&store, &query(&["light.missing".to_string()], 0)).json,
        "{}"
    );
}
