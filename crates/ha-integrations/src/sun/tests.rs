use super::*;
use serde_json::Value;

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

/// HA 2026.6.1's `Sun.update_events` / `update_sun_position`, run with
/// astral 2.2 (the script is in the PR description), at 33 instants in
/// Amsterdam, Tromsø (polar day and night) and Sydney.
#[test]
fn matches_ha_with_astral() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("vectors.json")).unwrap();
    let mut checked = 0;
    let (mut exact, mut total) = (0, 0);
    for case in &cases {
        let o = Observer {
            latitude: case["lat"].as_f64().unwrap(),
            longitude: case["lon"].as_f64().unwrap(),
            elevation: case["elev"].as_f64().unwrap(),
        };
        let tz = TimeZone::get(case["tz"].as_str().unwrap()).unwrap();
        let now = ts(case["utc"].as_str().unwrap());
        let want = &case["expect"];
        let ev = events(&o, &tz, now);
        let label = format!("{} @ {}", case["tz"], case["utc"]);
        for (key, got) in [
            ("next_dawn", ev.next_dawn),
            ("next_dusk", ev.next_dusk),
            ("next_midnight", ev.next_midnight),
            ("next_noon", ev.next_noon),
            ("next_rising", ev.next_rising),
            ("next_setting", ev.next_setting),
            ("next_change", ev.next_change),
        ] {
            let expected = ts(want[key].as_str().unwrap());
            // Two libms can differ in the last bit; allow 1 ms.
            assert!(
                got.duration_since(expected).abs() <= SignedDuration::from_millis(1),
                "{label} {key}: got {got}, HA {expected}"
            );
            total += 1;
            // Microsecond-exact here.
            if got == expected {
                exact += 1;
                assert_eq!(
                    iso(got),
                    want[key].as_str().unwrap(),
                    "{label} {key} format"
                );
            }
        }
        assert_eq!(
            ev.rising,
            want["rising"].as_bool().unwrap(),
            "{label} rising"
        );
        assert_eq!(
            ev.phase.as_str(),
            want["phase"].as_str().unwrap(),
            "{label} phase"
        );
        let (elevation, azimuth) = position(&o, now);
        assert!(
            (elevation - want["elevation"].as_f64().unwrap()).abs() <= 0.01 + 1e-9,
            "{label} elevation {elevation}"
        );
        assert!(
            (azimuth - want["azimuth"].as_f64().unwrap()).abs() <= 0.01 + 1e-9,
            "{label} azimuth {azimuth}"
        );
        checked += 1;
    }
    assert_eq!(checked, 33);
    eprintln!("{exact}/{total} event times match HA to the microsecond");
}

#[test]
fn iso_like_python() {
    assert_eq!(iso(ts("2026-06-21T11:42:09Z")), "2026-06-21T11:42:09+00:00");
    assert_eq!(
        iso(ts("2026-06-21T11:42:09.000123Z")),
        "2026-06-21T11:42:09.000123+00:00"
    );
}

fn hass_at(start: &str) -> (Hass, ha_core::SimulatedClock) {
    let sim = ha_core::SimulatedClock::new(ts(start));
    let hass = Hass::with_clock(sim.clock());
    hass.set_config(ha_core::config::CoreConfig {
        latitude: 52.37,
        longitude: 4.89,
        elevation: 3,
        time_zone: "Europe/Amsterdam".into(),
        ..Default::default()
    });
    (hass, sim)
}

fn state(hass: &Hass, id: &str) -> Option<std::sync::Arc<ha_core::State>> {
    hass.states().get(&EntityId::parse(id).unwrap())
}

