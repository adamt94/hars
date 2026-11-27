//! Replays Home Assistant 2026.6.1 vectors for `climate` (all 10 services) and
//! `notify` (`scripts/vectors/climate_notify.py`).

use ha_core::{Context, EntityId, Hass, ServiceError};
use ha_integrations::climate::{Climate, Climates, SetTemperature, Temp, feature as clim_feat};
use ha_integrations::light::BoxFuture;
use ha_integrations::notify::{Notifiers, Notify, feature as notify_feat};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::sync::Arc;

struct FakeClimate {
    id: EntityId,
    features: u32,
    hvac_modes: Vec<String>,
    hvac_mode: Option<String>,
    fan_modes: Vec<String>,
    preset_modes: Vec<String>,
    swing_modes: Vec<String>,
    swing_horizontal_modes: Vec<String>,
    min_temp: Temp,
    max_temp: Temp,
    min_humidity: Temp,
    max_humidity: Temp,
    calls: Mutex<Vec<Value>>,
}

impl FakeClimate {
    fn push(&self, v: Value) -> BoxFuture<'_> {
        self.calls.lock().push(v);
        Box::pin(async { Ok(()) })
    }
}

impl Climate for FakeClimate {
    fn entity_id(&self) -> &EntityId {
        &self.id
    }
    fn available(&self) -> bool {
        true
    }
    fn supported_features(&self) -> u32 {
        self.features
    }
    fn hvac_mode(&self) -> Option<String> {
        self.hvac_mode.clone()
    }
    fn hvac_modes(&self) -> &[&'static str] {
        &[]
    }
    fn hvac_modes_vec(&self) -> Vec<String> {
        self.hvac_modes.clone()
    }
    fn fan_modes_vec(&self) -> Vec<String> {
        self.fan_modes.clone()
    }
    fn preset_modes_vec(&self) -> Vec<String> {
        self.preset_modes.clone()
    }
    fn swing_modes_vec(&self) -> Vec<String> {
        self.swing_modes.clone()
    }
    fn swing_horizontal_modes_vec(&self) -> Vec<String> {
        self.swing_horizontal_modes.clone()
    }
    fn temp_bounds(&self) -> (Temp, Temp) {
        (self.min_temp, self.max_temp)
    }
    fn humidity_bounds(&self) -> (Temp, Temp) {
        (self.min_humidity, self.max_humidity)
    }
    fn set_hvac_mode(&self, mode: &str) -> BoxFuture<'_> {
        self.push(json!(["set_hvac_mode", mode]))
    }
    fn set_fan_mode(&self, mode: &str) -> BoxFuture<'_> {
        self.push(json!(["set_fan_mode", mode]))
    }
    fn set_preset_mode(&self, mode: &str) -> BoxFuture<'_> {
        self.push(json!(["set_preset_mode", mode]))
    }
    fn set_humidity(&self, humidity: i64) -> BoxFuture<'_> {
        self.push(json!(["set_humidity", humidity]))
    }
    fn set_swing_mode(&self, mode: &str) -> BoxFuture<'_> {
        self.push(json!(["set_swing_mode", mode]))
    }
    fn set_swing_horizontal_mode(&self, mode: &str) -> BoxFuture<'_> {
        self.push(json!(["set_swing_horizontal_mode", mode]))
    }
    fn set_temperature(&self, args: SetTemperature) -> BoxFuture<'_> {
        let mut kw = Map::new();
        kw.insert("entity_id".into(), json!([self.id.as_str()]));
        if let Some(t) = args.temperature {
            kw.insert("temperature".into(), json!(t));
        }
        if let Some(low) = args.target_temp_low {
            kw.insert("target_temp_low".into(), json!(low));
        }
        if let Some(high) = args.target_temp_high {
            kw.insert("target_temp_high".into(), json!(high));
        }
        if let Some(m) = args.hvac_mode {
            kw.insert("hvac_mode".into(), json!(m));
        }
        self.push(json!(["set_temperature", kw]))
    }
}

struct FakeNotify {
    id: EntityId,
    features: u32,
    hass: Hass,
    calls: Mutex<Vec<Value>>,
}

