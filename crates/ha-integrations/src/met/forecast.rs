//! pyMetno 0.13.0, as HA's `met` uses it: parse a `locationforecast/2.0/
//! complete` body and work out the current conditions and the daily and
//! hourly forecasts, value for value (Python rounding, summation order and
//! the int/float kind of passthrough numbers included).

use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp, ToSpan};
use serde::Deserialize;
use serde_json::Value;

/// A JSON number that keeps its kind: Python keeps `3` an int and `3.0` a
/// float, and values passed straight through show it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    pub fn f64(self) -> f64 {
        match self {
            Self::Int(i) => i as f64,
            Self::Float(f) => f,
        }
    }

    pub fn json(self) -> Value {
        match self {
            Self::Int(i) => i.into(),
            Self::Float(f) => f.into(),
        }
    }

    /// Python's `<`, across kinds.
    fn lt(self, other: Self) -> bool {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => a < b,
            (a, b) => a.f64() < b.f64(),
        }
    }
}

/// Python 3.14's `sum()` from int 0: ints add exactly until the first
/// float, which is added plainly; everything after it is added with
/// Neumaier's compensation, and the compensation is applied at the end.
/// If the ints overflow a C `long` first, CPython drops to plain `+` for
/// the rest (with exact ints), and so does this.
pub fn py_sum(values: &[Num]) -> Num {
    let mut it = values.iter();
    let mut int: i64 = 0;
    let mut f = loop {
        match it.next() {
            None => return Num::Int(int),
            Some(Num::Int(v)) => match int.checked_add(*v) {
                Some(n) => int = n,
                None => return plain_sum(Num2::Int(i128::from(int) + i128::from(*v)), it),
            },
            Some(Num::Float(x)) => break int as f64 + x,
        }
    };
    let mut c = 0.0;
    for v in it {
        let x = v.f64();
        let t = f + x;
        if f.abs() >= x.abs() {
            c += (f - t) + x;
        } else {
            c += (x - t) + f;
        }
        f = t;
    }
    if c != 0.0 && c.is_finite() {
        f += c;
    }
    Num::Float(f)
}

/// An int that may outgrow i64, or a float: CPython's generic `sum()`.
#[derive(Clone, Copy)]
enum Num2 {
    Int(i128),
    Float(f64),
}

/// The rest of a `sum()` with plain `+`, ints kept exact.
fn plain_sum<'a>(mut acc: Num2, rest: impl Iterator<Item = &'a Num>) -> Num {
    for v in rest {
        acc = match (acc, *v) {
            (Num2::Int(a), Num::Int(b)) => Num2::Int(a + i128::from(b)),
            (Num2::Int(a), Num::Float(b)) => Num2::Float(a as f64 + b),
            (Num2::Float(a), b) => Num2::Float(a + b.f64()),
        };
    }
    match acc {
        Num2::Int(i) => i64::try_from(i).map_or(Num::Float(i as f64), Num::Int),
        Num2::Float(f) => Num::Float(f),
    }
}

/// Python's `round(x, n)` for a float: correctly rounded from the exact
/// binary value, ties to even (Rust's fixed-precision formatting rounds
/// the same way).
pub fn round(x: f64, n: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.n$}").parse().unwrap_or(x)
}

/// Python's `round(x)`: an int, ties to even.
pub fn round_int(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// One field as it appears in the JSON.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
enum Field {
    /// The key isn't there: a lookup moves on to the next entry.
    #[default]
    Absent,
    /// There but not a number: a lookup stops here with nothing.
    Null,
    Num(Num),
}

impl<'de> Deserialize<'de> for Field {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match Value::deserialize(d)? {
            Value::Number(n) => match n.as_i64() {
                Some(i) => Self::Num(Num::Int(i)),
                None => n.as_f64().map_or(Self::Null, |f| Self::Num(Num::Float(f))),
            },
            _ => Self::Null,
        })
    }
}

/// `summary.symbol_code`, already mapped to HA's condition.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
enum Symbol {
    #[default]
    Absent,
    Found(Option<&'static str>),
}

impl<'de> Deserialize<'de> for Symbol {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self::Found(match Value::deserialize(d)? {
            Value::String(s) => condition(&s),
            _ => None,
        }))
    }
}

#[derive(Deserialize)]
struct Body {
    properties: Properties,
}

#[derive(Deserialize)]
struct Properties {
    timeseries: Vec<RawEntry>,
}

#[derive(Deserialize)]
struct RawEntry {
    time: Timestamp,
    #[serde(default)]
    data: Option<RawData>,
}

