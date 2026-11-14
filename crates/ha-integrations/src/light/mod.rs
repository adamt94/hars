//! The `light` entity component (`components/light`): colour modes, the
//! attributes HA derives from a light's state, and `light.turn_on`,
//! `light.turn_off` and `light.toggle` with HA's parameter processing
//! (profiles, colour names, brightness steps, colour-space conversion),
//! handed to each targeted light of a platform (hue, lifx).

pub mod color;
mod profiles;

pub use profiles::Profiles;

use ha_core::{EntityId, EntityTarget, Hass, Kind, Range, Schema, ServiceError, SupportsResponse};
use ha_registry::Registries;
use parking_lot::RwLock;
use serde_json::{Map, Value, json};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub const DOMAIN: &str = "light";

/// `LightEntityFeature`.
pub const FEATURE_EFFECT: u32 = 4;
pub const FEATURE_FLASH: u32 = 8;
pub const FEATURE_TRANSITION: u32 = 32;

/// `ColorMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorMode {
    Onoff,
    Brightness,
    ColorTemp,
    Hs,
    Xy,
    Rgb,
    Rgbw,
    Rgbww,
    White,
}

impl ColorMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Onoff => "onoff",
            Self::Brightness => "brightness",
            Self::ColorTemp => "color_temp",
            Self::Hs => "hs",
            Self::Xy => "xy",
            Self::Rgb => "rgb",
            Self::Rgbw => "rgbw",
            Self::Rgbww => "rgbww",
            Self::White => "white",
        }
    }

    /// `COLOR_MODES_BRIGHTNESS`: every mode but `onoff`.
    fn has_brightness(self) -> bool {
        self != Self::Onoff
    }

    /// `COLOR_MODES_COLOR`.
    fn is_color(self) -> bool {
        matches!(
            self,
            Self::Hs | Self::Rgb | Self::Rgbw | Self::Rgbww | Self::Xy
        )
    }
}

/// A set of colour modes, kept sorted by name (HA's attribute order).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Modes(Vec<ColorMode>);

impl Modes {
    pub fn new(modes: impl IntoIterator<Item = ColorMode>) -> Self {
        let mut v: Vec<ColorMode> = Vec::new();
        for m in modes {
            if !v.contains(&m) {
                v.push(m);
            }
        }
        v.sort_by_key(|m| m.as_str());
        Self(v)
    }

    /// `filter_supported_color_modes`: `onoff`, then `brightness`, are
    /// dropped when there is anything else.
    pub fn filtered(modes: impl IntoIterator<Item = ColorMode>) -> Self {
        let mut m = Self::new(modes);
        for drop in [ColorMode::Onoff, ColorMode::Brightness] {
            if m.0.len() > 1 {
                m.0.retain(|x| *x != drop);
            }
        }
        m
    }

    pub fn contains(&self, mode: ColorMode) -> bool {
        self.0.contains(&mode)
    }

    pub fn iter(&self) -> impl Iterator<Item = ColorMode> + '_ {
        self.0.iter().copied()
    }

    /// The only mode, if there's one.
    pub fn single(&self) -> Option<ColorMode> {
        match self.0.as_slice() {
            [m] => Some(*m),
            _ => None,
        }
    }

    fn brightness(&self) -> bool {
        self.iter().any(ColorMode::has_brightness)
    }

    fn color(&self) -> bool {
        self.iter().any(ColorMode::is_color)
    }

    fn color_temp(&self) -> bool {
        self.contains(ColorMode::ColorTemp)
    }
}

/// What a light can do, fixed when the entity is made.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Capabilities {
    pub modes: Modes,
    pub features: u32,
    /// `effect_list`, shown only with the EFFECT feature.
    pub effect_list: Option<Vec<String>>,
}

