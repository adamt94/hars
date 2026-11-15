use super::*;
use ha_core::Reported;
use jiff::Timestamp;
use serde_json::{Value, json};

const T0: i64 = 1_768_465_800_000_000; // 2026-01-15T08:30:00Z

fn ts(us: i64) -> Timestamp {
    Timestamp::from_microsecond(us).unwrap()
}

fn ctx(n: u8) -> Context {
    Context {
        id: ContextId::from_bytes([n; 16]),
        parent_id: None,
        user_id: None,
    }
}

fn state(id: &str, value: &str, attrs: Value, lu: i64, lc: i64, ctx: &Context) -> Arc<State> {
    let Value::Object(attributes) = attrs else {
        panic!("attributes must be an object")
    };
    Arc::new(State {
        entity_id: EntityId::parse(id).unwrap(),
        state: value.into(),
        attributes,
        last_changed: ts(lc),
        last_reported: Reported::new(ts(lu)),
        last_updated: ts(lu),
        context: ctx.clone(),
    })
}

fn changed(old: Option<&Arc<State>>, new: &Arc<State>) -> Event {
    Event::new(
        EventKind::StateChanged {
            entity_id: new.entity_id.clone(),
            old_state: old.cloned(),
            new_state: Some(new.clone()),
        },
        new.last_updated,
        new.context.clone(),
    )
}

fn state_row(row: Option<Row>) -> StateRow {
    match row {
        Some(Row::State(s)) => s,
        other => panic!("not a state row: {other:?}"),
    }
}

fn event_row(row: Option<Row>) -> EventRow {
    match row {
        Some(Row::Event(e)) => e,
        other => panic!("not an event row: {other:?}"),
    }
}

fn aug(a: &Option<Arc<str>>) -> Value {
    a.as_deref()
        .map_or(Value::Null, |s| serde_json::from_str(s).unwrap())
}

/// Recorded attributes leave out HA's unrecorded sets and keep UTF-8 and
/// floats as orjson writes them; past 16 KiB they are `{}`.
#[test]
fn attributes_are_filtered_and_encoded() {
    let hass = Hass::new();
    let mut intake = Intake::new(hass.clone());
    let c = ctx(1);
    let light = state(
        "light.hallway",
        "on",
        json!({"brightness": 200, "color_mode": "xy", "supported_color_modes": ["xy"],
               "friendly_name": "Hallway", "supported_features": 44, "restored": true,
               "mode": "normal"}),
        T0,
        T0,
        &c,
    );
    let row = state_row(intake.row(&changed(None, &light)));
    assert_eq!(
        &*row.attributes,
        r#"{"friendly_name":"Hallway","mode":"normal"}"#
    );
    assert_eq!(row.flags, 0, "no previous row: not in the logbook");

    let temp = state(
        "sensor.hall_temperature",
        "21.5",
        json!({"unit_of_measurement": "°C", "state_class": "measurement",
               "pressure": 1012.0, "options": ["a"]}),
        T0,
        T0,
        &c,
    );
    let row = state_row(intake.row(&changed(None, &temp)));
    assert_eq!(
        &*row.attributes,
        r#"{"pressure":1012.0,"state_class":"measurement","unit_of_measurement":"°C"}"#
    );
    assert_eq!(row.flags, HAS_UNIT);

    let big = state(
        "sensor.big",
        "x",
        json!({"blob": "y".repeat(MAX_ATTRS_BYTES)}),
        T0,
        T0,
        &c,
    );
    assert_eq!(
        &*state_row(intake.row(&changed(None, &big))).attributes,
        "{}"
    );
}