#[derive(Deserialize)]
struct RawData {
    #[serde(default)]
    instant: Option<RawInstant>,
    #[serde(default)]
    next_1_hours: Option<RawBlock>,
    #[serde(default)]
    next_6_hours: Option<RawBlock>,
    #[serde(default)]
    next_12_hours: Option<RawBlock>,
}

#[derive(Deserialize)]
struct RawInstant {
    #[serde(default)]
    details: Option<Instant>,
}

/// The `instant.details` pyMetno reads.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
struct Instant {
    air_temperature: Field,
    air_pressure_at_sea_level: Field,
    relative_humidity: Field,
    dew_point_temperature: Field,
    ultraviolet_index_clear_sky: Field,
    wind_speed: Field,
    wind_speed_of_gust: Field,
    wind_from_direction: Field,
    cloud_area_fraction: Field,
}

#[derive(Deserialize)]
struct RawBlock {
    #[serde(default)]
    summary: Option<Summary>,
    #[serde(default)]
    details: Option<BlockDetails>,
    /// Other keys: they still make the block non-empty (truthy).
    #[serde(flatten)]
    other: std::collections::HashMap<String, serde::de::IgnoredAny>,
}

impl RawBlock {
    fn truthy(&self) -> bool {
        self.summary.is_some() || self.details.is_some() || !self.other.is_empty()
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
struct Summary {
    symbol_code: Symbol,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
struct BlockDetails {
    precipitation_amount: Field,
    probability_of_precipitation: Field,
}

/// One timeseries entry, reduced to what pyMetno reads. `data` is `None`
/// when the entry lacks `data.instant.details`: pyMetno's lookup then hits
/// a `KeyError` and gives up on the whole lookup, not just this entry.
#[derive(Debug, Clone, Copy)]
struct Entry {
    time: Timestamp,
    data: Option<EntryData>,
}

#[derive(Debug, Clone, Copy)]
struct EntryData {
    instant: Instant,
    /// From the first non-empty of `next_1_hours`, `next_6_hours`,
    /// `next_12_hours` (pyMetno takes nothing from a coarser block).
    symbol: Symbol,
    precipitation: Field,
    probability: Field,
}

/// A parsed forecast body.
#[derive(Debug, Clone)]
pub struct Forecast {
    entries: Vec<Entry>,
}

impl Forecast {
    /// Parse a response body. Errors where pyMetno's own parsing would.
    pub fn parse(body: &[u8]) -> Result<Self, serde_json::Error> {
        let body: Body = serde_json::from_slice(body)?;
        let entries = body
            .properties
            .timeseries
            .into_iter()
            .map(|e| Entry {
                time: e.time,
                data: e.data.and_then(|data| {
                    let instant = data.instant?.details?;
                    let block = [data.next_1_hours, data.next_6_hours, data.next_12_hours]
                        .into_iter()
                        .flatten()
                        .find(RawBlock::truthy);
                    let summary = block.as_ref().and_then(|b| b.summary).unwrap_or_default();
                    let details = block.as_ref().and_then(|b| b.details).unwrap_or_default();
                    Some(EntryData {
                        instant,
                        symbol: summary.symbol_code,
                        precipitation: details.precipitation_amount,
                        probability: details.probability_of_precipitation,
                    })
                }),
            })
            .collect();
        Ok(Self { entries })
    }
}

/// pyMetno's `CONDITIONS`: a met.no symbol (with or without `_day` /
/// `_night`) to HA's condition. HA's own `CONDITIONS_MAP` then maps every
/// one of these to itself.
pub fn condition(symbol: &str) -> Option<&'static str> {
    if symbol == "clearsky_night" {
        return Some("clear-night");
    }
    let base = symbol
        .strip_suffix("_day")
        .or_else(|| symbol.strip_suffix("_night"))
        .unwrap_or(symbol);
    Some(match base {
        "clearsky" => "sunny",
        "cloudy" => "cloudy",
        "fair" | "partlycloudy" => "partlycloudy",
        "fog" => "fog",
        "heavyrain" | "heavyrainshowers" | "lightrain" | "lightrainshowers" | "rain"
        | "rainshowers" => "rainy",
        "heavyrainandthunder"
        | "heavyrainshowersandthunder"
        | "heavysleetandthunder"
        | "heavysleetshowersandthunder"
        | "heavysnowandthunder"
        | "heavysnowshowersandthunder"
        | "lightrainandthunder"
        | "lightrainshowersandthunder"
        | "lightsleetandthunder"
        | "lightsnowandthunder"
        | "lightssleetshowersandthunder"
        | "lightssnowshowersandthunder"
        | "rainandthunder"
        | "rainshowersandthunder"
        | "sleetandthunder"
        | "sleetshowersandthunder"
        | "snowshowersandthunder" => "lightning-rainy",
        "heavysleet" | "heavysleetshowers" | "lightsleet" | "lightsleetshowers" | "sleet"
        | "sleetshowers" => "snowy-rainy",
        "heavysnow" | "heavysnowshowers" | "lightsnow" | "lightsnowshowers" | "snow"
        | "snowandthunder" | "snowshowers" => "snowy",
        _ => return None,
    })
}

/// What pyMetno's `get_data` looks up.
#[derive(Debug, Clone, Copy)]
enum Param {
    Temperature,
    Pressure,
    Humidity,
    DewPoint,
    UvIndex,
    WindSpeed,
    WindGust,
    WindBearing,
    Cloudiness,
    Precipitation,
    Probability,
}

/// pyMetno's `get_data`: the first entry that has the value. A missing
/// `data.instant.details` ends the whole lookup with nothing, as its
/// `KeyError` does; so does a value that isn't a number.
fn get(param: Param, entries: &[&Entry]) -> Option<Num> {
    for e in entries {
        let d = e.data?;
        let i = &d.instant;
        let field = match param {
            Param::Temperature => i.air_temperature,
            Param::Pressure => i.air_pressure_at_sea_level,
            Param::Humidity => i.relative_humidity,
            Param::DewPoint => i.dew_point_temperature,
            Param::UvIndex => i.ultraviolet_index_clear_sky,
            Param::WindSpeed => i.wind_speed,
            Param::WindGust => i.wind_speed_of_gust,
            Param::WindBearing => i.wind_from_direction,
            Param::Cloudiness => i.cloud_area_fraction,
            Param::Precipitation => d.precipitation,
            Param::Probability => d.probability,
        };
        return match field {
            Field::Absent => continue,
            Field::Null => None,
            // m/s to km/h, rounded to 0.1.
            Field::Num(n) if matches!(param, Param::WindSpeed | Param::WindGust) => {
                Some(Num::Float(round(n.f64() * 3.6, 1)))
            }
            Field::Num(n) => Some(n),
        };
    }
    None
}

/// `get_data("symbol_code", ...)` through `CONDITIONS`.
fn get_condition(entries: &[&Entry]) -> Option<&'static str> {
    for e in entries {
        match e.data?.symbol {
            Symbol::Absent => continue,
            Symbol::Found(c) => return c,
        }
    }
    None
}

/// One of pyMetno's weather dicts, in native units (°C, hPa, %, km/h, mm).
/// `templow` is only in daily items, `cloudiness` only in hourly ones.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub datetime: Timestamp,
    pub condition: Option<&'static str>,
    pub pressure: Option<Num>,
    pub humidity: Option<Num>,
    pub wind_bearing: Option<Num>,
    pub temperature: Option<Num>,
    pub templow: Option<Num>,
    pub precipitation: Option<Num>,
    pub precipitation_probability: Option<Num>,
    pub wind_speed: Option<Num>,
    pub wind_gust: Option<Num>,
    pub cloudiness: Option<Num>,
    pub dew_point: Option<Num>,
    pub uv_index: Option<Num>,
}