/// A light's state as its platform reports it at one moment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LightState {
    pub on: bool,
    /// The mode it's in (ignored while off).
    pub color_mode: Option<ColorMode>,
    pub brightness: Option<i64>,
    pub color_temp_kelvin: Option<i64>,
    pub min_color_temp_kelvin: i64,
    pub max_color_temp_kelvin: i64,
    pub hs_color: Option<(f64, f64)>,
    pub xy_color: Option<(f64, f64)>,
    pub rgb_color: Option<[f64; 3]>,
    pub rgbw_color: Option<[f64; 4]>,
    pub rgbww_color: Option<[f64; 5]>,
    pub effect: Option<String>,
}

fn pair(p: (f64, f64)) -> Value {
    json!([p.0, p.1])
}

fn ints<const N: usize>(c: [i64; N]) -> Value {
    Value::Array(c.iter().map(|v| json!(v)).collect())
}

/// `_light_internal_convert_color`: hs, rgb and xy for the current mode.
fn convert_color(mode: ColorMode, s: &LightState, out: &mut Map<String, Value>) {
    use color::*;
    let r3 = |p: (f64, f64)| (crate::num::round(p.0, 3), crate::num::round(p.1, 3));
    let rgb_ints = |c: [f64; 3]| ints(c.map(|v| v as i64));
    let rgb_u8 = |c: [u8; 3]| ints(c.map(i64::from));
    match mode {
        ColorMode::Hs => {
            if let Some(hs) = s.hs_color {
                out.insert("hs_color".into(), pair(r3(hs)));
                out.insert("rgb_color".into(), rgb_u8(hs_to_rgb(hs.0, hs.1)));
                out.insert("xy_color".into(), pair(hs_to_xy(hs.0, hs.1)));
            }
        }
        ColorMode::Xy => {
            if let Some((x, y)) = s.xy_color {
                out.insert("hs_color".into(), pair(xy_to_hs(x, y)));
                out.insert("rgb_color".into(), rgb_u8(xy_to_rgb(x, y)));
                let r6 = |v| crate::num::round(v, 6);
                out.insert("xy_color".into(), pair((r6(x), r6(y))));
            }
        }
        ColorMode::Rgb => {
            if let Some(c) = s.rgb_color {
                out.insert("hs_color".into(), pair(rgb_to_hs(c[0], c[1], c[2])));
                out.insert("rgb_color".into(), rgb_ints(c));
                out.insert("xy_color".into(), pair(rgb_to_xy(c[0], c[1], c[2])));
            }
        }
        ColorMode::Rgbw => {
            if let Some(c) = s.rgbw_color {
                let rgb = rgbw_to_rgb(c[0], c[1], c[2], c[3]).map(|v| v as f64);
                out.insert("hs_color".into(), pair(rgb_to_hs(rgb[0], rgb[1], rgb[2])));
                out.insert("rgb_color".into(), rgb_ints(rgb));
                out.insert("rgbw_color".into(), ints(c.map(|v| v as i64)));
                out.insert("xy_color".into(), pair(rgb_to_xy(rgb[0], rgb[1], rgb[2])));
            }
        }
        ColorMode::Rgbww => {
            if let Some(c) = s.rgbww_color {
                let (lo, hi) = (
                    s.min_color_temp_kelvin as f64,
                    s.max_color_temp_kelvin as f64,
                );
                let rgb = rgbww_to_rgb(c[0], c[1], c[2], c[3], c[4], lo, hi).map(|v| v as f64);
                out.insert("hs_color".into(), pair(rgb_to_hs(rgb[0], rgb[1], rgb[2])));
                out.insert("rgb_color".into(), rgb_ints(rgb));
                out.insert("rgbww_color".into(), ints(c.map(|v| v as i64)));
                out.insert("xy_color".into(), pair(rgb_to_xy(rgb[0], rgb[1], rgb[2])));
            }
        }
        ColorMode::ColorTemp => {
            if let Some(k) = s.color_temp_kelvin.filter(|k| *k != 0) {
                let hs = temperature_to_hs(k as f64);
                out.insert("hs_color".into(), pair(r3(hs)));
                out.insert("rgb_color".into(), rgb_u8(hs_to_rgb(hs.0, hs.1)));
                out.insert("xy_color".into(), pair(hs_to_xy(hs.0, hs.1)));
            }
        }
        ColorMode::Onoff | ColorMode::Brightness | ColorMode::White => {}
    }
}

