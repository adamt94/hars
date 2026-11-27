//! Replay Home Assistant 2026.6.1 vectors for `select`, `number`, `cover`,
//! `lock`, and `fan` (`scripts/vectors/entity_components.py`).

use ha_core::{EntityId, Hass, ServiceError};
use ha_integrations::cover::{self, Cover, Covers};
use ha_integrations::fan::{self, Fan, Fans};
use ha_integrations::light::BoxFuture;
use ha_integrations::lock::{self, Lock, Locks};
use ha_integrations::number::{self, Number, Numbers};
use ha_integrations::select::{Select, Selects};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::sync::Arc;

const VECTORS: &str = include_str!("vectors/entity_components.json");

struct VecSelect {
    id: EntityId,
    options: Vec<String>,
    current: Mutex<Option<String>>,
    calls: Mutex<Vec<Value>>,
}

impl Select for VecSelect {
    fn entity_id(&self) -> &EntityId {
        &self.id
    }
    fn available(&self) -> bool {
        true
    }
    fn options(&self) -> Vec<String> {
        self.options.clone()
    }
    fn current_option(&self) -> Option<String> {
        self.current.lock().clone()
    }
    fn select_option(&self, option: &str) -> BoxFuture<'_> {
        *self.current.lock() = Some(option.to_owned());
        self.calls.lock().push(json!(option));
        Box::pin(async { Ok(()) })
    }
}

struct VecNumber {
    id: EntityId,
    min: f64,
    max: f64,
    calls: Mutex<Vec<Value>>,
}

impl Number for VecNumber {
    fn entity_id(&self) -> &EntityId {
        &self.id
    }
    fn available(&self) -> bool {
        true
    }
    fn min_value(&self) -> f64 {
        self.min
    }
    fn max_value(&self) -> f64 {
        self.max
    }
    fn set_value(&self, value: f64) -> BoxFuture<'_> {
        self.calls.lock().push(json!(value));
        Box::pin(async { Ok(()) })
    }
}

struct VecCover {
    id: EntityId,
    features: u32,
    calls: Mutex<Vec<Value>>,
}

impl VecCover {
    fn rec(&self, s: impl Into<String>) -> BoxFuture<'_> {
        self.calls.lock().push(json!(s.into()));
        Box::pin(async { Ok(()) })
    }
}

impl Cover for VecCover {
    fn entity_id(&self) -> &EntityId {
        &self.id
    }
    fn available(&self) -> bool {
        true
    }
    fn supported_features(&self) -> u32 {
        self.features
    }
    fn is_closed(&self) -> Option<bool> {
        Some(true)
    }
    fn is_opening(&self) -> Option<bool> {
        Some(false)
    }
    fn is_closing(&self) -> Option<bool> {
        Some(false)
    }
    fn current_position(&self) -> Option<u8> {
        Some(0)
    }
    fn current_tilt_position(&self) -> Option<u8> {
        Some(0)
    }
    fn open_cover(&self) -> BoxFuture<'_> {
        self.rec("open_cover")
    }
    fn close_cover(&self) -> BoxFuture<'_> {
        self.rec("close_cover")
    }
    fn stop_cover(&self) -> BoxFuture<'_> {
        self.rec("stop_cover")
    }
    fn set_cover_position(&self, position: u8) -> BoxFuture<'_> {
        self.rec(format!("set_cover_position:{position}"))
    }
    fn open_cover_tilt(&self) -> BoxFuture<'_> {
        self.rec("open_cover_tilt")
    }
    fn close_cover_tilt(&self) -> BoxFuture<'_> {
        self.rec("close_cover_tilt")
    }
    fn stop_cover_tilt(&self) -> BoxFuture<'_> {
        self.rec("stop_cover_tilt")
    }
    fn set_cover_tilt_position(&self, tilt_position: u8) -> BoxFuture<'_> {
        self.rec(format!("set_cover_tilt_position:{tilt_position}"))
    }
}

struct VecLock {
    id: EntityId,
    features: u32,
    code_format: Option<String>,
    default_code: Option<String>,
    calls: Mutex<Vec<Value>>,
}

impl VecLock {
    fn rec(&self, op: &str, code: Option<&str>) -> BoxFuture<'_> {
        let shown = code.unwrap_or("None");
        self.calls.lock().push(json!(format!("{op}:{shown}")));
        Box::pin(async { Ok(()) })
    }
}

impl Lock for VecLock {
    fn entity_id(&self) -> &EntityId {
        &self.id
    }
    fn available(&self) -> bool {
        true
    }
    fn supported_features(&self) -> u32 {
        self.features
    }
    fn code_format(&self) -> Option<String> {
        self.code_format.clone()
    }
    fn default_code(&self) -> Option<String> {
        self.default_code.clone()
    }
    fn lock(&self, code: Option<&str>) -> BoxFuture<'_> {
        self.rec("lock", code)
    }
    fn unlock(&self, code: Option<&str>) -> BoxFuture<'_> {
        self.rec("unlock", code)
    }
    fn open(&self, code: Option<&str>) -> BoxFuture<'_> {
        self.rec("open", code)
    }
}

