//! The `lock` entity component (`components/lock/__init__.py` and
//! `components/lock/const.py`): `lock.lock`, `lock.unlock`, and `lock.open`,
//! with HA's optional `code` parameter and `code_format` validation.

use crate::light::BoxFuture;
use crate::switch::select;
use ha_core::{EntityId, EntityTarget, Hass, Kind, Schema, ServiceError, SupportsResponse};
use parking_lot::RwLock;
use serde_json::{Map, Value, json};
use std::sync::Arc;

pub const DOMAIN: &str = "lock";

/// `LockEntityFeature` (`components/lock/__init__.py`).
pub mod feature {
    pub const OPEN: u32 = 1;
}

/// A method the entity doesn't implement (Python's `NotImplementedError`).
fn unsupported<'a>() -> BoxFuture<'a> {
    Box::pin(async { Err(ServiceError::Unknown(String::new())) })
}

/// A lock entity a platform runs (`LockEntity`).
pub trait Lock: Send + Sync {
    fn entity_id(&self) -> &EntityId;
    fn available(&self) -> bool;
    fn supported_features(&self) -> u32 {
        0
    }
    /// Optional regex string (Python `re.match` semantics) validating `code`.
    fn code_format(&self) -> Option<String> {
        None
    }
    /// Optional default code applied when the caller passes no code.
    fn default_code(&self) -> Option<String> {
        None
    }

    fn lock(&self, code: Option<&str>) -> BoxFuture<'_>;
    fn unlock(&self, code: Option<&str>) -> BoxFuture<'_>;
    fn open(&self, _code: Option<&str>) -> BoxFuture<'_> {
        unsupported()
    }
}

/// The running lock entities, for the services.
#[derive(Default)]
pub struct Locks {
    items: RwLock<Vec<Arc<dyn Lock>>>,
}

fn chosen(
    items: &[Arc<dyn Lock>],
    target: &EntityTarget,
    indirect: &[String],
    required: u32,
    service: &str,
) -> Result<Vec<Arc<dyn Lock>>, ServiceError> {
    let mut out = Vec::new();
    for l in select(
        items,
        |l| l.entity_id(),
        |l| l.available(),
        target,
        indirect,
    ) {
        if required != 0 && (l.supported_features() & required) != required {
            if let EntityTarget::Ids(ids) = target
                && ids.contains(l.entity_id())
            {
                return Err(ServiceError::not_supported(
                    DOMAIN,
                    service,
                    l.entity_id().as_str(),
                ));
            }
            continue;
        }
        out.push(l);
    }
    Ok(out)
}

/// HA's `LockEntity.add_default_code`.
fn resolve_code(lock: &dyn Lock, raw: Option<&str>) -> Result<Option<String>, ServiceError> {
    let mut code = raw.unwrap_or("").to_owned();
    if code.is_empty()
        && let Some(default) = lock.default_code()
    {
        code = default;
    }
    if let Some(fmt) = lock.code_format()
        && !code_format_matches(&fmt, &code)
    {
        let mut p = Map::new();
        p.insert("entity_id".into(), json!(lock.entity_id().as_str()));
        p.insert("code_format".into(), json!(fmt));
        return Err(ServiceError::validation(
            DOMAIN,
            "add_default_code",
            Some(p),
            format!(
                "The code for {} doesn't match pattern {fmt}",
                lock.entity_id().as_str()
            ),
        ));
    }
    Ok((!code.is_empty()).then_some(code))
}

impl Locks {
    pub fn add(&self, l: Arc<dyn Lock>) {
        self.items.write().push(l);
    }

    pub fn remove(&self, entity_id: &EntityId) {
        self.items.write().retain(|l| l.entity_id() != entity_id);
    }