/// HA's `capability_attributes` then `state_attributes` for a light. When
/// unavailable only the capability attributes are given. The platform adds
/// its extra attributes, `friendly_name` and `supported_features`.
pub fn attributes(caps: &Capabilities, s: &LightState, available: bool) -> Map<String, Value> {
    let mut a = Map::new();
    let modes = &caps.modes;
    if modes.color_temp() {
        a.insert(
            "min_color_temp_kelvin".into(),
            s.min_color_temp_kelvin.into(),
        );
        a.insert(
            "max_color_temp_kelvin".into(),
            s.max_color_temp_kelvin.into(),
        );
    }
    if caps.features & FEATURE_EFFECT != 0 {
        a.insert(
            "effect_list".into(),
            caps.effect_list.as_ref().map_or(Value::Null, |l| json!(l)),
        );
    }
    a.insert(
        "supported_color_modes".into(),
        Value::Array(modes.iter().map(|m| m.as_str().into()).collect()),
    );
    if !available {
        return a;
    }
    let mode = if s.on { s.color_mode } else { None };
    if caps.features & FEATURE_EFFECT != 0 {
        let effect = if s.on { s.effect.clone() } else { None };
        a.insert("effect".into(), effect.map_or(Value::Null, Value::String));
    }
    a.insert(
        "color_mode".into(),
        mode.map_or(Value::Null, |m| m.as_str().into()),
    );
    if modes.brightness() {
        let b = mode
            .filter(|m| m.has_brightness())
            .and(s.brightness)
            .map_or(Value::Null, Value::from);
        a.insert("brightness".into(), b);
    }
    if modes.color_temp() {
        let k = (mode == Some(ColorMode::ColorTemp))
            .then_some(s.color_temp_kelvin)
            .flatten()
            .map_or(Value::Null, Value::from);
        a.insert("color_temp_kelvin".into(), k);
    }
    if modes.color() || modes.color_temp() {
        for key in ["hs_color", "rgb_color", "xy_color"] {
            a.insert(key.into(), Value::Null);
        }
        if modes.contains(ColorMode::Rgbw) {
            a.insert("rgbw_color".into(), Value::Null);
        }
        if modes.contains(ColorMode::Rgbww) {
            a.insert("rgbww_color".into(), Value::Null);
        }
        if let Some(m) = mode {
            convert_color(m, s, &mut a);
        }
    }
    a
}

/// `white`: `True` (the light's brightness) or a level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum White {
    True,
    Level(Option<i64>),
}

/// `flash`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flash {
    Short,
    Long,
}

/// `light.turn_on`'s parameters: as given, then as processed for one
/// light (what reaches its `turn_on`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Params {
    pub profile: Option<String>,
    pub color_name: Option<String>,
    pub brightness_pct: Option<f64>,
    pub brightness_step: Option<i64>,
    pub brightness_step_pct: Option<f64>,
    pub transition: Option<f64>,
    pub brightness: Option<i64>,
    pub color_temp_kelvin: Option<i64>,
    pub hs_color: Option<(f64, f64)>,
    pub rgb_color: Option<[i64; 3]>,
    pub rgbw_color: Option<[i64; 4]>,
    pub rgbww_color: Option<[i64; 5]>,
    pub xy_color: Option<(f64, f64)>,
    pub white: Option<White>,
    pub flash: Option<Flash>,
    pub effect: Option<String>,
}