struct VecFan {
    id: EntityId,
    features: u32,
    speed_count: u32,
    preset_modes: Vec<String>,
    pct: Mutex<Option<u8>>,
    preset: Mutex<Option<String>>,
    calls: Mutex<Vec<Value>>,
}

impl Fan for VecFan {
    fn entity_id(&self) -> &EntityId {
        &self.id
    }
    fn available(&self) -> bool {
        true
    }
    fn supported_features(&self) -> u32 {
        self.features
    }
    fn is_on(&self) -> bool {
        self.pct.lock().is_some_and(|p| p > 0) || self.preset.lock().is_some()
    }
    fn percentage(&self) -> Option<u8> {
        *self.pct.lock()
    }
    fn speed_count(&self) -> u32 {
        self.speed_count
    }
    fn preset_modes(&self) -> Vec<String> {
        self.preset_modes.clone()
    }
    fn turn_on(&self, percentage: Option<u8>, preset_mode: Option<&str>) -> BoxFuture<'_> {
        if let Some(p) = percentage {
            *self.pct.lock() = Some(p);
        }
        if let Some(pm) = preset_mode {
            *self.preset.lock() = Some(pm.to_owned());
        }
        let p = percentage.map_or_else(|| "None".into(), |v| v.to_string());
        let pm = preset_mode.unwrap_or("None");
        self.calls.lock().push(json!(format!("turn_on:{p}:{pm}")));
        Box::pin(async { Ok(()) })
    }
    fn turn_off(&self) -> BoxFuture<'_> {
        *self.pct.lock() = Some(0);
        *self.preset.lock() = None;
        self.calls.lock().push(json!("turn_off"));
        Box::pin(async { Ok(()) })
    }
    fn set_percentage(&self, percentage: u8) -> BoxFuture<'_> {
        *self.pct.lock() = Some(percentage);
        self.calls
            .lock()
            .push(json!(format!("set_percentage:{percentage}")));
        Box::pin(async { Ok(()) })
    }
    fn set_preset_mode(&self, preset_mode: &str) -> BoxFuture<'_> {
        *self.preset.lock() = Some(preset_mode.to_owned());
        self.calls
            .lock()
            .push(json!(format!("set_preset_mode:{preset_mode}")));
        Box::pin(async { Ok(()) })
    }
    fn oscillate(&self, oscillating: bool) -> BoxFuture<'_> {
        let s = if oscillating { "True" } else { "False" };
        self.calls.lock().push(json!(format!("oscillate:{s}")));
        Box::pin(async { Ok(()) })
    }
    fn set_direction(&self, direction: &str) -> BoxFuture<'_> {
        self.calls
            .lock()
            .push(json!(format!("set_direction:{direction}")));
        Box::pin(async { Ok(()) })
    }
}

async fn check_step(hass: &Hass, step: &Value, calls: &Mutex<Vec<Value>>) {
    let domain = step["domain"].as_str().unwrap();
    let service = step["service"].as_str().unwrap();
    let data: Map<String, Value> = step["data"].as_object().cloned().unwrap();
    let before = calls.lock().len();
    let res = hass
        .call_service(domain, service, data, None, false, None)
        .await;
    if let Some(err) = step.get("error") {
        let got = res.expect_err(&format!("expected error for {domain}.{service}: {step}"));
        match err["kind"].as_str().unwrap() {
            "validation" => match got {
                ServiceError::Validation(r) => {
                    assert_eq!(
                        r.domain.as_str(),
                        err["translation_domain"].as_str().unwrap(),
                        "domain for {step}"
                    );
                    assert_eq!(
                        r.key.as_str(),
                        err["translation_key"].as_str().unwrap(),
                        "key for {step}"
                    );
                    assert_eq!(
                        json!(r.placeholders),
                        err["translation_placeholders"],
                        "placeholders for {step}"
                    );
                }
                other => panic!("expected Validation for {step}, got {other:?}"),
            },
            "invalid" => match got {
                ServiceError::Invalid(msg) => {
                    assert_eq!(
                        msg,
                        err["message"].as_str().unwrap(),
                        "invalid msg for {step}"
                    );
                }
                other => panic!("expected Invalid for {step}, got {other:?}"),
            },
            other => panic!("unexpected vector error kind {other}"),
        }
        assert_eq!(
            calls.lock().len(),
            before,
            "failed call must not reach entity: {step}"
        );
    } else {
        res.unwrap_or_else(|e| panic!("unexpected error for {step}: {e:?}"));
        let got_calls = calls.lock()[before..].to_vec();
        let want_calls = step["calls"].as_array().unwrap().clone();
        assert_eq!(got_calls, want_calls, "calls for {step}");
    }
}

