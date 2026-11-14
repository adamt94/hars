//! The engine against vectors made by running HA 2026.6.1's automation
//! engine on the user's 4 automations at simulated time (see `vectors/`):
//! each vector's steps are replayed here and the observable results
//! compared, in order: `automation_triggered` and `call_service` events,
//! the automation entities' state changes, the light calls, and HA's
//! WARNING (and `Restarting`) log lines. Contexts are compared by shape:
//! the vector's symbolic ids must map one-to-one onto ours.

use super::*;
use parking_lot::Mutex as PMutex;
use std::collections::BTreeMap;
use std::time::Duration;

const VECTORS: &[(&str, &str)] = &[
    ("ns_crossing", include_str!("vectors/ns_crossing.json")),
    (
        "ns_threshold_equality",
        include_str!("vectors/ns_threshold_equality.json"),
    ),
    (
        "ns_already_below_at_startup",
        include_str!("vectors/ns_already_below_at_startup.json"),
    ),
    (
        "ns_unavailable_recovery",
        include_str!("vectors/ns_unavailable_recovery.json"),
    ),
    (
        "ns_unknown_at_startup",
        include_str!("vectors/ns_unknown_at_startup.json"),
    ),
    (
        "ns_missing_at_startup",
        include_str!("vectors/ns_missing_at_startup.json"),
    ),
    (
        "ns_non_numeric",
        include_str!("vectors/ns_non_numeric.json"),
    ),
    (
        "ns_attribute_only",
        include_str!("vectors/ns_attribute_only.json"),
    ),
    (
        "ns_condition_motion_off",
        include_str!("vectors/ns_condition_motion_off.json"),
    ),
    ("st_to_on", include_str!("vectors/st_to_on.json")),
    (
        "st_on_at_startup",
        include_str!("vectors/st_on_at_startup.json"),
    ),
    ("st_created_on", include_str!("vectors/st_created_on.json")),
    (
        "st_change_during_startup",
        include_str!("vectors/st_change_during_startup.json"),
    ),
    (
        "same_tick_batch",
        include_str!("vectors/same_tick_batch.json"),
    ),
    ("wait_basic", include_str!("vectors/wait_basic.json")),
    (
        "wait_reenter_restart",
        include_str!("vectors/wait_reenter_restart.json"),
    ),
    (
        "wait_reenter_conditions_fail",
        include_str!("vectors/wait_reenter_conditions_fail.json"),
    ),
    (
        "wait_attribute_only",
        include_str!("vectors/wait_attribute_only.json"),
    ),
    (
        "wait_unavailable_blip",
        include_str!("vectors/wait_unavailable_blip.json"),
    ),
    (
        "wait_already_off",
        include_str!("vectors/wait_already_off.json"),
    ),
    (
        "wait_wall_jump",
        include_str!("vectors/wait_wall_jump.json"),
    ),
    (
        "mode_restart_order",
        include_str!("vectors/mode_restart_order.json"),
    ),
    (
        "mode_single_drop",
        include_str!("vectors/mode_single_drop.json"),
    ),
    ("time_day", include_str!("vectors/time_day.json")),
    (
        "time_light_off",
        include_str!("vectors/time_light_off.json"),
    ),
    (
        "time_condition_edges",
        include_str!("vectors/time_condition_edges.json"),
    ),
    (
        "kitchen_condition_edges",
        include_str!("vectors/kitchen_condition_edges.json"),
    ),
    ("dst_autumn", include_str!("vectors/dst_autumn.json")),
    ("dst_spring", include_str!("vectors/dst_spring.json")),
    (
        "jump_forward_over_trigger",
        include_str!("vectors/jump_forward_over_trigger.json"),
    ),
    (
        "jump_back_after_trigger",
        include_str!("vectors/jump_back_after_trigger.json"),
    ),
    (
        "jump_back_over_trigger",
        include_str!("vectors/jump_back_over_trigger.json"),
    ),
    (
        "startup_restore",
        include_str!("vectors/startup_restore.json"),
    ),
    (
        "startup_missed_time",
        include_str!("vectors/startup_missed_time.json"),
    ),
    (
        "startup_integration_late",
        include_str!("vectors/startup_integration_late.json"),
    ),
    (
        "service_turn_off_on",
        include_str!("vectors/service_turn_off_on.json"),
    ),
];

