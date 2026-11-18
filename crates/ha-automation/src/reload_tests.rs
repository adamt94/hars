//! `automation.reload` against HA 2026.6.1: `vectors/reload.json`, made by
//! `scripts/vectors/automation_reload.py` running HA's own automation
//! component, entity registry and restore state at simulated time. Each
//! scenario's automations and steps are replayed here (the file a step
//! rewrites is what the loader returns), and what hars did is compared
//! with what HA did, in order and step by step: every `state_changed` of an
//! automation with its attributes, the `entity_registry_updated`,
//! `automation_reloaded` and `automation_triggered` events, the service
//! calls the actions made and what a call raised; then the automations'
//! registry rows and states at the end. HA's ERROR lines are compared per
//! step.
//!
//! Not compared: how an invalid automation's log line names it and says
//! why (hars says `could not be validated` where HA names the stage) and
//! a YAML error's text past `Failed to load configuration:`.

use super::template_tests::{Logs, ONE_AT_A_TIME, advance, settle};
use super::*;
use ha_core::yaml::Yaml;
use parking_lot::Mutex as PMutex;
use std::collections::BTreeMap;

/// What `automations.yaml` holds, as `automation: !include` loads it.
fn file(text: &str) -> Result<Vec<Yaml>, String> {
    let list = ha_core::yaml::parse(text).map_err(|e| e.to_string())?;
    Ok(from_config_yaml(&Yaml::Map(vec![(
        Yaml::Str("automation".into()),
        list,
    )])))
}

fn state_json(s: Option<&Arc<State>>) -> Value {
    match s {
        Some(s) => json!({"state": s.state, "attributes": s.attributes}),
        None => Value::Null,
    }
}

/// HA's validation error, as compared: that an automation is invalid.
/// How it is named (Python's text of whatever the alias is), the stage
/// it failed at and the reason are worded by each engine.
fn log_text(msg: &str) -> String {
    match msg.contains("and has been disabled:") {
        true => "an automation is invalid".to_owned(),
        false => msg.to_owned(),
    }
}

/// The vector's observations and log lines, by step (`-1`: before the
/// first).
fn expected(v: &Value) -> (BTreeMap<i64, Vec<String>>, BTreeMap<i64, Vec<String>>) {
    let (mut obs, mut logs): (BTreeMap<i64, Vec<String>>, BTreeMap<i64, Vec<String>>) =
        Default::default();
    let mut step = -1;
    for e in v["expected"].as_array().unwrap() {
        let t = e["t"]
            .as_str()
            .unwrap()
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_second();
        let line = match e["kind"].as_str().unwrap() {
            "step" => {
                step = e["n"].as_i64().unwrap();
                continue;
            }
            "started" => continue,
            "log" => {
                logs.entry(step).or_default().push(format!(
                    "{} {}",
                    e["level"].as_str().unwrap(),
                    log_text(e["msg"].as_str().unwrap())
                ));
                continue;
            }
            "state" => format!(
                "state {} {} -> {} by_call={}",
                e["entity_id"].as_str().unwrap(),
                e["old"],
                e["new"],
                e["by_call"]
            ),
            "registry" => format!("registry {}", e["data"]),
            "reloaded" => format!("reloaded {} by_call={}", e["data"], e["by_call"]),
            "triggered" => format!("triggered {}", e["data"]),
            "call" => format!(
                "call {}.{} {}",
                e["domain"].as_str().unwrap(),
                e["service"].as_str().unwrap(),
                e["data"]
            ),
            "error" => {
                let msg = e["msg"].as_str().unwrap();
                let msg = match msg.strip_prefix("Failed to load configuration:") {
                    Some(_) => "Failed to load configuration:",
                    None => msg,
                };
                format!("error {} {msg}", e["type"].as_str().unwrap())
            }
            other => panic!("unknown kind {other}"),
        };
        obs.entry(step).or_default().push(format!("{t} {line}"));
    }
    (obs, logs)
}

