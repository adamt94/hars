use super::forecast::{Computed, Forecast, Num, py_sum, query, round, round_int};
use super::*;
use serde_json::json;

/// A scenario produced by running pyMetno 0.13.0 and HA 2026.6.1's
/// `met`/`weather` code on a fixture (see `vectors/`).
struct Vector {
    name: &'static str,
    v: Value,
    body: &'static [u8],
}

fn vectors() -> Vec<Vector> {
    let fixture = |name: &str| -> &'static [u8] {
        match name {
            "fixture_a.json" => include_bytes!("vectors/fixture_a.json"),
            "fixture_b.json" => include_bytes!("vectors/fixture_b.json"),
            _ => include_bytes!("vectors/fixture_c.json"),
        }
    };
    [
        ("london_metric", include_str!("vectors/london_metric.json")),
        (
            "london_us_dusk",
            include_str!("vectors/london_us_dusk.json"),
        ),
        ("oslo_tz_match", include_str!("vectors/oslo_tz_match.json")),
        ("oslo_tz_utc", include_str!("vectors/oslo_tz_utc.json")),
    ]
    .into_iter()
    .map(|(name, s)| {
        let v: Value = serde_json::from_str(s).unwrap();
        let body = fixture(v["fixture"].as_str().unwrap());
        Vector { name, v, body }
    })
    .collect()
}

fn config_for(v: &Value) -> CoreConfig {
    let loc = v["location"].as_array().unwrap();
    CoreConfig {
        latitude: loc[0].as_f64().unwrap(),
        longitude: loc[1].as_f64().unwrap(),
        elevation: loc[2].as_i64().unwrap(),
        time_zone: v["ha_time_zone"].as_str().unwrap().into(),
        unit_system_v2: if v["units"] == "us" {
            "us_customary".into()
        } else {
            "metric".into()
        },
        location_name: "Home".into(),
        ..CoreConfig::default()
    }
}

fn compute(v: &Value, body: &[u8]) -> Computed {
    let now: Timestamp = v["now_utc"].as_str().unwrap().parse().unwrap();
    let tz = TimeZone::get(v["ha_time_zone"].as_str().unwrap()).unwrap();
    let bucket = TimeZone::get(v["process_tz"].as_str().unwrap()).unwrap();
    Forecast::parse(body).unwrap().compute(now, &tz, &bucket)
}

#[test]
fn state_attributes_and_forecasts_match_ha() {
    for Vector { name, v, body } in vectors() {
        let config = config_for(&v);
        let now: Timestamp = v["now_utc"].as_str().unwrap().parse().unwrap();
        let computed = compute(&v, body);
        let units = Units::new(config.unit_system_v2 == "us_customary", None);
        let loc = Location::new(
            &json!({"track_home": true}).as_object().unwrap().clone(),
            &config,
        )
        .unwrap();

        let params = &v["request_params"];
        assert_eq!(
            query(loc.latitude, loc.longitude, loc.elevation),
            format!(
                "lat={}&lon={}&altitude={}",
                params["lat"].as_str().unwrap(),
                params["lon"].as_str().unwrap(),
                params["altitude"].as_str().unwrap()
            ),
            "{name}: query"
        );
        assert_eq!(sun_is_up(&config, now), v["sun_is_up"], "{name}: sun");
        assert_eq!(
            state(computed.current.as_ref(), true, || sun_is_up(&config, now)),
            v["state"],
            "{name}: state"
        );
        let attrs = weather::attributes(
            computed.current.as_ref(),
            &units,
            true,
            ATTRIBUTION,
            "Forecast Home",
            3,
        );
        assert_eq!(Value::Object(attrs), v["attributes"], "{name}: attributes");
        let list = |items: &[forecast::Item]| {
            Value::Array(
                items
                    .iter()
                    .map(|i| weather::forecast_item(i, &units))
                    .collect(),
            )
        };
        assert_eq!(list(&computed.daily), v["daily_forecast"], "{name}: daily");
        assert_eq!(
            list(&computed.hourly),
            v["hourly_forecast"],
            "{name}: hourly"
        );
    }
}