/// The running integration writes HA's states and refreshes the position
/// on HA's schedule, all on simulated time.
#[tokio::test(start_paused = true)]
async fn runs_on_has_schedule() {
    // 03:30:17 UTC on 21 June in Amsterdam: phase small_day (2-minute
    // position updates) until 04:42:40.926698.
    let (hass, _sim) = hass_at("2026-06-21T03:30:17Z");
    let task = tokio::spawn(run(
        hass.clone(),
        Default::default(),
        "entry1".into(),
        hass.setup_guard(),
    ));
    tokio::task::yield_now().await;

    let sun = state(&hass, "sun.sun").unwrap();
    assert_eq!(sun.state, "above_horizon");
    assert_eq!(sun.attributes["next_noon"], "2026-06-21T11:42:09+00:00");
    assert_eq!(
        sun.attributes["next_dawn"],
        "2026-06-22T02:27:14.306658+00:00"
    );
    assert_eq!(sun.attributes["rising"], true);
    assert_eq!(sun.attributes["friendly_name"], "Sun");
    let dawn = state(&hass, "sensor.sun_next_dawn").unwrap();
    // Timestamp sensors are to the second (`isoformat(timespec="seconds")`).
    assert_eq!(dawn.state, "2026-06-22T02:27:14+00:00");
    assert_eq!(dawn.attributes["device_class"], "timestamp");
    assert_eq!(dawn.attributes["friendly_name"], "Sun Next dawn");
    // Disabled by default in HA.
    assert!(state(&hass, "sensor.sun_solar_elevation").is_none());
    assert!(state(&hass, "binary_sensor.sun_solar_rising").is_none());

    // Two minutes later the position moved; the events didn't change.
    let first = sun.attributes["elevation"].as_f64().unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(121)).await;
    let later = state(&hass, "sun.sun").unwrap();
    assert!(
        later.attributes["elevation"].as_f64().unwrap() > first,
        "the sun is rising"
    );
    assert_eq!(
        later.last_updated,
        ts("2026-06-21T03:32:17Z"),
        "updated exactly on the 2-minute mark"
    );
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn the_registry_enables_renames_and_disables_sensors() {
    let (hass, _sim) = hass_at("2026-06-21T12:00:00Z");
    let entry =
        |key: &str, entity_id: &str, disabled: bool, name: Option<&str>| ha_registry::Entity {
            entity_id: entity_id.into(),
            platform: "sun".into(),
            unique_id: Some(json!(format!("e1-{key}"))),
            disabled_by: disabled.then(|| "integration".into()),
            name: name.map(Into::into),
            ..Default::default()
        };
    let registries = Registries {
        entities: ha_registry::rows![
            entry("solar_elevation", "sensor.sun_solar_elevation", false, None),
            entry("next_dusk", "sensor.evening", false, Some("Evening")),
            entry("next_dawn", "sensor.sun_next_dawn", true, None),
        ],
        ..Default::default()
    };
    let task = tokio::spawn(run(
        hass.clone(),
        std::sync::Arc::new(registries),
        "e1".into(),
        hass.setup_guard(),
    ));
    tokio::task::yield_now().await;
    let elevation = state(&hass, "sensor.sun_solar_elevation").unwrap();
    assert_eq!(elevation.attributes["unit_of_measurement"], "°");
    assert_eq!(
        elevation.state,
        py_float(
            state(&hass, "sun.sun").unwrap().attributes["elevation"]
                .as_f64()
                .unwrap()
        )
    );
    assert_eq!(
        state(&hass, "sensor.evening").unwrap().attributes["friendly_name"],
        "Evening",
        "a registry name is used as is"
    );
    assert!(state(&hass, "sensor.sun_next_dusk").is_none());
    assert!(
        state(&hass, "sensor.sun_next_dawn").is_none(),
        "disabled in the registry"
    );
    task.abort();
}

#[test]
fn states_print_like_python() {
    assert_eq!(py_float(60.0), "60.0");
    assert_eq!(py_float(-0.5), "-0.5");
    assert_eq!(py_float(181.27), "181.27");
    // Each from CPython's repr().
    for (x, want) in [
        (0.1, "0.1"),
        (1e-7, "1e-07"),
        (1.5e16, "1.5e+16"),
        (1e16, "1e+16"),
        (9999999999999998.0, "9999999999999998.0"),
        (0.0001, "0.0001"),
        (0.00012345, "0.00012345"),
        (-2.5e-5, "-2.5e-05"),
        (1e300, "1e+300"),
        (123456789.125, "123456789.125"),
        (-0.0, "-0.0"),
        (5e-324, "5e-324"),
    ] {
        assert_eq!(py_float(x), want, "{x:e}");
    }
    assert_eq!(
        iso_seconds(ts("2026-06-22T02:27:14.306658Z")),
        "2026-06-22T02:27:14+00:00"
    );
}

/// The wall clock stepped back an hour (an NTP correction) must not make
/// the next phase change run early: it waits for the wall-clock time.
#[tokio::test(start_paused = true)]
async fn a_clock_stepped_back_does_not_fire_early() {
    // Phase small_day until 04:42:40.926698; position refreshes stop 2.5
    // minutes before it, then one sleep until the phase change.
    let (hass, sim) = hass_at("2026-06-21T03:30:17Z");
    let task = tokio::spawn(run(
        hass.clone(),
        Default::default(),
        "entry1".into(),
        hass.setup_guard(),
    ));
    tokio::task::yield_now().await;
    // Into the final sleep.
    tokio::time::sleep(std::time::Duration::from_secs(70 * 60)).await;
    let before = state(&hass, "sun.sun").unwrap().last_updated;
    sim.jump_to(ts("2026-06-21T03:41:00Z"));
    // Past the old monotonic deadline, but an hour before 04:42 on the wall.
    tokio::time::sleep(std::time::Duration::from_secs(10 * 60)).await;
    assert_eq!(
        state(&hass, "sun.sun").unwrap().last_updated,
        before,
        "nothing ran early"
    );
    tokio::time::sleep(std::time::Duration::from_secs(52 * 60)).await;
    assert!(
        state(&hass, "sun.sun").unwrap().last_updated > before,
        "it ran at 04:42 on the wall clock"
    );
    task.abort();
}
