//! `Template._parse_result` (`helpers/template/__init__.py`): what a
//! rendered template becomes. HA runs Python's `ast.literal_eval` on the
//! string and keeps the value unless it is a `str` or `complex`, or a
//! number whose text isn't a plain decimal (`_IS_NUMERIC`); anything that
//! fails to parse stays the string. HA's WebSocket then serialises the
//! value with orjson (`helpers/json.py::json_bytes`), which fails for
//! bytes, `...`, complex numbers and integers outside 64 bits: HA then
//! sends `Invalid JSON in response` instead.
//!
//! The parser accepts Python's literal grammar: numbers (with `_`, hex,
//! octal, binary, exponents, `j`), strings and bytes (every quote style,
//! prefix, escape and implicit concatenation), `True`/`False`/`None`,
//! `...`, tuples, lists, dicts, sets and `set()`, one sign on a number and
//! `real ± imaginary`, comments, and newlines inside brackets.
//!
//! Deliberate differences: `\N{name}` escapes leave the result a string
//! (hars has no Unicode name table); a set is serialised in the order it
//! was written, where CPython uses hash order (random for strings); and
//! JSON objects come out with sorted keys, as everywhere in hars.

use serde_json::{Map, Number, Value};

/// A Python literal.
#[derive(Debug, Clone)]
pub(crate) enum Py {
    None,
    Bool(bool),
    Int(Int),
    Float(f64),
    Complex(f64, f64),
    /// `None` when it holds a lone surrogate, which a Rust `String` can't
    /// and orjson won't serialise.
    Str(Option<String>),
    Bytes(Vec<u8>),
    Ellipsis,
    List(Vec<Py>),
    Tuple(Vec<Py>),
    Set(Vec<Py>),
    Dict(Vec<(Py, Py)>),
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Int {
    Small(i128),
    /// Outside `i128`: never serialisable, never equal to anything here.
    Huge,
}

/// What a rendered template is, as HA's `render_template` sends it: the
/// string itself, or the value `literal_eval` made of it.
#[derive(Debug, Clone)]
pub struct Rendered(pub(crate) Parsed);

#[derive(Debug, Clone)]
pub(crate) enum Parsed {
    Text(String),
    Value(Py),
}

/// orjson can't serialise the value: HA sends `Invalid JSON in response`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unserializable;

impl Rendered {
    /// The value as JSON, with orjson's rules (`OPT_NON_STR_KEYS`, HA's
    /// default encoder turning sets and tuples into lists).
    pub fn to_json(&self) -> Result<Value, Unserializable> {
        match &self.0 {
            Parsed::Text(s) => Ok(Value::String(s.clone())),
            Parsed::Value(v) => to_json(v),
        }
    }