impl Params {
    /// From validated service data.
    pub fn from_data(d: &Map<String, Value>) -> Self {
        let s = |k: &str| d.get(k).and_then(Value::as_str).map(str::to_owned);
        let f = |k: &str| d.get(k).and_then(Value::as_f64);
        let i = |k: &str| d.get(k).and_then(Value::as_i64);
        let arr = |k: &str| d.get(k).and_then(Value::as_array);
        let pairf =
            |k: &str| arr(k).and_then(|a| Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?)));
        let ints3 = |k: &str| {
            let a = arr(k)?;
            Some([
                a.first()?.as_i64()?,
                a.get(1)?.as_i64()?,
                a.get(2)?.as_i64()?,
            ])
        };
        let ints4 = |k: &str| {
            let a = arr(k)?;
            let v: Option<Vec<i64>> = a.iter().map(Value::as_i64).collect();
            <[i64; 4]>::try_from(v?).ok()
        };
        let ints5 = |k: &str| {
            let a = arr(k)?;
            let v: Option<Vec<i64>> = a.iter().map(Value::as_i64).collect();
            <[i64; 5]>::try_from(v?).ok()
        };
        Self {
            profile: s("profile"),
            color_name: s("color_name"),
            brightness_pct: f("brightness_pct"),
            brightness_step: i("brightness_step"),
            brightness_step_pct: f("brightness_step_pct"),
            transition: f("transition"),
            brightness: i("brightness"),
            color_temp_kelvin: i("color_temp_kelvin"),
            hs_color: pairf("hs_color"),
            rgb_color: ints3("rgb_color"),
            rgbw_color: ints4("rgbw_color"),
            rgbww_color: ints5("rgbww_color"),
            xy_color: pairf("xy_color"),
            white: match d.get("white") {
                Some(Value::Bool(true)) => Some(White::True),
                Some(v) => Some(White::Level(v.as_i64())),
                None => None,
            },
            flash: match s("flash").as_deref() {
                Some("short") => Some(Flash::Short),
                Some("long") => Some(Flash::Long),
                _ => None,
            },
            effect: s("effect"),
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Any colour descriptor (what a profile's colour gives way to).
    fn has_color(&self) -> bool {
        self.color_name.is_some()
            || self.color_temp_kelvin.is_some()
            || self.hs_color.is_some()
            || self.rgb_color.is_some()
            || self.rgbw_color.is_some()
            || self.rgbww_color.is_some()
            || self.xy_color.is_some()
            || self.white.is_some()
    }
}

/// `preprocess_turn_on_alternatives`.
pub fn preprocess(p: &mut Params, profiles: &Profiles) {
    if p.brightness_step.is_some() || p.brightness_step_pct.is_some() {
        return;
    }
    if let Some(name) = p.profile.take() {
        profiles.apply(&name, p);
    }
    if let Some(name) = p.color_name.take() {
        let rgb = color::color_name_to_rgb(&name).unwrap_or_else(|| {
            tracing::warn!("Got unknown color {name}, falling back to white");
            [255, 255, 255]
        });
        p.rgb_color = Some(rgb.map(i64::from));
    }
    if let Some(pct) = p.brightness_pct.take() {
        p.brightness = Some((255.0 * pct / 100.0).round_ties_even() as i64);
    }
}

/// A conversion that would fail in HA (a `ZeroDivisionError`).
#[derive(Debug, PartialEq, Eq)]
pub struct ConversionError;

fn xy_temp(xy: (f64, f64)) -> Result<i64, ConversionError> {
    color::xy_to_temperature(xy.0, xy.1).ok_or(ConversionError)
}

/// `process_turn_on_params` for one light.
pub fn process_turn_on(
    params: &Params,
    entity_id: &str,
    caps: &Capabilities,
    s: &LightState,
    profiles: &Profiles,
) -> Result<Params, ConversionError> {
    use color::*;
    let mut p = params.clone();
    if !p.is_empty() && (p.brightness_step.is_some() || p.brightness_step_pct.is_some()) {
        let base = if s.on {
            s.brightness.filter(|b| *b != 0).unwrap_or(0)
        } else {
            0
        };
        let b = if let Some(step) = p.brightness_step.take() {
            base + step
        } else {
            let step_pct = p.brightness_step_pct.take().unwrap_or(0.0);
            let pct = (base as f64 / 255.0 * 100.0).round_ties_even();
            ((pct + step_pct) / 100.0 * 255.0).round_ties_even() as i64
        };
        p.brightness = Some(b.clamp(0, 255));
        preprocess(&mut p, profiles);
    }
    if (p.is_empty() || !s.on) || p.transition.is_none() {
        profiles.apply_default(entity_id, s.on, &mut p);
    }
    let modes = &caps.modes;
    let (lo, hi) = (
        s.min_color_temp_kelvin as f64,
        s.max_color_temp_kelvin as f64,
    );
    if let Some(k) = p.color_temp_kelvin {
        if !modes.color_temp() && modes.contains(ColorMode::Rgbww) {
            p.color_temp_kelvin = None;
            let b = p.brightness.or(s.brightness).unwrap_or(0);
            p.rgbww_color = Some(temperature_to_rgbww(k as f64, b as f64, lo, hi));
        } else if !modes.color_temp() {
            p.color_temp_kelvin = None;
            if modes.color() {
                p.hs_color = Some(temperature_to_hs(k as f64));
            }
        }
    }
    let f3 = |c: [i64; 3]| c.map(|v| v as f64);
    if let Some(hs) = p.hs_color.filter(|_| !modes.contains(ColorMode::Hs)) {
        p.hs_color = None;
        let rgb = hs_to_rgb(hs.0, hs.1).map(f64::from);
        if modes.contains(ColorMode::Rgb) {
            p.rgb_color = Some(rgb.map(|v| v as i64));
        } else if modes.contains(ColorMode::Rgbw) {
            p.rgbw_color = Some(rgb_to_rgbw(rgb[0], rgb[1], rgb[2]));
        } else if modes.contains(ColorMode::Rgbww) {
            p.rgbww_color = Some(rgb_to_rgbww(rgb[0], rgb[1], rgb[2], lo, hi));
        } else if modes.contains(ColorMode::Xy) {
            p.xy_color = Some(hs_to_xy(hs.0, hs.1));
        } else if modes.color_temp() {
            p.color_temp_kelvin = Some(xy_temp(hs_to_xy(hs.0, hs.1))?);
        }
    } else if let Some(rgb) = p.rgb_color.filter(|_| !modes.contains(ColorMode::Rgb)) {
        p.rgb_color = None;
        let c = f3(rgb);
        if modes.contains(ColorMode::Rgbw) {
            p.rgbw_color = Some(rgb_to_rgbw(c[0], c[1], c[2]));
        } else if modes.contains(ColorMode::Rgbww) {
            p.rgbww_color = Some(rgb_to_rgbww(c[0], c[1], c[2], lo, hi));
        } else if modes.contains(ColorMode::Hs) {
            p.hs_color = Some(rgb_to_hs(c[0], c[1], c[2]));
        } else if modes.contains(ColorMode::Xy) {
            p.xy_color = Some(rgb_to_xy(c[0], c[1], c[2]));
        } else if modes.color_temp() {
            p.color_temp_kelvin = Some(xy_temp(rgb_to_xy(c[0], c[1], c[2]))?);
        }
    } else if let Some(xy) = p.xy_color.filter(|_| !modes.contains(ColorMode::Xy)) {
        p.xy_color = None;
        let rgb = xy_to_rgb(xy.0, xy.1).map(f64::from);
        if modes.contains(ColorMode::Hs) {
            p.hs_color = Some(xy_to_hs(xy.0, xy.1));
        } else if modes.contains(ColorMode::Rgb) {
            p.rgb_color = Some(rgb.map(|v| v as i64));
        } else if modes.contains(ColorMode::Rgbw) {
            p.rgbw_color = Some(rgb_to_rgbw(rgb[0], rgb[1], rgb[2]));
        } else if modes.contains(ColorMode::Rgbww) {
            p.rgbww_color = Some(rgb_to_rgbww(rgb[0], rgb[1], rgb[2], lo, hi));
        } else if modes.color_temp() {
            p.color_temp_kelvin = Some(xy_temp(xy)?);
        }
    } else if let Some(c) = p.rgbw_color.filter(|_| !modes.contains(ColorMode::Rgbw)) {
        p.rgbw_color = None;
        let rgb = rgbw_to_rgb(c[0] as f64, c[1] as f64, c[2] as f64, c[3] as f64);
        let f = f3(rgb);
        if modes.contains(ColorMode::Rgb) {
            p.rgb_color = Some(rgb);
        } else if modes.contains(ColorMode::Rgbww) {
            p.rgbww_color = Some(rgb_to_rgbww(f[0], f[1], f[2], lo, hi));
        } else if modes.contains(ColorMode::Hs) {
            p.hs_color = Some(rgb_to_hs(f[0], f[1], f[2]));
        } else if modes.contains(ColorMode::Xy) {
            p.xy_color = Some(rgb_to_xy(f[0], f[1], f[2]));
        } else if modes.color_temp() {
            p.color_temp_kelvin = Some(xy_temp(rgb_to_xy(f[0], f[1], f[2]))?);
        }
    } else if let Some(c) = p.rgbww_color.filter(|_| !modes.contains(ColorMode::Rgbww)) {
        p.rgbww_color = None;
        let c = c.map(|v| v as f64);
        let rgb = rgbww_to_rgb(c[0], c[1], c[2], c[3], c[4], lo, hi);
        let f = f3(rgb);
        if modes.contains(ColorMode::Rgb) {
            p.rgb_color = Some(rgb);
        } else if modes.contains(ColorMode::Rgbw) {
            p.rgbw_color = Some(rgb_to_rgbw(f[0], f[1], f[2]));
        } else if modes.contains(ColorMode::Hs) {
            p.hs_color = Some(rgb_to_hs(f[0], f[1], f[2]));
        } else if modes.contains(ColorMode::Xy) {
            p.xy_color = Some(rgb_to_xy(f[0], f[1], f[2]));
        } else if modes.color_temp() {
            p.color_temp_kelvin = Some(xy_temp(rgb_to_xy(f[0], f[1], f[2]))?);
        }
    }
    if p.white == Some(White::True) {
        p.white = Some(White::Level(s.brightness));
    }
    if p.white.is_some() && modes.contains(ColorMode::White) {
        if let Some(b) = p.brightness.take() {
            p.white = Some(White::Level(Some(b)));
        }
    }
    Ok(p)
}

/// Whether processed parameters turn the light off (brightness or white 0).
pub fn is_off(p: &Params) -> bool {
    p.brightness == Some(0) || p.white == Some(White::Level(Some(0)))
}

/// `filter_turn_on_params`.
pub fn filter_turn_on(mut p: Params, caps: &Capabilities) -> Params {
    let modes = &caps.modes;
    if caps.features & FEATURE_EFFECT == 0 {
        p.effect = None;
    }
    if caps.features & FEATURE_FLASH == 0 {
        p.flash = None;
    }
    if caps.features & FEATURE_TRANSITION == 0 {
        p.transition = None;
    }
    if !modes.brightness() {
        p.brightness = None;
    }
    if !modes.color_temp() {
        p.color_temp_kelvin = None;
    }
    if !modes.contains(ColorMode::Hs) {
        p.hs_color = None;
    }
    if !modes.contains(ColorMode::Rgb) {
        p.rgb_color = None;
    }
    if !modes.contains(ColorMode::Rgbw) {
        p.rgbw_color = None;
    }
    if !modes.contains(ColorMode::Rgbww) {
        p.rgbww_color = None;
    }
    if !modes.contains(ColorMode::White) {
        p.white = None;
    }
    if !modes.contains(ColorMode::Xy) {
        p.xy_color = None;
    }
    p
}

/// What reaches a light's `turn_off`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TurnOff {
    pub transition: Option<f64>,
    pub flash: Option<Flash>,
}

