//! A single-zone bulb's light: HA's `LIFXColor` or `LIFXWhite`, its state
//! from the bulb's HSBK and power, and `find_hsbk` from a call's
//! parameters.

use super::products::{COLOR, Product};
use crate::light::{Capabilities, ColorMode, LightState, Modes, Params, color};

/// `TRANSITION | EFFECT`.
pub const FEATURES: u32 = 36;
const COLOUR_EFFECTS: &[&str] = &["effect_colorloop", "effect_pulse", "effect_stop"];
const WHITE_EFFECTS: &[&str] = &["effect_pulse", "effect_stop"];

/// What HA's light makes of a product (`lifx_features`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    /// `LIFXColor` (else `LIFXWhite`).
    pub color: bool,
    pub min_kelvin: i64,
    pub max_kelvin: i64,
}

impl Model {
    /// The model of a product, from its features.
    pub fn of(product: &Product) -> Self {
        let (min, max) = product.kelvin.unwrap_or((0, 0));
        Self {
            color: product.flags & COLOR != 0,
            min_kelvin: i64::from(min),
            max_kelvin: i64::from(max),
        }
    }

    /// A white light's one mode: colour temperature, or only brightness
    /// for a bulb with one temperature.
    fn white_mode(self) -> ColorMode {
        match self.min_kelvin != self.max_kelvin {
            true => ColorMode::ColorTemp,
            false => ColorMode::Brightness,
        }
    }
}

pub fn capabilities(model: Model) -> Capabilities {
    let (modes, effects) = match model.color {
        true => (vec![ColorMode::ColorTemp, ColorMode::Hs], COLOUR_EFFECTS),
        false => (vec![model.white_mode()], WHITE_EFFECTS),
    };
    Capabilities {
        modes: Modes::new(modes),
        features: FEATURES,
        effect_list: Some(effects.iter().map(|e| (*e).to_owned()).collect()),
    }
}

/// HA's entity properties for a bulb's `power` and `[H, S, B, K]`.
pub fn state(model: Model, power: u16, c: [u16; 4], effect: Option<&str>) -> LightState {
    let [h, s, b, k] = c;
    // `(int(P / 65535 * B)) >> 8`, in Python's float order.
    let brightness = ((power as f64 / 65535.0 * b as f64) as i64) >> 8;
    let colour = model.color && s != 0;
    LightState {
        on: power != 0,
        color_mode: Some(match (model.color, colour) {
            (true, true) => ColorMode::Hs,
            (true, false) => ColorMode::ColorTemp,
            (false, _) => model.white_mode(),
        }),
        brightness: Some(brightness),
        color_temp_kelvin: Some(k as i64),
        min_color_temp_kelvin: model.min_kelvin,
        max_color_temp_kelvin: model.max_kelvin,
        hs_color: colour.then(|| (h as f64 / 65535.0 * 360.0, s as f64 / 65535.0 * 100.0)),
        effect: effect.map(str::to_owned),
        ..Default::default()
    }
}

/// `[H, S, B, K]`, each `None` where unchanged.
pub type Hsbk = [Option<u16>; 4];

/// Python's `int(x)` into a 16-bit field (the bulb's own field width;
/// aiolifx packs with `struct`, which refuses out-of-range values).
fn u16_of(x: f64) -> u16 {
    x.trunc().clamp(0.0, 65535.0) as u16
}

/// `find_hsbk`, from the parameters `set_state` gets.
pub fn find_hsbk(p: &Params) -> Option<Hsbk> {
    let mut hs: Option<(f64, f64)> = None;
    if let Some(name) = &p.color_name {
        hs = Some(match color::color_name_to_rgb(name) {
            Some([r, g, b]) => color::rgb_to_hs(r as f64, g as f64, b as f64),
            None => {
                tracing::warn!("Got unknown color {name}, falling back to neutral white");
                (0.0, 0.0)
            }
        });
    }
    if let Some(c) = p.hs_color {
        hs = Some(c);
    } else if let Some([r, g, b]) = p.rgb_color {
        hs = Some(color::rgb_to_hs(r as f64, g as f64, b as f64));
    } else if let Some((x, y)) = p.xy_color {
        hs = color::xy_to_hs(x, y).ok();
    }
    let (mut h, mut s, mut b, mut k) = (None, None, None, None);
    if let Some((hue, sat)) = hs {
        h = Some(u16_of(hue / 360.0 * 65535.0));
        s = Some(u16_of(sat / 100.0 * 65535.0));
        k = Some(3500);
    }
    if let Some(kelvin) = p.color_temp_kelvin {
        k = Some(kelvin.clamp(0, 65535) as u16);
        s = Some(0);
    }
    if let Some(v) = p.brightness {
        b = Some(to_16(v));
    }
    if let Some(pct) = p.brightness_pct {
        b = Some(to_16(crate::num::round_int(255.0 * pct / 100.0)));
    }
    let hsbk = [h, s, b, k];
    (hsbk != [None; 4]).then_some(hsbk)
}

/// `convert_8_to_16`.
pub fn to_16(v: i64) -> u16 {
    let v = v.clamp(0, 255) as u16;
    (v << 8) | v
}

/// `merge_hsbk`: the change over the base, where given.
pub fn merge(base: [u16; 4], change: Hsbk) -> [u16; 4] {
    std::array::from_fn(|i| change[i].unwrap_or(base[i]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_hsbk_as_ha() {
        let p = |f: fn(&mut Params)| {
            let mut p = Params::default();
            f(&mut p);
            find_hsbk(&p)
        };
        assert_eq!(
            p(|p| p.hs_color = Some((120.0, 50.0))),
            Some([Some(21845), Some(32767), None, Some(3500)])
        );
        assert_eq!(
            p(|p| p.hs_color = Some((200.0, 0.0))),
            Some([Some(36408), Some(0), None, Some(3500)])
        );
        assert_eq!(
            p(|p| p.color_temp_kelvin = Some(4000)),
            Some([None, Some(0), None, Some(4000)])
        );
        assert_eq!(
            p(|p| p.brightness_pct = Some(50.0)),
            Some([None, None, Some(32896), None])
        );
        assert_eq!(
            p(|p| p.brightness = Some(128)),
            Some([None, None, Some(32896), None])
        );
        assert_eq!(p(|_| {}), None);
    }

    const COLOUR_1000: Model = Model {
        color: true,
        min_kelvin: 2500,
        max_kelvin: 9000,
    };

    #[test]
    fn state_as_ha() {
        let state = |p, c, e| state(COLOUR_1000, p, c, e);
        let s = state(65535, [0, 0, 39321, 2700], None);
        assert_eq!((s.on, s.brightness), (true, Some(153)));
        assert_eq!(s.color_mode, Some(ColorMode::ColorTemp));
        assert_eq!(
            state(32768, [0, 0, 65535, 2700], None).brightness,
            Some(128)
        );
        let s = state(1, [0, 0, 65535, 2700], None);
        assert_eq!((s.on, s.brightness), (true, Some(0)));
        let s = state(65535, [65535, 1, 65535, 3500], None);
        assert_eq!(s.color_mode, Some(ColorMode::Hs));
        assert_eq!(s.hs_color.map(|h| h.0), Some(360.0));
    }
}
