//! Python's number semantics, where HA's output depends on them.

use std::fmt::Write;

/// A fixed buffer to format into without allocating.
struct Stack {
    buf: [u8; 40],
    len: usize,
}

impl Write for Stack {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.len + s.len();
        if end > self.buf.len() {
            return Err(std::fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Python's `round(x, n)` for a float: correctly rounded from the exact
/// binary value, ties to even (Rust's fixed-precision formatting rounds
/// the same way). Allocation-free for ordinary magnitudes.
pub fn round(x: f64, n: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let mut s = Stack {
        buf: [0; 40],
        len: 0,
    };
    if write!(s, "{x:.n$}").is_ok() {
        if let Some(v) = std::str::from_utf8(&s.buf[..s.len])
            .ok()
            .and_then(|t| t.parse().ok())
        {
            return v;
        }
    }
    format!("{x:.n$}").parse().unwrap_or(x)
}

/// Python's `round(x)`: an int, ties to even.
pub fn round_int(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// Python's `format(x, ".{p}")` for a float (the format spec without a
/// type): `g`-like with `p` significant digits, but fixed notation keeps
/// at least one digit after the point, and scientific notation starts at
/// an exponent of `p - 1`. HA formats float sensor states this way
/// (`f"{value:.15}"`).
pub fn py_format(x: f64, p: usize) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.into();
    }
    let p = p.max(1);
    // Correctly rounded to p significant digits, as Python's.
    let sci = format!("{:.*e}", p - 1, x);
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i64 = exp.parse().unwrap_or(0);
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(m) => ("-", m),
        None => ("", mantissa),
    };
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    if exp < -4 || exp >= p as i64 - 1 {
        let (first, rest) = digits.split_at(1);
        let dot = if rest.is_empty() { "" } else { "." };
        let esign = if exp < 0 { '-' } else { '+' };
        return format!("{sign}{first}{dot}{rest}e{esign}{:02}", exp.abs());
    }
    if exp < 0 {
        let zeros = "0".repeat((-exp - 1) as usize);
        return format!("{sign}0.{zeros}{digits}");
    }
    let int_len = exp as usize + 1;
    if digits.len() <= int_len {
        let zeros = "0".repeat(int_len - digits.len());
        return format!("{sign}{digits}{zeros}.0");
    }
    let (int, frac) = digits.split_at(int_len);
    format!("{sign}{int}.{frac}")
}

/// A string as Python's `int()`/`float()` read it: whitespace stripped,
/// Unicode decimal digits as ASCII, `_` allowed only between digits.
/// `None` for anything else non-ASCII or a misplaced `_`.
pub fn py_numeric_ascii(s: &str) -> Option<String> {
    let t = s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c));
    let chars: Vec<char> = t.chars().collect();
    let mut ascii = String::with_capacity(chars.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' {
            let digit = |c: Option<&char>| c.is_some_and(|c| ha_core::decimal_value(*c).is_some());
            if !(i > 0 && digit(chars.get(i - 1)) && digit(chars.get(i + 1))) {
                return None;
            }
            continue;
        }
        match ha_core::decimal_value(c) {
            Some(d) => ascii.push(char::from_digit(d, 10)?),
            None if c.is_ascii() => ascii.push(c),
            None => return None,
        }
    }
    Some(ascii)
}

/// Python's `int(s)` for a string (bounded to `i128`).
pub fn py_int_str(s: &str) -> Option<i128> {
    let a = py_numeric_ascii(s)?;
    let digits = a.strip_prefix(['+', '-']).unwrap_or(&a);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    a.parse().ok()
}

/// Python's `float(s)` for a string (Rust's float grammar is Python's
/// here: `inf`, `nan`, `1.`, `.5`, exponents).
pub fn py_float_str(s: &str) -> Option<f64> {
    py_numeric_ascii(s)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_as_python() {
        // Each from CPython 3.14.
        for (x, n, want) in [
            (8.05, 1, 8.1),
            (9.85, 1, 9.8),
            (0.125, 2, 0.12),
            (0.375, 2, 0.38),
            (2.675, 2, 2.67),
            (28.391499999999997, 3, 28.391),
            (1e300, 2, 1e300),
            (123456789012345678.0, 3, 123456789012345680.0),
        ] {
            assert_eq!(round(x, n), want, "round({x}, {n})");
        }
        assert!(round(-0.04, 1).is_sign_negative());
        for (x, want) in [(72.5, 72), (73.5, 74), (4.5, 4), (2.5, 2)] {
            assert_eq!(round_int(x), want, "round({x})");
        }
    }

    #[test]
    fn formats_as_python() {
        // `format(x, ".15")` in CPython 3.14.
        for (x, want) in [
            (21.7, "21.7"),
            (21.0, "21.0"),
            (-3.25, "-3.25"),
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (100.0, "100.0"),
            (0.1 + 0.2, "0.3"),
            (21.700000000000003, "21.7"),
            (1e16, "1e+16"),
            (1.5e-05, "1.5e-05"),
            (123456789.12345679, "123456789.123457"),
            (22.35, "22.35"),
            (1e15, "1e+15"),
            (99.99999999999999, "100.0"),
            (1e14, "1e+14"),
            (123456789012345.0, "1.23456789012345e+14"),
            (12345678901234.0, "12345678901234.0"),
            (0.0001, "0.0001"),
            (1.0 / 3.0, "0.333333333333333"),
            (1e100, "1e+100"),
            (5e-324, "4.94065645841247e-324"),
            (f64::INFINITY, "inf"),
        ] {
            assert_eq!(py_format(x, 15), want, "{x:?}");
        }
    }
}
