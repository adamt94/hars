//! The `weather` entity component (`components/weather`): display units and
//! their conversion, a weather entity's attributes and forecast items, and
//! the forecasts its WS subscribers and `weather.get_forecasts` read.

use crate::met::forecast::{Item, Num, round, round_int};
use ha_core::{EntityId, EntityTarget, Hass, Schema, ServiceError, SupportsResponse};
use parking_lot::RwLock;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use tokio::sync::watch;

pub const DOMAIN: &str = "weather";

/// `WeatherEntityFeature`: daily and hourly forecasts.
pub const FORECAST_DAILY: u32 = 1;
pub const FORECAST_TWICE_DAILY: u32 = 4;
pub const FORECAST_HOURLY: u32 = 2;

// `util/unit_conversion.py`'s constants, as the same float expressions.
const IN_TO_M: f64 = 0.0254;
const FOOT_TO_M: f64 = IN_TO_M * 12.0;
const YARD_TO_M: f64 = FOOT_TO_M * 3.0;
const MILE_TO_M: f64 = YARD_TO_M * 1760.0;
const MM_TO_M: f64 = 0.001;
const STANDARD_GRAVITY: f64 = 9.80665;
const MERCURY_DENSITY: f64 = 13.5951;

/// A unit's ratio to the base unit (Pa, m/s, m), as HA's `_UNIT_CONVERSION`.
fn pressure_ratio(unit: &str) -> f64 {
    match unit {
        "hPa" | "mbar" => 1.0 / 100.0,
        "inHg" => 1.0 / (IN_TO_M * 1000.0 * STANDARD_GRAVITY * MERCURY_DENSITY),
        _ => 1.0 / (MM_TO_M * 1000.0 * STANDARD_GRAVITY * MERCURY_DENSITY),
    }
}

fn speed_ratio(unit: &str) -> f64 {
    match unit {
        "km/h" => 3600.0 / 1000.0,
        "mph" => 3600.0 / MILE_TO_M,
        "kn" => 3600.0 / 1852.0,
        "ft/s" => 1.0 / FOOT_TO_M,
        _ => 1.0,
    }
}

fn length_ratio(unit: &str) -> f64 {
    match unit {
        "mm" => 1.0 / MM_TO_M,
        "in" => 1.0 / IN_TO_M,
        "km" => 1.0 / 1000.0,
        _ => 1.0 / MILE_TO_M,
    }
}

/// `BaseUnitConverter.convert`: the identity for the same unit, else
/// `(value / from) * to`.
fn convert(value: f64, from: &str, to: &str, ratio: fn(&str) -> f64) -> f64 {
    if from == to {
        value
    } else {
        (value / ratio(from)) * ratio(to)
    }
}

/// `weather/const.py`'s `VALID_UNITS`, in `weather/convertible_units`'
/// order (sorted with `str.casefold`).
pub const VALID_PRESSURE: &[&str] = &["hPa", "inHg", "mbar", "mmHg"];
pub const VALID_TEMPERATURE: &[&str] = &["°C", "°F"];
pub const VALID_VISIBILITY: &[&str] = &["km", "mi"];
pub const VALID_PRECIPITATION: &[&str] = &["in", "mm"];
pub const VALID_WIND_SPEED: &[&str] = &["Beaufort", "ft/s", "km/h", "kn", "m/s", "mph"];

/// `weather/convertible_units`' result.
pub fn convertible_units() -> Value {
    json!({"units": {
        "pressure_unit": VALID_PRESSURE,
        "temperature_unit": VALID_TEMPERATURE,
        "visibility_unit": VALID_VISIBILITY,
        "precipitation_unit": VALID_PRECIPITATION,
        "wind_speed_unit": VALID_WIND_SPEED,
    }})
}

/// The units a weather entity shows: its registry options
/// (`options.weather.<x>_unit`, if valid), else the unit system's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Units {
    pub temperature: &'static str,
    pub pressure: &'static str,
    pub wind_speed: &'static str,
    pub visibility: &'static str,
    pub precipitation: &'static str,
}