    /// The string, for a result that stayed one.
    pub fn as_text(&self) -> Option<&str> {
        match &self.0 {
            Parsed::Text(s) => Some(s),
            Parsed::Value(_) => None,
        }
    }
}

/// `Template._parse_result` of a rendered (already stripped) string.
pub fn parse_result(rendered: String) -> Rendered {
    Rendered(match literal_eval(&rendered) {
        Some(Py::Str(_) | Py::Complex(..)) | None => Parsed::Text(rendered),
        Some(Py::Int(_) | Py::Float(_)) if !is_numeric(&rendered) => Parsed::Text(rendered),
        Some(v) => Parsed::Value(v),
    })
}

impl Rendered {
    /// A result that is the string as rendered (a static template's).
    pub fn text(s: String) -> Rendered {
        Rendered(Parsed::Text(s))
    }
}

/// `_IS_NUMERIC`: `^[+-]?(?!0\d)\d*(?:\.\d*)?$`, where `$` also matches
/// before a final newline.
fn is_numeric(s: &str) -> bool {
    let s = s.strip_suffix('\n').unwrap_or(s);
    let s = s.strip_prefix(['+', '-']).unwrap_or(s);
    let b = s.as_bytes();
    if b.len() >= 2 && b[0] == b'0' && b[1].is_ascii_digit() {
        return false;
    }
    let int_end = b.iter().take_while(|c| c.is_ascii_digit()).count();
    match &b[int_end..] {
        [] => true,
        [b'.', rest @ ..] => rest.iter().all(u8::is_ascii_digit),
        _ => false,
    }
}

fn to_json(v: &Py) -> Result<Value, Unserializable> {
    Ok(match v {
        Py::None => Value::Null,
        Py::Bool(b) => Value::Bool(*b),
        Py::Int(Int::Small(i)) => {
            if let Ok(i) = i64::try_from(*i) {
                Value::from(i)
            } else if let Ok(u) = u64::try_from(*i) {
                Value::from(u)
            } else {
                return Err(Unserializable);
            }
        }
        // orjson writes NaN and infinities as null.
        Py::Float(f) => Number::from_f64(*f).map_or(Value::Null, Value::Number),
        Py::Str(Some(s)) => Value::String(s.clone()),
        Py::List(items) | Py::Tuple(items) | Py::Set(items) => {
            Value::Array(items.iter().map(to_json).collect::<Result<_, _>>()?)
        }
        Py::Dict(pairs) => {
            let mut m = Map::new();
            for (k, v) in pairs {
                m.insert(json_key(k)?, to_json(v)?);
            }
            Value::Object(m)
        }
        Py::Int(Int::Huge) | Py::Complex(..) | Py::Str(None) | Py::Bytes(_) | Py::Ellipsis => {
            return Err(Unserializable);
        }
    })
}

/// orjson's `OPT_NON_STR_KEYS`.
fn json_key(k: &Py) -> Result<String, Unserializable> {
    Ok(match k {
        Py::Str(Some(s)) => s.clone(),
        Py::Int(Int::Small(i)) => i.to_string(),
        Py::Float(f) => Number::from_f64(*f).map_or_else(|| "null".into(), |n| n.to_string()),
        Py::Bool(b) => b.to_string(),
        Py::None => "null".into(),
        _ => return Err(Unserializable),
    })
}

/// Python's `ast.literal_eval`, after its `lstrip(" \t")`. `None` for
/// anything it rejects (a syntax error or a node that isn't a literal).
pub(crate) fn literal_eval(s: &str) -> Option<Py> {
    let mut p = Parser {
        s: s.trim_start_matches([' ', '\t']).as_bytes(),
        i: 0,
        depth: 0,
    };
    if p.s.contains(&0) {
        return None;
    }
    p.skip_blank_lines();
    let v = p.expr_list()?;
    // Only newlines, blanks and comments may follow (`eval` input).
    p.skip_blank_lines();
    (p.i == p.s.len()).then_some(v)
}

/// CPython's tokenizer refuses deeper nesting (`MAXLEVEL`).
const MAX_DEPTH: usize = 200;
/// `sys.int_info.str_digits_check_threshold`'s default limit.
const MAX_INT_DIGITS: usize = 4300;

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    /// Open brackets: inside them newlines are only whitespace.
    depth: usize,
}

/// A parsed expression and whether it was a bare constant (what
/// `literal_eval`'s number rules care about).
struct Node {
    value: Py,
    constant: bool,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn peek_at(&self, n: usize) -> Option<u8> {
        self.s.get(self.i + n).copied()
    }

