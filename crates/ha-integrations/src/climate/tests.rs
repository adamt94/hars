//! The component against HA 2026.6.1's own results for switchbot_cloud's
//! IR air conditioner (attributes, validation messages, required features).

use super::*;
use parking_lot::Mutex;

/// An entity shaped like switchbot_cloud's IR air conditioner; it records
/// what the component asked of it.
struct Ac {
    id: EntityId,
    mode: Mutex<String>,
    calls: Mutex<Vec<String>>,
}

impl Climate for Ac {
    fn entity_id(&self) -> &EntityId {
        &self.id
    }
    fn available(&self) -> bool {
        true
    }
    fn supported_features(&self) -> u32 {
        393
    }
    fn hvac_mode(&self) -> Option<String> {
        Some(self.mode.lock().clone())
    }
    fn hvac_modes(&self) -> &[&'static str] {
        &["heat_cool", "cool", "dry", "fan_only", "heat", "off"]
    }
    fn fan_mode(&self) -> Option<String> {
        Some("auto".into())
    }
    fn fan_modes(&self) -> &[&'static str] {
        &["auto", "low", "medium", "high"]
    }
    fn precision(&self) -> Precision {
        Precision::Whole
    }
    fn target_temperature_step(&self) -> Option<Temp> {
        Some(Temp::Int(1))
    }
    fn target_temperature(&self) -> Option<Temp> {
        Some(Temp::Float(22.5))
    }
    fn set_hvac_mode(&self, mode: &str) -> BoxFuture<'_> {
        self.calls.lock().push(format!("hvac {mode}"));
        Box::pin(async { Ok(()) })
    }
    fn set_fan_mode(&self, mode: &str) -> BoxFuture<'_> {
        self.calls.lock().push(format!("fan {mode}"));
        Box::pin(async { Ok(()) })
    }
    fn set_temperature(&self, args: SetTemperature) -> BoxFuture<'_> {
        self.calls.lock().push(format!("temp {args:?}"));
        Box::pin(async { Ok(()) })
    }
    fn turn_on(&self) -> BoxFuture<'_> {
        self.calls.lock().push("on".into());
        Box::pin(async { Ok(()) })
    }
    fn turn_off(&self) -> BoxFuture<'_> {
        self.calls.lock().push("off".into());
        Box::pin(async { Ok(()) })
    }
}

fn setup() -> (Hass, Arc<Ac>) {
    let hass = Hass::new();
    let climates = Arc::new(Climates::default());
    climates.register_services(&hass, Arc::new(ha_registry::Registries::default()));
    let ac = Arc::new(Ac {
        id: EntityId::parse("climate.air_conditioner").unwrap(),
        mode: Mutex::new("fan_only".into()),
        calls: Mutex::default(),
    });
    climates.add(ac.clone());
    (hass, ac)
}

async fn call(hass: &Hass, service: &str, data: Value) -> Result<(), ServiceError> {
    let mut data = data.as_object().cloned().unwrap();
    data.insert("entity_id".into(), json!("climate.air_conditioner"));
    hass.call_service("climate", service, data, None, false, None)
        .await
        .map(|_| ())
}

fn validation(r: Result<(), ServiceError>) -> (String, String, Value) {
    match r {
        Err(ServiceError::Validation(r)) => (r.key.to_string(), r.message, json!(r.placeholders)),
        other => panic!("expected a validation error, got {other:?}"),
    }
}

#[test]
fn attributes_as_ha() {
    let (_, ac) = setup();
    // HA's attributes for the AC, less the standard ones the platform adds;
    // a 22.5 target shows as 22 (`round`, half to even).
    assert_eq!(
        Value::Object(attributes(ac.as_ref())),
        json!({"hvac_modes":["heat_cool","cool","dry","fan_only","heat","off"],"min_temp":7,
            "max_temp":35,"target_temp_step":1,"fan_modes":["auto","low","medium","high"],
            "current_temperature":null,"temperature":22,"fan_mode":"auto"})
    );
}

#[test]
fn display_temp_as_ha() {
    let whole = |t| display_temp(Some(t), Precision::Whole);
    assert_eq!(whole(Temp::Float(21.5)), json!(22));
    assert_eq!(whole(Temp::Float(22.5)), json!(22));
    assert_eq!(whole(Temp::Float(22.7)), json!(23));
    assert_eq!(whole(Temp::Float(7.9)), json!(8));
    let tenths = |t| display_temp(Some(t), Precision::Tenths);
    assert_eq!(tenths(Temp::Int(7)), json!(7));
    assert_eq!(tenths(Temp::Float(21.0)), json!(21.0));
    assert_eq!(tenths(Temp::Float(21.25)), json!(21.2));
    assert_eq!(
        display_temp(Some(Temp::Int(7)), Precision::Halves),
        json!(7.0)
    );
    assert_eq!(display_temp(None, Precision::Whole), Value::Null);
}

