//! Jinja's filters, tests and `range` where minijinja's answer isn't
//! Jinja's (`jinja2/filters.py`, `jinja2/tests.py`, `jinja2/sandbox.py`).
//!
//! Jinja runs in Python, so its tests and filters follow Python's rules:
//! `'abc' < 2` and `'abc' is even` raise `TypeError`, `none | join` can't
//! iterate, `1 == 1.0 == True`, `range(3)` prints as `range(0, 3)`, and an
//! undefined value warns (lenient) or raises (strict) when it is printed or
//! iterated. minijinja is more forgiving and would answer instead. Each
//! function here is checked against HA by `vectors/kept.json`.

use super::{Dict, py_str, warn_undefined};
use minijinja::value::{Enumerator, Kwargs, Object, ObjectRepr, Rest, Tuple, ValueKind};
use minijinja::{Environment, Error, ErrorKind, State, UndefinedBehavior, Value};
use std::cmp::Ordering;
use std::sync::Arc;

/// Jinja's `MAX_RANGE` (`jinja2/sandbox.py`).
const MAX_RANGE: i64 = 100_000;

/// A Python exception: passed through as HA's `TemplateError` text.
pub(super) fn py_error(message: String) -> Error {
    Error::new(ErrorKind::InvalidOperation, message)
}

/// `StrictUndefined`, or an undefined value used by an operator in lenient
/// mode: Jinja raises without logging (its operator methods are the base
/// class's `_fail_with_undefined_error`, which `LoggingUndefined` doesn't
/// see).
pub(super) const QUIET_UNDEFINED: &str = "undefined (not logged)";

fn quiet_undefined() -> Error {
    Error::new(ErrorKind::UndefinedError, QUIET_UNDEFINED)
}

fn strict(state: &State) -> bool {
    state.undefined_behavior() == UndefinedBehavior::Strict
}

/// Python's type name of a value, for `TypeError` messages.
fn type_name(v: &Value) -> &'static str {
    match v.kind() {
        ValueKind::Undefined => "Undefined",
        ValueKind::None => "NoneType",
        ValueKind::Bool => "bool",
        ValueKind::Number if v.is_integer() => "int",
        ValueKind::Number => "float",
        ValueKind::String => "str",
        ValueKind::Seq if v.downcast_object_ref::<Range>().is_some() => "range",
        ValueKind::Seq if v.downcast_object_ref::<Tuple>().is_some() => "tuple",
        ValueKind::Seq => "list",
        ValueKind::Map => "dict",
        _ => "object",
    }
}

/// A number as Python sees it: `bool` is an `int`.
enum Num {
    Int(i128),
    Float(f64),
}

fn num(v: &Value) -> Option<Num> {
    match v.kind() {
        ValueKind::Bool => Some(Num::Int(i128::from(v.is_true()))),
        // An integer past `i128` (minijinja has `u128`) compares as a float.
        ValueKind::Number if v.is_integer() => i128::try_from(v.clone())
            .ok()
            .map(Num::Int)
            .or_else(|| f64::try_from(v.clone()).ok().map(Num::Float)),
        ValueKind::Number => f64::try_from(v.clone()).ok().map(Num::Float),
        _ => None,
    }
}

impl Num {
    fn f64(&self) -> f64 {
        match *self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }
}

/// Pays for visiting one item, and for the bytes of a string compared.
fn visit(state: &State, v: &Value) -> Result<(), Error> {
    let bytes = if v.kind() == ValueKind::String {
        v.as_str().map_or(0, str::len)
    } else {
        0
    };
    state.consume_fuel(1 + bytes as u64 / 64)
}

/// Python's `==`. Every item visited and every string byte compared is
/// paid for: a value is charged once when it is made, but it can be
/// compared again and again.
pub(super) fn py_eq(state: &State, a: &Value, b: &Value) -> Result<bool, Error> {
    py_eq_at(state, a, b, 0)
}