#[test]
fn days_are_split_in_the_process_zone_not_has() {
    // The same body, HA zone and time give different daily items when
    // pyMetno's process runs in UTC (as in HA's container) or in HA's zone.
    let vs = vectors();
    let get = |n: &str| vs.iter().find(|v| v.name == n).unwrap();
    let (matched, utc) = (get("oslo_tz_match"), get("oslo_tz_utc"));
    let a = compute(&matched.v, matched.body);
    let b = compute(&utc.v, utc.body);
    assert_ne!(a.daily, b.daily);
    assert_eq!((a.hourly.len(), b.hourly.len()), (40, 44));
}

#[test]
fn rounds_as_python() {
    for (x, n, want) in [
        (8.05, 1, 8.1),
        (9.85, 1, 9.8),
        (0.125, 2, 0.12),
        (0.375, 2, 0.38),
        (2.675, 2, 2.67),
    ] {
        assert_eq!(round(x, n), want, "round({x}, {n})");
    }
    let z = round(-0.04, 1);
    assert!(z == 0.0 && z.is_sign_negative(), "round(-0.04, 1) is -0.0");
    assert_eq!(serde_json::to_string(&z).unwrap(), "-0.0");
    for (x, want) in [(72.5, 72), (73.5, 74), (4.5, 4), (2.5, 2)] {
        assert_eq!(round_int(x), want, "round({x})");
    }
}

/// Hourly entries from `from` for `hours`, with `air_temperature` = index.
fn hourly_body(from: &str, hours: i64) -> Vec<u8> {
    let from: Timestamp = from.parse().unwrap();
    let series: Vec<Value> = (0..hours)
        .map(|i| {
            let t = from + SignedDuration::from_hours(i);
            json!({"time": t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string(), "data": {
                "instant": {"details": {"air_temperature": i}},
                "next_1_hours": {"summary": {"symbol_code": "cloudy"}, "details": {"precipitation_amount": 0.0}},
            }})
        })
        .collect();
    serde_json::to_vec(&json!({"properties": {"timeseries": series}})).unwrap()
}

use jiff::SignedDuration;

fn slots(c: &Computed) -> Vec<String> {
    c.hourly
        .iter()
        .take(4)
        .map(|i| crate::sun::iso(i.datetime))
        .collect()
}

#[test]
fn hourly_slots_add_wall_clock_hours_like_python() {
    let london = TimeZone::get("Europe/London").unwrap();
    // Spring forward: 01:00 local doesn't exist and resolves to 01:00Z,
    // so 01:00Z comes twice.
    let f = Forecast::parse(&hourly_body("2027-03-28T00:00:00Z", 8)).unwrap();
    let c = f.compute(
        "2027-03-27T23:10:00Z".parse().unwrap(),
        &london,
        &TimeZone::UTC,
    );
    assert_eq!(
        slots(&c),
        [
            "2027-03-28T00:00:00+00:00",
            "2027-03-28T01:00:00+00:00",
            "2027-03-28T01:00:00+00:00",
            "2027-03-28T02:00:00+00:00"
        ]
    );
    // Fall back: 01:00 local is ambiguous and takes the first (BST), so
    // 01:00Z is skipped.
    let f = Forecast::parse(&hourly_body("2026-10-25T00:00:00Z", 8)).unwrap();
    let c = f.compute(
        "2026-10-24T23:30:00Z".parse().unwrap(),
        &london,
        &TimeZone::UTC,
    );
    assert_eq!(
        slots(&c),
        [
            "2026-10-25T00:00:00+00:00",
            "2026-10-25T02:00:00+00:00",
            "2026-10-25T03:00:00+00:00",
            "2026-10-25T04:00:00+00:00"
        ]
    );
}