/// Run one scenario; what hars did, as [`expected`] gives it, then the
/// registry rows and the states at the end.
async fn replay(
    v: &Value,
    start: jiff::Timestamp,
) -> (
    BTreeMap<i64, Vec<String>>,
    BTreeMap<i64, Vec<String>>,
    Value,
    Value,
) {
    let sim = ha_core::SimulatedClock::new(start);
    let hass = Hass::with_clock(sim.clock());
    hass.set_config(ha_core::config::CoreConfig {
        time_zone: "Europe/London".into(),
        ..Default::default()
    });
    let logs = Logs(Arc::default(), sim.clock());
    use tracing_subscriber::layer::SubscriberExt;
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.clone()));
    for service in ["turn_on", "turn_off"] {
        hass.services()
            .register("light", service, SupportsResponse::None, |_| async {
                Ok(None)
            });
    }
    let mut bus = hass.bus().subscribe();
    for s in v["states"].as_array().unwrap() {
        let eid = EntityId::parse(s["entity_id"].as_str().unwrap()).unwrap();
        let attrs = s["attributes"].as_object().cloned().unwrap_or_default();
        hass.set_state(eid, s["state"].as_str().unwrap(), attrs);
    }
    settle().await;
    let registry = ha_registry::Registry::in_memory(&hass, Registries::default());
    let on_disk = Arc::new(PMutex::new(file(&v["automations"].to_string())));
    let items = on_disk.lock().clone().unwrap();
    let autos = Automations::setup_live(&hass, registry.clone(), &items).await;
    {
        let on_disk = on_disk.clone();
        autos.set_loader(Arc::new(move || {
            let now = on_disk.lock().clone();
            Box::pin(async move { now })
        }));
    }
    settle().await;
    hass.start();
    autos.started();
    settle().await;

    let (mut obs, mut lines): (BTreeMap<i64, Vec<String>>, BTreeMap<i64, Vec<String>>) =
        Default::default();
    // The context of the step's (last) service call.
    let mut call_context: Option<ha_core::ContextId> = None;
    let errors: Arc<PMutex<Vec<String>>> = Arc::default();
    let mut collect = |step: i64, call_context: Option<ha_core::ContextId>| {
        let out = obs.entry(step).or_default();
        while let Ok(e) = bus.try_recv() {
            let t = e.time_fired.as_second();
            let by_call = Some(e.context.id) == call_context;
            let line = match &e.kind {
                EventKind::StateChanged {
                    entity_id,
                    old_state,
                    new_state,
                } if entity_id.as_str().starts_with("automation.") => format!(
                    "state {entity_id} {} -> {} by_call={by_call}",
                    state_json(old_state.as_ref()),
                    state_json(new_state.as_ref()),
                ),
                EventKind::CallService {
                    domain,
                    service,
                    service_data,
                } if domain == "light" => format!(
                    "call {domain}.{service} {}",
                    Value::Object((**service_data).clone())
                ),
                EventKind::Custom { name, data } => match name.as_str() {
                    "entity_registry_updated" => format!("registry {data}"),
                    "automation_reloaded" => format!("reloaded {data} by_call={by_call}"),
                    "automation_triggered" => format!("triggered {data}"),
                    _ => continue,
                },
                _ => continue,
            };
            out.push(format!("{t} {line}"));
        }
        let now = hass.clock().now().as_second();
        out.extend(errors.lock().drain(..).map(|e| format!("{now} error {e}")));
        let logged = lines.entry(step).or_default();
        for (level, logger, msg, _) in logs.0.lock().drain(..) {
            if level == tracing::Level::ERROR && logger.starts_with("homeassistant.") {
                logged.push(format!("ERROR {}", log_text(&msg)));
            }
        }
        if out.is_empty() {
            obs.remove(&step);
        }
        if logged.is_empty() {
            lines.remove(&step);
        }
    };
    collect(-1, None);
    let call = |step: &Value| {
        let ctx = Context::at(hass.clock().now());
        let id = ctx.id;
        let (h, errors) = (hass.clone(), errors.clone());
        let (d, s) = (
            step["domain"].as_str().unwrap().to_owned(),
            step["service"].as_str().unwrap().to_owned(),
        );
        let data = step["data"].as_object().cloned().unwrap_or_default();
        tokio::spawn(async move {
            match h.call_service(&d, &s, data, None, false, Some(ctx)).await {
                Ok(_) => {}
                Err(ServiceError::Invalid(m)) => errors.lock().push(format!("MultipleInvalid {m}")),
                Err(ServiceError::Validation(r)) => {
                    let m = match r.message.strip_prefix("Failed to load configuration:") {
                        Some(_) => "Failed to load configuration:",
                        None => &r.message,
                    };
                    errors.lock().push(format!("ServiceValidationError {m}"));
                }
                Err(e) => errors.lock().push(format!("{e:?}")),
            }
        });
        id
    };
    for (n, step) in v["steps"].as_array().unwrap().iter().enumerate() {
        match step["op"].as_str().unwrap() {
            "set" => {
                let eid = EntityId::parse(step["entity_id"].as_str().unwrap()).unwrap();
                let attrs = step["attributes"].as_object().cloned().unwrap_or_default();
                hass.set_state(eid, step["state"].as_str().unwrap(), attrs);
                settle().await;
            }
            "wait" => advance(&hass, step["s"].as_f64().unwrap()).await,
            "write" => {
                let text = match step["text"].as_str() {
                    Some(t) => t.to_owned(),
                    None => step["automations"].to_string(),
                };
                *on_disk.lock() = file(&text);
            }
            "service" => {
                call_context = Some(call(step));
                settle().await;
            }
            "services" => {
                for c in step["calls"].as_array().unwrap() {
                    call_context = Some(call(c));
                }
                settle().await;
            }
            other => panic!("unknown op {other}"),
        }
        collect(i64::try_from(n).unwrap(), call_context);
    }
    let rows: Vec<Value> = registry
        .snapshot()
        .entities
        .iter()
        .filter(|e| e.platform == DOMAIN)
        .map(|e| {
            json!({
                "entity_id": e.entity_id, "unique_id": e.unique_id, "name": e.name,
                "original_name": e.original_name, "object_id_base": e.object_id_base,
                "capabilities": e.capabilities.as_ref().map(|c| Value::Object(c.object())),
                "options": Value::Object(e.options.object()),
                "disabled_by": e.disabled_by, "has_entity_name": e.has_entity_name,
                "supported_features": e.supported_features,
            })
        })
        .collect();
    let states: Map<String, Value> = hass
        .states()
        .all()
        .iter()
        .filter(|s| s.entity_id.as_str().starts_with("automation."))
        .map(|s| (s.entity_id.to_string(), state_json(Some(s))))
        .collect();
    (obs, lines, json!(rows), Value::Object(states))
}