    /// Register HA's `lock.unlock`, `lock.lock`, and `lock.open` services.
    pub fn register_services(
        self: &Arc<Self>,
        hass: &Hass,
        registries: impl Into<ha_registry::Lookup>,
    ) {
        let registries = registries.into();
        let schema = || Schema::entity_service().optional("code", Kind::String);
        let services: [(&'static str, u32); 3] =
            [("unlock", 0), ("lock", 0), ("open", feature::OPEN)];

        for (name, required) in services {
            let locks = self.clone();
            let registries = registries.clone();
            hass.services().register_with_schema(
                DOMAIN,
                name,
                Some(schema()),
                SupportsResponse::None,
                move |call| {
                    let locks = locks.clone();
                    let registries = registries.clone();
                    async move {
                        let target = call.entity_ids()?;
                        let indirect = registries.get().target_entities(&call.data);
                        let items = locks.items.read().clone();
                        let chosen = chosen(&items, &target, &indirect, required, name)?;
                        let raw_code = call.data.get("code").and_then(Value::as_str);
                        let calls = chosen.iter().map(|l| async move {
                            let code = resolve_code(l.as_ref(), raw_code)?;
                            match name {
                                "lock" => l.lock(code.as_deref()).await,
                                "unlock" => l.unlock(code.as_deref()).await,
                                "open" => l.open(code.as_deref()).await,
                                _ => Err(ServiceError::Unknown(format!(
                                    "lock.{name} is not supported"
                                ))),
                            }
                        });
                        crate::light::join_all(calls)
                            .await
                            .into_iter()
                            .collect::<Result<Vec<()>, _>>()?;
                        Ok(None)
                    }
                },
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Zero-dependency Thompson NFA matcher for `code_format` (Python `re.match`)
// ---------------------------------------------------------------------------

/// Match `input` against `pattern` with Python `re.match` semantics (anchored
/// at index 0; unanchored at the end unless `$` or `\Z` is present).
///
/// Uses a bounded Thompson NFA so matching runs in O(|states| * |input|) time
/// with bounded parse depth, bounded compile work, iterative epsilon-closure
/// (no call-stack blowup), and no external regex crate dependency.
pub fn code_format_matches(pattern: &str, input: &str) -> bool {
    if pattern.len() > MAX_PATTERN_BYTES {
        return false;
    }
    let Ok(prog) = compile(pattern) else {
        return false;
    };
    run_nfa(&prog, input)
}

const MAX_PATTERN_BYTES: usize = 512;
const MAX_PARSE_DEPTH: usize = 32;
const MAX_INSTS: usize = 2048;
const MAX_COMPILE_STEPS: usize = 4096;

/// Base codepoints of every 10-character Unicode Decimal_Number (`Nd`) block
/// (Python 3 `re` `\d` without `(?a)`). Sorted for binary search.
const UNICODE_ND_BASES: [u32; 76] = [
    0x0030, 0x0660, 0x06F0, 0x07C0, 0x0966, 0x09E6, 0x0A66, 0x0AE6, 0x0B66, 0x0BE6, 0x0C66, 0x0CE6,
    0x0D66, 0x0DE6, 0x0E50, 0x0ED0, 0x0F20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80,
    0x1A90, 0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0,
    0xFF10, 0x104A0, 0x10D30, 0x10D40, 0x11066, 0x110F0, 0x11136, 0x111D0, 0x112F0, 0x11450,
    0x114D0, 0x11650, 0x116C0, 0x116D0, 0x116DA, 0x11730, 0x118E0, 0x11950, 0x11BF0, 0x11C50,
    0x11D50, 0x11DA0, 0x11F50, 0x16130, 0x16A60, 0x16AC0, 0x16B50, 0x16D70, 0x1CCF0, 0x1D7CE,
    0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0, 0x1E5F1, 0x1E950, 0x1FBF0,
];

fn is_unicode_decimal_digit(c: char) -> bool {
    if c.is_ascii_digit() {
        return true;
    }
    let cp = u32::from(c);
    let idx = UNICODE_ND_BASES.partition_point(|&b| b <= cp);
    idx > 0 && cp < UNICODE_ND_BASES[idx - 1] + 10
}

#[derive(Clone, Copy, Default)]
struct Flags {
    ignore_case: bool,
    ascii: bool,
}

/// Return the exact Python 3.14 `re` `(?i)` Unicode case-equivalence class for
/// `c` (1..=4 characters, always including `c` itself).
///
/// Covers every codepoint whose Python `re` equivalence class differs from
/// `{c, single(c.to_lowercase()), single(c.to_uppercase())}`: Python's
/// `re._casefix._EXTRA_CASES`, Unicode titlecase digraphs, compatibility
/// uppercase signs (`K`, `Å`, `Ω`), `ß`/`ẞ`, and the Greek iota-subscript
/// lowercase/titlecase pairs.
fn py_unicode_case_class(c: char) -> ([char; 4], usize) {
    // 24 contiguous Greek iota-subscript pairs: U+1F80..=1F87 <-> +8,
    // U+1F90..=1F97 <-> +8, U+1FA0..=1FA7 <-> +8.
    let cp = u32::from(c);
    if matches!(cp, 0x1F80..=0x1F87 | 0x1F90..=0x1F97 | 0x1FA0..=0x1FA7) {
        return ([c, char::from_u32(cp + 8).unwrap_or(c), '\0', '\0'], 2);
    }
    if matches!(cp, 0x1F88..=0x1F8F | 0x1F98..=0x1F9F | 0x1FA8..=0x1FAF) {
        return ([char::from_u32(cp - 8).unwrap_or(c), c, '\0', '\0'], 2);
    }
    match c {
        'I' | 'i' | '\u{0130}' | '\u{0131}' => (['I', 'i', '\u{0130}', '\u{0131}'], 4),
        'K' | 'k' | '\u{212A}' => (['K', 'k', '\u{212A}', '\0'], 3),
        'S' | 's' | '\u{017F}' => (['S', 's', '\u{017F}', '\0'], 3),
        '\u{00B5}' | '\u{039C}' | '\u{03BC}' => (['\u{00B5}', '\u{039C}', '\u{03BC}', '\0'], 3),
        '\u{00DF}' | '\u{1E9E}' => (['\u{00DF}', '\u{1E9E}', '\0', '\0'], 2),
        '\u{00C5}' | '\u{00E5}' | '\u{212B}' => (['\u{00C5}', '\u{00E5}', '\u{212B}', '\0'], 3),
        '\u{01C4}' | '\u{01C5}' | '\u{01C6}' => (['\u{01C4}', '\u{01C5}', '\u{01C6}', '\0'], 3),
        '\u{01C7}' | '\u{01C8}' | '\u{01C9}' => (['\u{01C7}', '\u{01C8}', '\u{01C9}', '\0'], 3),
        '\u{01CA}' | '\u{01CB}' | '\u{01CC}' => (['\u{01CA}', '\u{01CB}', '\u{01CC}', '\0'], 3),
        '\u{01F1}' | '\u{01F2}' | '\u{01F3}' => (['\u{01F1}', '\u{01F2}', '\u{01F3}', '\0'], 3),
        '\u{0345}' | '\u{0399}' | '\u{03B9}' | '\u{1FBE}' => {
            (['\u{0345}', '\u{0399}', '\u{03B9}', '\u{1FBE}'], 4)
        }
        '\u{0390}' | '\u{1FD3}' => (['\u{0390}', '\u{1FD3}', '\0', '\0'], 2),
        '\u{03B0}' | '\u{1FE3}' => (['\u{03B0}', '\u{1FE3}', '\0', '\0'], 2),
        '\u{0392}' | '\u{03B2}' | '\u{03D0}' => (['\u{0392}', '\u{03B2}', '\u{03D0}', '\0'], 3),
        '\u{0395}' | '\u{03B5}' | '\u{03F5}' => (['\u{0395}', '\u{03B5}', '\u{03F5}', '\0'], 3),
        '\u{0398}' | '\u{03B8}' | '\u{03D1}' | '\u{03F4}' => {
            (['\u{0398}', '\u{03B8}', '\u{03D1}', '\u{03F4}'], 4)
        }
        '\u{039A}' | '\u{03BA}' | '\u{03F0}' => (['\u{039A}', '\u{03BA}', '\u{03F0}', '\0'], 3),
        '\u{03A0}' | '\u{03C0}' | '\u{03D6}' => (['\u{03A0}', '\u{03C0}', '\u{03D6}', '\0'], 3),
        '\u{03A1}' | '\u{03C1}' | '\u{03F1}' => (['\u{03A1}', '\u{03C1}', '\u{03F1}', '\0'], 3),
        '\u{03A3}' | '\u{03C2}' | '\u{03C3}' => (['\u{03A3}', '\u{03C2}', '\u{03C3}', '\0'], 3),
        '\u{03A6}' | '\u{03C6}' | '\u{03D5}' => (['\u{03A6}', '\u{03C6}', '\u{03D5}', '\0'], 3),
        '\u{03A9}' | '\u{03C9}' | '\u{2126}' => (['\u{03A9}', '\u{03C9}', '\u{2126}', '\0'], 3),
        '\u{0412}' | '\u{0432}' | '\u{1C80}' => (['\u{0412}', '\u{0432}', '\u{1C80}', '\0'], 3),
        '\u{0414}' | '\u{0434}' | '\u{1C81}' => (['\u{0414}', '\u{0434}', '\u{1C81}', '\0'], 3),
        '\u{041E}' | '\u{043E}' | '\u{1C82}' => (['\u{041E}', '\u{043E}', '\u{1C82}', '\0'], 3),
        '\u{0421}' | '\u{0441}' | '\u{1C83}' => (['\u{0421}', '\u{0441}', '\u{1C83}', '\0'], 3),
        '\u{0422}' | '\u{0442}' | '\u{1C84}' | '\u{1C85}' => {
            (['\u{0422}', '\u{0442}', '\u{1C84}', '\u{1C85}'], 4)
        }
        '\u{042A}' | '\u{044A}' | '\u{1C86}' => (['\u{042A}', '\u{044A}', '\u{1C86}', '\0'], 3),
        '\u{0462}' | '\u{0463}' | '\u{1C87}' => (['\u{0462}', '\u{0463}', '\u{1C87}', '\0'], 3),
        '\u{1C88}' | '\u{A64A}' | '\u{A64B}' => (['\u{1C88}', '\u{A64A}', '\u{A64B}', '\0'], 3),
        '\u{1E60}' | '\u{1E61}' | '\u{1E9B}' => (['\u{1E60}', '\u{1E61}', '\u{1E9B}', '\0'], 3),
        '\u{1FB3}' | '\u{1FBC}' => (['\u{1FB3}', '\u{1FBC}', '\0', '\0'], 2),
        '\u{1FC3}' | '\u{1FCC}' => (['\u{1FC3}', '\u{1FCC}', '\0', '\0'], 2),
        '\u{1FF3}' | '\u{1FFC}' => (['\u{1FF3}', '\u{1FFC}', '\0', '\0'], 2),
        '\u{FB05}' | '\u{FB06}' => (['\u{FB05}', '\u{FB06}', '\0', '\0'], 2),
        _ => {
            let mut out = [c, '\0', '\0', '\0'];
            let mut len = 1;
            let mut lc = c.to_lowercase();
            if let (Some(l), None) = (lc.next(), lc.next())
                && l != c
            {
                out[len] = l;
                len += 1;
            }
            let mut uc = c.to_uppercase();
            if let (Some(u), None) = (uc.next(), uc.next())
                && !out[..len].contains(&u)
            {
                out[len] = u;
                len += 1;
            }
            (out, len)
        }
    }
}

fn char_eq(a: char, b: char, flags: Flags) -> bool {
    if !flags.ignore_case {
        return a == b;
    }
    if flags.ascii {
        return a.eq_ignore_ascii_case(&b);
    }
    if a == b {
        return true;
    }
    let (cls, len) = py_unicode_case_class(a);
    cls[..len].contains(&b)
}

fn char_in_range(c: char, lo: char, hi: char, flags: Flags) -> bool {
    if lo <= c && c <= hi {
        return true;
    }
    if !flags.ignore_case {
        return false;
    }
    if flags.ascii {
        if c.is_ascii_alphabetic() {
            let low = c.to_ascii_lowercase();
            let up = c.to_ascii_uppercase();
            return (lo <= low && low <= hi) || (lo <= up && up <= hi);
        }
        return false;
    }
    let (cls, len) = py_unicode_case_class(c);
    cls[..len].iter().any(|&x| lo <= x && x <= hi)
}

#[derive(Clone)]
enum CharMatcher {
    Literal {
        ch: char,
        flags: Flags,
    },
    AnyNonNewline,
    Class {
        negated: bool,
        flags: Flags,
        items: Vec<ClassItem>,
    },
}

#[derive(Clone)]
enum ClassItem {
    Single(char),
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

impl CharMatcher {
    fn matches(&self, c: char) -> bool {
        match self {
            CharMatcher::Literal { ch, flags } => char_eq(c, *ch, *flags),
            CharMatcher::AnyNonNewline => c != '\n',
            CharMatcher::Class {
                negated,
                flags,
                items,
            } => {
                let hit = items.iter().any(|item| match *item {
                    ClassItem::Single(x) => char_eq(c, x, *flags),
                    ClassItem::Range(a, b) => char_in_range(c, a, b, *flags),
                    ClassItem::Digit(inv) => {
                        let d = if flags.ascii {
                            c.is_ascii_digit()
                        } else {
                            is_unicode_decimal_digit(c)
                        };
                        d ^ inv
                    }
                    ClassItem::Word(inv) => {
                        let w = if flags.ascii {
                            c.is_ascii_alphanumeric() || c == '_'
                        } else {
                            c.is_alphanumeric() || c == '_'
                        };
                        w ^ inv
                    }
                    ClassItem::Space(inv) => {
                        let s = if flags.ascii {
                            c.is_ascii_whitespace()
                        } else {
                            c.is_whitespace()
                        };
                        s ^ inv
                    }
                });
                hit ^ *negated
            }
        }
    }
}

#[derive(Clone)]
enum Inst {
    Char(CharMatcher),
    StartAnchor,
    EndAnchor,
    EndStrictAnchor,
    Split(usize, usize),
    Jump(usize),
    Match,
}

#[derive(Clone)]
enum Ast {
    Empty,
    Atom(CharMatcher),
    StartAnchor,
    EndAnchor,
    EndStrictAnchor,
    Seq(Vec<Ast>),
    Alt(Vec<Ast>),
    Repeat {
        node: Box<Ast>,
        min: usize,
        max: Option<usize>,
    },
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    depth: usize,
    flags: Flags,
}

impl Parser {
    fn new(raw: &str) -> Self {
        Self {
            chars: raw.chars().collect(),
            pos: 0,
            depth: 0,
            flags: Flags::default(),
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        Some(c)
    }

    fn parse_alt(&mut self) -> Result<Ast, ()> {
        if self.depth >= MAX_PARSE_DEPTH {
            return Err(());
        }
        self.depth += 1;
        let res = (|| {
            let mut branches = vec![self.parse_seq()?];
            while self.peek() == Some('|') {
                self.next();
                branches.push(self.parse_seq()?);
            }
            Ok(if branches.len() == 1 {
                branches.pop().unwrap()
            } else {
                Ast::Alt(branches)
            })
        })();
        self.depth -= 1;
        res
    }

    fn parse_seq(&mut self) -> Result<Ast, ()> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.parse_atom()?;
            let quant = self.parse_quantifier(atom)?;
            items.push(quant);
        }
        Ok(match items.len() {
            0 => Ast::Empty,
            1 => items.pop().unwrap(),
            _ => Ast::Seq(items),
        })
    }

    fn parse_atom(&mut self) -> Result<Ast, ()> {
        let Some(c) = self.next() else {
            return Err(());
        };
        match c {
            '^' => Ok(Ast::StartAnchor),
            '$' => Ok(Ast::EndAnchor),
            '.' => Ok(Ast::Atom(CharMatcher::AnyNonNewline)),
            '(' => self.parse_group(),
            '[' => self.parse_class(),
            '\\' => self.parse_escape(),
            '*' | '+' | '?' | '{' => Err(()),
            lit => Ok(Ast::Atom(CharMatcher::Literal {
                ch: lit,
                flags: self.flags,
            })),
        }
    }

    /// Parse `(...)`, `(?:...)`, global inline flags `(?i)` / `(?a)` / `(?u)`,
    /// and scoped inline flags `(?i:...)` / `(?-i:...)`.
    fn parse_group(&mut self) -> Result<Ast, ()> {
        if self.peek() == Some('?') {
            self.next();
            let mut new_flags = self.flags;
            let mut clearing = false;
            loop {
                match self.peek() {
                    Some(':') => {
                        self.next();
                        let saved = self.flags;
                        self.flags = new_flags;
                        let inner = self.parse_alt();
                        self.flags = saved;
                        let inner = inner?;
                        if self.next() != Some(')') {
                            return Err(());
                        }
                        return Ok(inner);
                    }
                    Some(')') if !clearing => {
                        self.next();
                        self.flags = new_flags;
                        return Ok(Ast::Empty);
                    }
                    Some('-') if !clearing => {
                        self.next();
                        clearing = true;
                    }
                    Some('i') => {
                        self.next();
                        new_flags.ignore_case = !clearing;
                    }
                    Some('a') if !clearing => {
                        self.next();
                        new_flags.ascii = true;
                    }
                    Some('u') if !clearing => {
                        self.next();
                        new_flags.ascii = false;
                    }
                    _ => return Err(()),
                }
            }
        }
        let inner = self.parse_alt()?;
        if self.next() != Some(')') {
            return Err(());
        }
        Ok(inner)
    }

    fn parse_escape(&mut self) -> Result<Ast, ()> {
        let Some(c) = self.next() else {
            return Err(());
        };
        let flags = self.flags;
        match c {
            'd' => Ok(Ast::Atom(CharMatcher::Class {
                negated: false,
                flags,
                items: vec![ClassItem::Digit(false)],
            })),
            'D' => Ok(Ast::Atom(CharMatcher::Class {
                negated: false,
                flags,
                items: vec![ClassItem::Digit(true)],
            })),
            'w' => Ok(Ast::Atom(CharMatcher::Class {
                negated: false,
                flags,
                items: vec![ClassItem::Word(false)],
            })),
            'W' => Ok(Ast::Atom(CharMatcher::Class {
                negated: false,
                flags,
                items: vec![ClassItem::Word(true)],
            })),
            's' => Ok(Ast::Atom(CharMatcher::Class {
                negated: false,
                flags,
                items: vec![ClassItem::Space(false)],
            })),
            'S' => Ok(Ast::Atom(CharMatcher::Class {
                negated: false,
                flags,
                items: vec![ClassItem::Space(true)],
            })),
            'A' => Ok(Ast::StartAnchor),
            'Z' => Ok(Ast::EndStrictAnchor),
            'n' => Ok(Ast::Atom(CharMatcher::Literal {
                ch: '\n',
                flags: Flags::default(),
            })),
            'r' => Ok(Ast::Atom(CharMatcher::Literal {
                ch: '\r',
                flags: Flags::default(),
            })),
            't' => Ok(Ast::Atom(CharMatcher::Literal {
                ch: '\t',
                flags: Flags::default(),
            })),
            other => Ok(Ast::Atom(CharMatcher::Literal { ch: other, flags })),
        }
    }

    fn parse_class(&mut self) -> Result<Ast, ()> {
        let negated = if self.peek() == Some('^') {
            self.next();
            true
        } else {
            false
        };
        let mut items = Vec::new();
        let mut first = true;
        while let Some(c) = self.peek() {
            if c == ']' && !first {
                self.next();
                return Ok(Ast::Atom(CharMatcher::Class {
                    negated,
                    flags: self.flags,
                    items,
                }));
            }
            first = false;
            let item = self.parse_class_item()?;
            if let ClassItem::Single(start) = item
                && self.peek() == Some('-')
                && self.chars.get(self.pos + 1).copied() != Some(']')
            {
                self.next(); // consume '-'
                match self.parse_class_item()? {
                    ClassItem::Single(end) if start <= end => {
                        items.push(ClassItem::Range(start, end));
                    }
                    _ => return Err(()),
                }
            } else {
                items.push(item);
            }
        }
        Err(())
    }

    fn parse_class_item(&mut self) -> Result<ClassItem, ()> {
        let Some(c) = self.next() else {
            return Err(());
        };
        if c == '\\' {
            let Some(e) = self.next() else {
                return Err(());
            };
            Ok(match e {
                'd' => ClassItem::Digit(false),
                'D' => ClassItem::Digit(true),
                'w' => ClassItem::Word(false),
                'W' => ClassItem::Word(true),
                's' => ClassItem::Space(false),
                'S' => ClassItem::Space(true),
                'n' => ClassItem::Single('\n'),
                'r' => ClassItem::Single('\r'),
                't' => ClassItem::Single('\t'),
                other => ClassItem::Single(other),
            })
        } else {
            Ok(ClassItem::Single(c))
        }
    }

    fn parse_quantifier(&mut self, node: Ast) -> Result<Ast, ()> {
        let (min, max) = match self.peek() {
            Some('*') => {
                self.next();
                (0, None)
            }
            Some('+') => {
                self.next();
                (1, None)
            }
            Some('?') => {
                self.next();
                (0, Some(1))
            }
            Some('{') => {
                let saved = self.pos;
                self.next();
                if let Some((min, max)) = self.try_parse_braces() {
                    (min, max)
                } else {
                    self.pos = saved;
                    return Ok(node);
                }
            }
            _ => return Ok(node),
        };
        // Non-greedy suffix `?` has identical boolean match semantics.
        if self.peek() == Some('?') {
            self.next();
        }
        Ok(Ast::Repeat {
            node: Box::new(node),
            min,
            max,
        })
    }

    fn try_parse_braces(&mut self) -> Option<(usize, Option<usize>)> {
        let min = self.parse_uint()?;
        match self.peek()? {
            '}' => {
                self.next();
                Some((min, Some(min)))
            }
            ',' => {
                self.next();
                if self.peek() == Some('}') {
                    self.next();
                    Some((min, None))
                } else {
                    let max = self.parse_uint()?;
                    if self.next() != Some('}') || max < min {
                        return None;
                    }
                    Some((min, Some(max)))
                }
            }
            _ => None,
        }
    }

    fn parse_uint(&mut self) -> Option<usize> {
        let start = self.pos;
        let mut n: usize = 0;
        while let Some(c) = self.peek() {
            if let Some(d) = c.to_digit(10) {
                n = n.checked_mul(10)?.checked_add(d as usize)?;
                self.next();
            } else {
                break;
            }
        }
        (self.pos > start).then_some(n)
    }
}

fn emit(ast: &Ast, out: &mut Vec<Inst>, steps: &mut usize) -> Result<(), ()> {
    *steps += 1;
    if *steps > MAX_COMPILE_STEPS || out.len() >= MAX_INSTS {
        return Err(());
    }
    match ast {
        Ast::Empty => {}
        Ast::Atom(m) => out.push(Inst::Char(m.clone())),
        Ast::StartAnchor => out.push(Inst::StartAnchor),
        Ast::EndAnchor => out.push(Inst::EndAnchor),
        Ast::EndStrictAnchor => out.push(Inst::EndStrictAnchor),
        Ast::Seq(items) => {
            for item in items {
                emit(item, out, steps)?;
            }
        }
        Ast::Alt(branches) => {
            let mut jumps_to_end = Vec::with_capacity(branches.len());
            for (i, b) in branches.iter().enumerate() {
                if i + 1 < branches.len() {
                    let split_idx = out.len();
                    out.push(Inst::Split(split_idx + 1, 0));
                    emit(b, out, steps)?;
                    let jmp_idx = out.len();
                    out.push(Inst::Jump(0));
                    jumps_to_end.push(jmp_idx);
                    let next_branch = out.len();
                    out[split_idx] = Inst::Split(split_idx + 1, next_branch);
                } else {
                    emit(b, out, steps)?;
                }
            }
            let end = out.len();
            for j in jumps_to_end {
                out[j] = Inst::Jump(end);
            }
        }
        Ast::Repeat { node, min, max } => {
            if *min > 128 || max.is_some_and(|m| m > 128) {
                return Err(());
            }
            for _ in 0..*min {
                emit(node, out, steps)?;
            }
            match *max {
                Some(m) => {
                    for _ in *min..m {
                        let split_idx = out.len();
                        out.push(Inst::Split(split_idx + 1, 0));
                        emit(node, out, steps)?;
                        let after = out.len();
                        out[split_idx] = Inst::Split(split_idx + 1, after);
                    }
                }
                None => {
                    let split_idx = out.len();
                    out.push(Inst::Split(split_idx + 1, 0));
                    emit(node, out, steps)?;
                    out.push(Inst::Jump(split_idx));
                    let after = out.len();
                    out[split_idx] = Inst::Split(split_idx + 1, after);
                }
            }
        }
    }
    if *steps > MAX_COMPILE_STEPS || out.len() >= MAX_INSTS {
        return Err(());
    }
    Ok(())
}

fn compile(pattern: &str) -> Result<Vec<Inst>, ()> {
    let mut parser = Parser::new(pattern);
    let ast = parser.parse_alt()?;
    if parser.pos != parser.chars.len() {
        return Err(());
    }
    let mut prog = Vec::new();
    let mut steps = 0;
    emit(&ast, &mut prog, &mut steps)?;
    prog.push(Inst::Match);
    Ok(prog)
}

/// Iterative epsilon-closure (no recursion, immune to stack overflow).
fn add_state(
    prog: &[Inst],
    start: usize,
    pos: usize,
    chars: &[char],
    visited: &mut [bool],
    work: &mut Vec<usize>,
    out: &mut Vec<usize>,
) {
    work.clear();
    work.push(start);
    while let Some(idx) = work.pop() {
        if visited[idx] {
            continue;
        }
        visited[idx] = true;
        match prog[idx] {
            Inst::Jump(target) => work.push(target),
            Inst::Split(a, b) => {
                work.push(b);
                work.push(a);
            }
            Inst::StartAnchor => {
                if pos == 0 {
                    work.push(idx + 1);
                }
            }
            Inst::EndAnchor => {
                // Python `$` matches at end of string, or right before a single trailing `\n`.
                if pos == chars.len() || (pos + 1 == chars.len() && chars[pos] == '\n') {
                    work.push(idx + 1);
                }
            }
            Inst::EndStrictAnchor => {
                if pos == chars.len() {
                    work.push(idx + 1);
                }
            }
            Inst::Char(_) | Inst::Match => out.push(idx),
        }
    }
}

fn run_nfa(prog: &[Inst], input: &str) -> bool {
    let chars: Vec<char> = input.chars().collect();
    let mut cur = Vec::new();
    let mut next = Vec::new();
    let mut work = Vec::new();
    let mut visited = vec![false; prog.len()];

    add_state(prog, 0, 0, &chars, &mut visited, &mut work, &mut cur);
    if cur.iter().any(|&pc| matches!(prog[pc], Inst::Match)) {
        return true;
    }

    for (i, &c) in chars.iter().enumerate() {
        visited.fill(false);
        next.clear();
        for &pc in &cur {
            if let Inst::Char(ref matcher) = prog[pc]
                && matcher.matches(c)
            {
                add_state(
                    prog,
                    pc + 1,
                    i + 1,
                    &chars,
                    &mut visited,
                    &mut work,
                    &mut next,
                );
            }
        }
        std::mem::swap(&mut cur, &mut next);
        if cur.iter().any(|&pc| matches!(prog[pc], Inst::Match)) {
            return true;
        }
        if cur.is_empty() {
            return false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    struct FakeLock {
        id: EntityId,
        available: bool,
        features: u32,
        code_format: Option<String>,
        default_code: Option<String>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeLock {
        fn new(
            id: &str,
            available: bool,
            features: u32,
            code_format: Option<&str>,
            default_code: Option<&str>,
        ) -> Arc<Self> {
            Arc::new(Self {
                id: EntityId::parse(id).unwrap(),
                available,
                features,
                code_format: code_format.map(str::to_owned),
                default_code: default_code.map(str::to_owned),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn record(&self, op: &str, code: Option<&str>) -> BoxFuture<'_> {
            self.calls.lock().push(format!("{op}:{code:?}"));
            Box::pin(async { Ok(()) })
        }
    }

    impl Lock for FakeLock {
        fn entity_id(&self) -> &EntityId {
            &self.id
        }
        fn available(&self) -> bool {
            self.available
        }
        fn supported_features(&self) -> u32 {
            self.features
        }
        fn code_format(&self) -> Option<String> {
            self.code_format.clone()
        }
        fn default_code(&self) -> Option<String> {
            self.default_code.clone()
        }
        fn lock(&self, code: Option<&str>) -> BoxFuture<'_> {
            self.record("lock", code)
        }
        fn unlock(&self, code: Option<&str>) -> BoxFuture<'_> {
            self.record("unlock", code)
        }
        fn open(&self, code: Option<&str>) -> BoxFuture<'_> {
            self.record("open", code)
        }
    }

    fn map_of(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn code_format_matcher_matches_python_re_match() {
        assert!(code_format_matches(r"^\d{4}$", "1234"));
        assert!(!code_format_matches(r"^\d{4}$", "123"));
        assert!(!code_format_matches(r"^\d{4}$", "12345"));
        assert!(!code_format_matches(r"^\d{4}$", "12a4"));
        assert!(code_format_matches(r"^\d{4,8}$", "123456"));
        assert!(code_format_matches(r"[0-9]{4}", "1234xyz")); // re.match allows trailing chars without $
        assert!(!code_format_matches(r"[0-9]{4}", "a1234")); // anchored at start
        assert!(code_format_matches(r"^(?:1234|5678)$", "5678"));
        assert!(!code_format_matches(r"^(?:1234|5678)$", "9999"));
        assert!(code_format_matches(r"^[^a-z]+$", "1234"));
        assert!(!code_format_matches(r"^[^a-z]+$", "12a4"));
        assert!(code_format_matches(r".*", ""));
        // Unicode decimal digits match `\d` (Python 3 default) unless `(?a)` is set.
        assert!(code_format_matches(r"^\d{4}$", "１２３４"));
        assert!(!code_format_matches(r"(?a)^\d{4}$", "１２３４"));
        assert!(!code_format_matches(r"^\d$", "²")); // superscript is not Nd
        // Inline case-insensitive flags `(?i)` and scoped `(?i:...)`.
        assert!(code_format_matches(r"(?i)^ab[0-9]{2}$", "AB12"));
        assert!(code_format_matches(r"(?i)^[a-f]{4}$", "AbCd"));
        assert!(code_format_matches(r"^(?i:ab)cd$", "ABcd"));
        assert!(!code_format_matches(r"^(?i:ab)cd$", "ABCD"));
        // ASCII vs Unicode casefolding parity with Python 3.14 `re`.
        assert!(code_format_matches(r"(?i)^k$", "\u{212A}"));
        assert!(!code_format_matches(r"(?ai)^k$", "\u{212A}"));
        assert!(code_format_matches(r"(?i)^[a-z]$", "\u{0130}"));
        assert!(!code_format_matches(r"(?ai)^[a-z]$", "\u{0130}"));
        assert!(code_format_matches(r"(?i)^σ$", "ς"));
        assert!(code_format_matches(r"(?i)^[İ-ı]$", "i"));
        assert!(code_format_matches(r"(?i)^[İ-ı]$", "I"));
        assert!(code_format_matches(r"(?i)^ß$", "ẞ"));
        assert!(code_format_matches(r"(?i)^[ᾀ-ᾇ]$", "ᾈ"));
        // Unclosed pattern fails closed without panicking.
        assert!(!code_format_matches(r"[0-9", "1"));
        // Deeply nested groups and nested empty repetitions fail bounded & fast.
        let deep = format!("{}{}", "(".repeat(200), ")".repeat(200));
        assert!(!code_format_matches(&deep, ""));
        let empty_bomb = "((()()){128}){128}";
        assert!(!code_format_matches(empty_bomb, ""));
    }

    #[tokio::test]
    async fn lock_services_code_format_and_open_feature() {
        let hass = Hass::new();
        let locks = Arc::new(Locks::default());
        locks.register_services(&hass, Arc::new(ha_registry::Registries::default()));

        let front = FakeLock::new(
            "lock.front_door",
            true,
            feature::OPEN,
            Some(r"^\d{4}$"),
            None,
        );
        let with_default = FakeLock::new("lock.side_door", true, 0, Some(r"^\d{4}$"), Some("4321"));
        let offline = FakeLock::new("lock.offline", false, feature::OPEN, None, None);
        locks.add(front.clone());
        locks.add(with_default.clone());
        locks.add(offline.clone());

        // Missing/invalid code raises HA's add_default_code validation error.
        let err = hass
            .call_service(
                "lock",
                "unlock",
                map_of(json!({"entity_id": "lock.front_door"})),
                None,
                false,
                None,
            )
            .await
            .unwrap_err();
        match err {
            ServiceError::Validation(r) => {
                assert_eq!(r.domain.as_str(), "lock");
                assert_eq!(r.key.as_str(), "add_default_code");
                assert_eq!(
                    r.message,
                    r"The code for lock.front_door doesn't match pattern ^\d{4}$"
                );
                assert_eq!(
                    json!(r.placeholders),
                    json!({"entity_id": "lock.front_door", "code_format": r"^\d{4}$"})
                );
            }
            other => panic!("expected validation error, got {other:?}"),
        }

        // Valid code reaches lock, unlock, and open.
        for svc in ["lock", "unlock", "open"] {
            hass.call_service(
                "lock",
                svc,
                map_of(json!({"entity_id": "lock.front_door", "code": "1234"})),
                None,
                false,
                None,
            )
            .await
            .unwrap();
        }
        assert_eq!(
            *front.calls.lock(),
            vec![
                r#"lock:Some("1234")"#,
                r#"unlock:Some("1234")"#,
                r#"open:Some("1234")"#,
            ]
        );

        // Default code fills in when caller passes none.
        hass.call_service(
            "lock",
            "unlock",
            map_of(json!({"entity_id": "lock.side_door"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(*with_default.calls.lock(), vec![r#"unlock:Some("4321")"#]);

        // `lock.open` on a lock without feature::OPEN returns ServiceNotSupported.
        let err = hass
            .call_service(
                "lock",
                "open",
                map_of(json!({"entity_id": "lock.side_door"})),
                None,
                false,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ServiceError::not_supported("lock", "open", "lock.side_door")
        );

        // Unavailable lock is skipped.
        hass.call_service(
            "lock",
            "lock",
            map_of(json!({"entity_id": "lock.offline"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        assert!(offline.calls.lock().is_empty());
    }

    #[test]
    fn services_hold_no_cycle() {
        let hass = Hass::new();
        let locks = Arc::new(Locks::default());
        locks.register_services(&hass, Arc::new(ha_registry::Registries::default()));
        let weak = Arc::downgrade(&locks);
        drop((hass, locks));
        assert!(weak.upgrade().is_none());
    }
}