#[tokio::test]
async fn validation_as_ha() {
    let (hass, ac) = setup();
    let (key, msg, p) =
        validation(call(&hass, "set_hvac_mode", json!({"hvac_mode": "auto"})).await);
    assert_eq!(key, "not_valid_hvac_mode");
    assert_eq!(
        msg,
        "HVAC mode auto is not valid. Valid HVAC modes are: heat_cool, cool, dry, fan_only, heat, off"
    );
    assert_eq!(p["modes"], "heat_cool, cool, dry, fan_only, heat, off");
    let (key, msg, _) = validation(call(&hass, "set_fan_mode", json!({"fan_mode": "turbo"})).await);
    assert_eq!(key, "not_valid_fan_mode");
    assert_eq!(
        msg,
        "Fan mode turbo is not valid. Valid fan modes are: auto, low, medium, high"
    );
    for (t, shown) in [(json!(35.5), "35.5"), (json!(6), "6.0")] {
        let (key, msg, p) =
            validation(call(&hass, "set_temperature", json!({"temperature": t})).await);
        assert_eq!(key, "temp_out_of_range");
        assert_eq!(
            msg,
            format!("Provided temperature {shown} is not valid. Accepted range is 7 to 35")
        );
        assert_eq!(
            p,
            json!({"check_temp": shown, "min_temp": "7", "max_temp": "35"})
        );
    }
    let (key, msg, p) = validation(
        call(
            &hass,
            "set_temperature",
            json!({"target_temp_low": 18, "target_temp_high": 24}),
        )
        .await,
    );
    assert_eq!(key, "missing_target_temperature_range_entity_feature");
    assert_eq!(
        msg,
        "Set temperature action was used with the 'Lower/Upper target temperature' parameter but the entity does not support it"
    );
    assert_eq!(p, Value::Null);
    for service in ["set_preset_mode", "set_humidity", "set_swing_mode"] {
        let field = match service {
            "set_preset_mode" => "preset_mode",
            "set_humidity" => "humidity",
            _ => "swing_mode",
        };
        let (key, msg, p) = validation(call(&hass, service, json!({ field: "1" })).await);
        assert_eq!(key, "service_not_supported");
        assert_eq!(
            msg,
            format!("Entity climate.air_conditioner does not support action climate.{service}")
        );
        assert_eq!(
            p,
            json!({"domain": "climate", "service": service, "entity_id": "climate.air_conditioner"})
        );
    }
    // The bounds are valid; none of the refusals reached the entity.
    call(&hass, "set_temperature", json!({"temperature": 7}))
        .await
        .unwrap();
    call(
        &hass,
        "set_temperature",
        json!({"temperature": "35", "hvac_mode": "heat"}),
    )
    .await
    .unwrap();
    assert_eq!(
        *ac.calls.lock(),
        [
            "temp SetTemperature { temperature: Some(7.0), target_temp_low: None, target_temp_high: None, hvac_mode: None }",
            "temp SetTemperature { temperature: Some(35.0), target_temp_low: None, target_temp_high: None, hvac_mode: Some(\"heat\") }",
        ]
    );
}

#[tokio::test]
async fn services_reach_the_entity() {
    let (hass, ac) = setup();
    call(&hass, "set_hvac_mode", json!({"hvac_mode": "cool"}))
        .await
        .unwrap();
    call(&hass, "set_fan_mode", json!({"fan_mode": "high"}))
        .await
        .unwrap();
    call(&hass, "turn_off", json!({})).await.unwrap();
    // The base toggle: off → turn_on, anything else → turn_off.
    call(&hass, "toggle", json!({})).await.unwrap();
    *ac.mode.lock() = "off".into();
    call(&hass, "toggle", json!({})).await.unwrap();
    call(&hass, "turn_on", json!({})).await.unwrap();
    assert_eq!(
        *ac.calls.lock(),
        ["hvac cool", "fan high", "off", "off", "on", "on"]
    );
}

/// An entity that only sets modes gets HA's base turn_on/turn_off.
struct Heater {
    id: EntityId,
    calls: Mutex<Vec<String>>,
}

impl Climate for Heater {
    fn entity_id(&self) -> &EntityId {
        &self.id
    }
    fn available(&self) -> bool {
        true
    }
    fn supported_features(&self) -> u32 {
        feature::TURN_ON | feature::TURN_OFF
    }
    fn hvac_mode(&self) -> Option<String> {
        Some("off".into())
    }
    fn hvac_modes(&self) -> &[&'static str] {
        &["off", "heat"]
    }
    fn set_hvac_mode(&self, mode: &str) -> BoxFuture<'_> {
        self.calls.lock().push(mode.to_owned());
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn base_turn_on_and_off_set_modes() {
    let hass = Hass::new();
    let climates = Arc::new(Climates::default());
    climates.register_services(&hass, Arc::new(ha_registry::Registries::default()));
    let h = Arc::new(Heater {
        id: EntityId::parse("climate.heater").unwrap(),
        calls: Mutex::default(),
    });
    climates.add(h.clone());
    for service in ["turn_on", "turn_off", "toggle"] {
        let data = json!({"entity_id": "climate.heater"})
            .as_object()
            .cloned()
            .unwrap();
        hass.call_service("climate", service, data, None, false, None)
            .await
            .unwrap();
    }
    // The toggle from off turns on.
    assert_eq!(*h.calls.lock(), ["heat", "off", "heat"]);
}