    /// Spaces, tabs, form feeds, comments, line continuations, and inside
    /// brackets newlines.
    fn skip_ws(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\x0c') => self.i += 1,
                Some(b'\\') if matches!(self.peek_at(1), Some(b'\n' | b'\r')) => {
                    self.i += 1;
                    self.newline();
                }
                Some(b'#') => {
                    while !matches!(self.peek(), None | Some(b'\n' | b'\r')) {
                        self.i += 1;
                    }
                }
                Some(b'\n' | b'\r') if self.depth > 0 => self.newline(),
                _ => return,
            }
        }
    }

    fn newline(&mut self) {
        if self.peek() == Some(b'\r') {
            self.i += 1;
        }
        if self.peek() == Some(b'\n') {
            self.i += 1;
        }
    }

    /// Blank lines and comments before or after the expression.
    fn skip_blank_lines(&mut self) {
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'\n' | b'\r') => self.newline(),
                _ => return,
            }
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.skip_ws();
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    /// The top level `a, b, …`: a tuple when there is a comma.
    fn expr_list(&mut self) -> Option<Py> {
        let first = self.expr()?;
        self.skip_ws();
        if self.peek() != Some(b',') {
            return Some(first.value);
        }
        let mut items = vec![first.value];
        while self.eat(b',') {
            self.skip_ws();
            match self.peek() {
                None | Some(b'\n' | b'\r') => break,
                _ => items.push(self.expr()?.value),
            }
        }
        Some(Py::Tuple(items))
    }

    /// One element: a signed number, `real ± imag`, or an atom.
    fn expr(&mut self) -> Option<Node> {
        let left = self.term()?;
        self.skip_ws();
        let Some(op @ (b'+' | b'-')) = self.peek() else {
            return Some(left);
        };
        self.i += 1;
        let right = self.term()?;
        // `_convert_signed_num(left)` and `_convert_num(right)`: only a
        // real number and an imaginary constant combine, and only once.
        let re = match left.value {
            Py::Int(Int::Small(i)) => i as f64,
            Py::Float(f) => f,
            _ => return None,
        };
        // An imaginary constant: its real part is 0.
        let im = match right.value {
            Py::Complex(_, i) if right.constant => i,
            _ => return None,
        };
        let im = if op == b'+' { im } else { -im };
        self.skip_ws();
        if matches!(self.peek(), Some(b'+' | b'-')) {
            return None;
        }
        Some(Node {
            value: Py::Complex(re, im),
            constant: false,
        })
    }

    /// An atom, or one `+`/`-` applied to a number constant.
    fn term(&mut self) -> Option<Node> {
        self.skip_ws();
        let Some(sign @ (b'+' | b'-')) = self.peek() else {
            return self.atom();
        };
        self.i += 1;
        let operand = self.atom()?;
        if !operand.constant {
            return None;
        }
        let neg = sign == b'-';
        let value = match operand.value {
            Py::Int(Int::Small(i)) => Py::Int(Int::Small(if neg { -i } else { i })),
            Py::Int(Int::Huge) => Py::Int(Int::Huge),
            Py::Float(f) => Py::Float(if neg { -f } else { f }),
            Py::Complex(r, i) => {
                if neg {
                    Py::Complex(-r, -i)
                } else {
                    Py::Complex(r, i)
                }
            }
            _ => return None,
        };
        Some(Node {
            value,
            constant: false,
        })
    }

    fn atom(&mut self) -> Option<Node> {
        self.skip_ws();
        let c = self.peek()?;
        let value = match c {
            b'(' | b'[' | b'{' => {
                self.depth += 1;
                if self.depth > MAX_DEPTH {
                    return None;
                }
                self.i += 1;
                let v = match c {
                    b'(' => self.paren()?,
                    b'[' => self.list()?,
                    _ => self.brace()?,
                };
                self.depth -= 1;
                // A parenthesised constant is still that constant.
                return Some(match v {
                    Paren::Inner(node) => node,
                    Paren::Built(value) => Node {
                        value,
                        constant: false,
                    },
                });
            }
            b'0'..=b'9' => self.number()?,
            b'.' if self.peek_at(1).is_some_and(|d| d.is_ascii_digit()) => self.number()?,
            b'.' if self.s[self.i..].starts_with(b"...") => {
                self.i += 3;
                Py::Ellipsis
            }
            b'\'' | b'"' => self.strings()?,
            c if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 => self.name()?,
            _ => return None,
        };
        Some(Node {
            value,
            constant: true,
        })
    }

    fn paren(&mut self) -> Option<Paren> {
        if self.eat(b')') {
            return Some(Paren::Built(Py::Tuple(Vec::new())));
        }
        let first = self.expr()?;
        self.skip_ws();
        if self.eat(b')') {
            return Some(Paren::Inner(first));
        }
        if self.peek() != Some(b',') {
            return None;
        }
        let mut items = vec![first.value];
        while self.eat(b',') {
            if self.eat(b')') {
                return Some(Paren::Built(Py::Tuple(items)));
            }
            items.push(self.expr()?.value);
        }
        self.eat(b')').then_some(Paren::Built(Py::Tuple(items)))
    }

    fn list(&mut self) -> Option<Paren> {
        let mut items = Vec::new();
        loop {
            if self.eat(b']') {
                return Some(Paren::Built(Py::List(items)));
            }
            items.push(self.expr()?.value);
            if !self.eat(b',') {
                return self.eat(b']').then_some(Paren::Built(Py::List(items)));
            }
        }
    }

    fn brace(&mut self) -> Option<Paren> {
        if self.eat(b'}') {
            return Some(Paren::Built(Py::Dict(Vec::new())));
        }
        let first = self.expr()?.value;
        if self.eat(b':') {
            let mut pairs = vec![(first, self.expr()?.value)];
            loop {
                if !self.eat(b',') {
                    break;
                }
                if self.eat(b'}') {
                    return dict(pairs);
                }
                let k = self.expr()?.value;
                if !self.eat(b':') {
                    return None;
                }
                pairs.push((k, self.expr()?.value));
            }
            return if self.eat(b'}') { dict(pairs) } else { None };
        }
        let mut items = vec![first];
        loop {
            if !self.eat(b',') {
                break;
            }
            if self.eat(b'}') {
                return set(items);
            }
            items.push(self.expr()?.value);
        }
        if self.eat(b'}') { set(items) } else { None }
    }

    /// `True`, `False`, `None` and `set()`; any other name isn't a literal.
    /// A name directly followed by a quote is a string prefix.
    fn name(&mut self) -> Option<Py> {
        let start = self.i;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80)
        {
            self.i += 1;
        }
        let word = &self.s[start..self.i];
        if matches!(self.peek(), Some(b'\'' | b'"')) {
            self.i = start;
            return self.strings();
        }
        match word {
            b"True" => Some(Py::Bool(true)),
            b"False" => Some(Py::Bool(false)),
            b"None" => Some(Py::None),
            b"set" => {
                if self.eat(b'(') {
                    self.depth += 1;
                    let closed = self.eat(b')');
                    self.depth -= 1;
                    closed.then_some(Py::Set(Vec::new()))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// A number token, by Python's lexical rules.
    fn number(&mut self) -> Option<Py> {
        let s = self.s;
        let start = self.i;
        // `0x…`, `0o…`, `0b…`.
        if s[start] == b'0'
            && let Some(radix) = match self.peek_at(1).map(|c| c.to_ascii_lowercase()) {
                Some(b'x') => Some(16),
                Some(b'o') => Some(8),
                Some(b'b') => Some(2),
                _ => None,
            }
        {
            self.i += 2;
            let mut digits = String::new();
            loop {
                if self.peek() == Some(b'_') {
                    self.i += 1;
                }
                match self.peek().and_then(|c| (c as char).to_digit(radix)) {
                    Some(_) => {
                        digits.push(self.s[self.i] as char);
                        self.i += 1;
                    }
                    None if digits.is_empty() || self.s[self.i - 1] == b'_' => return None,
                    None => break,
                }
            }
            self.after_number()?;
            return Some(Py::Int(
                i128::from_str_radix(&digits, radix).map_or(Int::Huge, Int::Small),
            ));
        }
        let int_part = self.digit_part();
        let mut is_float = false;
        let mut text = int_part.clone()?;
        if self.peek() == Some(b'.') {
            self.i += 1;
            is_float = true;
            text.push('.');
            if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                text.push_str(&self.digit_part()?);
            } else if int_part.as_deref().is_none_or(str::is_empty) {
                return None;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let save = self.i;
            self.i += 1;
            let mut exp = String::from("e");
            if let Some(sign @ (b'+' | b'-')) = self.peek() {
                exp.push(sign as char);
                self.i += 1;
            }
            match self.peek() {
                Some(c) if c.is_ascii_digit() => {
                    exp.push_str(&self.digit_part()?);
                    text.push_str(&exp);
                    is_float = true;
                }
                // `1e` is `1` then the name `e`: not a literal anyway.
                _ => {
                    self.i = save;
                    return None;
                }
            }
        }
        if matches!(self.peek(), Some(b'j' | b'J')) {
            self.i += 1;
            self.after_number()?;
            return Some(Py::Complex(0.0, text.parse().ok()?));
        }
        self.after_number()?;
        if is_float {
            return text.parse().ok().map(Py::Float);
        }
        // Decimal integers: no leading zeros unless all zeros.
        if text.len() > 1 && text.starts_with('0') && text.bytes().any(|c| c != b'0') {
            return None;
        }
        if text.len() > MAX_INT_DIGITS {
            return None;
        }
        Some(Py::Int(text.parse().map_or(Int::Huge, Int::Small)))
    }

    /// `digit (["_"] digit)*`, without the underscores. `Some("")` when
    /// there are no digits (`.5`).
    fn digit_part(&mut self) -> Option<String> {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                out.push(c as char);
                self.i += 1;
            } else if c == b'_'
                && !out.is_empty()
                && self.peek_at(1).is_some_and(|d| d.is_ascii_digit())
            {
                self.i += 1;
            } else if c == b'_' {
                return None;
            } else {
                break;
            }
        }
        Some(out)
    }

    /// A number can't run straight into a name or another number.
    fn after_number(&self) -> Option<()> {
        match self.peek() {
            Some(c) if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c >= 0x80 => None,
            _ => Some(()),
        }
    }

    /// One or more adjacent string (or bytes) literals.
    fn strings(&mut self) -> Option<Py> {
        let mut text: Option<String> = Some(String::new());
        let mut bytes: Option<Vec<u8>> = None;
        let mut any = false;
        loop {
            self.skip_ws();
            let start = self.i;
            while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                self.i += 1;
            }
            let prefix = self.s[start..self.i].to_ascii_lowercase();
            if !matches!(self.peek(), Some(b'\'' | b'"')) {
                self.i = start;
                break;
            }
            let (raw, is_bytes) = match prefix.as_slice() {
                b"" | b"u" => (false, false),
                b"r" => (true, false),
                b"b" => (false, true),
                b"br" | b"rb" => (true, true),
                // f-strings and t-strings aren't constants.
                _ => return None,
            };
            if any && is_bytes != bytes.is_some() {
                return None;
            }
            any = true;
            let lit = self.string_body(raw, is_bytes)?;
            match lit {
                Lit::Text(s) => {
                    if let (Some(t), Some(s)) = (&mut text, s) {
                        t.push_str(&s);
                    } else {
                        text = None;
                    }
                }
                Lit::Bytes(b) => bytes.get_or_insert_with(Vec::new).extend(b),
            }
        }
        if !any {
            return None;
        }
        Some(match bytes {
            Some(b) => Py::Bytes(b),
            None => Py::Str(text),
        })
    }

    fn string_body(&mut self, raw: bool, is_bytes: bool) -> Option<Lit> {
        let q = self.peek()?;
        let triple = self.peek_at(1) == Some(q) && self.peek_at(2) == Some(q);
        self.i += if triple { 3 } else { 1 };
        let start = self.i;
        let end;
        // Find the closing quote first; escapes are decoded after.
        loop {
            match self.peek()? {
                b'\\' => {
                    self.i += 1;
                    self.peek()?;
                    self.i += 1;
                }
                c if c == q
                    && (!triple || (self.peek_at(1) == Some(q) && self.peek_at(2) == Some(q))) =>
                {
                    end = self.i;
                    self.i += if triple { 3 } else { 1 };
                    break;
                }
                b'\n' | b'\r' if !triple => return None,
                _ => self.i += 1,
            }
        }
        // The source is UTF-8 (it came from a `str`).
        let body = std::str::from_utf8(&self.s[start..end]).ok()?;
        if is_bytes {
            if !body.is_ascii() {
                return None;
            }
            return Some(Lit::Bytes(if raw {
                body.as_bytes().to_vec()
            } else {
                unescape(body, true)?.into_iter().map(|c| c as u8).collect()
            }));
        }
        if raw {
            return Some(Lit::Text(Some(body.to_owned())));
        }
        let chars = unescape(body, false)?;
        Some(Lit::Text(
            chars
                .into_iter()
                .map(char::from_u32)
                .collect::<Option<String>>(),
        ))
    }
}