fn py_eq_at(state: &State, a: &Value, b: &Value, depth: usize) -> Result<bool, Error> {
    if depth > super::MAX_REPR_DEPTH {
        return Err(Error::new(ErrorKind::InvalidOperation, super::RECURSION));
    }
    visit(state, a)?;
    if let (Some(x), Some(y)) = (num(a), num(b)) {
        return Ok(match (x, y) {
            (Num::Int(x), Num::Int(y)) => x == y,
            (x, y) => x.f64() == y.f64(),
        });
    }
    Ok(match (a.kind(), b.kind()) {
        (ValueKind::None, ValueKind::None) => true,
        (ValueKind::String, ValueKind::String) => a.as_str() == b.as_str(),
        (ValueKind::Seq, ValueKind::Seq) if type_name(a) == type_name(b) => {
            if a.len() != b.len() {
                return Ok(false);
            }
            let (mut x, mut y) = (a.try_iter()?, b.try_iter()?);
            loop {
                match (x.next(), y.next()) {
                    (Some(p), Some(q)) => {
                        if !py_eq_at(state, &p, &q, depth + 1)? {
                            return Ok(false);
                        }
                    }
                    (None, None) => return Ok(true),
                    _ => return Ok(false),
                }
            }
        }
        (ValueKind::Map, ValueKind::Map) => {
            if a.len() != b.len() {
                return Ok(false);
            }
            for k in a.try_iter()? {
                visit(state, &k)?;
                let bv = b.get_item(&k).unwrap_or_default();
                if bv.is_undefined()
                    || !py_eq_at(state, &a.get_item(&k).unwrap_or_default(), &bv, depth + 1)?
                {
                    return Ok(false);
                }
            }
            true
        }
        _ => false,
    })
}

/// Python's ordering for `<`, `<=`, `>`, `>=`: `None` when every
/// comparison is false (NaN); a `TypeError` between unordered types. Paid
/// for as [`py_eq`] is.
fn py_order(state: &State, a: &Value, b: &Value, op: &str) -> Result<Option<Ordering>, Error> {
    py_order_at(state, a, b, op, 0)
}

fn py_order_at(
    state: &State,
    a: &Value,
    b: &Value,
    op: &str,
    depth: usize,
) -> Result<Option<Ordering>, Error> {
    if a.is_undefined() || b.is_undefined() {
        return Err(quiet_undefined());
    }
    if depth > super::MAX_REPR_DEPTH {
        return Err(Error::new(ErrorKind::InvalidOperation, super::RECURSION));
    }
    visit(state, a)?;
    if let (Some(x), Some(y)) = (num(a), num(b)) {
        return Ok(match (x, y) {
            (Num::Int(x), Num::Int(y)) => Some(x.cmp(&y)),
            (x, y) => x.f64().partial_cmp(&y.f64()),
        });
    }
    match (a.kind(), b.kind()) {
        (ValueKind::String, ValueKind::String) => Ok(Some(a.as_str().cmp(&b.as_str()))),
        (ValueKind::Seq, ValueKind::Seq) if type_name(a) == type_name(b) => {
            let (mut x, mut y) = (a.try_iter()?, b.try_iter()?);
            loop {
                match (x.next(), y.next()) {
                    (Some(p), Some(q)) => {
                        if !py_eq_at(state, &p, &q, depth + 1)? {
                            return py_order_at(state, &p, &q, op, depth + 1);
                        }
                    }
                    (None, None) => return Ok(Some(Ordering::Equal)),
                    (None, Some(_)) => return Ok(Some(Ordering::Less)),
                    (Some(_), None) => return Ok(Some(Ordering::Greater)),
                }
            }
        }
        _ => Err(py_error(format!(
            "TypeError: '{op}' not supported between instances of '{}' and '{}'",
            type_name(a),
            type_name(b)
        ))),
    }
}

