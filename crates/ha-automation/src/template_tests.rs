//! Templates in automations against HA 2026.6.1: `vectors/templates.json`,
//! made by `scripts/vectors/automation_templates.py` running HA's own
//! automation engine at simulated time. Each scenario's states, automations
//! and steps are replayed here, and what hars did is compared with what HA
//! did: the service calls with their rendered data, the
//! `automation_triggered` events, the automations' states and HA's WARNING
//! and ERROR lines. Observations at one instant are compared grouped by
//! kind and by automation, as the vectors are written; times to the second
//! (time triggers fire a random 50–500 ms past it, in HA and here).
//!
//! The rest are cases the vectors can't hold: what hars refuses to load,
//! and that a template trigger sleeps when nothing it reads changes.

use super::*;
use parking_lot::Mutex as PMutex;
use std::collections::BTreeMap;
use std::time::Duration;

/// Template tests share the render threads' in-flight count: one at a
/// time.
pub(super) static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The loggers the vectors record.
const LOGGERS: &[&str] = &[
    "homeassistant.components.automation",
    "homeassistant.components.homeassistant.triggers",
    "homeassistant.components.template",
    "homeassistant.helpers.condition",
    "homeassistant.helpers.event",
    "homeassistant.helpers.script",
    "homeassistant.helpers.service",
    "homeassistant.helpers.template",
    "homeassistant.helpers.trigger",
];

/// A log line: level, logger, message, time.
pub(super) type Line = (tracing::Level, String, String, jiff::Timestamp);

/// Log lines, as a layer captures them.
#[derive(Clone)]
pub(super) struct Logs(pub(super) Arc<PMutex<Vec<Line>>>, pub(super) ha_core::Clock);

#[derive(Default)]
struct Fields {
    logger: String,
    message: String,
}

