//! Against vectors from HA 2026.6.1's own `util/color.py` and
//! `components/light` code (see `vectors/`).

use super::*;
use serde_json::json;

fn vectors(name: &str) -> Value {
    let text = match name {
        "ct_mode_by_mirek" => include_str!("vectors/ct_mode_by_mirek.json"),
        "ct_mode_by_kelvin" => include_str!("vectors/ct_mode_by_kelvin.json"),
        "xy_mode_samples" => include_str!("vectors/xy_mode_samples.json"),
        "to_xy_samples" => include_str!("vectors/to_xy_samples.json"),
        "brightness_pct" => include_str!("vectors/brightness_pct.json"),
        _ => include_str!("vectors/service_calls.json"),
    };
    serde_json::from_str(text).unwrap()
}

fn ct_light() -> Capabilities {
    Capabilities {
        modes: Modes::filtered([
            ColorMode::Onoff,
            ColorMode::ColorTemp,
            ColorMode::Xy,
            ColorMode::Brightness,
        ]),
        features: FEATURE_EFFECT | FEATURE_FLASH | FEATURE_TRANSITION,
        effect_list: Some(vec!["off".into(), "candle".into()]),
    }
}

/// The hs/rgb/xy attributes for a light in `mode`.
fn colors(mode: ColorMode, s: &LightState) -> Value {
    let mut m = Map::new();
    convert_color(mode, s, &mut m);
    Value::Object(m)
}

#[test]
fn color_temperature_mode_matches_ha() {
    for v in vectors("ct_mode_by_mirek").as_array().unwrap() {
        let mirek = v["mirek"].as_f64().unwrap();
        let k = color::mired_to_kelvin(mirek).unwrap();
        assert_eq!(k, v["color_temp_kelvin"], "{v}");
        let s = LightState {
            color_temp_kelvin: Some(k),
            ..Default::default()
        };
        let want = json!({"hs_color": v["hs_color"], "rgb_color": v["rgb_color"], "xy_color": v["xy_color"]});
        assert_eq!(colors(ColorMode::ColorTemp, &s), want, "mirek {mirek}");
    }
    for v in vectors("ct_mode_by_kelvin").as_array().unwrap() {
        let s = LightState {
            color_temp_kelvin: v[0].as_i64(),
            ..Default::default()
        };
        let want = json!({"hs_color": v[1], "rgb_color": v[2], "xy_color": v[3]});
        assert_eq!(colors(ColorMode::ColorTemp, &s), want, "kelvin {}", v[0]);
    }
}

#[test]
fn xy_mode_matches_ha() {
    for v in vectors("xy_mode_samples").as_array().unwrap() {
        let xy = (v["xy"][0].as_f64().unwrap(), v["xy"][1].as_f64().unwrap());
        let s = LightState {
            xy_color: Some(xy),
            ..Default::default()
        };
        let want = json!({"hs_color": v["hs_color"], "rgb_color": v["rgb_color"], "xy_color": v["xy_color"]});
        assert_eq!(colors(ColorMode::Xy, &s), want, "xy {xy:?}");
    }
}

#[test]
fn turn_on_colours_convert_as_ha() {
    let v = vectors("to_xy_samples");
    let xy = |a: &Value| json!(a);
    for c in v["rgb"].as_array().unwrap() {
        let rgb: Vec<f64> = c["rgb"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_f64().unwrap())
            .collect();
        let got = color::rgb_to_xy(rgb[0], rgb[1], rgb[2]);
        assert_eq!(json!([got.0, got.1]), xy(&c["xy_for_xy_light"]), "{c}");
        assert_eq!(
            json!(color::xy_to_temperature(got.0, got.1).unwrap()),
            c["kelvin_for_ct_light"],
            "{c}"
        );
    }
    for c in v["hs"].as_array().unwrap() {
        let (h, s) = (c["hs"][0].as_f64().unwrap(), c["hs"][1].as_f64().unwrap());
        let got = color::hs_to_xy(h, s);
        assert_eq!(json!([got.0, got.1]), xy(&c["xy_for_xy_light"]), "{c}");
        assert_eq!(
            json!(color::xy_to_temperature(got.0, got.1).unwrap()),
            c["kelvin_for_ct_light"],
            "{c}"
        );
    }
    for c in v["color_name"].as_array().unwrap() {
        // An unknown name (`warm white`) falls back to white, as in HA.
        let mut p = Params {
            color_name: c["name"].as_str().map(Into::into),
            ..Default::default()
        };
        preprocess(&mut p, &Profiles::default());
        assert_eq!(json!(p.rgb_color), c["rgb"], "{c}");
    }
    assert_eq!(
        color::color_name_to_rgb("Dodger Blue"),
        Some([30, 144, 255])
    );
    assert_eq!(color::color_name_to_rgb("not-a-colour"), None);
    assert_eq!(color::color_name_to_rgb("warm white"), None);
    // Python raises ZeroDivisionError here; hars reports an error.
    assert_eq!(
        color::xy_to_temperature(0.3, 0.1858),
        Err(ConversionError::DivisionByZero)
    );
}