#[tokio::test(start_paused = true)]
async fn reload_matches_ha() {
    let _one = ONE_AT_A_TIME.lock().await;
    let all: Value = serde_json::from_str(include_str!("vectors/reload.json")).unwrap();
    let start = all["start"]
        .as_str()
        .unwrap()
        .parse::<jiff::Zoned>()
        .map(|z| z.timestamp())
        .or_else(|_| all["start"].as_str().unwrap().parse())
        .unwrap();
    let mut failures = Vec::new();
    for v in all["scenarios"].as_array().unwrap() {
        let name = v["name"].as_str().unwrap();
        let (want, want_logs) = expected(v);
        let (got, got_logs, rows, states) = replay(v, start).await;
        let mut report = String::new();
        for step in want
            .keys()
            .chain(got.keys())
            .collect::<std::collections::BTreeSet<_>>()
        {
            let (w, g) = (want.get(step), got.get(step));
            if w != g {
                report += &format!(
                    "  step {step}:\n    HA:   {}\n    hars: {}\n",
                    w.map(|w| w.join("\n          ")).unwrap_or_default(),
                    g.map(|g| g.join("\n          ")).unwrap_or_default(),
                );
            }
        }
        if want_logs != got_logs {
            report += &format!("  logs:\n    HA:   {want_logs:?}\n    hars: {got_logs:?}\n");
        }
        if rows != v["registry"] {
            report += &format!(
                "  registry:\n    HA:   {}\n    hars: {rows}\n",
                v["registry"]
            );
        }
        if states != v["final"] {
            report += &format!("  final:\n    HA:   {}\n    hars: {states}\n", v["final"]);
        }
        if !report.is_empty() {
            failures.push(format!("{name}:\n{report}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} scenarios differ from HA:\n{}",
        failures.len(),
        all["scenarios"].as_array().unwrap().len(),
        failures.join("\n")
    );
}

/// One automation, a loader that returns `file`'s automations, started.
async fn one(hass: &Hass, on_disk: &Arc<PMutex<Result<Vec<Yaml>, String>>>) -> Automations {
    let registry = ha_registry::Registry::in_memory(hass, Registries::default());
    let items = on_disk.lock().clone().unwrap();
    let autos = Automations::setup_live(hass, registry, &items).await;
    let on_disk = on_disk.clone();
    autos.set_loader(Arc::new(move || {
        let now = on_disk.lock().clone();
        Box::pin(async move { now })
    }));
    hass.start();
    autos.started();
    settle().await;
    autos
}

const HALL: &str = r#"[{"id": "hall", "alias": "Hall motion",
    "triggers": [{"trigger": "state", "entity_id": "binary_sensor.hall_motion", "to": "on"}],
    "actions": [{"action": "light.turn_on", "target": {"entity_id": "light.hall"}}]}]"#;

/// HA's admin services: a call with no user is the system's; a user must
/// exist and be an admin.
#[tokio::test(start_paused = true)]
async fn reload_is_for_admins() {
    let _one = ONE_AT_A_TIME.lock().await;
    let hass = Hass::new();
    let autos = one(&hass, &Arc::new(PMutex::new(file(HALL)))).await;
    autos.set_admin_check(Arc::new(|user| match user {
        "admin" => Some(true),
        "guest" => Some(false),
        _ => None,
    }));
    let mut bus = hass.bus().subscribe();
    let reload = |user: Option<&str>| {
        let mut ctx = Context::new();
        ctx.user_id = user.map(Into::into);
        let hass = hass.clone();
        async move {
            hass.call_service("automation", "reload", Map::new(), None, false, Some(ctx))
                .await
        }
    };
    let reloaded = |bus: &mut tokio::sync::broadcast::Receiver<Arc<Event>>| {
        let mut n = 0;
        while let Ok(e) = bus.try_recv() {
            if matches!(&e.kind, EventKind::Custom { name, .. } if name == "automation_reloaded") {
                n += 1;
            }
        }
        n
    };
    assert_eq!(
        reload(Some("guest")).await,
        Err(ServiceError::Failed("Unauthorized".into()))
    );
    assert_eq!(
        reload(Some("gone")).await,
        Err(ServiceError::Failed("UnknownUser".into()))
    );
    assert_eq!(reloaded(&mut bus), 0);
    assert_eq!(reload(Some("admin")).await, Ok(None));
    assert_eq!(reload(None).await, Ok(None));
    assert_eq!(reloaded(&mut bus), 2);
}

/// Python's `==` between raw configs: key order, `1 == 1.0 == True`.
#[test]
fn raw_configs_compare_as_python_dicts() {
    let eq = |a: &str, b: &str| {
        py_eq(
            &serde_json::from_str(a).unwrap(),
            &serde_json::from_str(b).unwrap(),
        )
    };
    assert!(eq(
        r#"{"a": 1, "b": [1, {"c": "x"}]}"#,
        r#"{"b": [1.0, {"c": "x"}], "a": true}"#
    ));
    assert!(eq(r#"{"a": 0}"#, r#"{"a": false}"#));
    assert!(!eq(r#"{"a": "1"}"#, r#"{"a": 1}"#));
    assert!(!eq(r#"{"a": 1}"#, r#"{"a": 1, "b": null}"#));
    assert!(!eq(r#"[1, 2]"#, r#"[2, 1]"#));
    assert!(!eq(r#"{"a": null}"#, r#"{"a": false}"#));
    // Integers past what a float holds are still told apart.
    assert!(!eq("9007199254740992", "9007199254740993"));
    assert!(eq("9007199254740993", "9007199254740993"));
    assert!(eq("2", "2.0"));
}

/// A reload whose call is dropped half way ends its turn: the next one
/// runs.
#[tokio::test(start_paused = true)]
async fn a_dropped_reload_does_not_block_the_next() {
    let _one = ONE_AT_A_TIME.lock().await;
    let hass = Hass::new();
    let on_disk = Arc::new(PMutex::new(file(HALL)));
    let autos = one(&hass, &on_disk).await;
    // A loader that never answers, then one that does.
    autos.set_loader(Arc::new(|| Box::pin(std::future::pending())));
    let stuck = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        autos.reload(None, &Context::new()),
    )
    .await;
    assert!(stuck.is_err());
    let on_disk = on_disk.clone();
    autos.set_loader(Arc::new(move || {
        let now = on_disk.lock().clone();
        Box::pin(async move { now })
    }));
    let done = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        autos.reload(None, &Context::new()),
    )
    .await;
    assert_eq!(done, Ok(Ok(())));
}

/// An automation that reloads itself, changed on disk: the reload stops
/// the very run that called it and still finishes (it replaces the
/// automations in a task of its own). The old run's next action never
/// happens; the new config is what runs from then on.
#[tokio::test(start_paused = true)]
async fn an_automation_can_reload_itself() {
    let _one = ONE_AT_A_TIME.lock().await;
    let hass = Hass::new();
    hass.services()
        .register("light", "turn_on", SupportsResponse::None, |_| async {
            Ok(None)
        });
    let motion = EntityId::parse("binary_sensor.hall_motion").unwrap();
    hass.set_state(motion.clone(), "off", Map::new());
    let config = |actions: &str| {
        file(&format!(
            r#"[{{"id": "self", "alias": "Self", "triggers": [{{"trigger": "state",
                "entity_id": "binary_sensor.hall_motion", "to": "on"}}], "actions": {actions}}}]"#
        ))
    };
    let on_disk = Arc::new(PMutex::new(config(
        r#"[{"action": "automation.reload"},
            {"action": "light.turn_on", "target": {"entity_id": "light.old"}}]"#,
    )));
    let autos = one(&hass, &on_disk).await;
    *on_disk.lock() =
        config(r#"[{"action": "light.turn_on", "target": {"entity_id": "light.new"}}]"#);
    let mut bus = hass.bus().subscribe();
    let seen = |bus: &mut tokio::sync::broadcast::Receiver<Arc<Event>>| {
        let mut out = Vec::new();
        while let Ok(e) = bus.try_recv() {
            match &e.kind {
                EventKind::CallService {
                    domain,
                    service_data,
                    ..
                } if domain == "light" => out.push(service_data["entity_id"].to_string()),
                EventKind::Custom { name, .. } if name == "automation_reloaded" => {
                    out.push("reloaded".to_owned());
                }
                _ => {}
            }
        }
        out
    };
    hass.set_state(motion.clone(), "on", Map::new());
    settle().await;
    assert_eq!(seen(&mut bus), ["reloaded"]);
    let now = autos.by_entity("automation.self").unwrap();
    assert_eq!(now.live.lock().runs.len(), 0);
    let state = hass.states().get(&now.entity_id).unwrap();
    assert_eq!(
        (state.state.as_str(), &state.attributes["current"]),
        ("on", &json!(0))
    );
    // The next reload isn't stuck behind that one.
    assert_eq!(autos.reload(None, &Context::new()).await, Ok(()));
    assert_eq!(seen(&mut bus), ["reloaded"]);
    hass.set_state(motion.clone(), "off", Map::new());
    settle().await;
    hass.set_state(motion, "on", Map::new());
    settle().await;
    assert_eq!(seen(&mut bus), [r#"["light.new"]"#]);
}