impl Units {
    /// `us` is the `us_customary` unit system; `options` the entity's
    /// registry `options`.
    pub fn new(us: bool, options: Option<&Map<String, Value>>) -> Self {
        let weather = options
            .and_then(|o| o.get(DOMAIN))
            .and_then(Value::as_object);
        let pick = |key: &str, valid: &'static [&'static str], default: &'static str| {
            weather
                .and_then(|w| w.get(key))
                .and_then(Value::as_str)
                .and_then(|u| valid.iter().copied().find(|v| *v == u))
                .unwrap_or(default)
        };
        let (t, p, w, v, r) = if us {
            ("°F", "inHg", "mph", "mi", "in")
        } else {
            ("°C", "hPa", "km/h", "km", "mm")
        };
        Self {
            temperature: pick("temperature_unit", VALID_TEMPERATURE, t),
            pressure: pick("pressure_unit", VALID_PRESSURE, p),
            wind_speed: pick("wind_speed_unit", VALID_WIND_SPEED, w),
            visibility: pick("visibility_unit", VALID_VISIBILITY, v),
            precipitation: pick("precipitation_unit", VALID_PRECIPITATION, r),
        }
    }

    /// A native °C temperature, converted and `round_temperature`d: tenths
    /// (a float) in °C, whole (an int) otherwise.
    fn temperature(&self, c: Num) -> Value {
        let c = c.f64();
        if self.temperature == "°C" {
            round(c, 1).into()
        } else {
            round_int(c * 1.8 + 32.0).into()
        }
    }

    /// A native hPa pressure, converted and rounded to 2 places.
    fn pressure(&self, hpa: Num) -> f64 {
        round(convert(hpa.f64(), "hPa", self.pressure, pressure_ratio), 2)
    }

    /// A native km/h speed, converted and rounded to 2 places.
    fn speed(&self, kmh: Num) -> f64 {
        let kmh = kmh.f64();
        let v = match self.wind_speed {
            "km/h" => kmh,
            // `_ms_to_beaufort`.
            "Beaufort" => {
                let ms = kmh / speed_ratio("km/h");
                ((ms / 0.836).powi(2)).powf(1.0 / 3.0).round_ties_even()
            }
            to => convert(kmh, "km/h", to, speed_ratio),
        };
        round(v, 2)
    }

    /// A native mm depth, converted and rounded to 2 places.
    fn precipitation(&self, mm: Num) -> f64 {
        round(convert(mm.f64(), "mm", self.precipitation, length_ratio), 2)
    }
}

/// HA's `state_attributes` for a weather entity, then the generic tail.
/// While unavailable only the tail is there.
pub fn attributes(
    current: Option<&Item>,
    units: &Units,
    available: bool,
    attribution: &str,
    friendly_name: &str,
    supported_features: u32,
) -> Map<String, Value> {
    let mut a = Map::new();
    if available {
        let c = current;
        let field = |f: fn(&Item) -> Option<Num>| c.and_then(f);
        if let Some(t) = field(|i| i.temperature) {
            a.insert("temperature".into(), units.temperature(t));
        }
        if let Some(t) = field(|i| i.dew_point) {
            a.insert("dew_point".into(), units.temperature(t));
        }
        a.insert("temperature_unit".into(), units.temperature.into());
        if let Some(h) = field(|i| i.humidity) {
            a.insert("humidity".into(), round_int(h.f64()).into());
        }
        if let Some(v) = field(|i| i.cloudiness) {
            a.insert("cloud_coverage".into(), v.json());
        }
        if let Some(v) = field(|i| i.uv_index) {
            a.insert("uv_index".into(), v.json());
        }
        if let Some(p) = field(|i| i.pressure) {
            a.insert("pressure".into(), units.pressure(p).into());
        }
        a.insert("pressure_unit".into(), units.pressure.into());
        if let Some(v) = field(|i| i.wind_bearing) {
            a.insert("wind_bearing".into(), v.json());
        }
        if let Some(v) = field(|i| i.wind_gust) {
            a.insert("wind_gust_speed".into(), units.speed(v).into());
        }
        if let Some(v) = field(|i| i.wind_speed) {
            a.insert("wind_speed".into(), units.speed(v).into());
        }
        a.insert("wind_speed_unit".into(), units.wind_speed.into());
        a.insert("visibility_unit".into(), units.visibility.into());
        a.insert("precipitation_unit".into(), units.precipitation.into());
    }
    a.insert("attribution".into(), attribution.into());
    if !friendly_name.is_empty() {
        a.insert("friendly_name".into(), friendly_name.into());
    }
    a.insert("supported_features".into(), supported_features.into());
    a
}