enum Paren {
    /// `(x)`: the node inside, unchanged.
    Inner(Node),
    Built(Py),
}

enum Lit {
    /// `None` when it holds a lone surrogate.
    Text(Option<String>),
    Bytes(Vec<u8>),
}

/// Python's escape sequences, as code points (bytes for a bytes literal).
fn unescape(body: &str, is_bytes: bool) -> Option<Vec<u32>> {
    let mut out = Vec::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c as u32);
            continue;
        }
        let e = chars.next()?;
        match e {
            '\n' => {}
            '\r' => {
                chars.next_if_eq(&'\n');
            }
            '\\' | '\'' | '"' => out.push(e as u32),
            'a' => out.push(7),
            'b' => out.push(8),
            'f' => out.push(0x0c),
            'n' => out.push(0x0a),
            'r' => out.push(0x0d),
            't' => out.push(9),
            'v' => out.push(0x0b),
            '0'..='7' => {
                let mut v = e.to_digit(8)?;
                for _ in 0..2 {
                    match chars.peek().and_then(|d| d.to_digit(8)) {
                        Some(d) => {
                            v = v * 8 + d;
                            chars.next();
                        }
                        None => break,
                    }
                }
                out.push(if is_bytes { v & 0xff } else { v });
            }
            'x' => out.push(hex(&mut chars, 2)?),
            'u' if !is_bytes => out.push(hex(&mut chars, 4)?),
            'U' if !is_bytes => {
                let v = hex(&mut chars, 8)?;
                if v > 0x10ffff {
                    return None;
                }
                out.push(v);
            }
            // Needs Unicode's name table.
            'N' if !is_bytes => return None,
            // Unknown escapes stay as written.
            e => {
                out.push('\\' as u32);
                out.push(e as u32);
            }
        }
    }
    Some(out)
}