impl Notify for FakeNotify {
    fn entity_id(&self) -> Option<EntityId> {
        Some(self.id.clone())
    }
    fn available(&self) -> bool {
        true
    }
    fn supported_features(&self) -> u32 {
        self.features
    }
    fn send_message<'a>(
        &'a self,
        message: &'a str,
        title: Option<&'a str>,
        context: Context,
    ) -> BoxFuture<'a> {
        self.calls
            .lock()
            .push(json!(["send_message", message, title]));
        let ts = ha_core::isoformat(self.hass.clock().now());
        self.hass
            .set_state_with_context(self.id.clone(), ts, Default::default(), Some(context));
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn matches_ha_2026_6_1_climate_and_notify_vectors() {
    let raw = include_str!("vectors/climate_notify.json");
    let doc: Value = serde_json::from_str(raw).unwrap();
    assert_eq!(doc["ha_version"], "2026.6.1");

    let hass = Hass::new();
    let reg = Arc::new(ha_registry::Registries::default());
    let climates = Arc::new(Climates::default());
    climates.register_services(&hass, reg.clone());
    let notifiers = Arc::new(Notifiers::default());
    notifiers.register_services(&hass, reg);

    let all_clim = clim_feat::TARGET_TEMPERATURE
        | clim_feat::TARGET_TEMPERATURE_RANGE
        | clim_feat::TARGET_HUMIDITY
        | clim_feat::FAN_MODE
        | clim_feat::PRESET_MODE
        | clim_feat::SWING_MODE
        | clim_feat::SWING_HORIZONTAL_MODE
        | clim_feat::TURN_OFF
        | clim_feat::TURN_ON;

    let clim_list = vec![
        Arc::new(FakeClimate {
            id: EntityId::parse("climate.living_room").unwrap(),
            features: all_clim,
            hvac_modes: vec!["off".into(), "heat".into(), "cool".into(), "auto".into()],
            hvac_mode: Some("off".into()),
            fan_modes: vec!["auto".into(), "low".into(), "high".into()],
            preset_modes: vec!["eco".into(), "boost".into(), "away".into()],
            swing_modes: vec!["off".into(), "vertical".into()],
            swing_horizontal_modes: vec!["off".into(), "horizontal".into()],
            min_temp: Temp::Int(16),
            max_temp: Temp::Int(30),
            min_humidity: Temp::Int(30),
            max_humidity: Temp::Int(80),
            calls: Mutex::new(Vec::new()),
        }),
        Arc::new(FakeClimate {
            id: EntityId::parse("climate.minimal").unwrap(),
            features: 0,
            hvac_modes: vec!["off".into(), "heat".into()],
            hvac_mode: Some("heat".into()),
            fan_modes: Vec::new(),
            preset_modes: Vec::new(),
            swing_modes: Vec::new(),
            swing_horizontal_modes: Vec::new(),
            min_temp: Temp::Int(16),
            max_temp: Temp::Int(30),
            min_humidity: Temp::Int(30),
            max_humidity: Temp::Int(80),
            calls: Mutex::new(Vec::new()),
        }),
    ];
    for c in &clim_list {
        climates.add(c.clone());
    }

    let notify_list = vec![
        Arc::new(FakeNotify {
            id: EntityId::parse("notify.telegram").unwrap(),
            features: notify_feat::TITLE,
            hass: hass.clone(),
            calls: Mutex::new(Vec::new()),
        }),
        Arc::new(FakeNotify {
            id: EntityId::parse("notify.plain").unwrap(),
            features: 0,
            hass: hass.clone(),
            calls: Mutex::new(Vec::new()),
        }),
    ];
    for n in &notify_list {
        notifiers.add(n.clone());
    }

    let cases = doc["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 24);

    for case in cases {
        let name = case["name"].as_str().unwrap();
        let domain = case["domain"].as_str().unwrap();
        let service = case["service"].as_str().unwrap();
        let data = case["data"].as_object().unwrap().clone();
        for c in &clim_list {
            c.calls.lock().clear();
        }
        for n in &notify_list {
            n.calls.lock().clear();
        }

        let res = hass
            .call_service(domain, service, data, None, false, None)
            .await;

        if let Some(err_obj) = case.get("error") {
            let err = res.expect_err(&format!("case {name}: expected error"));
            match err_obj["kind"].as_str().unwrap() {
                "invalid" => match err {
                    ServiceError::Invalid(msg) => {
                        assert_eq!(msg, err_obj["message"].as_str().unwrap(), "case {name}");
                    }
                    other => panic!("case {name}: expected Invalid, got {other:?}"),
                },
                "validation" => match err {
                    ServiceError::Validation(r) => {
                        assert_eq!(
                            r.domain.as_str(),
                            err_obj["domain"].as_str().unwrap(),
                            "case {name}"
                        );
                        assert_eq!(
                            r.key.as_str(),
                            err_obj["key"].as_str().unwrap(),
                            "case {name}"
                        );
                        assert_eq!(
                            json!(r.placeholders),
                            err_obj["placeholders"],
                            "case {name}"
                        );
                    }
                    other => panic!("case {name}: expected Validation, got {other:?}"),
                },
                other => panic!("case {name}: unexpected vector error kind {other}"),
            }
        } else {
            res.unwrap_or_else(|e| panic!("case {name}: unexpected error {e:?}"));
            let mut got = Map::new();
            for c in &clim_list {
                let calls = c.calls.lock().clone();
                if !calls.is_empty() {
                    got.insert(c.id.as_str().to_owned(), Value::Array(calls));
                }
            }
            for n in &notify_list {
                let calls = n.calls.lock().clone();
                if !calls.is_empty() {
                    got.insert(n.id.as_str().to_owned(), Value::Array(calls));
                }
            }
            assert_eq!(Value::Object(got), case["calls"], "case {name}");
        }
    }

    if doc["notify_state_is_timestamp"].as_bool().unwrap() {
        let st = hass
            .states()
            .get(&EntityId::parse("notify.telegram").unwrap())
            .unwrap();
        assert!(
            st.state.contains('T'),
            "expected ISO timestamp, got {}",
            st.state
        );
    }
}