/// D2: an attribute-only write that records nothing new writes no row (a
/// light's brightness); in a significant domain it does (a climate's
/// temperature), and so does any recorded attribute change.
#[test]
fn repeated_content_is_skipped_outside_significant_domains() {
    let mut intake = Intake::new(Hass::new());
    let c = ctx(1);
    let on = state("light.a", "on", json!({"brightness": 10}), T0, T0, &c);
    assert!(intake.row(&changed(None, &on)).is_some());
    let dimmer = state("light.a", "on", json!({"brightness": 90}), T0 + 1, T0, &c);
    assert!(intake.row(&changed(Some(&on), &dimmer)).is_none());
    let renamed = state(
        "light.a",
        "on",
        json!({"brightness": 90, "friendly_name": "A"}),
        T0 + 2,
        T0,
        &c,
    );
    assert!(intake.row(&changed(Some(&dimmer), &renamed)).is_some());

    let ac = |t: i64, cur: f64| {
        state(
            "climate.ac",
            "cool",
            json!({"current_temperature": cur, "hvac_modes": ["cool"]}),
            t,
            T0,
            &c,
        )
    };
    let a = ac(T0, 20.0);
    assert!(intake.row(&changed(None, &a)).is_some());
    let b = ac(T0 + 1, 21.0);
    assert!(intake.row(&changed(Some(&a), &b)).is_some());
    // Even unchanged: a significant domain keeps every write.
    let b2 = ac(T0 + 2, 21.0);
    assert!(intake.row(&changed(Some(&b), &b2)).is_some());
}

/// The logbook flag: a state change after a recorded row with another
/// state. Not the first row after a start, not an attribute-only write.
#[test]
fn visible_follows_has_old_state_rule() {
    let mut intake = Intake::new(Hass::new());
    let c = ctx(1);
    let a = state("sensor.washer", "running", json!({}), T0, T0, &c);
    // Its old state was from before the start: no row of ours to point at.
    let before = state("sensor.washer", "idle", json!({}), T0 - 9, T0 - 9, &c);
    assert_eq!(state_row(intake.row(&changed(Some(&before), &a))).flags, 0);
    let b = state("sensor.washer", "done", json!({}), T0 + 1, T0 + 1, &c);
    assert_eq!(state_row(intake.row(&changed(Some(&a), &b))).flags, VISIBLE);
    let attr = state("sensor.washer", "done", json!({"x": 1}), T0 + 2, T0 + 1, &c);
    assert_eq!(state_row(intake.row(&changed(Some(&b), &attr))).flags, 0);
}