#[test]
fn brightness_pct_rounds_as_python() {
    let profiles = Profiles::default();
    for v in vectors("brightness_pct").as_array().unwrap() {
        let mut p = Params {
            brightness_pct: v[0].as_f64(),
            ..Default::default()
        };
        preprocess(&mut p, &profiles);
        assert_eq!(json!(p.brightness), v[1], "{v}");
    }
}

#[test]
fn attributes_follow_mode_and_availability() {
    let caps = ct_light();
    let on = LightState {
        on: true,
        color_mode: Some(ColorMode::ColorTemp),
        brightness: Some(89),
        color_temp_kelvin: Some(2702),
        min_color_temp_kelvin: 2000,
        max_color_temp_kelvin: 6535,
        effect: Some("off".into()),
        ..Default::default()
    };
    assert_eq!(
        Value::Object(attributes(&caps, &on, true)),
        json!({
            "min_color_temp_kelvin": 2000, "max_color_temp_kelvin": 6535,
            "effect_list": ["off", "candle"], "supported_color_modes": ["color_temp", "xy"],
            "effect": "off", "color_mode": "color_temp", "brightness": 89, "color_temp_kelvin": 2702,
            "hs_color": [28.391, 65.659], "rgb_color": [255, 167, 88], "xy_color": [0.524, 0.388],
        })
    );
    let off = LightState {
        on: false,
        ..on.clone()
    };
    assert_eq!(
        Value::Object(attributes(&caps, &off, true)),
        json!({
            "min_color_temp_kelvin": 2000, "max_color_temp_kelvin": 6535,
            "effect_list": ["off", "candle"], "supported_color_modes": ["color_temp", "xy"],
            "effect": null, "color_mode": null, "brightness": null, "color_temp_kelvin": null,
            "hs_color": null, "rgb_color": null, "xy_color": null,
        })
    );
    assert_eq!(
        Value::Object(attributes(&caps, &on, false)),
        json!({
            "min_color_temp_kelvin": 2000, "max_color_temp_kelvin": 6535,
            "effect_list": ["off", "candle"], "supported_color_modes": ["color_temp", "xy"],
        })
    );
    // A plain on/off light: no brightness or colour keys at all.
    let plug = Capabilities {
        modes: Modes::filtered([ColorMode::Onoff]),
        ..Default::default()
    };
    let s = LightState {
        on: true,
        color_mode: Some(ColorMode::Onoff),
        ..Default::default()
    };
    assert_eq!(
        Value::Object(attributes(&plug, &s, true)),
        json!({"supported_color_modes": ["onoff"], "color_mode": "onoff"})
    );
}

/// The kwargs HA hands the entity, as JSON.
fn kwargs(p: &Params) -> Value {
    let mut m = Map::new();
    if let Some(v) = p.transition {
        m.insert("transition".into(), json!(v));
    }
    if let Some(v) = p.brightness {
        m.insert("brightness".into(), json!(v));
    }
    if let Some(v) = p.color_temp_kelvin {
        m.insert("color_temp_kelvin".into(), json!(v));
    }
    if let Some(v) = p.hs_color {
        m.insert("hs_color".into(), json!([v.0, v.1]));
    }
    if let Some(v) = p.xy_color {
        m.insert("xy_color".into(), json!([v.0, v.1]));
    }
    if let Some(v) = p.rgb_color {
        m.insert("rgb_color".into(), json!(v));
    }
    if let Some(f) = p.flash {
        m.insert(
            "flash".into(),
            json!(if f == Flash::Short { "short" } else { "long" }),
        );
    }
    if let Some(e) = &p.effect {
        m.insert("effect".into(), json!(e));
    }
    Value::Object(m)
}