/// Python's `x % m` for integers: the sign of the divisor, and no
/// overflow (`i128::MIN % -1` is 0).
fn py_int_mod(x: i128, m: i128) -> i128 {
    let r = x.checked_rem(m).unwrap_or(0);
    if r != 0 && (r < 0) != (m < 0) {
        r + m
    } else {
        r
    }
}

/// `value % n == want`, by Python's rules.
fn py_mod_is(v: &Value, n: &Value, want: i128) -> Result<bool, Error> {
    if v.is_undefined() {
        return Err(quiet_undefined());
    }
    let (Some(x), Some(m)) = (num(v), num(n)) else {
        return Err(py_error(if v.kind() == ValueKind::String {
            "TypeError: not all arguments converted during string formatting".into()
        } else {
            format!(
                "TypeError: unsupported operand type(s) for %: '{}' and '{}'",
                type_name(v),
                type_name(n)
            )
        }));
    };
    Ok(match (x, m) {
        (_, Num::Int(0)) => {
            return Err(py_error("ZeroDivisionError: integer modulo by zero".into()));
        }
        (Num::Int(x), Num::Int(m)) => py_int_mod(x, m) == want,
        (x, m) => {
            let (x, m) = (x.f64(), m.f64());
            if m == 0.0 {
                return Err(py_error("ZeroDivisionError: float modulo by zero".into()));
            }
            // Python's float `%` takes the divisor's sign.
            let r = x % m;
            let r = if r != 0.0 && (r < 0.0) != (m < 0.0) {
                r + m
            } else {
                r
            };
            r == want as f64
        }
    })
}

/// `str(value)` with Jinja's undefined handling: lenient warns and gives
/// `""`, strict raises.
fn text(state: &State, v: &Value) -> Result<String, Error> {
    if v.is_undefined() {
        if strict(state) {
            return Err(Error::from(ErrorKind::UndefinedError));
        }
        warn_undefined();
        return Ok(String::new());
    }
    let mut s = String::new();
    py_str(&mut super::Charged::new(&mut s, state), v)?;
    Ok(s)
}

/// What iterating `v` does in Python before minijinja's filter runs:
/// `None` can't be iterated; an undefined value warns (lenient) or raises.
fn iterate(state: &State, v: &Value) -> Result<(), Error> {
    match v.kind() {
        ValueKind::Undefined if strict(state) => Err(Error::from(ErrorKind::UndefinedError)),
        ValueKind::Undefined => {
            warn_undefined();
            Ok(())
        }
        ValueKind::None | ValueKind::Bool | ValueKind::Number => Err(py_error(format!(
            "TypeError: '{}' object is not iterable",
            type_name(v)
        ))),
        _ => Ok(()),
    }
}