#[test]
fn lookups_follow_pymetno() {
    let body = json!({"properties": {"timeseries": [
        // An empty `next_1_hours` falls through to `next_6_hours`; no
        // symbol there means the next entry's, never `next_12_hours`'.
        {"time": "2026-11-14T10:00:00Z", "data": {
            "instant": {"details": {"air_temperature": 1.5, "relative_humidity": null}},
            "next_1_hours": {},
            "next_6_hours": {"details": {"precipitation_amount": 1}},
            "next_12_hours": {"summary": {"symbol_code": "fog"}}}},
        {"time": "2026-11-14T11:00:00Z", "data": {
            "instant": {"details": {"air_temperature": 2, "relative_humidity": 50.0, "wind_speed": 1}},
            "next_1_hours": {"summary": {"symbol_code": "rain"}, "details": {"precipitation_amount": 2}}}},
        // No `instant`: a lookup reaching it gives up entirely.
        {"time": "2026-11-14T12:00:00Z", "data": {}},
        {"time": "2026-11-14T13:00:00Z", "data": {
            "instant": {"details": {"air_pressure_at_sea_level": 1000.0}}}},
    ]}});
    let f = Forecast::parse(&serde_json::to_vec(&body).unwrap()).unwrap();
    let c = f.compute(
        "2026-11-14T09:00:00Z".parse().unwrap(),
        &TimeZone::UTC,
        &TimeZone::UTC,
    );
    let cur = c.current.unwrap();
    assert_eq!(cur.condition, Some("rainy"));
    assert_eq!(cur.temperature, Some(Num::Float(1.5)));
    // `null` stops the search with nothing.
    assert_eq!(cur.humidity, None);
    assert_eq!(cur.precipitation, Some(Num::Int(1)));
    // m/s to km/h: always a float.
    assert_eq!(cur.wind_speed, Some(Num::Float(3.6)));
    // Only after the 12:00 entry, which ends the lookup.
    assert_eq!(cur.pressure, None);
    // Today's daily item: ints summed stay an int, max keeps its kind.
    let today = &c.daily[0];
    assert_eq!(today.precipitation, Some(Num::Int(3)));
    assert_eq!(today.temperature, Some(Num::Int(2)));
    assert_eq!(today.templow, Some(Num::Float(1.5)));
}

#[test]
fn display_units_follow_the_registry_options() {
    let opts = json!({"weather": {"wind_speed_unit": "m/s", "temperature_unit": "K", "pressure_unit": "mmHg"}});
    let u = Units::new(false, opts.as_object());
    assert_eq!(
        (
            u.temperature,
            u.pressure,
            u.wind_speed,
            u.visibility,
            u.precipitation
        ),
        ("°C", "mmHg", "m/s", "km", "mm")
    );
    let item = forecast::Item {
        datetime: "2026-11-14T12:00:00Z".parse().unwrap(),
        condition: None,
        pressure: Some(Num::Float(1018.4)),
        humidity: None,
        wind_bearing: None,
        temperature: None,
        templow: None,
        precipitation: Some(Num::Float(3.45)),
        precipitation_probability: None,
        wind_speed: Some(Num::Float(16.6)),
        wind_gust: Some(Num::Float(35.6)),
        cloudiness: None,
        dew_point: None,
        uv_index: None,
    };
    let attrs = |u: &Units| weather::attributes(Some(&item), u, true, "a", "n", 3);
    let a = attrs(&u);
    assert_eq!(
        (a["wind_speed"].clone(), a["pressure"].clone()),
        (json!(4.61), json!(763.86))
    );
    let knots = Units::new(
        false,
        json!({"weather": {"wind_speed_unit": "kn"}}).as_object(),
    );
    assert_eq!(attrs(&knots)["wind_speed"], json!(8.96));
    let fts = Units::new(
        false,
        json!({"weather": {"wind_speed_unit": "ft/s"}}).as_object(),
    );
    assert_eq!(attrs(&fts)["wind_speed"], json!(15.13));
    let bft = Units::new(
        false,
        json!({"weather": {"wind_speed_unit": "Beaufort"}}).as_object(),
    );
    let a = attrs(&bft);
    assert_eq!(
        (a["wind_speed"].clone(), a["wind_gust_speed"].clone()),
        (json!(3.0), json!(5.0))
    );
    let mbar = Units::new(
        true,
        json!({"weather": {"pressure_unit": "mbar"}}).as_object(),
    );
    let a = attrs(&mbar);
    assert_eq!(
        (a["pressure"].clone(), a["wind_speed"].clone()),
        (json!(1018.4), json!(10.31))
    );
    // A forecast's precipitation in inches; no temperature is `null`.
    let f = weather::forecast_item(&item, &Units::new(true, None));
    assert_eq!(
        (f["precipitation"].clone(), f["temperature"].clone()),
        (json!(0.14), Value::Null)
    );
    // Unavailable: only the tail.
    assert_eq!(
        Value::Object(weather::attributes(Some(&item), &u, false, "a", "n", 3)),
        json!({"attribution": "a", "friendly_name": "n", "supported_features": 3})
    );
}