#[test]
fn service_parameters_reach_the_light_as_in_ha() {
    let profiles = Profiles::default();
    let filament = Capabilities {
        modes: Modes::filtered([
            ColorMode::Onoff,
            ColorMode::ColorTemp,
            ColorMode::Brightness,
        ]),
        features: FEATURE_EFFECT | FEATURE_FLASH | FEATURE_TRANSITION,
        effect_list: None,
    };
    let group = Capabilities {
        modes: Modes::new([ColorMode::ColorTemp]),
        features: FEATURE_FLASH | FEATURE_TRANSITION,
        effect_list: None,
    };
    let mut checked = 0;
    for v in vectors("service_calls").as_array().unwrap() {
        let id = v["entity_id"].as_str().unwrap();
        let caps = match id {
            "light.hallway_hallway" => &group,
            "light.bedroom_lamp" => &ct_light(),
            _ => &filament,
        };
        let (lo, hi) = if id == "light.bedroom_lamp" {
            (2000, 6535)
        } else {
            (2202, 4504)
        };
        let state = LightState {
            on: v["light_is_on_before"].as_bool().unwrap(),
            brightness: Some(89),
            min_color_temp_kelvin: lo,
            max_color_temp_kelvin: hi,
            ..Default::default()
        };
        let service = v["service"].as_str().unwrap();
        let schema = if service == "light.turn_off" {
            turn_off_schema()
        } else {
            turn_on_schema()
        };
        let mut data = v["data"].as_object().unwrap().clone();
        data.insert("entity_id".into(), json!(id));
        let data = schema.validate(&data).unwrap().unwrap_or(data);
        let mut params = Params::from_data(&data);
        preprocess(&mut params, &profiles);
        match service {
            "light.turn_off" => {
                let off = turn_off_params(&params, id, caps, &profiles);
                let mut m = Map::new();
                if let Some(t) = off.transition {
                    m.insert("transition".into(), json!(t));
                }
                assert_eq!(Value::Object(m), v["entity_kwargs"], "{v}");
            }
            "light.turn_on" => {
                let p = process_turn_on(&params, id, caps, &state, &profiles).unwrap();
                if v["routed_to"] == "turn_off" {
                    assert!(is_off(&p), "{v}");
                } else {
                    assert!(!is_off(&p), "{v}");
                    assert_eq!(kwargs(&filter_turn_on(p, caps)), v["entity_kwargs"], "{v}");
                }
            }
            _ => continue,
        }
        checked += 1;
    }
    assert_eq!(checked, 48);
}

#[test]
fn profiles_and_defaults() {
    let mut profiles = Profiles::default();
    // A user file: a default for one light, then a bad row that ends it.
    profiles.read(
        "test",
        "id,x,y,brightness,transition\nlight.desk.default,,,50,3\nbad,2,0,1\nlater,0.1,0.1,1\n",
    );
    let caps = ct_light();
    let off = LightState::default();
    // Off: the default profile applies in full.
    let p = process_turn_on(&Params::default(), "light.desk", &caps, &off, &profiles).unwrap();
    assert_eq!((p.brightness, p.transition), (Some(50), Some(3.0)));
    // On with data: only its transition, and only if none was given.
    let on = LightState {
        on: true,
        ..Default::default()
    };
    let given = Params {
        brightness: Some(10),
        ..Default::default()
    };
    let p = process_turn_on(&given, "light.desk", &caps, &on, &profiles).unwrap();
    assert_eq!((p.brightness, p.transition), (Some(10), Some(3.0)));
    // The rows after a bad one aren't read.
    let mut p = Params {
        profile: Some("later".into()),
        ..Default::default()
    };
    preprocess(&mut p, &profiles);
    assert_eq!(p.brightness, None);
    // turn_off takes the default's transition.
    let off = turn_off_params(&Params::default(), "light.desk", &caps, &profiles);
    assert_eq!(off.transition, Some(3.0));
}

/// A light that records what reaches it.
struct Fake {
    id: EntityId,
    caps: Capabilities,
    state: parking_lot::Mutex<LightState>,
    available: bool,
    calls: Arc<parking_lot::Mutex<Vec<String>>>,
}