/// The tests whose answers are Python's.
pub(super) fn add_tests(env: &mut Environment<'static>) {
    let eq = |state: &State, v: &Value, other: &Value| -> Result<bool, Error> {
        if v.is_undefined() && strict(state) {
            return Err(Error::from(ErrorKind::UndefinedError));
        }
        py_eq(state, v, other)
    };
    let ne = move |state: &State, v: &Value, other: &Value| eq(state, v, other).map(|e| !e);
    for name in ["eq", "equalto", "=="] {
        env.add_test(name, eq);
    }
    for name in ["ne", "!="] {
        env.add_test(name, ne);
    }
    let compare = |op: &'static str, keep: fn(Ordering) -> bool| {
        move |state: &State, v: &Value, other: &Value| -> Result<bool, Error> {
            Ok(py_order(state, v, other, op)?.is_some_and(keep))
        }
    };
    for (names, op, keep) in [
        (
            &["lt", "lessthan", "<"][..],
            "<",
            Ordering::is_lt as fn(Ordering) -> bool,
        ),
        (&["le", "<="][..], "<=", Ordering::is_le),
        (&["gt", "greaterthan", ">"][..], ">", Ordering::is_gt),
        (&["ge", ">="][..], ">=", Ordering::is_ge),
    ] {
        for name in names {
            env.add_test(*name, compare(op, keep));
        }
    }
    env.add_test("even", |v: &Value| py_mod_is(v, &Value::from(2), 0));
    env.add_test("odd", |v: &Value| py_mod_is(v, &Value::from(2), 1));
    env.add_test("divisibleby", |v: &Value, n: &Value| py_mod_is(v, n, 0));
    env.add_test(
        "in",
        |state: &State, v: &Value, seq: &Value| -> Result<bool, Error> {
            match seq.kind() {
                ValueKind::String => match v.as_str() {
                    Some(s) if v.kind() == ValueKind::String => {
                        visit(state, seq)?;
                        Ok(seq.as_str().unwrap_or_default().contains(s))
                    }
                    _ => Err(py_error(format!(
                        "TypeError: 'in <string>' requires string as left operand, not {}",
                        type_name(v)
                    ))),
                },
                ValueKind::Seq | ValueKind::Map => {
                    if v.is_undefined() && strict(state) {
                        return Err(Error::from(ErrorKind::UndefinedError));
                    }
                    for item in seq.try_iter()? {
                        if py_eq(state, &item, v)? {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                }
                _ => Err(py_error(format!(
                    "TypeError: argument of type '{}' is not iterable",
                    type_name(seq)
                ))),
            }
        },
    );
    // `str(value).islower()` / `.isupper()`: some cased character, and
    // none of the other case.
    env.add_test("lower", |state: &State, v: &Value| -> Result<bool, Error> {
        let s = text(state, v)?;
        Ok(s.chars().any(char::is_lowercase) && !s.chars().any(char::is_uppercase))
    });
    env.add_test("upper", |state: &State, v: &Value| -> Result<bool, Error> {
        let s = text(state, v)?;
        Ok(s.chars().any(char::is_uppercase) && !s.chars().any(char::is_lowercase))
    });
    env.add_test(
        "iterable",
        |state: &State, v: &Value| -> Result<bool, Error> {
            Ok(match v.kind() {
                ValueKind::Undefined => {
                    iterate(state, v)?;
                    true
                }
                ValueKind::None | ValueKind::Bool | ValueKind::Number => false,
                _ => v.try_iter().is_ok(),
            })
        },
    );
    // `len(value)` and `value.__getitem__`: strings, lists, tuples, dicts,
    // ranges; lenient undefined has both, strict raises (so: false).
    env.add_test("sequence", |state: &State, v: &Value| match v.kind() {
        ValueKind::String | ValueKind::Seq | ValueKind::Map => true,
        ValueKind::Undefined => !strict(state),
        _ => v.downcast_object_ref::<Dict>().is_some(),
    });
}

/// The filters whose answers are Python's.
pub(super) fn add_filters(env: &mut Environment<'static>) {
    use minijinja::filters as f;
    // Filters that iterate their input.
    env.add_filter("list", |state: &State, v: Value| {
        iterate(state, &v)?;
        f::list(state, v)
    });
    env.add_filter("sort", |state: &State, v: Value, kwargs: Kwargs| {
        iterate(state, &v)?;
        sort_check(state, &v, &kwargs)?;
        charge_sorting(state, &v)?;
        f::sort(state, v, kwargs)
    });
    env.add_filter(
        "sum",
        |state: &State, v: Value, attribute: Option<&str>, start: Option<Value>, kwargs: Kwargs| {
            iterate(state, &v)?;
            let named = kwargs.peek::<Option<Value>>("attribute")?;
            charge_attribute(state, &v, attribute.map(Value::from).or(named).as_ref())?;
            f::sum(state, v, attribute, start, kwargs)
        },
    );
    env.add_filter("unique", |state: &State, v: Value, kwargs: Kwargs| {
        iterate(state, &v)?;
        charge_attribute(
            state,
            &v,
            kwargs.peek::<Option<Value>>("attribute")?.as_ref(),
        )?;
        charge_sorting(state, &v)?;
        f::unique(state, v, kwargs)
    });
    env.add_filter(
        "batch",
        |state: &State, v: Value, n: usize, fill: Option<Value>| {
            iterate(state, &v)?;
            f::batch(state, v, n, fill)
        },
    );
    env.add_filter(
        "slice",
        |state: &State, v: Value, n: usize, fill: Option<Value>| {
            iterate(state, &v)?;
            f::slice(state, v, n, fill)
        },
    );
    env.add_filter("reverse", |state: &State, v: &Value| {
        iterate(state, v)?;
        f::reverse_fueled(state, v)
    });
    env.add_filter("items", |state: &State, v: &Value| {
        iterate(state, v)?;
        f::items_checked(state, v)
    });
    env.add_filter("dictsort", |state: &State, v: &Value, kwargs: Kwargs| {
        iterate(state, v)?;
        charge_sorting(state, v)?;
        f::dictsort_fueled(state, v, kwargs)
    });
    env.add_filter(
        "groupby",
        |state: &State, v: Value, attribute: Option<&str>, kwargs: Kwargs| {
            iterate(state, &v)?;
            charge_attribute(state, &v, attribute.map(Value::from).as_ref())?;
            charge_sorting(state, &v)?;
            f::groupby_fueled(state, v, attribute, kwargs)
        },
    );
    env.add_filter(
        "map",
        |state: &mut State, v: Value, args: Rest<minijinja::value::ValueOrKwargs>| {
            iterate(state, &v)?;
            let attribute = args
                .iter()
                .filter(|a| a.kind() == ValueKind::Map)
                .find_map(|a| {
                    a.get_item(&Value::from("attribute"))
                        .ok()
                        .filter(|x| !x.is_undefined())
                });
            charge_attribute(state, &v, attribute.as_ref())?;
            f::map(state, v, args)
        },
    );
    env.add_filter(
        "select",
        |state: &mut State,
         v: Value,
         test: Option<std::borrow::Cow<'_, str>>,
         args: Rest<minijinja::value::ValueOrKwargs>| {
            iterate(state, &v)?;
            f::select(state, v, test, args)
        },
    );
    env.add_filter(
        "reject",
        |state: &mut State,
         v: Value,
         test: Option<std::borrow::Cow<'_, str>>,
         args: Rest<minijinja::value::ValueOrKwargs>| {
            iterate(state, &v)?;
            f::reject(state, v, test, args)
        },
    );
    env.add_filter(
        "selectattr",
        |state: &mut State,
         v: Value,
         attr: std::borrow::Cow<'_, str>,
         test: Option<std::borrow::Cow<'_, str>>,
         args: Rest<minijinja::value::ValueOrKwargs>| {
            iterate(state, &v)?;
            charge_attribute(state, &v, Some(&Value::from(&*attr)))?;
            f::selectattr(state, v, attr, test, args)
        },
    );
    env.add_filter(
        "rejectattr",
        |state: &mut State,
         v: Value,
         attr: std::borrow::Cow<'_, str>,
         test: Option<std::borrow::Cow<'_, str>>,
         args: Rest<minijinja::value::ValueOrKwargs>| {
            iterate(state, &v)?;
            charge_attribute(state, &v, Some(&Value::from(&*attr)))?;
            f::rejectattr(state, v, attr, test, args)
        },
    );
    // Empty input: an undefined value whose message is Jinja's hint.
    let hint = |v: Value, message: &'static str| {
        if v.is_undefined() {
            super::note_miss(message.into());
        }
        v
    };
    env.add_filter("first", move |state: &State, v: &Value| {
        iterate(state, v)?;
        f::first_checked(state, v).map(|r| hint(r, "No first item, sequence was empty."))
    });
    // `next(iter(reversed(value)))`: reversing an undefined value doesn't
    // warn (`LoggingUndefined` only logs `__iter__`, `__str__`, `__bool__`).
    env.add_filter("last", move |state: &State, v: Value| {
        if !v.is_undefined() || strict(state) {
            iterate(state, &v)?;
        }
        f::last_fueled(state, v).map(|r| hint(r, "No last item, sequence was empty."))
    });
    env.add_filter("min", move |state: &State, v: Value, kwargs: Kwargs| {
        iterate(state, &v)?;
        sort_check(state, &v, &kwargs)?;
        charge_sorting(state, &v)?;
        f::min(state, v, kwargs).map(|r| hint(r, "No aggregated item, sequence was empty."))
    });
    env.add_filter("max", move |state: &State, v: Value, kwargs: Kwargs| {
        iterate(state, &v)?;
        sort_check(state, &v, &kwargs)?;
        charge_sorting(state, &v)?;
        f::max(state, v, kwargs).map(|r| hint(r, "No aggregated item, sequence was empty."))
    });
    // Filters that take `str(value)`.
    env.add_filter("lower", |state: &State, v: &Value| {
        text(state, v).map(|s| s.to_lowercase())
    });
    env.add_filter("upper", |state: &State, v: &Value| {
        text(state, v).map(|s| s.to_uppercase())
    });
    env.add_filter("title", |state: &State, v: &Value| {
        text(state, v).map(|s| f::title(s.into()))
    });
    // `str.capitalize()`.
    env.add_filter("capitalize", |state: &State, v: &Value| {
        text(state, v).map(|s| {
            let mut chars = s.chars();
            match chars.next() {
                Some(c) => c
                    .to_uppercase()
                    .chain(chars.as_str().to_lowercase().chars())
                    .collect(),
                None => String::new(),
            }
        })
    });
    // `str.strip(chars)`.
    env.add_filter(
        "trim",
        |state: &State, v: &Value, chars: Option<Value>| -> Result<String, Error> {
            let s = text(state, v)?;
            Ok(match chars.filter(|c| !c.is_none()) {
                Some(c) => {
                    let c: std::collections::HashSet<char> = text(state, &c)?.chars().collect();
                    s.trim_matches(|x| c.contains(&x)).to_owned()
                }
                None => crate::py::strip(&s).to_owned(),
            })
        },
    );
    env.add_filter(
        "replace",
        |state: &mut State,
         v: &Value,
         from: Value,
         to: Value,
         count: Option<Value>|
         -> Result<Value, Error> {
            let s = text(state, v)?;
            let (from, to) = (text(state, &from)?, text(state, &to)?);
            let count = match count {
                Some(c) => i64::try_from(c)?,
                None => -1,
            };
            // `str.replace(old, new, count)`; a negative count is all.
            let limit = usize::try_from(count).unwrap_or(usize::MAX);
            // The result's size is known before it is built.
            let grows = to.len().saturating_sub(from.len()).max(1);
            let most = s.len() / from.len().max(1) + 1;
            state.consume_fuel((most.min(limit).saturating_mul(grows) / 64) as u64)?;
            Ok(Value::from(replacen(&s, &from, &to, limit)))
        },
    );
    env.add_filter("safe", |state: &State, v: &Value| -> Result<Value, Error> {
        if v.is_safe() {
            return Ok(v.clone());
        }
        text(state, v).map(Value::from_safe_string)
    });
    env.add_filter("abs", |v: &Value| -> Result<Value, Error> {
        match num(v) {
            // `abs(i128::MIN)` is 2**127: past `i128`, inside `u128`.
            Some(Num::Int(i)) => Ok(match i.checked_abs() {
                Some(a) => Value::from(a),
                None => Value::from(i.unsigned_abs()),
            }),
            Some(Num::Float(x)) => Ok(Value::from(x.abs())),
            None => Err(py_error(format!(
                "TypeError: bad operand type for abs(): '{}'",
                type_name(v)
            ))),
        }
    });
    env.add_filter("join", join);
}