/// The user's automations, by `id`, as hars loads them.
fn configs() -> BTreeMap<String, Value> {
    let y = ha_core::yaml::parse(include_str!("vectors/automations.yaml")).unwrap();
    let Value::Array(items) = y.to_json() else {
        panic!()
    };
    items
        .into_iter()
        .map(|a| (a["id"].as_str().unwrap().to_owned(), a))
        .collect()
}

fn registries() -> Registries {
    Registries {
        entities: serde_json::from_str(include_str!("vectors/registry_automation.json")).unwrap(),
        ..Default::default()
    }
}

/// Log lines (level, message), as a layer captures them.
#[derive(Clone, Default)]
struct Logs(
    Arc<PMutex<Vec<(tracing::Level, String, jiff::Timestamp)>>>,
    Option<ha_core::Clock>,
);

struct Msg(String);
impl tracing::field::Visit for Msg {
    fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
        if f.name() == "message" {
            self.0 = format!("{v:?}");
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Logs {
    fn on_event(&self, e: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut m = Msg(String::new());
        e.record(&mut m);
        let now = self
            .1
            .as_ref()
            .map_or_else(jiff::Timestamp::now, |c| c.now());
        self.0.lock().push((*e.metadata().level(), m.0, now));
    }
}

/// One observation, comparable across HA and hars.
#[derive(Debug, Clone, PartialEq)]
struct Obs {
    t: jiff::Timestamp,
    what: String,
    ctx: Option<String>,
    parent: Option<String>,
}

/// `last_triggered` to the second: time triggers fire at a random
/// 50-500 ms past it, different in each run.
fn norm(a: &mut Value) {
    if let Some(Value::String(t)) = a.get_mut("last_triggered") {
        let secs = ts(t).as_second();
        *t = jiff::Timestamp::from_second(secs).unwrap().to_string();
    }
}

/// A vector time: an instant if it has an offset, else London wall time.
fn local(s: &str) -> jiff::Timestamp {
    match s.parse::<jiff::Timestamp>() {
        Ok(t) => t,
        Err(_) => s
            .parse::<jiff::civil::DateTime>()
            .unwrap()
            .to_zoned(jiff::tz::TimeZone::get("Europe/London").unwrap())
            .unwrap()
            .timestamp(),
    }
}

/// Move paused time to the wall time `at` exactly: auto-advance (every
/// timer due on the way fires, in order) to just short of it, then the
/// exact remainder (sleeps round up to whole milliseconds).
async fn advance_in_steps(hass: &Hass, at: jiff::Timestamp) {
    let d = at.duration_since(hass.clock().now());
    if d <= jiff::SignedDuration::from_millis(2) {
        if d > jiff::SignedDuration::ZERO {
            tokio::time::advance(Duration::try_from(d).unwrap()).await;
        }
    } else {
        let d = Duration::try_from(d).unwrap();
        tokio::time::sleep(d - Duration::from_millis(2)).await;
        let rest = at.duration_since(hass.clock().now());
        if rest > jiff::SignedDuration::ZERO {
            tokio::time::advance(Duration::try_from(rest).unwrap()).await;
        }
    }
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

fn ts(s: &str) -> jiff::Timestamp {
    s.parse()
        .unwrap_or_else(|_| format!("{s}Z").parse().unwrap())
}

/// Whether the vector's observation is one we compare.
fn wanted(e: &Value) -> Option<Obs> {
    let t = ts(e["t"].as_str()?);
    let sym = |k: &str| e[k].as_str().map(str::to_owned);
    let what = match e["kind"].as_str()? {
        "event" => match e["event_type"].as_str()? {
            "automation_triggered" => format!("triggered {}", e["data"]),
            "call_service" if e["data"]["domain"] == "light" => format!("call {}", e["data"]),
            _ => return None,
        },
        "state" => {
            let mut a = e["attributes"].clone();
            a.as_object_mut()?.remove("friendly_name");
            norm(&mut a);
            format!("state {} {} {}", e["entity_id"], e["new"], a)
        }
        "log" if e["level"] == "WARNING" || e["msg"].as_str()?.ends_with(": Restarting") => {
            format!("log {} {}", e["level"].as_str()?, e["msg"])
        }
        _ => return None,
    };
    Some(Obs {
        t,
        what,
        ctx: sym("ctx"),
        parent: sym("parent"),
    })
}

/// Contexts seen on our side, by id, named in order of appearance.
#[derive(Default)]
struct Names(HashMap<ha_core::ContextId, String>);

impl Names {
    fn name(&mut self, id: ha_core::ContextId) -> String {
        let n = self.0.len();
        self.0.entry(id).or_insert_with(|| format!("x{n}")).clone()
    }
}

/// Run one vector; the differences, if any.
async fn run(name: &str, text: &str) -> Vec<String> {
    let v: Value = serde_json::from_str(text).unwrap();
    // The vector's start is London wall time.
    let start = v["start"]
        .as_str()
        .unwrap()
        .parse::<jiff::civil::DateTime>()
        .unwrap()
        .to_zoned(jiff::tz::TimeZone::get("Europe/London").unwrap())
        .unwrap()
        .timestamp();
    let sim = ha_core::SimulatedClock::new(start);
    let hass = Hass::with_clock(sim.clock());
    hass.set_config(ha_core::config::CoreConfig {
        time_zone: "Europe/London".into(),
        ..Default::default()
    });
    // Last states, as `core.restore_state` would give them.
    if let Some(rows) = v["restore"].as_array().filter(|r| !r.is_empty()) {
        let dir = std::env::temp_dir().join(format!("hars-auto-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rows: Vec<Value> = rows
            .iter()
            .map(|r| {
                let mut r = r.clone();
                r["last_seen"] = json!(ha_core::isoformat(start));
                r
            })
            .collect();
        ha_core::storage::Store::new(&dir, "core.restore_state", 1, 1, false)
            .save(&json!(rows))
            .unwrap();
        hass.set_restore(ha_core::restore::RestoreState::load(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
    let logs = Logs(Arc::default(), Some(sim.clock()));
    use tracing_subscriber::layer::SubscriberExt;
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.clone()));

    // HA's harness lights: after the latency they write on/off with the
    // call's context.
    let latency = Arc::new(PMutex::new(Duration::ZERO));
    for service in ["turn_on", "turn_off"] {
        let latency = latency.clone();
        let h = hass.clone();
        hass.services()
            .register("light", service, SupportsResponse::None, move |call| {
                let d = *latency.lock();
                let h = h.clone();
                async move {
                    tokio::time::sleep(d).await;
                    let ids = match call.data.get("entity_id") {
                        Some(Value::Array(a)) => a.clone(),
                        Some(v) => vec![v.clone()],
                        None => vec![],
                    };
                    for id in ids.iter().filter_map(Value::as_str) {
                        let id = EntityId::parse(id).unwrap();
                        let attrs = h
                            .states()
                            .get(&id)
                            .map(|s| s.attributes.clone())
                            .unwrap_or_default();
                        let state = if service == "turn_on" { "on" } else { "off" };
                        h.set_state_with_context(id, state, attrs, Some(call.context.clone()));
                    }
                    Ok(None)
                }
            });
    }
    let mut bus = hass.bus().subscribe();
    let mut ours: Vec<(Obs, Option<ha_core::Context>)> = Vec::new();
    let configs = configs();
    let registries = registries();
    let mut autos: Option<Automations> = None;
    let settle = || async {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    };
    let collect = |bus: &mut tokio::sync::broadcast::Receiver<Arc<Event>>,
                   ours: &mut Vec<(Obs, Option<ha_core::Context>)>,
                   logs: &Logs| {
        let mut batch = Vec::new();
        while let Ok(e) = bus.try_recv() {
            if let EventKind::StateChanged {
                entity_id,
                new_state: Some(st),
                ..
            } = &e.kind
            {
                if !entity_id.as_str().starts_with("automation.") {
                    continue;
                }
                let mut a = Value::Object(st.attributes.clone());
                a.as_object_mut().unwrap().remove("friendly_name");
                norm(&mut a);
                batch.push((
                    Obs {
                        t: st.last_updated,
                        what: format!("state \"{entity_id}\" \"{}\" {a}", st.state),
                        ctx: None,
                        parent: None,
                    },
                    Some(st.context.clone()),
                ));
                continue;
            }
            let what = match &e.kind {
                EventKind::Custom { name, data } if name == "automation_triggered" => {
                    format!("triggered {data}")
                }
                EventKind::CallService {
                    domain,
                    service,
                    service_data,
                } if domain == "light" => format!(
                    "call {}",
                    json!({"domain": domain, "service": service, "service_data": &**service_data})
                ),
                _ => continue,
            };
            batch.push((
                Obs {
                    t: e.time_fired,
                    what,
                    ctx: None,
                    parent: None,
                },
                Some(e.context.clone()),
            ));
        }
        for (level, msg, t) in logs.0.lock().drain(..) {
            if level == tracing::Level::WARN || msg.ends_with(": Restarting") {
                let level = if level == tracing::Level::WARN {
                    "WARNING"
                } else {
                    "INFO"
                };
                batch.push((
                    Obs {
                        t,
                        what: format!("log {level} {}", json!(msg)),
                        ctx: None,
                        parent: None,
                    },
                    None,
                ));
            }
        }
        ours.extend(batch);
    };
    let mut pending_services = Vec::new();
    for step in v["steps"].as_array().unwrap() {
        let set = |s: &Value| {
            let eid = EntityId::parse(s["entity_id"].as_str().unwrap()).unwrap();
            let attrs = s["attributes"].as_object().cloned().unwrap_or_default();
            hass.set_state(eid, s["state"].as_str().unwrap(), attrs);
        };
        match step["op"].as_str().unwrap() {
            "set" => {
                set(step);
                settle().await;
            }
            "set_batch" => {
                for item in step["items"].as_array().unwrap() {
                    set(item);
                }
                settle().await;
            }
            "setup" => {
                let items: Vec<Value> = v["automations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|id| configs[id.as_str().unwrap()].clone())
                    .collect();
                autos = Some(Automations::setup(&hass, &registries, &items));
                settle().await;
            }
            "start" => {
                // HA's `homeassistant_start`: registered inputs nobody wrote
                // are `unavailable`.
                if v["input_registry"] == true {
                    let r = Registries {
                        entities: serde_json::from_str(include_str!(
                            "vectors/registry_inputs.json"
                        ))
                        .unwrap(),
                        ..Default::default()
                    };
                    r.write_unavailable_states(&hass);
                }
                hass.start();
                if let Some(a) = &autos {
                    a.started();
                }
                settle().await;
            }
            "wait" => {
                let to = hass.clock().now()
                    + jiff::SignedDuration::from_secs_f64(step["s"].as_f64().unwrap());
                advance_in_steps(&hass, to).await;
            }
            "at" => {
                let at = local(step["t"].as_str().unwrap());
                advance_in_steps(&hass, at).await;
            }
            "jump_wall" => {
                let s = step["s"].as_f64().unwrap();
                let now = hass.clock().now();
                sim.jump_to(now + jiff::SignedDuration::from_secs_f64(s));
                settle().await;
            }
            "light" => *latency.lock() = Duration::from_secs_f64(step["latency"].as_f64().unwrap()),
            "service" => {
                let hass = hass.clone();
                let (d, s) = (
                    step["domain"].as_str().unwrap().to_owned(),
                    step["service"].as_str().unwrap().to_owned(),
                );
                let data = step["data"].as_object().cloned().unwrap_or_default();
                pending_services.push(tokio::spawn(async move {
                    hass.call_service(&d, &s, data, None, false, None).await
                }));
                settle().await;
            }
            other => panic!("{name}: unknown op {other}"),
        }
        collect(&mut bus, &mut ours, &logs);
    }
    // The inputs' contexts on our side: the vector's input contexts are
    // named in order of the writes.
    let want: Vec<Obs> = v["expected"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(wanted)
        .collect();
    let is_log = |o: &Obs| o.what.starts_with("log ");
    let (want_logs, want_rest): (Vec<Obs>, Vec<Obs>) = want.into_iter().partition(is_log);
    let (our_logs, our_rest): (Vec<_>, Vec<_>) = ours.into_iter().partition(|(o, _)| is_log(o));
    let mut diffs = compare(name, &want_rest, &our_rest);
    diffs.extend(compare(&format!("{name} (logs)"), &want_logs, &our_logs));
    diffs
}

fn compare(name: &str, want: &[Obs], ours: &[(Obs, Option<ha_core::Context>)]) -> Vec<String> {
    let mut diffs = Vec::new();
    let mut names = Names::default();
    let mut map: HashMap<String, String> = HashMap::new();
    let n = want.len().max(ours.len());
    for i in 0..n {
        match (want.get(i), ours.get(i)) {
            (Some(w), Some((o, ctx))) => {
                let near = (w.t.as_microsecond() - o.t.as_microsecond()).abs() <= 600_000;
                if w.what != o.what || !near {
                    diffs.push(format!(
                        "{name} #{i}:\n  want {} {}\n  got  {} {}",
                        w.t, w.what, o.t, o.what
                    ));
                    break;
                }
                // Contexts: the vector's symbols map one-to-one onto ours.
                if let (Some(sym), Some(c)) = (&w.ctx, ctx) {
                    let our = names.name(c.id);
                    match map.get(sym) {
                        Some(m) if *m != our => diffs.push(format!(
                            "{name} #{i}: context {sym} was {m}, now {our} ({})",
                            w.what
                        )),
                        _ => {
                            map.insert(sym.clone(), our);
                        }
                    }
                    let parent_given = w.parent.is_some();
                    if parent_given != c.parent_id.is_some() {
                        diffs.push(format!(
                            "{name} #{i}: parent {:?} vs ours {:?} ({})",
                            w.parent, c.parent_id, w.what
                        ));
                    }
                }
            }
            (Some(w), None) => {
                diffs.push(format!("{name} #{i}: missing {} {}", w.t, w.what));
                break;
            }
            (None, Some((o, _))) => {
                diffs.push(format!("{name} #{i}: extra {} {}", o.t, o.what));
                break;
            }
            (None, None) => break,
        }
    }
    diffs
}

#[tokio::test(start_paused = true)]
async fn matches_has_engine() {
    let mut failures = Vec::new();
    for (name, text) in VECTORS {
        failures.extend(run(name, text).await);
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn py_float_as_python() {
    for (s, want) in [
        ("25", Some(25.0)),
        (" 25 ", Some(25.0)),
        ("1e1", Some(10.0)),
        ("1_000", Some(1000.0)),
        ("inf", Some(f64::INFINITY)),
        ("-Infinity", Some(f64::NEG_INFINITY)),
        ("abc", None),
        ("1__0", None),
        ("_1", None),
        ("", None),
    ] {
        assert_eq!(py_float(s), want, "{s}");
    }
    assert!(py_float("nan").unwrap().is_nan());
}

/// ROADMAP M3's acceptance: the user's recorded inputs (10 days) replayed
/// through the engine give every trigger and light call HA's engine gave
/// (`expected_triggered` / `expected_calls`, each matched by automation,
/// source and the input that caused it, within 1 s). The vector is the
/// household's motion timeline, so it isn't committed: point
/// `HARS_AUTOMATION_REPLAY` at it to run this.
#[tokio::test(start_paused = true)]
async fn replay_matches_ha() {
    let Ok(path) = std::env::var("HARS_AUTOMATION_REPLAY") else {
        eprintln!("HARS_AUTOMATION_REPLAY not set: skipped");
        return;
    };
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let t0 = ts(v["t0"].as_str().unwrap());
    let at = |us: i64| t0 + jiff::SignedDuration::from_micros(us);
    let entities: Vec<EntityId> = v["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| EntityId::parse(e.as_str().unwrap()).unwrap())
        .collect();
    let autos_ids: Vec<String> = v["automations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_owned())
        .collect();
    let sources: Vec<String> = v["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_owned())
        .collect();
    let inputs = v["inputs"].as_array().unwrap();
    let first = inputs
        .iter()
        .map(|i| i[0].as_i64().unwrap())
        .min()
        .unwrap()
        .min(0);
    let sim = ha_core::SimulatedClock::new(at(first));
    let hass = Hass::with_clock(sim.clock());
    hass.set_config(ha_core::config::CoreConfig {
        time_zone: v["tz"].as_str().unwrap().into(),
        ..Default::default()
    });
    let dir = std::env::temp_dir().join(format!("hars-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    hass.set_restore(ha_core::restore::RestoreState::load(&dir));
    // Lights: recorded, 0.2 s, no state writes (states come from inputs).
    for service in ["turn_on", "turn_off"] {
        hass.services()
            .register("light", service, SupportsResponse::None, |_| async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok(None)
            });
    }
    let all: Vec<Value> = configs().into_values().collect();
    let registries = registries();
    let mut bus = hass.bus().subscribe();
    // Our observations: (t, automation, source, input index) and calls.
    let mut input_of: HashMap<ha_core::ContextId, usize> = HashMap::new();
    let mut triggered: Vec<(i64, String, Option<String>, Option<usize>)> = Vec::new();
    let mut calls: Vec<(i64, String, Value, Option<usize>)> = Vec::new();
    let mut run_input: HashMap<ha_core::ContextId, Option<usize>> = HashMap::new();
    let mut drain = |bus: &mut tokio::sync::broadcast::Receiver<Arc<Event>>,
                     input_of: &HashMap<ha_core::ContextId, usize>| {
        while let Ok(e) = bus.try_recv() {
            let t = e.time_fired.as_microsecond() - t0.as_microsecond();
            match &e.kind {
                EventKind::Custom { name, data } if name == "automation_triggered" => {
                    let input = e.context.parent_id.and_then(|p| input_of.get(&p).copied());
                    run_input.insert(e.context.id, input);
                    triggered.push((
                        t,
                        data["entity_id"].as_str().unwrap().to_owned(),
                        data["source"].as_str().map(str::to_owned),
                        input,
                    ));
                }
                EventKind::CallService {
                    domain,
                    service,
                    service_data,
                } if domain == "light" => {
                    let input = run_input.get(&e.context.id).copied().flatten();
                    calls.push((
                        t,
                        format!("light.{service}"),
                        Value::Object((**service_data).clone()),
                        input,
                    ));
                }
                _ => {}
            }
        }
    };
    // The timeline: inputs and lifecycle steps, by time (lifecycle first).
    let mut timeline: Vec<(i64, u8, usize)> = inputs
        .iter()
        .enumerate()
        .map(|(i, r)| (r[0].as_i64().unwrap(), 1, i))
        .collect();
    for (i, l) in v["lifecycle"].as_array().unwrap().iter().enumerate() {
        timeline.push((l["t_us"].as_i64().unwrap(), 0, i));
    }
    timeline.sort();
    let mut autos: Option<Automations> = None;
    let mut last_t = i64::MIN;
    for (t, kind, i) in timeline {
        // Inputs closer than 5 ms to the previous one share a tick.
        if t - last_t >= 5_000 || kind == 0 {
            advance_in_steps(&hass, at(t)).await;
        }
        last_t = t;
        if kind == 0 {
            match v["lifecycle"][i]["op"].as_str().unwrap() {
                "setup" => {
                    hass.set_restore(ha_core::restore::RestoreState::load(&dir));
                    autos = Some(Automations::setup(&hass, &registries, &all));
                }
                "start" => {
                    if let Some(a) = &autos {
                        a.started();
                    }
                }
                "stop" => {
                    if let Some(a) = autos.take() {
                        a.stop();
                        drop(a);
                        hass.restore().dump(&hass).await;
                    }
                }
                _ => {}
            }
        } else {
            let r = &inputs[i];
            let eid = entities[r[1].as_u64().unwrap() as usize].clone();
            let mut attrs = Map::new();
            attrs.insert("rev".into(), r[3].clone());
            let state = hass.set_state(eid, r[2].as_str().unwrap(), attrs);
            input_of.insert(state.context.id, i);
        }
        if inputs
            .get(i + 1)
            .is_none_or(|n| n[0].as_i64().unwrap() - t >= 5_000)
        {
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
        }
        drain(&mut bus, &input_of);
    }
    let _ = std::fs::remove_dir_all(&dir);

    // Match each expected row to one of ours.
    let near = |a: i64, b: i64| (a - b).abs() <= 1_000_000;
    let mut used = vec![false; triggered.len()];
    let mut missing = Vec::new();
    for row in v["expected_triggered"]["rows"].as_array().unwrap() {
        let (t, a) = (
            row[0].as_i64().unwrap(),
            &autos_ids[row[1].as_u64().unwrap() as usize],
        );
        let src = row[2].as_u64().map(|s| sources[s as usize].clone());
        let input = row[3].as_u64().map(|x| x as usize);
        match triggered
            .iter()
            .enumerate()
            .position(|(k, (tt, aa, ss, ii))| {
                !used[k] && aa == a && *ss == src && *ii == input && near(*tt, t)
            }) {
            Some(k) => used[k] = true,
            None => missing.push(format!("triggered {} {a} {src:?} input {input:?}", at(t))),
        }
    }
    let extra_triggered = used.iter().filter(|u| !**u).count();
    let service_data = v["service_data"].as_array().unwrap();
    let mut used = vec![false; calls.len()];
    for row in v["expected_calls"]["rows"].as_array().unwrap() {
        let t = row[0].as_i64().unwrap();
        let service = row[2].as_str().unwrap();
        let data = &service_data[row[3].as_u64().unwrap() as usize];
        let input = row[4].as_u64().map(|x| x as usize);
        match calls.iter().enumerate().position(|(k, (tt, s, d, ii))| {
            !used[k] && s == service && d == data && *ii == input && near(*tt, t)
        }) {
            Some(k) => used[k] = true,
            None => missing.push(format!("call {} {service} {data} input {input:?}", at(t))),
        }
    }
    let extra_calls = used.iter().filter(|u| !**u).count();
    eprintln!(
        "replay: {} triggered ({} extra), {} calls ({} extra), {} missing",
        triggered.len(),
        extra_triggered,
        calls.len(),
        extra_calls,
        missing.len()
    );
    assert!(
        missing.is_empty() && extra_triggered == 0 && extra_calls == 0,
        "{} missing:\n{}",
        missing.len(),
        missing
            .iter()
            .take(30)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn collects_automations_from_configuration_yaml() {
    let c = ha_core::yaml::parse(
        "automation: [{id: a}, {id: b}]\nautomation extra:\n  id: c\nautomation old: []\nscript: {}\n",
    )
    .unwrap();
    let ids: Vec<Value> = from_config(&c).iter().map(|a| a["id"].clone()).collect();
    assert_eq!(ids, [json!("a"), json!("b"), json!("c")]);
}

/// A small engine under paused time, for the cases the vectors don't
/// cover.
async fn engine(yaml: &str) -> (Hass, Automations, Arc<PMutex<Vec<String>>>) {
    let hass = Hass::with_clock(
        ha_core::SimulatedClock::new("2026-11-10T12:00:00Z".parse().unwrap()).clock(),
    );
    let calls: Arc<PMutex<Vec<String>>> = Arc::default();
    for service in ["turn_on", "turn_off"] {
        let calls = calls.clone();
        hass.services()
            .register("light", service, SupportsResponse::None, move |_| {
                calls.lock().push(service.to_owned());
                async { Ok(None) }
            });
    }
    let items = match ha_core::yaml::parse(yaml).unwrap().to_json() {
        Value::Array(a) => a,
        v => vec![v],
    };
    let autos = Automations::setup(&hass, &Registries::default(), &items);
    (hass, autos, calls)
}

fn set(hass: &Hass, id: &str, state: &str) {
    hass.set_state(EntityId::parse(id).unwrap(), state, Map::new());
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn disabled_parts_and_state_for() {
    let (hass, autos, calls) = engine(
        "
- id: a
  triggers:
    - {trigger: state, entity_id: binary_sensor.m, to: 'off', for: {minutes: 5}}
    - {trigger: state, entity_id: binary_sensor.n, to: 'on', enabled: false}
  conditions:
    - {condition: state, entity_id: binary_sensor.nope, state: 'on', enabled: false}
  actions:
    - {action: light.turn_on, enabled: false}
    - action: light.turn_off
",
    )
    .await;
    hass.start();
    autos.started();
    set(&hass, "binary_sensor.m", "on");
    set(&hass, "binary_sensor.n", "off");
    settle().await;
    // A disabled trigger isn't attached.
    set(&hass, "binary_sensor.n", "on");
    // `for`: leaving `off` before 5 min cancels; staying 5 min fires once.
    set(&hass, "binary_sensor.m", "off");
    settle().await;
    tokio::time::sleep(Duration::from_secs(120)).await;
    set(&hass, "binary_sensor.m", "on");
    settle().await;
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert!(calls.lock().is_empty());
    set(&hass, "binary_sensor.m", "off");
    settle().await;
    tokio::time::sleep(Duration::from_secs(299)).await;
    assert!(calls.lock().is_empty());
    tokio::time::sleep(Duration::from_secs(2)).await;
    settle().await;
    // The disabled condition passed; the disabled action was skipped.
    assert_eq!(*calls.lock(), ["turn_off"]);
}

#[tokio::test(start_paused = true)]
async fn wait_for_is_per_entity_and_ends_on_removal() {
    let (hass, autos, calls) = engine(
        "
- id: w
  mode: restart
  triggers: [{trigger: state, entity_id: binary_sensor.go, to: 'on'}]
  actions:
    - wait_for_trigger:
        - {trigger: state, entity_id: [binary_sensor.a, binary_sensor.b], to: 'off', for: {minutes: 5}}
    - action: light.turn_off
",
    )
    .await;
    for e in ["binary_sensor.a", "binary_sensor.b", "binary_sensor.go"] {
        set(&hass, e, "on");
    }
    hass.start();
    autos.started();
    set(&hass, "binary_sensor.go", "off");
    settle().await;
    set(&hass, "binary_sensor.go", "on");
    settle().await;
    // A goes off; B changing doesn't cancel A's 5 minutes.
    set(&hass, "binary_sensor.a", "off");
    settle().await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    set(&hass, "binary_sensor.b", "off");
    set(&hass, "binary_sensor.b", "on");
    settle().await;
    tokio::time::sleep(Duration::from_secs(241)).await;
    settle().await;
    assert_eq!(*calls.lock(), ["turn_off"]);
    // Removing the entity cancels its pending `for`.
    calls.lock().clear();
    set(&hass, "binary_sensor.go", "off");
    set(&hass, "binary_sensor.a", "on");
    settle().await;
    set(&hass, "binary_sensor.go", "on");
    settle().await;
    set(&hass, "binary_sensor.a", "off");
    settle().await;
    hass.remove_state(&EntityId::parse("binary_sensor.a").unwrap());
    settle().await;
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert!(calls.lock().is_empty());
}

/// A change still queued when the triggers attach isn't heard.
#[tokio::test(start_paused = true)]
async fn changes_before_attaching_are_not_heard() {
    let (hass, autos, calls) = engine(
        "
- id: q
  triggers: [{trigger: state, entity_id: binary_sensor.m, to: 'on'}]
  actions: [{action: light.turn_on}]
",
    )
    .await;
    set(&hass, "binary_sensor.m", "off");
    settle().await;
    hass.start();
    set(&hass, "binary_sensor.m", "on");
    autos.started();
    settle().await;
    assert!(calls.lock().is_empty());
    set(&hass, "binary_sensor.m", "off");
    set(&hass, "binary_sensor.m", "on");
    settle().await;
    assert_eq!(*calls.lock(), ["turn_on"]);
}

#[test]
fn next_local_folds_and_the_current_second() {
    let hass = Hass::new();
    hass.set_config(ha_core::config::CoreConfig {
        time_zone: "Europe/London".into(),
        ..Default::default()
    });
    let t = |s: &str| s.parse::<jiff::Timestamp>().unwrap();
    let at = jiff::civil::time(1, 30, 0, 0);
    // 2026-10-25 01:30 happens twice: BST, then GMT.
    let first = time::next_local(&hass, t("2026-10-25T00:00:00Z"), at, 0).unwrap();
    assert_eq!(first, t("2026-10-25T00:30:00Z"));
    let second =
        time::next_local(&hass, first + jiff::SignedDuration::from_secs(1), at, 0).unwrap();
    assert_eq!(second, t("2026-10-25T01:30:00Z"));
    // Attached at 10:00:00.1: today's 10:00:00 still counts.
    let ten = jiff::civil::time(10, 0, 0, 0);
    assert_eq!(
        time::next_local(&hass, t("2026-11-10T10:00:00.1Z"), ten, 300_000).unwrap(),
        t("2026-11-10T10:00:00.3Z")
    );
}

#[test]
fn unsupported_wait_triggers_are_refused() {
    let y = ha_core::yaml::parse(
        "{id: x, triggers: [{trigger: time, at: '10:00'}], actions: [{wait_for_trigger: [{trigger: time, at: '11:00'}]}]}",
    )
    .unwrap();
    assert!(config::parse(&y.to_json()).is_err());
}
