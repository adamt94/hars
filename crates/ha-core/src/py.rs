//! Python's `repr` of floats and strings, which Home Assistant's output
//! often shows: state strings, voluptuous messages, rendered templates.

/// Python's `repr(float)`: the shortest round-trip digits, `60.0` not
/// `60`, and scientific (`1e-07`, `1.5e+16`) outside `1e-4..1e16`.
///
/// Where two shortest digit strings round-trip (the value is exactly
/// half way: `1000000000000000.25`), Python takes the one correctly
/// rounded from the exact value, half to even (`…000.2`); Rust's shortest
/// formatting may take the other (`…000.3`).
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
    let sign = if x.is_sign_negative() { "-" } else { "" };
    let a = x.abs();
    if a == 0.0 {
        return format!("{sign}0.0");
    }
    // The shortest digits, then the same number of digits rounded from
    // the exact value: Python's, if they still read back as `a`.
    let split = |s: &str| -> (String, i32) {
        let (mantissa, exp) = s.split_once('e').unwrap_or((s, "0"));
        (mantissa.replace('.', ""), exp.parse().unwrap_or(0))
    };
    let (mut digits, mut exp) = split(&format!("{a:e}"));
    let exact = format!("{:.*e}", digits.len() - 1, a);
    if exact.parse::<f64>() == Ok(a) {
        (digits, exp) = split(&exact);
    }
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    if !(1e-4..1e16).contains(&a) {
        let mantissa = match digits.len() {
            1 => digits.to_owned(),
            _ => format!("{}.{}", &digits[..1], &digits[1..]),
        };
        let exp_sign = if exp < 0 { '-' } else { '+' };
        return format!("{sign}{mantissa}e{exp_sign}{:02}", exp.abs());
    }
    let n = digits.len() as i32;
    let body = if exp < 0 {
        format!("0.{}{digits}", "0".repeat((-exp - 1) as usize))
    } else if exp >= n - 1 {
        format!("{digits}{}.0", "0".repeat((exp - (n - 1)) as usize))
    } else {
        let point = (exp + 1) as usize;
        format!("{}.{}", &digits[..point], &digits[point..])
    };
    format!("{sign}{body}")
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

/// Python's `str()` of a value `json.loads` (orjson) returned: a string as
/// it is, anything else as its `repr` (`True`, `None`, `1.5`, `1e-07`,
/// `[1, 'a']`, `{'k': 2.0}`).
pub fn str_json(v: &serde_json::Value) -> std::borrow::Cow<'_, str> {
    match v {
        serde_json::Value::String(s) => std::borrow::Cow::Borrowed(s),
        v => {
            let mut out = String::new();
            repr_json(&mut out, v);
            std::borrow::Cow::Owned(out)
        }
    }
}

fn repr_json(out: &mut String, v: &serde_json::Value) {
    use serde_json::Value;
    match v {
        Value::Null => out.push_str("None"),
        Value::Bool(true) => out.push_str("True"),
        Value::Bool(false) => out.push_str("False"),
        Value::Number(n) => match (n.is_f64(), n.as_f64()) {
            (true, Some(f)) => out.push_str(&repr_float(f)),
            _ => out.push_str(&n.to_string()),
        },
        Value::String(s) => repr_str(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                repr_json(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                repr_str(out, k);
                out.push_str(": ");
                repr_json(out, item);
            }
            out.push('}');
        }
    }
}

/// Python's `str.strip()` with no argument: Unicode whitespace and the
/// separators U+1C–1F, which `str.isspace` also counts.
pub fn strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}