/// Python's `max`/`min`: the first extreme wins a tie.
fn extreme(values: &[Num], max: bool) -> Option<Num> {
    let mut it = values.iter().copied();
    let mut best = it.next()?;
    for v in it {
        if (max && best.lt(v)) || (!max && v.lt(best)) {
            best = v;
        }
    }
    Some(best)
}

/// The computed forecast: what HA's coordinator stores after a fetch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Computed {
    /// `None` when pyMetno's dict is empty (no entries).
    pub current: Option<Item>,
    pub daily: Vec<Item>,
    pub hourly: Vec<Item>,
}

impl Forecast {
    /// pyMetno's `get_weather(time, hourly)`. `bucket` is the zone days are
    /// split in: pyMetno's bare `astimezone()`, i.e. the process's zone.
    fn weather(&self, dates: &[Date], time: Timestamp, day: Date, hourly: bool) -> Option<Item> {
        let mut temps = Vec::new();
        let mut precips = Vec::new();
        let mut probs = Vec::new();
        let mut winds = Vec::new();
        let mut gusts = Vec::new();
        let mut dews = Vec::new();
        let mut uvs = Vec::new();
        let mut entries: Vec<&Entry> = Vec::new();
        for (e, date) in self.entries.iter().zip(dates) {
            if *date != day {
                continue;
            }
            let one = [e];
            for (param, list) in [
                (Param::Temperature, &mut temps),
                (Param::WindSpeed, &mut winds),
                (Param::WindGust, &mut gusts),
                (Param::Precipitation, &mut precips),
                (Param::Probability, &mut probs),
                (Param::DewPoint, &mut dews),
                (Param::UvIndex, &mut uvs),
            ] {
                if let Some(v) = get(param, &one) {
                    list.push(v);
                }
            }
            if time <= e.time {
                entries.push(e);
            }
        }
        if entries.is_empty() {
            return None;
        }
        let max = |v: &[Num]| extreme(v, true);
        let at = |p| get(p, &entries);
        let mut item = Item {
            datetime: time,
            condition: get_condition(&entries),
            pressure: at(Param::Pressure),
            humidity: at(Param::Humidity),
            wind_bearing: at(Param::WindBearing),
            temperature: None,
            templow: None,
            precipitation: None,
            precipitation_probability: None,
            wind_speed: None,
            wind_gust: None,
            cloudiness: None,
            dew_point: None,
            uv_index: None,
        };
        if hourly {
            item.temperature = at(Param::Temperature);
            item.precipitation = at(Param::Precipitation);
            item.precipitation_probability = at(Param::Probability);
            item.wind_speed = at(Param::WindSpeed);
            item.wind_gust = at(Param::WindGust);
            item.cloudiness = at(Param::Cloudiness);
            item.dew_point = at(Param::DewPoint);
            item.uv_index = at(Param::UvIndex);
        } else {
            item.temperature = max(&temps);
            item.templow = extreme(&temps, false);
            // `round(sum(...), 1)`.
            item.precipitation = (!precips.is_empty()).then(|| match py_sum(&precips) {
                Num::Int(i) => Num::Int(i),
                Num::Float(f) => Num::Float(round(f, 1)),
            });
            item.precipitation_probability = max(&probs);
            item.wind_speed = max(&winds);
            item.wind_gust = max(&gusts);
            item.dew_point = max(&dews);
            item.uv_index = max(&uvs);
        }
        Some(item)
    }