#[tokio::test]
async fn matches_ha_2026_6_1_entity_component_vectors() {
    let v: Value = serde_json::from_str(VECTORS).unwrap();
    assert_eq!(v["ha_version"], "2026.6.1");

    // Unrecorded attribute sets match HA 2026.6.1.
    for domain in ["select", "number", "cover", "lock", "fan"] {
        let mut got: Vec<&str> = ha_core::recorded::domain_unrecorded(domain).to_vec();
        got.sort_unstable();
        let want: Vec<&str> = v["unrecorded"][domain]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(got, want, "unrecorded attributes for {domain}");
    }

    // Number step calculation matches HA 2026.6.1.
    assert_eq!(
        number::calculate_step(0.0, 10.0, None),
        v["number_steps"]["0_10"].as_f64().unwrap()
    );
    assert_eq!(
        number::calculate_step(0.0, 1.0, None),
        v["number_steps"]["0_1"].as_f64().unwrap()
    );

    let hass = Hass::new();
    let regs = Arc::new(ha_registry::Registries::default());

    let selects = Arc::new(Selects::default());
    selects.register_services(&hass, regs.clone());
    let sel = Arc::new(VecSelect {
        id: EntityId::parse("select.mode").unwrap(),
        options: vec![
            "off".into(),
            "mild".into(),
            "moderate".into(),
            "intense".into(),
        ],
        current: Mutex::new(None),
        calls: Mutex::default(),
    });
    selects.add(sel.clone());

    let numbers = Arc::new(Numbers::default());
    numbers.register_services(&hass, regs.clone());
    let num = Arc::new(VecNumber {
        id: EntityId::parse("number.level").unwrap(),
        min: 0.0,
        max: 10.0,
        calls: Mutex::default(),
    });
    numbers.add(num.clone());

    let covers = Arc::new(Covers::default());
    covers.register_services(&hass, regs.clone());
    let all_cov = cover::feature::OPEN
        | cover::feature::CLOSE
        | cover::feature::SET_POSITION
        | cover::feature::STOP
        | cover::feature::OPEN_TILT
        | cover::feature::CLOSE_TILT
        | cover::feature::STOP_TILT
        | cover::feature::SET_TILT_POSITION;
    let cov = Arc::new(VecCover {
        id: EntityId::parse("cover.blinds").unwrap(),
        features: all_cov,
        calls: Mutex::default(),
    });
    let cov_open = Arc::new(VecCover {
        id: EntityId::parse("cover.open_only").unwrap(),
        features: cover::feature::OPEN,
        calls: Mutex::default(),
    });
    covers.add(cov.clone());
    covers.add(cov_open.clone());

    let locks = Arc::new(Locks::default());
    locks.register_services(&hass, regs.clone());
    let lck = Arc::new(VecLock {
        id: EntityId::parse("lock.front").unwrap(),
        features: lock::feature::OPEN,
        code_format: Some(r"^\d{4}$".into()),
        default_code: None,
        calls: Mutex::default(),
    });
    let lck_side = Arc::new(VecLock {
        id: EntityId::parse("lock.side").unwrap(),
        features: 0,
        code_format: Some(r"^\d{4}$".into()),
        default_code: Some("4321".into()),
        calls: Mutex::default(),
    });
    locks.add(lck.clone());
    locks.add(lck_side.clone());

    let fans = Arc::new(Fans::default());
    fans.register_services(&hass, regs);
    let all_fan = fan::feature::SET_SPEED
        | fan::feature::OSCILLATE
        | fan::feature::DIRECTION
        | fan::feature::PRESET_MODE
        | fan::feature::TURN_OFF
        | fan::feature::TURN_ON;
    let f = Arc::new(VecFan {
        id: EntityId::parse("fan.bedroom").unwrap(),
        features: all_fan,
        speed_count: 3,
        preset_modes: vec!["auto".into(), "sleep".into(), "breeze".into()],
        pct: Mutex::new(Some(0)),
        preset: Mutex::new(None),
        calls: Mutex::default(),
    });
    let f_basic = Arc::new(VecFan {
        id: EntityId::parse("fan.basic").unwrap(),
        features: fan::feature::TURN_ON | fan::feature::TURN_OFF,
        speed_count: 100,
        preset_modes: vec![],
        pct: Mutex::new(Some(0)),
        preset: Mutex::new(None),
        calls: Mutex::default(),
    });
    fans.add(f.clone());
    fans.add(f_basic.clone());

    for step in v["select"].as_array().unwrap() {
        check_step(&hass, step, &sel.calls).await;
    }
    for step in v["number"].as_array().unwrap() {
        check_step(&hass, step, &num.calls).await;
    }
    for step in v["cover"].as_array().unwrap() {
        let calls = if step["data"]["entity_id"] == "cover.open_only" {
            &cov_open.calls
        } else {
            &cov.calls
        };
        check_step(&hass, step, calls).await;
    }
    for step in v["lock"].as_array().unwrap() {
        let calls = if step["data"]["entity_id"] == "lock.side" {
            &lck_side.calls
        } else {
            &lck.calls
        };
        check_step(&hass, step, calls).await;
    }
    for step in v["fan"].as_array().unwrap() {
        let calls = if step["data"]["entity_id"] == "fan.basic" {
            &f_basic.calls
        } else {
            &f.calls
        };
        check_step(&hass, step, calls).await;
    }
}
