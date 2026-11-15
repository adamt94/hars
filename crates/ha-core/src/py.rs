//! Python's `repr` of floats and strings, which Home Assistant's output
//! often shows: state strings, voluptuous messages, rendered templates.

/// Python's `repr(float)`: the shortest round-trip digits, `60.0` not
/// `60`, and scientific (`1e-07`, `1.5e+16`) outside `1e-4..1e16`.
pub fn repr_float(x: f64) -> String {
    if !x.is_finite() {
        return if x.is_nan() {
            "nan".into()
        } else if x > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        };
    }
    let a = x.abs();
    if a != 0.0 && !(1e-4..1e16).contains(&a) {
        let s = format!("{x:e}");
        let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let exp: i32 = exp.parse().unwrap_or(0);
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exp.abs());
    }
    if x.fract() == 0.0 {
        format!("{x:.1}")
    } else {
        x.to_string()
    }
}

/// Python's `repr` of a `str`: single quotes unless only `'` is inside,
/// escapes for `\\`, the quote and control characters.
pub fn repr_str(out: &mut String, s: &str) {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x100 && c.is_control() => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
}

/// Python's `str.strip()` with no argument: Unicode whitespace and the
/// separators U+1C–1F, which `str.isspace` also counts.
pub fn strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}