/// `process_turn_off_params` then `filter_turn_off_params`.
pub fn turn_off_params(
    params: &Params,
    entity_id: &str,
    caps: &Capabilities,
    profiles: &Profiles,
) -> TurnOff {
    let mut p = params.clone();
    if p.transition.is_none() {
        profiles.apply_default(entity_id, true, &mut p);
    }
    TurnOff {
        transition: p
            .transition
            .filter(|_| caps.features & FEATURE_TRANSITION != 0),
        flash: p.flash.filter(|_| caps.features & FEATURE_FLASH != 0),
    }
}

type BoxFuture<'a> = Pin<Box<dyn Future<Output = Result<(), ServiceError>> + Send + 'a>>;

/// A light a platform runs.
pub trait Light: Send + Sync {
    fn entity_id(&self) -> &EntityId;
    fn capabilities(&self) -> &Capabilities;
    /// The state now (what HA's entity properties would return).
    fn state(&self) -> LightState;
    fn available(&self) -> bool;
    /// `async_turn_on` with processed, filtered parameters.
    fn turn_on(&self, params: Params) -> BoxFuture<'_>;
    fn turn_off(&self, params: TurnOff) -> BoxFuture<'_>;
}

/// The running lights, for the services.
#[derive(Default)]
pub struct Lights {
    lights: RwLock<Vec<Arc<dyn Light>>>,
    profiles: Profiles,
}