/// A forecast item as HA serves it: met's `_forecast` (only keys with a
/// value) then `_convert_forecast`. `temperature` is always there, `null`
/// when unknown; met drops pressure and dew point.
pub fn forecast_item(item: &Item, units: &Units) -> Value {
    let mut m = Map::new();
    if let Some(c) = item.condition {
        m.insert("condition".into(), c.into());
    }
    if let Some(v) = item.precipitation_probability {
        m.insert("precipitation_probability".into(), v.json());
    }
    m.insert("datetime".into(), crate::sun::iso(item.datetime).into());
    if let Some(v) = item.wind_bearing {
        m.insert("wind_bearing".into(), v.json());
    }
    if let Some(v) = item.cloudiness {
        m.insert("cloud_coverage".into(), v.json());
    }
    if let Some(v) = item.uv_index {
        m.insert("uv_index".into(), v.json());
    }
    m.insert(
        "temperature".into(),
        item.temperature
            .map_or(Value::Null, |t| units.temperature(t)),
    );
    if let Some(t) = item.templow {
        m.insert("templow".into(), units.temperature(t));
    }
    if let Some(v) = item.wind_gust {
        m.insert("wind_gust_speed".into(), units.speed(v).into());
    }
    if let Some(v) = item.wind_speed {
        m.insert("wind_speed".into(), units.speed(v).into());
    }
    if let Some(v) = item.precipitation {
        m.insert("precipitation".into(), units.precipitation(v).into());
    }
    if let Some(h) = item.humidity {
        m.insert("humidity".into(), round_int(h.f64()).into());
    }
    Value::Object(m)
}

/// A forecast type `weather/subscribe_forecast` and `get_forecasts` take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Daily,
    Hourly,
    TwiceDaily,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "daily" => Self::Daily,
            "hourly" => Self::Hourly,
            "twice_daily" => Self::TwiceDaily,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Hourly => "hourly",
            Self::TwiceDaily => "twice_daily",
        }
    }

    fn feature(self) -> u32 {
        match self {
            Self::Daily => FORECAST_DAILY,
            Self::Hourly => FORECAST_HOURLY,
            Self::TwiceDaily => FORECAST_TWICE_DAILY,
        }
    }
}

/// An entity's forecasts in display units, ready to send. Replaced after
/// each refresh; subscribers are woken even when nothing changed, as HA
/// pushes after every update.
#[derive(Debug, Clone, Default)]
pub struct Forecasts {
    pub daily: Arc<Value>,
    pub hourly: Arc<Value>,
}

impl Forecasts {
    pub fn get(&self, kind: Kind) -> Option<&Arc<Value>> {
        match kind {
            Kind::Daily => Some(&self.daily),
            Kind::Hourly => Some(&self.hourly),
            Kind::TwiceDaily => None,
        }
    }
}

struct Entity {
    supported_features: u32,
    /// Whether the last refresh worked. Forecast subscribers are served
    /// either way; `weather.get_forecasts` skips unavailable entities.
    available: bool,
    forecasts: watch::Sender<Forecasts>,
}

/// Why a forecast subscription was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum SubscribeError {
    /// `invalid_entity_id`.
    NotFound,
    /// `forecast_not_supported`.
    NotSupported,
}