    /// What HA's coordinator works out after a fetch at `now`: the current
    /// conditions, 6 daily items at local noon from today and 48 hourly
    /// ones from the next whole hour. Times are built in `tz` (HA's zone),
    /// wall-clock, with Python's `fold=0` resolution; days are bucketed in
    /// `bucket` (the process's zone, as pyMetno's bare `astimezone()`).
    pub fn compute(&self, now: Timestamp, tz: &TimeZone, bucket: &TimeZone) -> Computed {
        let dates: Vec<Date> = self
            .entries
            .iter()
            .map(|e| e.time.to_zoned(bucket.clone()).date())
            .collect();
        let day_of = |t: Timestamp| t.to_zoned(bucket.clone()).date();
        let weather = |t: Timestamp, hourly| self.weather(&dates, t, day_of(t), hourly);
        let resolve = |dt: DateTime| tz.to_ambiguous_timestamp(dt).compatible().ok();

        // `get_current_weather`: at the first entry's time.
        let current = self.entries.first().map_or(now, |e| e.time);
        let current = weather(current, true);

        let local = now.to_zoned(tz.clone()).datetime();
        let noon = local.date().at(12, 0, 0, 0);
        let daily = (0..6)
            .filter_map(|k| noon.checked_add(k.days()).ok())
            .filter_map(resolve)
            .filter_map(|t| weather(t, false))
            .collect();
        let hour = local.date().at(local.hour(), 0, 0, 0);
        let hourly = (1..49)
            .filter_map(|k| hour.checked_add(SignedDuration::from_hours(k)).ok())
            .filter_map(resolve)
            .filter_map(|t| weather(t, true))
            .collect();
        Computed {
            current,
            daily,
            hourly,
        }
    }
}

/// The request's query: `lat`/`lon` rounded to 4 places as Python prints
/// them, and the altitude truncated to an int.
pub fn query(latitude: f64, longitude: f64, elevation: f64) -> String {
    let coord = |x: f64| crate::sun::py_float(round(x, 4));
    format!(
        "lat={}&lon={}&altitude={}",
        coord(latitude),
        coord(longitude),
        elevation.trunc() as i64
    )
}