/// The vector's context chains: a UI toggle carries the user and the
/// service; the motion automation's entry names the trigger state, and the
/// light it turns on names the automation.
#[test]
fn context_fields_follow_has_augmenter() {
    let mut intake = Intake::new(Hass::new());
    let user = "0a0a0a0a0a0a4a0a8a0a0a0a0a0a0a01";

    // A UI toggle: call_service then the light, in the user's context.
    let mut ui = ctx(1);
    ui.user_id = Some(user.into());
    let off = state("light.hallway_hallway", "on", json!({}), T0, T0, &ctx(9));
    intake.row(&changed(None, &off));
    let call = Event::new(
        EventKind::CallService {
            domain: "light".into(),
            service: "turn_off".into(),
            service_data: Arc::new(
                json!({"entity_id": ["light.hallway_hallway"]})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        },
        ts(T0 + 10),
        ui.clone(),
    );
    let row = event_row(intake.row(&call));
    assert_eq!(
        row.data.as_deref(),
        Some(r#"{"domain":"light","service":"turn_off","entity_id":["light.hallway_hallway"]}"#)
    );
    assert_eq!(row.aug, None);
    let toggled = state(
        "light.hallway_hallway",
        "off",
        json!({}),
        T0 + 11,
        T0 + 11,
        &ui,
    );
    let row = state_row(intake.row(&changed(Some(&off), &toggled)));
    assert_eq!(row.flags, VISIBLE);
    assert_eq!(
        row.aug.as_deref(),
        Some(
            r#"{"context_user_id":"0a0a0a0a0a0a4a0a8a0a0a0a0a0a0a01","context_domain":"light","context_service":"turn_off","context_event_type":"call_service"}"#
        )
    );

    // Motion, then the automation (a child context), then its light.
    let m = ctx(2);
    let idle = state(
        "binary_sensor.hall_motion",
        "off",
        json!({}),
        T0,
        T0,
        &ctx(9),
    );
    intake.row(&changed(None, &idle));
    let motion = state(
        "binary_sensor.hall_motion",
        "on",
        json!({}),
        T0 + 20,
        T0 + 20,
        &m,
    );
    let row = state_row(intake.row(&changed(Some(&idle), &motion)));
    assert_eq!(row.aug, None, "a root context adds nothing");
    let run = ctx(3).with_parent(&m);
    let triggered = Event::new(
        EventKind::Custom {
            name: "automation_triggered".into(),
            data: json!({"name": "Motion light", "entity_id": "automation.motion_light",
                         "source": "state of binary_sensor.hall_motion"}),
        },
        ts(T0 + 21),
        run.clone(),
    );
    let row = event_row(intake.row(&triggered));
    assert_eq!(
        aug(&row.aug),
        json!({"context_state": "on", "context_entity_id": "binary_sensor.hall_motion"})
    );
    let lit = state(
        "light.hallway_hallway",
        "on",
        json!({}),
        T0 + 23,
        T0 + 23,
        &run,
    );
    let row = state_row(intake.row(&changed(Some(&toggled), &lit)));
    assert_eq!(
        row.aug.as_deref(),
        Some(
            r#"{"context_event_type":"automation_triggered","context_domain":"automation","context_name":"Motion light","context_message":"triggered by state of binary_sensor.hall_motion","context_source":"state of binary_sensor.hall_motion","context_entity_id":"automation.motion_light"}"#
        )
    );
}

/// A child context with no user takes its parent's (a thermostat's switch
/// turned on by a user's change).
#[test]
fn user_ids_come_from_the_parent() {
    let mut intake = Intake::new(Hass::new());
    let mut parent = ctx(1);
    parent.user_id = Some("u1".into());
    let set = state("climate.t", "heat", json!({}), T0, T0, &parent);
    intake.row(&changed(None, &set));
    let child = ctx(2).with_parent(&parent);
    let off = state("switch.heater", "off", json!({}), T0, T0, &ctx(9));
    intake.row(&changed(None, &off));
    let on = state("switch.heater", "on", json!({}), T0 + 1, T0 + 1, &child);
    let row = state_row(intake.row(&changed(Some(&off), &on)));
    assert_eq!(
        row.aug.as_deref(),
        Some(r#"{"context_state":"heat","context_entity_id":"climate.t","context_user_id":"u1"}"#)
    );
}

/// Start and stop are rows (described), other events aren't recorded, and
/// the context map keeps the first row of a context and a bounded number.
#[test]
fn events_and_the_context_bound() {
    let mut intake = Intake::new(Hass::new());
    let start = Event::new(EventKind::HomeassistantStart {}, ts(T0), ctx(1));
    let row = event_row(intake.row(&start));
    assert_eq!(
        (row.event_type.as_str(), row.data.as_deref()),
        ("homeassistant_start", None)
    );
    let other = Event::new(EventKind::HomeassistantStarted {}, ts(T0), ctx(1));
    assert!(intake.row(&other).is_none());
    assert!(!recorded(&other));
    let custom = Event::new(
        EventKind::Custom {
            name: "my_event".into(),
            data: json!({}),
        },
        ts(T0),
        ctx(1),
    );
    assert!(!recorded(&custom));

    let mut contexts = Contexts::new(2);
    let first = Origin::State {
        entity_id: EntityId::parse("a.first").unwrap(),
        state: "1".into(),
    };
    contexts.see(&ctx(1), first);
    contexts.see(
        &ctx(1),
        Origin::State {
            entity_id: EntityId::parse("a.second").unwrap(),
            state: "2".into(),
        },
    );
    let child = ctx(5).with_parent(&ctx(1));
    assert_eq!(
        aug(&contexts.augment(&child)),
        json!({"context_state": "1", "context_entity_id": "a.first"})
    );
    contexts.see(&ctx(2), Origin::Fields(Arc::from("")));
    contexts.see(&ctx(3), Origin::Fields(Arc::from("")));
    assert!(contexts.augment(&child).is_none(), "the oldest went");
    assert_eq!(contexts.map.len(), 2);
}

/// A row the store drops (backlog full) changes nothing the next rows are
/// compared with.
#[test]
fn dropped_rows_leave_no_trace() {
    let mut intake = Intake::new(Hass::new());
    let c = ctx(1);
    let off = state("light.a", "off", json!({}), T0, T0, &c);
    assert!(intake.row(&changed(None, &off)).is_some());
    let on = state("light.a", "on", json!({}), T0 + 1, T0 + 1, &ctx(2));
    intake.record(&changed(Some(&off), &on), |_| false);
    // Had the dropped `on` counted, this repeat would be skipped (D2).
    let again = state("light.a", "on", json!({}), T0 + 2, T0 + 1, &ctx(3));
    let row = state_row(intake.row(&changed(Some(&on), &again)));
    assert_eq!(row.state, "on");
    // And the dropped row's context isn't anyone's origin.
    let child = ctx(4).with_parent(&ctx(2));
    assert!(intake.contexts.augment(&child).is_none());
}

/// A removed entity is forgotten, so ids that come and go don't pile up.
#[test]
fn removals_forget_the_entity() {
    let mut intake = Intake::new(Hass::new());
    let c = ctx(1);
    for i in 0..100 {
        let id = format!("sensor.phone_{i}");
        let s = state(
            &id,
            "1",
            json!({"blob": "y".repeat(MAX_ATTRS_BYTES)}),
            T0,
            T0,
            &c,
        );
        intake.row(&changed(None, &s));
        let gone = Event::new(
            EventKind::StateChanged {
                entity_id: s.entity_id.clone(),
                old_state: Some(s.clone()),
                new_state: None,
            },
            ts(T0 + 1),
            c.clone(),
        );
        // HA's row for a removal: an empty state, ending the entity's line.
        let row = state_row(intake.row(&gone));
        assert_eq!((&*row.state, &*row.attributes, row.ts), ("", "{}", T0 + 1));
        assert_eq!(row.flags, REMOVED);
        // One that was recorded before this process started has a line to
        // end too: the removal writes a row whatever the intake remembers.
        assert_eq!(state_row(intake.row(&gone)).state, "");
    }
    assert!(intake.last.is_empty());
    assert!(intake.oversized.is_empty());
}

/// An oversized attribute isn't encoded whole: the scratch buffer stays
/// near the 16 KiB limit.
#[test]
fn oversized_attributes_stop_encoding_at_the_limit() {
    let mut intake = Intake::new(Hass::new());
    let huge = state(
        "sensor.huge",
        "x",
        json!({"blob": "y".repeat(1 << 20)}),
        T0,
        T0,
        &ctx(1),
    );
    assert_eq!(
        &*state_row(intake.row(&changed(None, &huge))).attributes,
        "{}"
    );
    assert!(
        intake.buf.capacity() <= 2 * MAX_ATTRS_BYTES,
        "{}",
        intake.buf.capacity()
    );
    // Exactly at the limit still fits.
    let pad = MAX_ATTRS_BYTES - r#"{"b":""}"#.len();
    let edge = state(
        "sensor.edge",
        "x",
        json!({"b": "y".repeat(pad)}),
        T0,
        T0,
        &ctx(1),
    );
    assert_eq!(
        state_row(intake.row(&changed(None, &edge)))
            .attributes
            .len(),
        MAX_ATTRS_BYTES
    );
}
