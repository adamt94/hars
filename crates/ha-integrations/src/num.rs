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
}