/// `str.replace(old, new, count)` for a count that isn't "all".
fn replacen(s: &str, from: &str, to: &str, count: usize) -> String {
    if from.is_empty() {
        // Python inserts `new` before each character (and at the end).
        let mut out = String::new();
        let mut n = 0;
        for c in s.chars() {
            if n < count {
                out.push_str(to);
                n += 1;
            }
            out.push(c);
        }
        if n < count {
            out.push_str(to);
        }
        return out;
    }
    s.replacen(from, to, count)
}

/// Sorting compares with Python's `<`: mixed types raise. The keys are
/// the items, or each `attribute` (comma-separated) of them. The keys are
/// paid for (items × attributes × path segments, before any is looked up,
/// which also covers the key table the sort itself builds) and checked as
/// they are found, never kept.
fn sort_check(state: &State, v: &Value, kwargs: &Kwargs) -> Result<(), Error> {
    let attributes: Option<Value> = kwargs.peek("attribute")?;
    let parts: Vec<&str> = match attributes.as_ref().and_then(Value::as_str) {
        Some(a) => a.split(',').map(str::trim).collect(),
        None => Vec::new(),
    };
    charge_attribute(state, v, attributes.as_ref())?;
    let Ok(items) = v.try_iter() else {
        return Ok(());
    };
    if parts.is_empty() {
        // A `TypeError` only depends on the types: comparing each key with
        // the first finds it.
        let mut first: Option<Value> = None;
        for item in items {
            match &first {
                None => first = Some(item),
                Some(a) => {
                    py_type_order(state, a, &item)?;
                }
            }
        }
        return Ok(());
    }
    let mut first: Vec<Option<Value>> = vec![None; parts.len()];
    for item in items {
        for (part, first) in parts.iter().zip(&mut first) {
            let key = attr_path(state, &item, &Value::from(*part))?;
            match first {
                None => *first = Some(key),
                Some(a) => {
                    py_type_order(state, a, &key)?;
                }
            }
        }
    }
    Ok(())
}