impl tracing::field::Visit for Fields {
    fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
        if f.name() == "logger" {
            self.logger = v.to_owned();
        }
    }

    fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
        match f.name() {
            "message" => self.message = format!("{v:?}"),
            "logger" => self.logger = format!("{v:?}").trim_matches('"').to_owned(),
            _ => {}
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Logs {
    fn on_event(&self, e: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut f = Fields::default();
        e.record(&mut f);
        self.0
            .lock()
            .push((*e.metadata().level(), f.logger, f.message, self.1.now()));
    }
}

/// Let everything due run: spawned tasks, and the renders they wait for
/// (on threads of their own, which a paused clock doesn't wait for).
pub(super) async fn settle() {
    let mut quiet = 0;
    while quiet < 3 {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        if ha_core::template::run::in_flight() == 0 {
            quiet += 1;
        } else {
            quiet = 0;
            std::thread::sleep(Duration::from_micros(200));
        }
    }
}

/// Move paused time on by `s` seconds: every timer due on the way fires,
/// in order, and the renders they start finish before time moves on (a
/// render runs from tokio's blocking pool, which holds a paused clock
/// still). Sleeps round up to whole milliseconds: the last 2 ms are
/// advanced exactly.
pub(super) async fn advance(hass: &Hass, s: f64) {
    let at = hass.clock().now() + jiff::SignedDuration::from_secs_f64(s);
    let d = Duration::from_secs_f64(s);
    if d > Duration::from_millis(2) {
        tokio::time::sleep(d - Duration::from_millis(2)).await;
    }
    settle().await;
    let rest = at.duration_since(hass.clock().now());
    if rest > jiff::SignedDuration::ZERO {
        tokio::time::advance(Duration::try_from(rest).unwrap()).await;
    }
    settle().await;
}

/// One observation, as compared: (second, kind, who, what).
type Obs = (i64, &'static str, String, String);

fn second(t: &str) -> i64 {
    t.parse::<jiff::Timestamp>()
        .unwrap_or_else(|_| panic!("a time: {t}"))
        .as_second()
}

/// The vector's observations, as compared.
fn expected(v: &Value) -> Vec<Obs> {
    let mut out = Vec::new();
    for e in v["expected"].as_array().unwrap() {
        let t = second(e["t"].as_str().unwrap());
        match e["kind"].as_str().unwrap() {
            "call" => out.push((
                t,
                "call",
                String::new(),
                format!("{}.{} {}", e["domain"], e["service"], e["data"]).replace('"', ""),
            )),
            "triggered" => out.push((
                t,
                "triggered",
                e["data"]["entity_id"].as_str().unwrap().to_owned(),
                e["data"].to_string(),
            )),
            "automation" => out.push((
                t,
                "automation",
                e["entity_id"].as_str().unwrap().to_owned(),
                e["state"].to_string(),
            )),
            "log" => out.push((
                t,
                "log",
                e["logger"].as_str().unwrap().to_owned(),
                format!(
                    "{} {}",
                    e["level"].as_str().unwrap(),
                    log_text(e["msg"].as_str().unwrap())
                ),
            )),
            other => panic!("unknown kind {other}"),
        }
    }
    sort(out)
}

/// A validation error's text is the validator's, not HA's voluptuous one:
/// only that it is one is compared.
fn log_text(msg: &str) -> String {
    match msg.find("could not be validated and has been disabled:") {
        Some(i) => msg[..i + "could not be validated and has been disabled:".len()].to_owned(),
        None => msg.to_owned(),
    }
}

/// Grouped by instant, kind and automation, each group in its own order.
/// Service calls name no automation, and loggers other than an
/// automation's own are shared: automations that run side by side write
/// those in no set order (in HA, Python's string hashing picks it), so
/// they are compared by what they are.
fn sort(mut obs: Vec<Obs>) -> Vec<Obs> {
    let shared = |o: &Obs| {
        o.1 == "call" || (o.1 == "log" && !o.2.starts_with("homeassistant.components.automation."))
    };
    obs.sort_by(|a, b| {
        let key = |o: &Obs| {
            let what = if shared(o) {
                o.3.clone()
            } else {
                String::new()
            };
            (o.0, o.1, o.2.clone(), what)
        };
        key(a).cmp(&key(b))
    });
    obs
}

/// ha-core's own deliberate differences in error text (see its template
/// tests' `DIFFERENT`): minijinja words an arithmetic error its own way.
const ENGINE_WORDING: &[(&str, &str)] = &[(
    "TemplateError: unable to calculate 1 / 0",
    "ZeroDivisionError: division by zero",
)];

fn engine_wording(msg: &str) -> String {
    let mut msg = msg.to_owned();
    for (ours, ha) in ENGINE_WORDING {
        msg = msg.replace(ours, ha);
    }
    msg
}

/// A scenario's automations as loaded (JSON is YAML): the variables keep
/// the order they were written in.
fn yaml_automations(name: &str) -> Vec<ha_core::yaml::Yaml> {
    use ha_core::yaml::Yaml;
    let all = ha_core::yaml::parse(include_str!("vectors/templates.json")).unwrap();
    let Some(Yaml::Seq(scenarios)) = all.get("scenarios") else {
        panic!("scenarios")
    };
    let scenario = scenarios
        .iter()
        .find(|s| s.get("name").and_then(Yaml::as_str) == Some(name))
        .unwrap();
    match scenario.get("automations") {
        Some(Yaml::Seq(items)) => items.clone(),
        _ => panic!("automations"),
    }
}

/// Run one scenario; what hars did.
async fn replay(v: &Value, yaml: &[ha_core::yaml::Yaml], start: jiff::Timestamp) -> Vec<Obs> {
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
    let calls: Arc<PMutex<Vec<Obs>>> = Arc::default();
    for (domain, service) in [
        ("notify", "mobile_app_phone"),
        ("light", "turn_on"),
        ("light", "turn_off"),
    ] {
        let (calls, h) = (calls.clone(), hass.clone());
        hass.services()
            .register(domain, service, SupportsResponse::None, move |call| {
                let what = format!("{domain}.{service} {}", Value::Object((*call.data).clone()))
                    .replace('"', "");
                calls
                    .lock()
                    .push((h.clock().now().as_second(), "call", String::new(), what));
                async { Ok(None) }
            });
    }
    let mut bus = hass.bus().subscribe();
    let set = |s: &Value| {
        let eid = EntityId::parse(s["entity_id"].as_str().unwrap()).unwrap();
        let attrs = s["attributes"].as_object().cloned().unwrap_or_default();
        hass.set_state(eid, s["state"].as_str().unwrap(), attrs);
    };
    for s in v["states"].as_array().unwrap() {
        set(s);
    }
    settle().await;
    let items = v["automations"].as_array().unwrap().clone();
    // The entity registry entries HA made, with their ids.
    let entities = v["registry"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let mut e = e.clone();
            e["platform"] = json!("vectors");
            Arc::new(serde_json::from_value(e).unwrap())
        })
        .collect();
    let registries = Registries {
        entities,
        ..Default::default()
    };
    let autos = Automations::setup_yaml(&hass, &registries, yaml).await;
    autos.set_registry(ha_registry::Registry::in_memory(&hass, registries));
    settle().await;
    hass.start();
    autos.started();
    settle().await;
    let mut ours: Vec<Obs> = Vec::new();
    for a in &items {
        let id = format!("automation.{}", a["id"].as_str().unwrap());
        let state = EntityId::parse(&id)
            .ok()
            .and_then(|e| hass.states().get(&e))
            .map(|s| s.state.to_string());
        ours.push((
            start.as_second(),
            "automation",
            id,
            json!(state).to_string(),
        ));
    }
    for step in v["steps"].as_array().unwrap() {
        match step["op"].as_str().unwrap() {
            "set" => {
                set(step);
                settle().await;
            }
            "wait" => advance(&hass, step["s"].as_f64().unwrap()).await,
            "service" => {
                let h = hass.clone();
                let (d, s) = (
                    step["domain"].as_str().unwrap().to_owned(),
                    step["service"].as_str().unwrap().to_owned(),
                );
                let data = step["data"].as_object().cloned().unwrap_or_default();
                tokio::spawn(async move { h.call_service(&d, &s, data, None, false, None).await });
                settle().await;
            }
            other => panic!("unknown op {other}"),
        }
    }
    while let Ok(e) = bus.try_recv() {
        if let EventKind::Custom { name, data } = &e.kind
            && name == "automation_triggered"
        {
            ours.push((
                e.time_fired.as_second(),
                "triggered",
                data["entity_id"].as_str().unwrap_or_default().to_owned(),
                data.to_string(),
            ));
        }
    }
    ours.extend(calls.lock().drain(..));
    for (level, logger, msg, t) in logs.0.lock().drain(..) {
        let level = match level {
            tracing::Level::WARN => "WARNING",
            tracing::Level::ERROR => "ERROR",
            _ => continue,
        };
        if !LOGGERS.iter().any(|l| logger.starts_with(l)) {
            continue;
        }
        let msg = engine_wording(&msg);
        ours.push((
            t.as_second(),
            "log",
            logger,
            format!("{level} {}", log_text(&msg)),
        ));
    }
    sort(ours)
}

#[tokio::test(start_paused = true)]
async fn templates_match_ha() {
    let _one = ONE_AT_A_TIME.lock().await;
    let all: Value = serde_json::from_str(include_str!("vectors/templates.json")).unwrap();
    let start: jiff::Timestamp = all["start"]
        .as_str()
        .unwrap()
        .parse::<jiff::Zoned>()
        .map(|z| z.timestamp())
        .or_else(|_| all["start"].as_str().unwrap().parse())
        .unwrap();
    let mut failures = BTreeMap::new();
    for v in all["scenarios"].as_array().unwrap() {
        let name = v["name"].as_str().unwrap();
        let want = expected(v);
        let got = replay(v, &yaml_automations(name), start).await;
        if want != got {
            let line = |(t, k, w, x): &Obs| format!("{} {k} {w} {x}", t - start.as_second());
            let missing: Vec<String> = want
                .iter()
                .filter(|o| !got.contains(o))
                .map(|o| format!("  - {}", line(o)))
                .collect();
            let extra: Vec<String> = got
                .iter()
                .filter(|o| !want.contains(o))
                .map(|o| format!("  + {}", line(o)))
                .collect();
            let order = if missing.is_empty() && extra.is_empty() {
                let all = |o: &[Obs]| o.iter().map(line).collect::<Vec<_>>().join("\n    ");
                format!(
                    "  in another order: HA\n    {}\n  hars\n    {}",
                    all(&want),
                    all(&got)
                )
            } else {
                String::new()
            };
            failures.insert(
                name.to_owned(),
                format!("{}\n{}{order}", missing.join("\n"), extra.join("\n")),
            );
        }
    }
    let n = all["scenarios"].as_array().unwrap().len();
    assert!(
        failures.is_empty(),
        "{} of {n} scenarios differ:\n{}",
        failures.len(),
        failures
            .iter()
            .map(|(k, v)| format!("== {k}\n{v}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn parse_yaml(yaml: &str) -> Result<config::Config, String> {
    config::parse(&ha_core::yaml::parse(yaml).unwrap().to_json())
}

/// What HA takes as a template and hars can't render yet fails validation
/// (the automation is `unavailable`), saying so; a template that doesn't
/// compile fails as HA's does.
#[test]
fn unsupported_templates_fail_validation() {
    let on = "actions: [{action: light.turn_on}]";
    let state = "triggers: [{trigger: state, entity_id: light.a}]";
    for (yaml, says) in [
        (
            format!(
                "triggers: [{{trigger: state, entity_id: light.a, for: \"{{{{ 5 }}}}\"}}]\n{on}"
            ),
            "templates in 'for' are not supported by hars yet",
        ),
        (
            format!(
                "triggers: [{{trigger: template, value_template: '{{{{ true }}}}', for: {{seconds: '{{{{ 5 }}}}'}}}}]\n{on}"
            ),
            "templates in 'for' are not supported by hars yet",
        ),
        (
            format!("triggers: [{{trigger: time, at: \"{{{{ '10:00' }}}}\"}}]\n{on}"),
            "templates in 'at' are not supported by hars yet",
        ),
        (
            format!(
                "triggers: [{{trigger: state, entity_id: light.a, enabled: '{{{{ true }}}}'}}]\n{on}"
            ),
            "templates in 'enabled' are not supported by hars yet",
        ),
        (
            format!(
                "triggers: [{{trigger: state, entity_id: light.a, variables: {{x: 1}}}}]\n{on}"
            ),
            "a trigger's 'variables' are not supported by hars yet",
        ),
        (
            format!("{state}\nactions: [{{delay: 5}}]"),
            "the 'delay' action is not supported by hars yet",
        ),
        (
            format!("{state}\nactions: [{{wait_template: '{{{{ true }}}}'}}]"),
            "the 'wait_template' action is not supported by hars yet",
        ),
        (
            format!("{state}\nactions: [{{variables: {{x: 1}}}}]"),
            "the 'variables' action is not supported by hars yet",
        ),
        (
            format!("{state}\nactions: [{{action: light.turn_on, response_variable: r}}]"),
            "extra keys not allowed",
        ),
        (
            format!("{state}\nactions: [{{action: light.turn_on, data: {{x: '{{{{ 1 + }}}}'}}}}]"),
            "invalid template (TemplateSyntaxError",
        ),
        (
            format!("{state}\nconditions: 'not a template'\n{on}"),
            "Expected a dictionary",
        ),
        (
            format!("{state}\nactions: [{{action: light.turn_on, service: light.turn_on}}]"),
            "Cannot specify both 'service' and 'action'",
        ),
    ] {
        let err = parse_yaml(&yaml).expect_err(&yaml);
        assert!(err.contains(says), "{yaml}\n  said: {err}");
    }
    // What renders, loads.
    for yaml in [
        format!("{state}\n{on}"),
        "trigger_variables: {a: '{{ 1 }}'}\nvariables: {b: '{{ a }}'}\ntriggers: [{trigger: template, value_template: '{{ b }}'}]\nactions: [{action: \"light.{{ 'turn_on' }}\", target: '{{ {} }}', data_template: {x: '{{ 1 }}'}}]".to_owned(),
        format!(
            "triggers: [{{trigger: state, entity_id: light.a, to: '{{{{ literal }}}}'}}]\nconditions: '{{{{ true }}}}'\n{on}"
        ),
    ] {
        parse_yaml(&yaml).unwrap_or_else(|e| panic!("{yaml}: {e}"));
    }
}

/// A template trigger renders only when something it read changes: other
/// states changing leave it asleep, and a template that doesn't read the
/// clock has no timer.
#[tokio::test(start_paused = true)]
async fn a_template_trigger_sleeps_until_what_it_reads_changes() {
    let _one = ONE_AT_A_TIME.lock().await;
    let sim = ha_core::SimulatedClock::new("2026-11-10T16:00:00Z".parse().unwrap());
    let hass = Hass::with_clock(sim.clock());
    let calls: Arc<PMutex<u32>> = Arc::default();
    let c = calls.clone();
    hass.services()
        .register("light", "turn_on", SupportsResponse::None, move |_| {
            *c.lock() += 1;
            async { Ok(None) }
        });
    hass.set_state(EntityId::parse("sensor.t").unwrap(), "20", Map::new());
    let items = ha_core::yaml::parse(
        "- id: hot\n  alias: hot\n  triggers: [{trigger: template, value_template: \"{{ states('sensor.t') | float > 27 }}\"}]\n  actions: [{action: light.turn_on}]",
    )
    .unwrap();
    let ha_core::yaml::Yaml::Seq(items) = items else {
        panic!("a list")
    };
    let autos = Automations::setup_yaml(&hass, &Registries::default(), &items).await;
    hass.start();
    autos.started();
    settle().await;
    let renders = || {
        let a = autos.by_entity("automation.hot").unwrap();
        let Ok(cfg) = &a.config else { panic!("valid") };
        let TriggerKind::Template { renders, .. } = &cfg.triggers[0].kind else {
            panic!("a template trigger")
        };
        renders.0.load(std::sync::atomic::Ordering::Relaxed)
    };
    // The trigger's own render and the tracker's (which HA counts twice).
    assert_eq!(renders(), 3);
    // Nobody listens to other entities: none of their changes is handed
    // to the trigger (not even to be filtered out).
    let delivered = autos.shared.listeners.delivered();
    for i in 0..50 {
        hass.set_state(
            EntityId::parse("sensor.other").unwrap(),
            i.to_string(),
            Map::new(),
        );
        settle().await;
    }
    tokio::time::sleep(Duration::from_secs(3600)).await;
    settle().await;
    assert_eq!(renders(), 3, "nothing it reads changed");
    assert_eq!(
        autos.shared.listeners.delivered(),
        delivered,
        "no change was delivered"
    );
    hass.set_state(EntityId::parse("sensor.t").unwrap(), "28", Map::new());
    settle().await;
    assert_eq!(renders(), 5);
    assert_eq!(*calls.lock(), 1);
}

/// An engine for one YAML automation, with `light.turn_on` counted.
async fn one_automation(
    hass: &Hass,
    yaml: &str,
    registries: &Registries,
) -> (Automations, Arc<PMutex<u32>>) {
    let calls: Arc<PMutex<u32>> = Arc::default();
    let c = calls.clone();
    hass.services()
        .register("light", "turn_on", SupportsResponse::None, move |_| {
            *c.lock() += 1;
            async { Ok(None) }
        });
    let ha_core::yaml::Yaml::Seq(items) = ha_core::yaml::parse(yaml).unwrap() else {
        panic!("a list")
    };
    let autos = Automations::setup_yaml(hass, registries, &items).await;
    hass.start();
    autos.started();
    settle().await;
    (autos, calls)
}

/// A minute tick re-renders a template that now reads another entity: a
/// change to it made after the render read it, before its listener is in,
/// is still heard (HA renders and re-listens in one go).
#[tokio::test(start_paused = true)]
async fn a_change_while_a_timed_render_relistens_is_heard() {
    let _one = ONE_AT_A_TIME.lock().await;
    let sim = ha_core::SimulatedClock::new("2026-11-10T16:00:00Z".parse().unwrap());
    let hass = Hass::with_clock(sim.clock());
    for e in ["sensor.a", "sensor.b"] {
        hass.set_state(EntityId::parse(e).unwrap(), "off", Map::new());
    }
    let (_autos, calls) = one_automation(
        &hass,
        "- {id: b, alias: b, triggers: [{trigger: template, value_template: \"{{ is_state('sensor.b', 'on') if now().minute >= 1 else is_state('sensor.a', 'on') }}\"}], actions: [{action: light.turn_on}]}",
        &Registries::default(),
    )
    .await;
    // Past this minute's tick, which still reads `sensor.a`.
    tokio::time::sleep(Duration::from_secs(30)).await;
    settle().await;
    let (reached, go) = track::gap::hold();
    // 16:01's tick has rendered, reading `sensor.b` (off); its listener
    // isn't in yet.
    reached.notified().await;
    hass.set_state(EntityId::parse("sensor.b").unwrap(), "on", Map::new());
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    go.notify_one();
    settle().await;
    assert_eq!(*calls.lock(), 1, "sensor.b turning on fired the trigger");
}

/// A target's registry id resolves against the registry when the call is
/// made: an entry removed while the render waited for a thread fails the
/// call with HA's error.
#[tokio::test(start_paused = true)]
async fn a_registry_id_removed_while_rendering_fails_the_call() {
    let _one = ONE_AT_A_TIME.lock().await;
    let sim = ha_core::SimulatedClock::new("2026-11-10T16:00:00Z".parse().unwrap());
    let hass = Hass::with_clock(sim.clock());
    let logs = Logs(Arc::default(), sim.clock());
    use tracing_subscriber::layer::SubscriberExt;
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.clone()));
    let uuid = "1e2feb89414c343c1027c4d1c386bbc4";
    let registries = Registries {
        entities: vec![Arc::new(
            serde_json::from_value(
                json!({"entity_id": "light.hall", "id": uuid, "platform": "vectors"}),
            )
            .unwrap(),
        )],
        ..Default::default()
    };
    hass.set_state(
        EntityId::parse("binary_sensor.w").unwrap(),
        "off",
        Map::new(),
    );
    let (autos, calls) = one_automation(
        &hass,
        &format!(
            "- {{id: r, alias: r, triggers: [{{trigger: state, entity_id: binary_sensor.w, to: 'on'}}], actions: [{{action: light.turn_on, target: {{entity_id: \"{{{{ '{uuid}' }}}}\"}}}}]}}"
        ),
        &registries,
    )
    .await;
    let registry = ha_registry::Registry::in_memory(&hass, registries);
    autos.set_registry(registry.clone());
    // Every render thread busy: the call's render waits.
    let held = ha_core::template::run::take_all_renders().await;
    hass.set_state(
        EntityId::parse("binary_sensor.w").unwrap(),
        "on",
        Map::new(),
    );
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    registry.remove_entity("light.hall");
    drop(held);
    settle().await;
    assert_eq!(*calls.lock(), 0);
    let errors: Vec<String> = logs
        .0
        .lock()
        .iter()
        .filter(|(level, ..)| *level == tracing::Level::ERROR)
        .map(|(_, _, msg, _)| msg.clone())
        .collect();
    assert!(
        errors
            .iter()
            .any(|m| m.ends_with(&format!("Template rendered invalid entity IDs: {uuid}"))),
        "{errors:?}"
    );
}