fn hex(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, n: usize) -> Option<u32> {
    let mut v = 0;
    for _ in 0..n {
        v = v * 16 + chars.next()?.to_digit(16)?;
    }
    Some(v)
}

/// `dict(...)`: later values win, the first key stays; unhashable keys
/// are a `TypeError`.
fn dict(pairs: Vec<(Py, Py)>) -> Option<Paren> {
    let mut out: Vec<(Py, Py)> = Vec::with_capacity(pairs.len());
    for (k, v) in pairs {
        if !hashable(&k) {
            return None;
        }
        match out.iter_mut().find(|(have, _)| py_eq(have, &k)) {
            Some(slot) => slot.1 = v,
            None => out.push((k, v)),
        }
    }
    Some(Paren::Built(Py::Dict(out)))
}

fn set(items: Vec<Py>) -> Option<Paren> {
    let mut out: Vec<Py> = Vec::with_capacity(items.len());
    for item in items {
        if !hashable(&item) {
            return None;
        }
        if !out.iter().any(|have| py_eq(have, &item)) {
            out.push(item);
        }
    }
    Some(Paren::Built(Py::Set(out)))
}

fn hashable(v: &Py) -> bool {
    match v {
        Py::List(_) | Py::Set(_) | Py::Dict(_) => false,
        Py::Tuple(items) => items.iter().all(hashable),
        _ => true,
    }
}

/// Python's `==` between hashable literals (`1 == 1.0 == True`).
fn py_eq(a: &Py, b: &Py) -> bool {
    fn int(v: &Py) -> Option<i128> {
        match v {
            Py::Bool(b) => Some(i128::from(*b)),
            Py::Int(Int::Small(i)) => Some(*i),
            _ => None,
        }
    }
    /// (real, imaginary).
    fn number(v: &Py) -> Option<(f64, f64)> {
        match v {
            Py::Float(f) => Some((*f, 0.0)),
            Py::Complex(r, i) => Some((*r, *i)),
            v => int(v).map(|i| (i as f64, 0.0)),
        }
    }
    match (a, b) {
        (Py::None, Py::None) | (Py::Ellipsis, Py::Ellipsis) => true,
        (Py::Str(Some(x)), Py::Str(Some(y))) => x == y,
        (Py::Bytes(x), Py::Bytes(y)) => x == y,
        (Py::Tuple(x), Py::Tuple(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| py_eq(a, b))
        }
        _ => match (int(a), int(b)) {
            (Some(x), Some(y)) => x == y,
            _ => number(a).is_some_and(|x| number(b) == Some(x)),
        },
    }
}