#[test]
fn locations_and_unique_ids() {
    let config = CoreConfig {
        latitude: 51.5,
        longitude: -0.12,
        elevation: 11,
        location_name: "Home".into(),
        ..CoreConfig::default()
    };
    let data = |v: Value| v.as_object().unwrap().clone();
    let home = Location::new(&data(json!({"track_home": true})), &config).unwrap();
    assert_eq!(
        (home.unique_id.as_str(), home.name.as_str(), home.elevation),
        ("home", "Home", 11.0)
    );
    // No home set (HA's onboarding default): not set up.
    let unset = CoreConfig {
        latitude: 52.3731339,
        longitude: 4.8903147,
        ..config.clone()
    };
    assert_eq!(
        Location::new(&data(json!({"track_home": true})), &unset),
        None
    );
    let fixed = Location::new(
        &data(
            json!({"name": "Cabin", "latitude": 51.5072, "longitude": -0.1276, "elevation": 3.0}),
        ),
        &config,
    )
    .unwrap();
    assert_eq!(fixed.unique_id, "51.5072--0.1276");
    assert_eq!(fixed.name, "Cabin");
    let whole = Location::new(&data(json!({"latitude": 60.0, "longitude": 10})), &config).unwrap();
    assert_eq!(
        (whole.unique_id.as_str(), whole.name.as_str()),
        ("60.0-10", "Met.no")
    );

    // The entity: from the registry if it's there, else HA's default id.
    let r = Registries::default();
    let Entity { id, name, .. } = entity(&r, &fixed).unwrap();
    assert_eq!(
        (id.as_str(), name.as_str()),
        ("weather.forecast_cabin", "Forecast Cabin")
    );
}

fn fixture_a() -> Fetched {
    Fetched {
        forecast: Forecast::parse(include_bytes!("vectors/fixture_a.json")).unwrap(),
        last_modified: None,
    }
}