/// The weather entities running, for the forecast APIs.
#[derive(Default)]
pub struct Weather {
    entities: RwLock<Vec<(EntityId, Entity)>>,
}

impl Weather {
    /// Publish `entity_id`'s forecasts and availability (adding the entity
    /// on first use).
    pub fn publish(
        &self,
        entity_id: &EntityId,
        supported_features: u32,
        available: bool,
        forecasts: Forecasts,
    ) {
        let mut entities = self.entities.write();
        match entities.iter_mut().find(|(id, _)| id == entity_id) {
            Some((_, e)) => {
                e.available = available;
                e.forecasts.send_replace(forecasts);
            }
            None => entities.push((
                entity_id.clone(),
                Entity {
                    supported_features,
                    available,
                    forecasts: watch::channel(forecasts).0,
                },
            )),
        }
    }

    /// `weather/subscribe_forecast`: the current forecasts (marked seen)
    /// and every later publish.
    pub fn subscribe(
        &self,
        entity_id: &str,
        kind: Kind,
    ) -> Result<watch::Receiver<Forecasts>, SubscribeError> {
        let entities = self.entities.read();
        let (_, e) = entities
            .iter()
            .find(|(id, _)| id.as_str() == entity_id)
            .ok_or(SubscribeError::NotFound)?;
        if e.supported_features & kind.feature() == 0 {
            return Err(SubscribeError::NotSupported);
        }
        Ok(e.forecasts.subscribe())
    }

    /// `weather.get_forecasts` for these targets: `entity_id`s plus the
    /// entities `indirect` (devices, areas, floors, labels) reach. Only
    /// available entities answer, as HA's entity services.
    pub(crate) fn get_forecasts(
        &self,
        target: &EntityTarget,
        indirect: &[String],
        kind: Kind,
    ) -> Result<Map<String, Value>, ServiceError> {
        let entities = self.entities.read();
        let selected: Vec<&(EntityId, Entity)> = match target {
            EntityTarget::All => entities.iter().collect(),
            EntityTarget::Ids(ids) => entities
                .iter()
                .filter(|(e, _)| ids.contains(e) || indirect.iter().any(|i| i == e.as_str()))
                .collect(),
        };
        let mut response = Map::new();
        for (id, e) in selected.iter().filter(|(_, e)| e.available) {
            let forecasts = e.forecasts.borrow();
            let Some(list) = forecasts
                .get(kind)
                .filter(|_| e.supported_features & kind.feature() != 0)
            else {
                return Err(ServiceError::Failed(format!(
                    "Weather entity '{id}' does not support '{}' forecast",
                    kind.as_str()
                )));
            };
            response.insert(id.to_string(), json!({ "forecast": &**list }));
        }
        if response.is_empty() {
            return Err(ServiceError::Failed(
                "Service call requested response data but did not match any entities".into(),
            ));
        }
        Ok(response)
    }

    /// Register `weather.get_forecasts` (response only). `registries`
    /// resolve device, area, floor and label targets.
    pub fn register_services(
        self: &Arc<Self>,
        hass: &Hass,
        registries: impl Into<ha_registry::Lookup>,
    ) {
        let registries = registries.into();
        let schema = Schema::entity_service().required(
            "type",
            ha_core::Kind::OneOf(&["daily", "hourly", "twice_daily"]),
        );
        let weather = self.clone();
        hass.services().register_with_schema(
            DOMAIN,
            "get_forecasts",
            Some(schema),
            SupportsResponse::Only,
            move |call| {
                let weather = weather.clone();
                let registries = registries.clone();
                async move {
                    let kind = call
                        .data
                        .get("type")
                        .and_then(Value::as_str)
                        .and_then(Kind::parse)
                        .ok_or_else(|| {
                            ServiceError::Invalid("required key not provided @ data['type']".into())
                        })?;
                    let target = call.entity_ids()?;
                    let indirect = registries.get().target_entities(&call.data);
                    weather.get_forecasts(&target, &indirect, kind).map(Some)
                }
            },
        );
    }
}