/// Pays for the comparisons a sort (or `min`, `max`, `unique`, `groupby`,
/// `dictsort`) of `v` makes: each walks two items deeply, about
/// `log2(n)` times per item. Walks `v` once, paying for each part.
fn charge_sorting(state: &State, v: &Value) -> Result<(), Error> {
    let n = v.len().unwrap_or(0) as u64;
    let rounds = u64::from(64 - n.leading_zeros()).max(1);
    let mut parts = 0u64;
    let mut pending = vec![v.clone()];
    while let Some(x) = pending.pop() {
        visit(state, &x)?;
        parts += 1;
        if matches!(
            x.kind(),
            ValueKind::Seq | ValueKind::Map | ValueKind::Iterable
        ) {
            if let Ok(items) = x.try_iter() {
                for item in items {
                    if x.kind() == ValueKind::Map {
                        pending.push(x.get_item(&item).unwrap_or_default());
                    }
                    pending.push(item);
                }
            }
        }
    }
    state.consume_fuel(parts.saturating_mul(rounds))
}

/// Whether `a < b` would raise, without walking more than the first item
/// of two lists.
fn py_type_order(state: &State, a: &Value, b: &Value) -> Result<(), Error> {
    py_order(state, a, b, "<").map(|_| ())
}

/// Pays for looking up `attribute` (dotted, comma-separated) in each item
/// of `v`, before any lookup is made.
fn charge_attribute(state: &State, v: &Value, attribute: Option<&Value>) -> Result<(), Error> {
    let Some(a) = attribute.and_then(Value::as_str) else {
        return Ok(());
    };
    let segments = a.split([',', '.']).count() as u64;
    let items = v.len().unwrap_or(1) as u64;
    state.consume_fuel(items.saturating_mul(segments))
}

