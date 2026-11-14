//! HA's `util/color.py`, the parts lights use, value for value: the same
//! formulas, constants, rounding (Python's) and truncation.

use crate::num::round;

include!("colors.rs");

/// `color_name_to_rgb`: spaces removed, lower-cased; `None` if unknown.
pub fn color_name_to_rgb(name: &str) -> Option<[u8; 3]> {
    let key: String = name
        .chars()
        .filter(|c| *c != ' ')
        .flat_map(char::to_lowercase)
        .collect();
    COLORS
        .binary_search_by(|(n, _)| (*n).cmp(key.as_str()))
        .ok()
        .map(|i| COLORS[i].1)
}

/// `color_RGB_to_xy`, without a gamut: `(round(x, 3), round(y, 3))`.
pub fn rgb_to_xy(r: f64, g: f64, b: f64) -> (f64, f64) {
    if r + g + b == 0.0 {
        return (0.0, 0.0);
    }
    let lin = |c: f64| {
        let c = c / 255.0;
        if c > 0.04045 {
            ((c + 0.055) / (1.0 + 0.055)).powf(2.4)
        } else {
            c / 12.92
        }
    };
    let (r, g, b) = (lin(r), lin(g), lin(b));
    let x = r * 0.664511 + g * 0.154324 + b * 0.162028;
    let y = r * 0.283881 + g * 0.668433 + b * 0.047685;
    let z = r * 0.000088 + g * 0.072310 + b * 0.986039;
    (round(x / (x + y + z), 3), round(y / (x + y + z), 3))
}

/// `color_xy_brightness_to_RGB`, without a gamut (components truncated).
pub fn xy_brightness_to_rgb(vx: f64, vy: f64, brightness: u8) -> [u8; 3] {
    let y_ = f64::from(brightness) / 255.0;
    if y_ == 0.0 {
        return [0, 0, 0];
    }
    let vy = if vy == 0.0 { vy + 1e-11 } else { vy };
    let x = (y_ / vy) * vx;
    let z = (y_ / vy) * (1.0 - vx - vy);
    let r = x * 1.656492 - y_ * 0.354851 - z * 0.255038;
    let g = -x * 0.707196 + y_ * 1.655397 + z * 0.036152;
    let b = x * 0.051713 - y_ * 0.121364 + z * 1.011530;
    let gamma = |c: f64| {
        if c <= 0.0031308 {
            12.92 * c
        } else {
            (1.0 + 0.055) * c.powf(1.0 / 2.4) - 0.055
        }
    };
    let mut c = [gamma(r), gamma(g), gamma(b)].map(|v| if v > 0.0 { v } else { 0.0 });
    // Python's `max` of the three, as written.
    let max = py_max3(c[0], c[1], c[2]);
    if max > 1.0 {
        c = c.map(|v| v / max);
    }
    c.map(|v| (v * 255.0) as u8)
}

/// Python's `max(a, b, c)`: the first of equal maxima.
fn py_max3(a: f64, b: f64, c: f64) -> f64 {
    let mut m = a;
    if b > m {
        m = b;
    }
    if c > m {
        m = c;
    }
    m
}

fn py_min3(a: f64, b: f64, c: f64) -> f64 {
    let mut m = a;
    if b < m {
        m = b;
    }
    if c < m {
        m = c;
    }
    m
}

/// `color_xy_to_RGB`.
pub fn xy_to_rgb(x: f64, y: f64) -> [u8; 3] {
    xy_brightness_to_rgb(x, y, 255)
}

/// `colorsys.rgb_to_hsv`.
fn rgb_to_hsv_unit(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let maxc = py_max3(r, g, b);
    let minc = py_min3(r, g, b);
    let rangec = maxc - minc;
    let v = maxc;
    if minc == maxc {
        return (0.0, 0.0, v);
    }
    let s = rangec / maxc;
    let rc = (maxc - r) / rangec;
    let gc = (maxc - g) / rangec;
    let bc = (maxc - b) / rangec;
    let h = if r == maxc {
        bc - gc
    } else if g == maxc {
        2.0 + rc - bc
    } else {
        4.0 + gc - rc
    };
    // Python's float `%` takes the divisor's sign.
    let h = (h / 6.0).rem_euclid(1.0);
    (h, s, v)
}