/// Which service is being handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Service {
    TurnOn,
    TurnOff,
    Toggle,
}

impl Lights {
    pub fn new(profiles: Profiles) -> Self {
        Self {
            lights: RwLock::default(),
            profiles,
        }
    }

    pub fn add(&self, light: Arc<dyn Light>) {
        self.lights.write().push(light);
    }

    /// The targeted, available lights, as HA's entity services pick them.
    fn targets(&self, target: &EntityTarget, indirect: &[String]) -> Vec<Arc<dyn Light>> {
        let lights = self.lights.read();
        lights
            .iter()
            .filter(|l| match target {
                EntityTarget::All => true,
                EntityTarget::Ids(ids) => {
                    ids.contains(l.entity_id())
                        || indirect.iter().any(|i| i == l.entity_id().as_str())
                }
            })
            .filter(|l| l.available())
            .cloned()
            .collect()
    }

    /// One light's part of a call.
    async fn handle(
        &self,
        light: &dyn Light,
        service: Service,
        params: &Params,
    ) -> Result<(), ServiceError> {
        let id = light.entity_id().as_str();
        let caps = light.capabilities();
        let off = |p: &Params| turn_off_params(p, id, caps, &self.profiles);
        let state = light.state();
        let invalid = |_| ServiceError::Failed("float division by zero".into());
        match service {
            Service::TurnOff => light.turn_off(off(params)).await,
            Service::TurnOn => {
                let p =
                    process_turn_on(params, id, caps, &state, &self.profiles).map_err(invalid)?;
                if is_off(&p) {
                    light.turn_off(off(params)).await
                } else {
                    light.turn_on(filter_turn_on(p, caps)).await
                }
            }
            // `async_toggle`: off lights turn on with the data; on lights
            // turn off, ignoring it.
            Service::Toggle => {
                if !state.on {
                    let p = process_turn_on(params, id, caps, &state, &self.profiles)
                        .map_err(invalid)?;
                    if !is_off(&p) {
                        return light.turn_on(filter_turn_on(p, caps)).await;
                    }
                }
                light.turn_off(off(params)).await
            }
        }
    }