/// `do_join`: `str(d).join(str(item) …)`, with `attribute` taken first.
fn join(
    state: &State,
    v: &Value,
    d: Option<Value>,
    attribute: Option<Value>,
    kwargs: Kwargs,
) -> Result<Value, Error> {
    iterate(state, v)?;
    let d = match (d, kwargs.get::<Option<Value>>("d")?) {
        (_, Some(d)) | (Some(d), None) => text(state, &d)?,
        (None, None) => String::new(),
    };
    let attribute = match (attribute, kwargs.get::<Option<Value>>("attribute")?) {
        (_, Some(a)) | (Some(a), None) => Some(a),
        (None, None) => None,
    };
    kwargs.assert_all_used()?;
    charge_attribute(state, v, attribute.as_ref())?;
    if v.is_undefined() {
        return Ok(Value::from(""));
    }
    let mut out = String::new();
    for (i, item) in v.try_iter()?.enumerate() {
        if i > 0 {
            out.push_str(&d);
        }
        let item = match &attribute {
            Some(a) => attr_path(state, &item, a)?,
            None => item,
        };
        out.push_str(&text(state, &item)?);
        state.consume_fuel(1 + out.len() as u64 / 4096)?;
    }
    Ok(Value::from(out))
}

/// `make_attrgetter`: a dotted path, digits indexing. Each segment looked
/// up is paid for.
fn attr_path(state: &State, v: &Value, path: &Value) -> Result<Value, Error> {
    let Some(path) = path.as_str() else {
        state.consume_fuel(1)?;
        return Ok(v.get_item(path).unwrap_or_default());
    };
    let mut cur = v.clone();
    for part in path.split('.') {
        state.consume_fuel(1)?;
        let key = part
            .parse::<i64>()
            .map_or_else(|_| Value::from(part), Value::from);
        cur = cur.get_item(&key).unwrap_or_default();
    }
    Ok(cur)
}