/// `color_RGB_to_hsv`: hue 0..360, saturation and value 0..100, each
/// rounded to 3 places.
pub fn rgb_to_hsv(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let (h, s, v) = rgb_to_hsv_unit(r / 255.0, g / 255.0, b / 255.0);
    (
        round(h * 360.0, 3),
        round(s * 100.0, 3),
        round(v * 100.0, 3),
    )
}

/// `color_RGB_to_hs`.
pub fn rgb_to_hs(r: f64, g: f64, b: f64) -> (f64, f64) {
    let (h, s, _) = rgb_to_hsv(r, g, b);
    (h, s)
}

/// `colorsys.hsv_to_rgb`.
fn hsv_to_rgb_unit(h: f64, s: f64, v: f64) -> (f64, f64, f64) {
    if s == 0.0 {
        return (v, v, v);
    }
    let i = (h * 6.0) as i64;
    let f = h * 6.0 - i as f64;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match i.rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

/// `color_hsv_to_RGB` (components rounded, ties to even).
pub fn hsv_to_rgb(h: f64, s: f64, v: f64) -> [u8; 3] {
    let (r, g, b) = hsv_to_rgb_unit(h / 360.0, s / 100.0, v / 100.0);
    [r, g, b].map(|c| (c * 255.0).round_ties_even() as u8)
}

/// `color_hs_to_RGB`.
pub fn hs_to_rgb(h: f64, s: f64) -> [u8; 3] {
    hsv_to_rgb(h, s, 100.0)
}

/// `color_xy_to_hs`.
pub fn xy_to_hs(x: f64, y: f64) -> (f64, f64) {
    let [r, g, b] = xy_to_rgb(x, y);
    rgb_to_hs(r.into(), g.into(), b.into())
}

/// `color_hs_to_xy`.
pub fn hs_to_xy(h: f64, s: f64) -> (f64, f64) {
    let [r, g, b] = hs_to_rgb(h, s);
    rgb_to_xy(r.into(), g.into(), b.into())
}

/// `match_max_scale`.
fn match_max_scale<const N: usize>(input: &[f64], output: [f64; N]) -> [i64; N] {
    let max_in = input.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let max_out = output.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let factor = if max_out == 0.0 {
        0.0
    } else {
        max_in / max_out
    };
    output.map(|o| (o * factor).round_ties_even() as i64)
}

/// `color_rgb_to_rgbw`.
pub fn rgb_to_rgbw(r: f64, g: f64, b: f64) -> [i64; 4] {
    let w = py_min3(r, g, b);
    match_max_scale(&[r, g, b], [r - w, g - w, b - w, w])
}

/// `color_rgbw_to_rgb`.
pub fn rgbw_to_rgb(r: f64, g: f64, b: f64, w: f64) -> [i64; 3] {
    match_max_scale(&[r, g, b, w], [r + w, g + w, b + w])
}

/// `color_rgb_to_rgbww`.
pub fn rgb_to_rgbww(r: f64, g: f64, b: f64, min_kelvin: f64, max_kelvin: f64) -> [i64; 5] {
    let max_mireds = kelvin_to_mired(min_kelvin) as f64;
    let min_mireds = kelvin_to_mired(max_kelvin) as f64;
    let mid = min_mireds + (max_mireds - min_mireds) / 2.0;
    let (wr, wg, wb) = temperature_to_rgb(mired_to_kelvin(mid) as f64);
    let part = |c: f64, w: f64| if w != 0.0 { c / w } else { 0.0 };
    let white = py_min3(part(r, wr), part(g, wg), part(b, wb));
    let level = (white * 255.0).round_ties_even();
    match_max_scale(
        &[r, g, b],
        [r - wr * white, g - wg * white, b - wb * white, level, level],
    )
}

/// `color_rgbww_to_rgb`.
pub fn rgbww_to_rgb(
    r: f64,
    g: f64,
    b: f64,
    cw: f64,
    ww: f64,
    min_kelvin: f64,
    max_kelvin: f64,
) -> [i64; 3] {
    let max_mireds = kelvin_to_mired(min_kelvin) as f64;
    let min_mireds = kelvin_to_mired(max_kelvin) as f64;
    let ratio = if cw + ww == 0.0 { 0.5 } else { ww / (cw + ww) };
    let mired = min_mireds + ratio * (max_mireds - min_mireds);
    let kelvin = if mired != 0.0 {
        mired_to_kelvin(mired) as f64
    } else {
        0.0
    };
    let (wr, wg, wb) = temperature_to_rgb(kelvin);
    let level = if cw > ww { cw } else { ww } / 255.0;
    match_max_scale(
        &[r, g, b, cw, ww],
        [r + wr * level, g + wg * level, b + wb * level],
    )
}

/// `color_temperature_to_rgbww`.
pub fn temperature_to_rgbww(
    kelvin: f64,
    brightness: f64,
    min_kelvin: f64,
    max_kelvin: f64,
) -> [i64; 5] {
    let max_mireds = kelvin_to_mired(min_kelvin) as f64;
    let min_mireds = kelvin_to_mired(max_kelvin) as f64;
    let t = kelvin_to_mired(kelvin) as f64;
    let cold = ((max_mireds - t) / (max_mireds - min_mireds)) * brightness;
    let warm = brightness - cold;
    [
        0,
        0,
        0,
        cold.round_ties_even() as i64,
        warm.round_ties_even() as i64,
    ]
}

/// `color_temperature_to_rgb` (Tanner Helland), unrounded.
pub fn temperature_to_rgb(kelvin: f64) -> (f64, f64, f64) {
    let k = kelvin.clamp(1000.0, 40000.0) / 100.0;
    let clamp = |c: f64| c.clamp(0.0, 255.0);
    let red = if k <= 66.0 {
        255.0
    } else {
        clamp(329.698727446 * (k - 60.0).powf(-0.1332047592))
    };
    let green = if k <= 66.0 {
        clamp(99.4708025861 * k.ln() - 161.1195681661)
    } else {
        clamp(288.1221695283 * (k - 60.0).powf(-0.0755148492))
    };
    let blue = if k >= 66.0 {
        255.0
    } else if k <= 19.0 {
        0.0
    } else {
        clamp(138.5177312231 * (k - 10.0).ln() - 305.0447927307)
    };
    (red, green, blue)
}

/// `color_temperature_to_hs`.
pub fn temperature_to_hs(kelvin: f64) -> (f64, f64) {
    let (r, g, b) = temperature_to_rgb(kelvin);
    rgb_to_hs(r, g, b)
}

/// `color_xy_to_temperature` (McCamy), truncated to an int. `None` where
/// Python raises `ZeroDivisionError` (`y == 0.1858`).
pub fn xy_to_temperature(x: f64, y: f64) -> Option<i64> {
    let d = 0.1858 - y;
    if d == 0.0 {
        return None;
    }
    let n = (x - 0.3320) / d;
    // Python's `n**3` is C `pow`, not repeated multiplication.
    let cct = 437.0 * n.powf(3.0) + 3601.0 * n.powf(2.0) + 6861.0 * n + 5517.0;
    Some(cct as i64)
}

/// `color_temperature_mired_to_kelvin`: `floor(1e6 / mired)`.
pub fn mired_to_kelvin(mired: f64) -> i64 {
    (1_000_000.0 / mired).floor() as i64
}

/// `color_temperature_kelvin_to_mired`: `floor(1e6 / kelvin)`.
pub fn kelvin_to_mired(kelvin: f64) -> i64 {
    (1_000_000.0 / kelvin).floor() as i64
}