    /// Register `light.turn_on`, `light.turn_off` and `light.toggle`.
    pub fn register_services(self: &Arc<Self>, hass: &Hass, registries: Arc<Registries>) {
        for (name, service) in [
            ("turn_on", Service::TurnOn),
            ("turn_off", Service::TurnOff),
            ("toggle", Service::Toggle),
        ] {
            let schema = if service == Service::TurnOff {
                turn_off_schema()
            } else {
                turn_on_schema()
            };
            let lights = self.clone();
            let registries = registries.clone();
            hass.services().register_with_schema(
                DOMAIN,
                name,
                Some(schema),
                SupportsResponse::None,
                move |call| {
                    let lights = lights.clone();
                    let registries = registries.clone();
                    async move {
                        let target = call.entity_ids()?;
                        let indirect = registries.target_entities(&call.data);
                        let mut params = Params::from_data(&call.data);
                        preprocess(&mut params, &lights.profiles);
                        let targets = lights.targets(&target, &indirect);
                        // Concurrently, as HA gathers entity calls; the
                        // first error is the call's.
                        let calls = targets
                            .iter()
                            .map(|l| lights.handle(l.as_ref(), service, &params));
                        let results = futures_join_all(calls).await;
                        results.into_iter().collect::<Result<Vec<()>, _>>()?;
                        Ok(None)
                    }
                },
            );
        }
    }
}