impl Light for Fake {
    fn entity_id(&self) -> &EntityId {
        &self.id
    }
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }
    fn state(&self) -> LightState {
        self.state.lock().clone()
    }
    fn available(&self) -> bool {
        self.available
    }
    fn turn_on(&self, params: Params) -> BoxFuture<'_> {
        self.calls
            .lock()
            .push(format!("{} on {}", self.id, kwargs(&params)));
        self.state.lock().on = true;
        Box::pin(async { Ok(()) })
    }
    fn turn_off(&self, params: TurnOff) -> BoxFuture<'_> {
        self.calls
            .lock()
            .push(format!("{} off {:?}", self.id, params.transition));
        self.state.lock().on = false;
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn services_reach_the_targeted_available_lights() {
    let hass = Hass::new();
    let calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let lights = Arc::new(Lights::new(Profiles::default()));
    for (id, available) in [("light.a", true), ("light.b", true), ("light.gone", false)] {
        lights.add(Arc::new(Fake {
            id: EntityId::parse(id).unwrap(),
            caps: ct_light(),
            state: Default::default(),
            available,
            calls: calls.clone(),
        }));
    }
    let registries = Registries {
        entities: vec![ha_registry::Entity {
            entity_id: "light.b".into(),
            platform: "hue".into(),
            device_id: Some("dev".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    lights.register_services(&hass, Arc::new(registries));
    let call = |service: &str, data: Value| {
        let hass = hass.clone();
        let service = service.to_owned();
        async move {
            let Value::Object(data) = data else { panic!() };
            hass.call_service("light", &service, data, None, false, None)
                .await
        }
    };
    let take = || std::mem::take(&mut *calls.lock());

    call(
        "turn_on",
        json!({"entity_id": "light.a", "brightness_pct": 50, "color_name": "red"}),
    )
    .await
    .unwrap();
    assert_eq!(
        take(),
        [r#"light.a on {"brightness":128,"xy_color":[0.701,0.299]}"#]
    );
    // A device target; brightness 0 is a turn_off.
    call("turn_on", json!({"device_id": "dev", "brightness": 0}))
        .await
        .unwrap();
    assert_eq!(take(), ["light.b off None"]);
    // `all` skips the unavailable light.
    call("turn_off", json!({"entity_id": "all", "transition": 2}))
        .await
        .unwrap();
    let mut got = take();
    got.sort();
    assert_eq!(got, ["light.a off Some(2.0)", "light.b off Some(2.0)"]);
    // Toggle: off turns on with the data; on turns off without it.
    call("toggle", json!({"entity_id": "light.a", "brightness": 10}))
        .await
        .unwrap();
    call("toggle", json!({"entity_id": "light.a", "brightness": 10}))
        .await
        .unwrap();
    assert_eq!(
        take(),
        [r#"light.a on {"brightness":10}"#, "light.a off None"]
    );
    // Bad data never reaches a light.
    let e = call(
        "turn_on",
        json!({"entity_id": "light.a", "brightness": 1, "brightness_pct": 2}),
    )
    .await;
    assert!(matches!(e, Err(ServiceError::Invalid(_))), "{e:?}");
    assert!(take().is_empty());
}

/// Where HA raises during conversion, the call fails with its message and
/// nothing reaches the light.
#[test]
fn conversions_that_raise_in_ha_fail_the_call() {
    let profiles = Profiles::default();
    let rgbww = Capabilities {
        modes: Modes::new([ColorMode::Rgbww]),
        ..Default::default()
    };
    let s = LightState {
        min_color_temp_kelvin: 2000,
        max_color_temp_kelvin: 6500,
        ..Default::default()
    };
    let p = Params {
        color_temp_kelvin: Some(0),
        brightness: Some(128),
        ..Default::default()
    };
    assert_eq!(
        process_turn_on(&p, "light.x", &rgbww, &s, &profiles),
        Err(ConversionError::DivisionByZero)
    );
    // A subnormal y: `int(NaN)` in HA.
    let rgb = Capabilities {
        modes: Modes::new([ColorMode::Rgb]),
        ..Default::default()
    };
    let p = Params {
        xy_color: Some((0.3, 5e-324)),
        ..Default::default()
    };
    let e = process_turn_on(&p, "light.x", &rgb, &s, &profiles).unwrap_err();
    assert_eq!(e.to_string(), "cannot convert float NaN to integer");
    // And the state keeps such colours null rather than inventing one.
    let on = LightState {
        on: true,
        color_mode: Some(ColorMode::Xy),
        xy_color: Some((0.3, 5e-324)),
        ..Default::default()
    };
    let caps = Capabilities {
        modes: Modes::new([ColorMode::Xy]),
        ..Default::default()
    };
    let a = attributes(&caps, &on, true);
    assert_eq!(
        (a["hs_color"].clone(), a["rgb_color"].clone()),
        (Value::Null, Value::Null)
    );
}

#[test]
fn quoted_profile_fields_load_as_in_python_csv() {
    let mut profiles = Profiles::default();
    profiles.read(
        "test",
        "id,x,y,brightness\nlight.desk.default,\"0.5\",\"0.4\",\"50\"\n\"quo\"\"ted\",0.5,0.4,60\n",
    );
    let mut p = Params::default();
    profiles.apply_default("light.desk", false, &mut p);
    assert_eq!(
        (p.brightness, p.hs_color),
        (Some(50), Some((32.484, 61.569)))
    );
    let mut p = Params {
        profile: Some("quo\"ted".into()),
        ..Default::default()
    };
    preprocess(&mut p, &profiles);
    assert_eq!(p.brightness, Some(60));
}