#[tokio::test(start_paused = true)]
async fn refreshes_on_has_schedule_and_goes_unavailable_on_failure() {
    // The fixture's hour: the forecast's days are counted from "now", so
    // the wall clock would change them from day to day.
    let sim = ha_core::SimulatedClock::new("2026-11-14T10:00:00Z".parse().unwrap());
    let hass = Hass::with_clock(sim.clock());
    // Running: a failed setup retries on the backoff (while starting it
    // would wait for `homeassistant_started`).
    hass.start();
    let weather = Arc::new(Weather::default());
    let entity = Entity {
        id: EntityId::parse("weather.forecast_home").unwrap(),
        name: "Forecast Home".into(),
        options: None,
    };
    let start = tokio::time::Instant::now();
    // Each call: when (seconds since start) and what it was handed.
    let calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let log = calls.clone();
    let mut n = 0;
    let fetch = move |prev: Option<Fetched>| {
        n += 1;
        log.lock()
            .push((start.elapsed().as_secs_f64(), prev.is_some()));
        let res = match n {
            // Setup fails twice, then works.
            1 | 2 => Err((FetchError::Status(500), prev)),
            3 => Ok(fixture_a()),
            // A failure after setup, then two more.
            4..=6 => Err((FetchError::Status(503), prev)),
            _ => Ok(prev.unwrap()),
        };
        std::future::ready(res)
    };
    let (h, w) = (hass.clone(), weather.clone());
    let task =
        tokio::spawn(async move { schedule(&h, &w, &entity, true, h.setup_guard(), fetch).await });
    let id = EntityId::parse("weather.forecast_home").unwrap();

    tokio::time::sleep(Duration::from_secs(20)).await;
    // Setup retried after 5 s and 10 s more, each plus 0.05-0.5 s of
    // jitter (tokio rounds timers up to the next millisecond).
    {
        let c = calls.lock();
        assert_eq!(c.len(), 3);
        assert!((5.05..=5.501).contains(&c[1].0), "{c:?}");
        assert!((15.1..=16.002).contains(&c[2].0), "{c:?}");
    }
    assert_eq!(hass.states().get(&id).unwrap().state, "partlycloudy");
    let mut sub = weather
        .subscribe("weather.forecast_home", weather::Kind::Daily)
        .unwrap();
    assert_eq!(sub.borrow_and_update().daily.as_array().unwrap().len(), 3);
    assert_eq!(
        weather
            .subscribe("weather.forecast_home", weather::Kind::TwiceDaily)
            .unwrap_err(),
        weather::SubscribeError::NotSupported
    );
    assert_eq!(
        weather
            .subscribe("weather.nope", weather::Kind::Daily)
            .unwrap_err(),
        weather::SubscribeError::NotFound
    );

    // The next refresh is 55-64 minutes (plus jitter) later, and fails.
    tokio::time::sleep(Duration::from_secs(65 * 60)).await;
    let gap = {
        let c = calls.lock();
        assert_eq!(c.len(), 4, "{c:?}");
        assert!(c[3].1, "handed the last good fetch");
        c[3].0 - c[2].0
    };
    assert!((55.0 * 60.0..65.0 * 60.0 + 0.6).contains(&gap), "{gap}");
    let s = hass.states().get(&id).unwrap();
    assert_eq!(s.state, "unavailable");
    assert_eq!(s.attributes.len(), 3);
    // `weather.get_forecasts` skips it while it's unavailable.
    let all = ha_core::EntityTarget::All;
    assert_eq!(
        weather
            .get_forecasts(&all, &[], weather::Kind::Daily)
            .unwrap_err(),
        ha_core::ServiceError::Failed(
            "Service call requested response data but did not match any entities".into()
        )
    );
    // Subscribers got the old forecast once, at the failure.
    assert!(sub.has_changed().unwrap());
    assert_eq!(sub.borrow_and_update().daily.as_array().unwrap().len(), 3);

    // Two more failures, a gap apart, change nothing...
    let t3 = calls.lock()[2].0;
    let wait = t3 + 3.0 * gap + 1.0 - start.elapsed().as_secs_f64();
    tokio::time::sleep(Duration::from_secs_f64(wait)).await;
    assert_eq!(calls.lock().len(), 6);
    assert!(!sub.has_changed().unwrap());
    assert_eq!(hass.states().get(&id).unwrap().state, "unavailable");
    // ...then it recovers on its own and tells subscribers.
    tokio::time::sleep(Duration::from_secs_f64(gap)).await;
    assert_eq!(calls.lock().len(), 7);
    assert!(sub.has_changed().unwrap());
    assert_eq!(hass.states().get(&id).unwrap().state, "partlycloudy");
    assert!(
        weather
            .get_forecasts(&all, &[], weather::Kind::Daily)
            .is_ok()
    );
    task.abort();
}