/// Run futures concurrently on this task and collect their results.
async fn futures_join_all<F: Future>(futures: impl IntoIterator<Item = F>) -> Vec<F::Output> {
    let mut pending: Vec<Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut out: Vec<Option<F::Output>> = (0..pending.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut done = true;
        for (i, f) in pending.iter_mut().enumerate() {
            if out[i].is_none() {
                match f.as_mut().poll(cx) {
                    std::task::Poll::Ready(v) => out[i] = Some(v),
                    std::task::Poll::Pending => done = false,
                }
            }
        }
        if done {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
    out.into_iter().map(|v| v.expect("all ready")).collect()
}

const COLOR_GROUP: &str = "Color descriptors";

/// `LIGHT_TURN_ON_SCHEMA` as an entity-service schema.
pub fn turn_on_schema() -> Schema {
    let byte = || Kind::Int(Range::new(0.0, 255.0));
    let small_float = || Kind::Float(Range::new(0.0, 1.0));
    Schema::entity_service()
        .optional("profile", Kind::String)
        .optional("transition", Kind::FloatClamp(0.0, 6553.0))
        .optional("brightness", Kind::IntClamp(0, 255))
        .optional("brightness_pct", Kind::Float(Range::new(0.0, 100.0)))
        .optional("brightness_step", Kind::IntClamp(-255, 255))
        .optional("brightness_step_pct", Kind::FloatClamp(-100.0, 100.0))
        .optional("color_name", Kind::String)
        .optional(
            "color_temp_kelvin",
            Kind::Int(Range {
                min: Some(0.0),
                max: None,
            }),
        )
        .optional(
            "hs_color",
            Kind::Tuple(vec![
                Kind::Float(Range::new(0.0, 360.0)),
                Kind::Float(Range::new(0.0, 100.0)),
            ]),
        )
        .optional("rgb_color", Kind::Tuple(vec![byte(); 3]))
        .optional("rgbw_color", Kind::Tuple(vec![byte(); 4]))
        .optional("rgbww_color", Kind::Tuple(vec![byte(); 5]))
        .optional("xy_color", Kind::Tuple(vec![small_float(); 2]))
        .optional("white", Kind::TrueOr(Box::new(Kind::IntClamp(0, 255))))
        .optional("flash", Kind::OneOf(&["short", "long"]))
        .optional("effect", Kind::String)
        .exclusive(
            "brightness",
            &[
                "brightness",
                "brightness_pct",
                "brightness_step",
                "brightness_step_pct",
            ],
        )
        .exclusive(
            COLOR_GROUP,
            &[
                "profile",
                "color_name",
                "color_temp_kelvin",
                "hs_color",
                "rgb_color",
                "rgbw_color",
                "rgbww_color",
                "xy_color",
                "white",
            ],
        )
}

/// `LIGHT_TURN_OFF_SCHEMA` as an entity-service schema.
pub fn turn_off_schema() -> Schema {
    Schema::entity_service()
        .optional("transition", Kind::FloatClamp(0.0, 6553.0))
        .optional("flash", Kind::OneOf(&["short", "long"]))
}

#[cfg(test)]
mod tests;
