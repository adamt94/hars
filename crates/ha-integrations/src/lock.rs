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
/// with no backtracking blowup and no external regex crate dependency.
pub fn code_format_matches(pattern: &str, input: &str) -> bool {
    let Ok(prog) = compile(pattern) else {
        return false;
    };
    run_nfa(&prog, input)
}

const MAX_INSTS: usize = 2048;

#[derive(Clone)]
enum CharMatcher {
    Literal(char),
    AnyNonNewline,
    Class {
        negated: bool,
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
            CharMatcher::Literal(lit) => c == *lit,
            CharMatcher::AnyNonNewline => c != '\n',
            CharMatcher::Class { negated, items } => {
                let hit = items.iter().any(|item| match *item {
                    ClassItem::Single(x) => c == x,
                    ClassItem::Range(a, b) => a <= c && c <= b,
                    ClassItem::Digit(inv) => c.is_ascii_digit() ^ inv,
                    ClassItem::Word(inv) => (c.is_alphanumeric() || c == '_') ^ inv,
                    ClassItem::Space(inv) => c.is_whitespace() ^ inv,
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

struct Parser<'a> {
    chars: Vec<char>,
    pos: usize,
    _raw: &'a str,
}

impl<'a> Parser<'a> {
    fn new(raw: &'a str) -> Self {
        Self {
            chars: raw.chars().collect(),
            pos: 0,
            _raw: raw,
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
            '(' => {
                // Support `(?:...)` non-capturing groups alongside `(...)`.
                if self.peek() == Some('?') {
                    self.next();
                    if self.next() != Some(':') {
                        return Err(());
                    }
                }
                let inner = self.parse_alt()?;
                if self.next() != Some(')') {
                    return Err(());
                }
                Ok(inner)
            }
            '[' => self.parse_class(),
            '\\' => self.parse_escape(),
            '*' | '+' | '?' | '{' => Err(()),
            lit => Ok(Ast::Atom(CharMatcher::Literal(lit))),
        }
    }

    fn parse_escape(&mut self) -> Result<Ast, ()> {
        let Some(c) = self.next() else {
            return Err(());
        };
        match c {
            'd' => Ok(Ast::Atom(CharMatcher::Class {
                negated: false,
                items: vec![ClassItem::Digit(false)],
            })),
            'D' => Ok(Ast::Atom(CharMatcher::Class {
                negated: false,
                items: vec![ClassItem::Digit(true)],
            })),
            'w' => Ok(Ast::Atom(CharMatcher::Class {
                negated: false,
                items: vec![ClassItem::Word(false)],
            })),
            'W' => Ok(Ast::Atom(CharMatcher::Class {
                negated: false,
                items: vec![ClassItem::Word(true)],
            })),
            's' => Ok(Ast::Atom(CharMatcher::Class {
                negated: false,
                items: vec![ClassItem::Space(false)],
            })),
            'S' => Ok(Ast::Atom(CharMatcher::Class {
                negated: false,
                items: vec![ClassItem::Space(true)],
            })),
            'A' => Ok(Ast::StartAnchor),
            'Z' => Ok(Ast::EndStrictAnchor),
            'n' => Ok(Ast::Atom(CharMatcher::Literal('\n'))),
            'r' => Ok(Ast::Atom(CharMatcher::Literal('\r'))),
            't' => Ok(Ast::Atom(CharMatcher::Literal('\t'))),
            other => Ok(Ast::Atom(CharMatcher::Literal(other))),
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
                return Ok(Ast::Atom(CharMatcher::Class { negated, items }));
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

fn emit(ast: &Ast, out: &mut Vec<Inst>) -> Result<(), ()> {
    if out.len() >= MAX_INSTS {
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
                emit(item, out)?;
            }
        }
        Ast::Alt(branches) => {
            // Chain `Split(branch_i, next_split)` ... `Jump(end)`.
            let mut jumps_to_end = Vec::with_capacity(branches.len());
            for (i, b) in branches.iter().enumerate() {
                if i + 1 < branches.len() {
                    let split_idx = out.len();
                    out.push(Inst::Split(split_idx + 1, 0));
                    emit(b, out)?;
                    let jmp_idx = out.len();
                    out.push(Inst::Jump(0));
                    jumps_to_end.push(jmp_idx);
                    let next_branch = out.len();
                    out[split_idx] = Inst::Split(split_idx + 1, next_branch);
                } else {
                    emit(b, out)?;
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
                emit(node, out)?;
            }
            match *max {
                Some(m) => {
                    for _ in *min..m {
                        let split_idx = out.len();
                        out.push(Inst::Split(split_idx + 1, 0));
                        emit(node, out)?;
                        let after = out.len();
                        out[split_idx] = Inst::Split(split_idx + 1, after);
                    }
                }
                None => {
                    let split_idx = out.len();
                    out.push(Inst::Split(split_idx + 1, 0));
                    emit(node, out)?;
                    out.push(Inst::Jump(split_idx));
                    let after = out.len();
                    out[split_idx] = Inst::Split(split_idx + 1, after);
                }
            }
        }
    }
    if out.len() >= MAX_INSTS {
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
    emit(&ast, &mut prog)?;
    prog.push(Inst::Match);
    Ok(prog)
}

fn add_state(
    prog: &[Inst],
    idx: usize,
    pos: usize,
    chars: &[char],
    visited: &mut [bool],
    out: &mut Vec<usize>,
) {
    if visited[idx] {
        return;
    }
    visited[idx] = true;
    match prog[idx] {
        Inst::Jump(target) => add_state(prog, target, pos, chars, visited, out),
        Inst::Split(a, b) => {
            add_state(prog, a, pos, chars, visited, out);
            add_state(prog, b, pos, chars, visited, out);
        }
        Inst::StartAnchor => {
            if pos == 0 {
                add_state(prog, idx + 1, pos, chars, visited, out);
            }
        }
        Inst::EndAnchor => {
            // Python `$` matches at end of string, or right before a single trailing `\n`.
            if pos == chars.len() || (pos + 1 == chars.len() && chars[pos] == '\n') {
                add_state(prog, idx + 1, pos, chars, visited, out);
            }
        }
        Inst::EndStrictAnchor => {
            if pos == chars.len() {
                add_state(prog, idx + 1, pos, chars, visited, out);
            }
        }
        Inst::Char(_) | Inst::Match => out.push(idx),
    }
}

fn run_nfa(prog: &[Inst], input: &str) -> bool {
    let chars: Vec<char> = input.chars().collect();
    let mut cur = Vec::new();
    let mut next = Vec::new();
    let mut visited = vec![false; prog.len()];

    add_state(prog, 0, 0, &chars, &mut visited, &mut cur);
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
                add_state(prog, pc + 1, i + 1, &chars, &mut visited, &mut next);
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
        // Unclosed pattern fails closed without panicking.
        assert!(!code_format_matches(r"[0-9", "1"));
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