#[tokio::test]
async fn fetch_sends_if_modified_since_and_keeps_the_forecast_on_304() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = include_bytes!("vectors/fixture_a.json");
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for status in [
            "200 OK",
            "304 Not Modified",
            "500 Internal Server Error",
            "200 OK",
        ] {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0; 4096];
            let n = s.read(&mut buf).await.unwrap();
            requests.push(String::from_utf8_lossy(&buf[..n]).to_lowercase());
            let payload: &[u8] = match status {
                "200 OK" if requests.len() == 1 => body,
                "200 OK" => b"not json",
                _ => b"",
            };
            let head = format!(
                "HTTP/1.1 {status}\r\ncontent-length: {}\r\nlast-modified: Sat, 14 Nov 2026 10:00:00 GMT\r\nconnection: close\r\n\r\n",
                payload.len()
            );
            s.write_all(head.as_bytes()).await.unwrap();
            s.write_all(payload).await.unwrap();
        }
        requests
    });
    let client = ha_http::Client::new("hars-test").unwrap();
    let url = format!("http://{addr}/complete?lat=1.0&lon=2.0&altitude=3");
    let first = fetch(&client, &url, None)
        .await
        .unwrap_or_else(|_| panic!("200"));
    assert_eq!(
        first.last_modified.as_deref(),
        Some("Sat, 14 Nov 2026 10:00:00 GMT")
    );
    // 304: the same forecast back.
    let kept = fetch(&client, &url, Some(first))
        .await
        .unwrap_or_else(|_| panic!("304"));
    assert!(kept.last_modified.is_some());
    // 500, then a body that isn't JSON: failures that hand the forecast back.
    let (e, back) = fetch(&client, &url, Some(kept)).await.err().unwrap();
    assert!(matches!(e, FetchError::Status(500)), "{e}");
    let (e, back) = fetch(&client, &url, back).await.err().unwrap();
    assert!(matches!(e, FetchError::Parse(_)), "{e}");
    assert!(back.is_some());

    let requests = server.await.unwrap();
    assert!(requests[0].starts_with("get /complete?lat=1.0&lon=2.0&altitude=3 http/1.1"));
    assert!(!requests[0].contains("if-modified-since"));
    assert!(requests[1].contains("if-modified-since: sat, 14 nov 2026 10:00:00 gmt"));
    assert!(requests[1].contains("user-agent: hars-test"));
}

#[test]
fn sums_like_python() {
    use Num::{Float as F, Int as I};
    // Each checked against CPython 3.14's `sum()`.
    for (xs, want) in [
        (vec![F(0.52), F(0.91), F(0.62)], F(2.05)),
        (vec![F(0.0), F(0.0), F(0.3), F(0.9), F(2.25)], F(3.45)),
        (
            vec![F(0.1), I(1), F(0.2), F(1e16), F(0.3), F(-1e16)],
            F(1.6),
        ),
        (vec![F(1e16), F(1.0), F(-1e16)], F(1.0)),
        // The first float joins the int prefix plainly.
        (vec![I(10_000_000_000_000_000), F(1.0), F(-1e16)], F(0.0)),
        (vec![I(3), F(1e16), F(1.0), F(-1e16)], F(5.0)),
        (vec![I(1), I(2)], I(3)),
        // Python's ints don't overflow.
        (vec![I(i64::MAX), I(2), I(-i64::MAX), F(0.0)], F(2.0)),
        (vec![I(i64::MAX), I(1), I(-i64::MAX)], I(1)),
        // After an overflow CPython adds plainly, with no compensation.
        (
            vec![I(i64::MAX), I(2), I(-i64::MAX), F(1e16), F(1.0), F(-1e16)],
            F(4.0),
        ),
        (
            vec![I(i64::MAX), I(1), I(-i64::MAX), F(0.52), F(0.91), F(0.62)],
            F(3.0500000000000003),
        ),
    ] {
        assert_eq!(py_sum(&xs), want, "sum({xs:?})");
    }
    // pyMetno rounds that first sum to 2.0; a plain left fold gives 2.1.
    assert_eq!(round(0.52 + 0.91 + 0.62, 1), 2.1);
    let Num::Float(s) = py_sum(&[F(0.52), F(0.91), F(0.62)]) else {
        unreachable!()
    };
    assert_eq!(round(s, 1), 2.0);
}

#[tokio::test(start_paused = true)]
async fn polling_disabled_fetches_only_at_setup() {
    let hass = Hass::new();
    let weather = Arc::new(Weather::default());
    let entity = Entity {
        id: EntityId::parse("weather.forecast_home").unwrap(),
        name: "Forecast Home".into(),
        options: None,
    };
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let n = calls.clone();
    let fetch = move |_| {
        n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::future::ready(Ok(fixture_a()))
    };
    let (h, w) = (hass.clone(), weather.clone());
    let task =
        tokio::spawn(async move { schedule(&h, &w, &entity, false, h.setup_guard(), fetch).await });
    tokio::time::sleep(Duration::from_secs(5 * 3600)).await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let id = EntityId::parse("weather.forecast_home").unwrap();
    assert_eq!(hass.states().get(&id).unwrap().state, "partlycloudy");
    task.abort();
}