/// Python's `range`, as Jinja's sandbox limits it (`safe_range`).
pub(super) fn range(start: i64, stop: Option<i64>, step: Option<i64>) -> Result<Value, Error> {
    let (start, stop) = match stop {
        Some(stop) => (start, stop),
        None => (0, start),
    };
    let step = step.unwrap_or(1);
    if step == 0 {
        return Err(py_error(
            "ValueError: range() arg 3 must not be zero".into(),
        ));
    }
    let r = Range { start, stop, step };
    if r.len() > MAX_RANGE as usize {
        return Err(py_error(
            "OverflowError: Range too big. The sandbox blocks ranges larger than MAX_RANGE (100000)."
                .into(),
        ));
    }
    Ok(Value::from_object(r))
}

/// A Python `range` object. Its arithmetic is done in `i128`, where no
/// `i64` bound can overflow it.
#[derive(Debug)]
pub(super) struct Range {
    start: i64,
    stop: i64,
    step: i64,
}

impl Range {
    fn len(&self) -> usize {
        let (start, stop, step) = (
            i128::from(self.start),
            i128::from(self.stop),
            i128::from(self.step),
        );
        let (lo, hi, step) = if step > 0 {
            (start, stop, step)
        } else {
            (stop, start, -step)
        };
        if hi <= lo {
            return 0;
        }
        usize::try_from((hi - lo - 1) / step + 1).unwrap_or(usize::MAX)
    }

    /// `repr(range)`.
    pub(super) fn repr(&self) -> String {
        if self.step == 1 {
            format!("range({}, {})", self.start, self.stop)
        } else {
            format!("range({}, {}, {})", self.start, self.stop, self.step)
        }
    }
}

impl Object for Range {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Seq
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let i = i128::try_from(key.clone()).ok()?;
        let len = self.len() as i128;
        let i = if i < 0 { i + len } else { i };
        (0..len)
            .contains(&i)
            .then(|| Value::from(i128::from(self.start) + i * i128::from(self.step)))
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Seq(self.len())
    }

    fn render(self: &Arc<Self>, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&Range::repr(self))
    }
}

/// `py_repr` of a range.
pub(super) fn repr_range<W: std::fmt::Write>(
    out: &mut super::Charged<'_, W>,
    v: &Value,
) -> Option<Result<(), Error>> {
    v.downcast_object_ref::<Range>().map(|r| out.put(&r.repr()))
}
