//! YAML as Home Assistant loads it (`annotatedyaml` on PyYAML's
//! `CSafeLoader`): libyaml's parser (`libyaml-safer`, a safe port of the
//! same C library), PyYAML's YAML 1.1 implicit resolution of plain scalars
//! (`on` is `true`, `23:00:00` is the int 82800), its safe constructors,
//! and HA's tags (`!include`, `!include_dir_*`, `!secret`, `!env_var`,
//! `!input`). Mappings keep their order; a duplicate key keeps its first
//! position and takes the last value, as HA's `construct_pairs` does.

use std::path::{Path, PathBuf};

/// A loaded YAML value, with Python's types.
#[derive(Debug, Clone, PartialEq)]
pub enum Yaml {
    Null,
    Bool(bool),
    /// Python ints are unbounded; past `i128` the decimal text is kept.
    Int(i128),
    BigInt(String),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Date(jiff::civil::Date),
    /// A timestamp, with its UTC offset in seconds if it had one.
    DateTime(jiff::civil::DateTime, Option<i32>),
    Seq(Vec<Yaml>),
    /// `!!set`.
    Set(Vec<Yaml>),
    /// `!!omap`/`!!pairs` entries.
    Tuple(Vec<Yaml>),
    Map(Vec<(Yaml, Yaml)>),
    /// A blueprint's `!input name`.
    Input(String),
}

impl Yaml {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Yaml::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The value at a string key of a mapping.
    pub fn get(&self, key: &str) -> Option<&Yaml> {
        match self {
            Yaml::Map(pairs) => pairs
                .iter()
                .find(|(k, _)| k.as_str() == Some(key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// Python truthiness (HA turns falsy top-level values into `{}`).
    pub fn is_truthy(&self) -> bool {
        match self {
            Yaml::Null => false,
            Yaml::Bool(b) => *b,
            Yaml::Int(i) => *i != 0,
            Yaml::BigInt(_) => true,
            Yaml::Float(f) => *f != 0.0,
            Yaml::Str(s) => !s.is_empty(),
            Yaml::Bytes(b) => !b.is_empty(),
            Yaml::Seq(v) | Yaml::Set(v) | Yaml::Tuple(v) => !v.is_empty(),
            Yaml::Map(m) => !m.is_empty(),
            Yaml::Date(_) | Yaml::DateTime(..) | Yaml::Input(_) => true,
        }
    }

    /// As JSON, the way HA's `json_bytes` would serialize the loaded value
    /// (keys stringified; dates as ISO strings). Key order follows
    /// `serde_json`'s map.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::{Value, json};
        match self {
            Yaml::Null => Value::Null,
            Yaml::Bool(b) => json!(b),
            Yaml::Int(i) => i64::try_from(*i)
                .map(Value::from)
                .or_else(|_| u64::try_from(*i).map(Value::from))
                .unwrap_or_else(|_| json!(i.to_string())),
            Yaml::BigInt(s) => json!(s),
            Yaml::Float(f) => serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number),
            Yaml::Str(s) | Yaml::Input(s) => json!(s),
            Yaml::Bytes(b) => json!(String::from_utf8_lossy(b)),
            Yaml::Date(d) => json!(d.to_string()),
            Yaml::DateTime(dt, off) => json!(datetime_iso(dt, *off)),
            Yaml::Seq(v) | Yaml::Set(v) | Yaml::Tuple(v) => {
                Value::Array(v.iter().map(Yaml::to_json).collect())
            }
            Yaml::Map(pairs) => Value::Object(
                pairs
                    .iter()
                    .map(|(k, v)| (k.key_string(), v.to_json()))
                    .collect(),
            ),
        }
    }

    /// A key as JSON names it (`str(k)` for non-strings, as orjson's
    /// `OPT_NON_STR_KEYS`).
    fn key_string(&self) -> String {
        match self {
            Yaml::Str(s) => s.clone(),
            Yaml::Null => "null".into(),
            Yaml::Bool(true) => "true".into(),
            Yaml::Bool(false) => "false".into(),
            Yaml::Int(i) => i.to_string(),
            Yaml::BigInt(s) => s.clone(),
            other => match other.to_json() {
                serde_json::Value::String(s) => s,
                v => v.to_string(),
            },
        }
    }
}

/// Python's `datetime.isoformat()`.
fn datetime_iso(dt: &jiff::civil::DateTime, off: Option<i32>) -> String {
    let mut s = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    );
    let us = dt.subsec_nanosecond() / 1000;
    if us != 0 {
        s.push_str(&format!(".{us:06}"));
    }
    if let Some(o) = off {
        let sign = if o < 0 { '-' } else { '+' };
        let o = o.abs();
        s.push_str(&format!("{sign}{:02}:{:02}", o / 3600, o % 3600 / 60));
    }
    s
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct YamlError(pub String);

fn err<T>(msg: impl Into<String>) -> Result<T, YamlError> {
    Err(YamlError(msg.into()))
}

/// Where `!secret` looks: `secrets.yaml` beside the including file, then
/// in each parent up to the config directory.
#[derive(Debug, Clone)]
pub struct Secrets {
    pub config_dir: PathBuf,
}

impl Secrets {
    fn get(&self, from: &Path, name: &str) -> Result<Yaml, YamlError> {
        // From the including file's directory up to the config directory,
        // and never outside it. `logger` is the secrets file's own setting.
        let mut dir = from.parent().map(Path::to_path_buf);
        while let Some(d) = dir.filter(|d| d.starts_with(&self.config_dir)) {
            let file = d.join("secrets.yaml");
            if let Ok(text) = std::fs::read_to_string(&file) {
                let secrets = Loader {
                    file: Some(file.clone()),
                    secrets: None,
                    nodes: Default::default(),
                }
                .load_str(&text)?;
                if name != "logger"
                    && let Some(v) = secrets.get(name)
                {
                    return Ok(v.clone());
                }
            }
            if d == self.config_dir {
                break;
            }
            dir = d.parent().map(Path::to_path_buf);
        }
        err(format!("Secret {name} not defined"))
    }
}

/// Parse YAML text with no file (HA's `parse_yaml`): `!include` and
/// `!secret` aren't available.
pub fn parse(text: &str) -> Result<Yaml, YamlError> {
    Loader {
        file: None,
        secrets: None,
        nodes: Default::default(),
    }
    .load_str(text)
}

/// Load a file, with `!secret` support if `secrets` is given (HA's
/// `load_yaml`). An empty file is `Null`.
pub fn load_file(path: &Path, secrets: Option<&Secrets>) -> Result<Yaml, YamlError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| YamlError(format!("Unable to read file {}: {e}", path.display())))?;
    Loader {
        file: Some(path.to_path_buf()),
        secrets: secrets.cloned(),
        nodes: Default::default(),
    }
    .load_str(&text)
}

// ----- The node tree (events, before construction) -----

#[derive(Debug, Clone)]
enum Node {
    Scalar {
        tag: Option<String>,
        value: String,
        plain: bool,
    },
    Seq {
        tag: Option<String>,
        items: Vec<Node>,
    },
    Map {
        tag: Option<String>,
        pairs: Vec<(Node, Node)>,
        line: u64,
    },
}

fn compose(text: &str) -> Result<Option<Node>, YamlError> {
    use libyaml_safer::{EventData, Parser, ScalarStyle};
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut input = text.as_bytes();
    let mut parser = Parser::new();
    parser.set_input_string(&mut input);
    let mut anchors: std::collections::HashMap<String, (Node, usize)> = Default::default();
    // Nodes in the tree so far, aliases counted as what they copy.
    let mut total: usize = 0;
    let mut open_size: Vec<usize> = Vec::new();
    // Open collections: (node, anchor, pending key for maps).
    let mut stack: Vec<(Node, Option<String>, Option<Node>)> = Vec::new();
    let mut root: Option<Node> = None;
    let mut documents = 0;
    let add = |stack: &mut Vec<(Node, Option<String>, Option<Node>)>,
               root: &mut Option<Node>,
               node: Node|
     -> Result<(), YamlError> {
        match stack.last_mut() {
            None => *root = Some(node),
            Some((Node::Seq { items, .. }, _, _)) => items.push(node),
            Some((Node::Map { pairs, .. }, _, key)) => match key.take() {
                None => *key = Some(node),
                Some(k) => pairs.push((k, node)),
            },
            Some(_) => return err("unexpected node"),
        }
        Ok(())
    };
    loop {
        let ev = parser.parse().map_err(|e| YamlError(e.to_string()))?;
        match ev.data {
            EventData::StreamEnd => break,
            EventData::StreamStart { .. } | EventData::DocumentEnd { .. } => {}
            EventData::DocumentStart { .. } => {
                documents += 1;
                if documents > 1 {
                    return err("expected a single document in the stream");
                }
            }
            EventData::Alias { anchor } => {
                let (node, size) = anchors
                    .get(&anchor)
                    .cloned()
                    .ok_or_else(|| YamlError(format!("found undefined alias '{anchor}'")))?;
                total += size;
                if total > MAX_NODES {
                    return err("the document expands to too many values (aliases)");
                }
                if let Some(s) = open_size.last_mut() {
                    *s += size;
                }
                add(&mut stack, &mut root, node)?;
            }
            EventData::Scalar {
                anchor,
                tag,
                value,
                style,
                ..
            } => {
                // `!` is non-specific: resolved as if untagged.
                let tag = tag.filter(|t| t != "!");
                let node = Node::Scalar {
                    plain: style == ScalarStyle::Plain && tag.is_none(),
                    tag,
                    value,
                };
                total += 1;
                if let Some(s) = open_size.last_mut() {
                    *s += 1;
                }
                if let Some(a) = anchor {
                    if anchors.contains_key(&a) {
                        return err(format!("found duplicate anchor '{a}'"));
                    }
                    anchors.insert(a, (node.clone(), 1));
                }
                add(&mut stack, &mut root, node)?;
            }
            EventData::SequenceStart { anchor, tag, .. } => {
                let tag = tag.filter(|t| t != "!");
                open_size.push(1);
                total += 1;
                stack.push((Node::Seq { tag, items: vec![] }, anchor, None));
            }
            EventData::MappingStart { anchor, tag, .. } => {
                let tag = tag.filter(|t| t != "!");
                open_size.push(1);
                total += 1;
                stack.push((
                    Node::Map {
                        tag,
                        pairs: vec![],
                        line: ev.start_mark.line,
                    },
                    anchor,
                    None,
                ));
            }
            EventData::SequenceEnd | EventData::MappingEnd => {
                let (node, anchor, _) =
                    stack.pop().ok_or_else(|| YamlError("unbalanced".into()))?;
                let size = open_size.pop().unwrap_or(1);
                if let Some(s) = open_size.last_mut() {
                    *s += size;
                }
                if let Some(a) = anchor {
                    if anchors.contains_key(&a) {
                        return err(format!("found duplicate anchor '{a}'"));
                    }
                    anchors.insert(a, (node.clone(), size));
                }
                add(&mut stack, &mut root, node)?;
            }
        }
    }
    Ok(root)
}

// ----- Construction -----

struct Loader {
    file: Option<PathBuf>,
    secrets: Option<Secrets>,
    /// Nodes made so far, against [`MAX_NODES`].
    nodes: std::rc::Rc<std::cell::Cell<usize>>,
}

/// Aliases make shared values in Python; here they're copied, so a few
/// bytes of nested aliases could expand without bound ("billion laughs").
/// A config never comes near this.
const MAX_NODES: usize = 1_000_000;

const TAG: &str = "tag:yaml.org,2002:";

impl Loader {
    fn load_str(&self, text: &str) -> Result<Yaml, YamlError> {
        match compose(text)? {
            None => Ok(Yaml::Null),
            Some(n) => self.construct(&n),
        }
    }

    fn construct(&self, node: &Node) -> Result<Yaml, YamlError> {
        let n = self.nodes.get() + 1;
        if n > MAX_NODES {
            return err("the document expands to too many values (aliases)");
        }
        self.nodes.set(n);
        match node {
            Node::Scalar { tag, value, plain } => self.scalar(tag.as_deref(), value, *plain),
            Node::Seq { tag, items } => match tag.as_deref() {
                None => Ok(Yaml::Seq(
                    items
                        .iter()
                        .map(|n| self.construct(n))
                        .collect::<Result<_, _>>()?,
                )),
                Some(t) if t == format!("{TAG}seq") => Ok(Yaml::Seq(
                    items
                        .iter()
                        .map(|n| self.construct(n))
                        .collect::<Result<_, _>>()?,
                )),
                Some(t) if t == format!("{TAG}omap") || t == format!("{TAG}pairs") => {
                    let mut out = Vec::new();
                    for item in items {
                        let Node::Map { pairs, .. } = item else {
                            return err(format!(
                                "expected a mapping of length 1, but found {item:?}"
                            ));
                        };
                        let [(k, v)] = pairs.as_slice() else {
                            return err("expected a single mapping item");
                        };
                        out.push(Yaml::Tuple(vec![self.construct(k)?, self.construct(v)?]));
                    }
                    Ok(Yaml::Seq(out))
                }
                Some("!env_var") => err("'list' object has no attribute 'split'"),
                Some(t) => err(format!(
                    "could not determine a constructor for the tag '{t}'"
                )),
            },
            Node::Map { tag, pairs, line } => match tag.as_deref() {
                None => self.mapping(pairs, *line),
                Some(t) if t == format!("{TAG}map") => self.mapping(pairs, *line),
                Some(t) if t == format!("{TAG}set") => {
                    let m = self.mapping(pairs, *line)?;
                    let Yaml::Map(m) = m else { unreachable!() };
                    Ok(Yaml::Set(m.into_iter().map(|(k, _)| k).collect()))
                }
                Some("!env_var") => err("'list' object has no attribute 'split'"),
                Some(t) => err(format!(
                    "could not determine a constructor for the tag '{t}'"
                )),
            },
        }
    }

    /// `construct_pairs` after `flatten_mapping`: merges (`<<`) first,
    /// then the explicit pairs; a duplicate key keeps its first position
    /// and takes the last value.
    fn mapping(&self, pairs: &[(Node, Node)], line: u64) -> Result<Yaml, YamlError> {
        let merged = flatten(pairs)?;
        let mut out: Vec<(Yaml, Yaml)> = Vec::new();
        for (k, v) in &merged {
            // `flatten_mapping` retags a `value` key (`=`) as a string.
            let key = match k {
                Node::Scalar {
                    value, plain: true, ..
                } if value == "=" => Yaml::Str("=".into()),
                k => self.construct(k)?,
            };
            if matches!(key, Yaml::Seq(_) | Yaml::Map(_) | Yaml::Set(_)) {
                return err(format!("invalid key: \"{key:?}\""));
            }
            let value = self.construct(v)?;
            match out.iter_mut().find(|(k2, _)| py_eq(k2, &key)) {
                Some(slot) => {
                    tracing::warn!(
                        "YAML file {} contains duplicate key \"{}\". Check lines {line}",
                        self.file
                            .as_deref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                        key.key_string()
                    );
                    slot.1 = value;
                }
                None => out.push((key, value)),
            }
        }
        Ok(Yaml::Map(out))
    }

    fn scalar(&self, tag: Option<&str>, value: &str, plain: bool) -> Result<Yaml, YamlError> {
        let Some(tag) = tag else {
            return match plain {
                true => resolve_plain(value),
                false => Ok(Yaml::Str(value.to_owned())),
            };
        };
        match tag.strip_prefix(TAG) {
            Some("str") => Ok(Yaml::Str(value.to_owned())),
            Some("null") => Ok(Yaml::Null),
            Some("bool") => bool_of(&value.to_lowercase())
                .map(Yaml::Bool)
                .ok_or_else(|| YamlError(format!("invalid bool {value}"))),
            Some("int") => int_of(value),
            Some("float") => {
                float_of(value).ok_or_else(|| YamlError(format!("invalid float {value}")))
            }
            Some("timestamp") => {
                timestamp_of(value)?.ok_or_else(|| YamlError(format!("invalid timestamp {value}")))
            }
            Some("binary") => {
                use base64::Engine;
                // `base64.decodebytes` skips anything outside the alphabet.
                let clean: String = value
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/'))
                    .collect();
                let engine = base64::engine::GeneralPurpose::new(
                    &base64::alphabet::STANDARD,
                    base64::engine::GeneralPurposeConfig::new()
                        .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
                );
                engine
                    .decode(clean)
                    .map(Yaml::Bytes)
                    .map_err(|e| YamlError(format!("failed to decode base64 data: {e}")))
            }
            Some(other) => err(format!(
                "could not determine a constructor for the tag '{TAG}{other}'"
            )),
            None => self.ha_tag(tag, value),
        }
    }

    /// HA's tags, relative to the including file's directory.
    fn ha_tag(&self, tag: &str, value: &str) -> Result<Yaml, YamlError> {
        let dir = || {
            self.file
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_default()
        };
        match tag {
            "!" => Ok(Yaml::Str(value.to_owned())),
            "!include" => {
                if value.is_empty() {
                    return err("!include needs an argument.");
                }
                let path = dir().join(value);
                self.child(&path)
            }
            "!include_dir_named"
            | "!include_dir_merge_named"
            | "!include_dir_list"
            | "!include_dir_merge_list" => {
                if value.is_empty() {
                    return err(format!("{tag} needs an argument."));
                }
                let files = yaml_files(&dir().join(value));
                let mut map: Vec<(Yaml, Yaml)> = Vec::new();
                let mut list = Vec::new();
                for f in files {
                    let raw = self.child_raw(&f)?;
                    // `!include_dir_list` skips empty files; the others
                    // read them as `{}`.
                    if raw == Yaml::Null && tag == "!include_dir_list" {
                        continue;
                    }
                    let v = if raw == Yaml::Null {
                        Yaml::Map(vec![])
                    } else {
                        raw
                    };
                    match tag {
                        "!include_dir_named" => {
                            let name = f.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
                            let v = if v == Yaml::Null {
                                Yaml::Map(vec![])
                            } else {
                                v
                            };
                            upsert(&mut map, Yaml::Str(name.to_owned()), v);
                        }
                        "!include_dir_merge_named" => {
                            if let Yaml::Map(m) = v {
                                for (k, v) in m {
                                    upsert(&mut map, k, v);
                                }
                            }
                        }
                        "!include_dir_list" => {
                            if v != Yaml::Null {
                                list.push(v);
                            }
                        }
                        _ => {
                            if let Yaml::Seq(items) = v {
                                list.extend(items);
                            }
                        }
                    }
                }
                Ok(match tag {
                    "!include_dir_named" | "!include_dir_merge_named" => Yaml::Map(map),
                    _ => Yaml::Seq(list),
                })
            }
            "!secret" => {
                let (Some(secrets), Some(file)) = (&self.secrets, &self.file) else {
                    return err("Secrets not supported in this YAML file");
                };
                secrets.get(file, value)
            }
            "!env_var" => {
                let mut parts = value.split_whitespace();
                let name = parts.next().unwrap_or_default();
                let default: Vec<&str> = parts.collect();
                match env_var(name) {
                    Ok(v) => Ok(Yaml::Str(v)),
                    Err(_) if !default.is_empty() => Ok(Yaml::Str(default.join(" "))),
                    Err(_) => err(name),
                }
            }
            "!input" => Ok(Yaml::Input(value.to_owned())),
            other => err(format!(
                "could not determine a constructor for the tag '{other}'"
            )),
        }
    }

    /// An included file: the same secrets; an empty file is `{}`.
    fn child(&self, path: &Path) -> Result<Yaml, YamlError> {
        let v = self.child_raw(path)?;
        Ok(if v == Yaml::Null {
            Yaml::Map(vec![])
        } else {
            v
        })
    }

    fn child_raw(&self, path: &Path) -> Result<Yaml, YamlError> {
        let text = std::fs::read_to_string(path)
            .map_err(|_| YamlError(format!("Unable to read file {}", path.display())))?;
        Loader {
            file: Some(path.to_path_buf()),
            secrets: self.secrets.clone(),
            nodes: self.nodes.clone(),
        }
        .load_str(&text)
    }
}

#[cfg(not(test))]
fn env_var(name: &str) -> Result<String, std::env::VarError> {
    std::env::var(name)
}

/// Tests can't set the process environment (`unsafe` is denied): these
/// stand in for it.
#[cfg(test)]
fn env_var(name: &str) -> Result<String, std::env::VarError> {
    match name {
        "HARS_TEST_ENV" => Ok("from env".into()),
        "HARS_TEST_ENV_INT" => Ok("8123".into()),
        n => std::env::var(n),
    }
}

/// PyYAML's `flatten_mapping`: the pairs of the mappings merged in with
/// `<<` (a mapping, or a list of them, later ones first), each flattened,
/// then the mapping's own pairs.
fn flatten(pairs: &[(Node, Node)]) -> Result<Vec<(Node, Node)>, YamlError> {
    let mut merged = Vec::new();
    let mut own = Vec::new();
    for (k, v) in pairs {
        let is_merge = matches!(k, Node::Scalar { value, plain: true, .. } if value == "<<")
            || matches!(k, Node::Scalar { tag: Some(t), .. } if t == "tag:yaml.org,2002:merge");
        if !is_merge {
            own.push((k.clone(), v.clone()));
            continue;
        }
        let sources: Vec<&Node> = match v {
            Node::Map { .. } => vec![v],
            Node::Seq { items, .. } => items.iter().collect(),
            _ => return err("expected a mapping or list of mappings for merging"),
        };
        let mut sub = Vec::new();
        for s in sources {
            let Node::Map { pairs, .. } = s else {
                return err("expected a mapping for merging");
            };
            sub.push(flatten(pairs)?);
        }
        sub.reverse();
        merged.extend(sub.into_iter().flatten());
    }
    merged.extend(own);
    Ok(merged)
}

fn upsert(map: &mut Vec<(Yaml, Yaml)>, k: Yaml, v: Yaml) {
    match map.iter_mut().find(|(k2, _)| py_eq(k2, &k)) {
        Some(slot) => slot.1 = v,
        None => map.push((k, v)),
    }
}

/// Every `*.yaml` under `dir` (not hidden, not `secrets.yaml`), files
/// sorted within each directory, directories in sorted order too (HA's
/// `os.walk` order is the filesystem's).
fn yaml_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<_> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    let hidden = |p: &Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'))
    };
    let mut out = Vec::new();
    for p in entries.iter().filter(|p| p.is_file() && !hidden(p)) {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if name.ends_with(".yaml") && name != "secrets.yaml" {
            out.push(p.clone());
        }
    }
    // `os.walk` doesn't follow directory symlinks.
    let real_dir = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    for p in entries.iter().filter(|p| real_dir(p) && !hidden(p)) {
        out.extend(yaml_files(p));
    }
    out
}

/// Python's `==` for keys: `True == 1 == 1.0` (ints and floats compared
/// exactly), PyYAML's one NaN object equal to itself, aware datetimes by
/// instant.
fn py_eq(a: &Yaml, b: &Yaml) -> bool {
    let int_float = |i: i128, f: f64| f.fract() == 0.0 && f.abs() < 1.7e38 && f as i128 == i;
    let as_int = |y: &Yaml| match y {
        Yaml::Bool(b) => Some(i128::from(*b)),
        Yaml::Int(i) => Some(*i),
        _ => None,
    };
    match (a, b) {
        (Yaml::Float(x), Yaml::Float(y)) => x == y || (x.is_nan() && y.is_nan()),
        (Yaml::Float(f), other) | (other, Yaml::Float(f)) => {
            as_int(other).is_some_and(|i| int_float(i, *f))
        }
        (Yaml::DateTime(x, Some(ox)), Yaml::DateTime(y, Some(oy))) => {
            let instant = |d: &jiff::civil::DateTime, o: i32| {
                jiff::tz::Offset::from_seconds(o)
                    .ok()
                    .and_then(|o| o.to_timestamp(*d).ok())
            };
            instant(x, *ox).is_some() && instant(x, *ox) == instant(y, *oy)
        }
        _ => match (as_int(a), as_int(b)) {
            (Some(x), Some(y)) => x == y,
            _ => a == b,
        },
    }
}

// ----- PyYAML's resolver and constructors -----

fn bool_of(v: &str) -> Option<bool> {
    match v {
        "yes" | "Yes" | "YES" | "true" | "True" | "TRUE" | "on" | "On" | "ON" => Some(true),
        "no" | "No" | "NO" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF" => Some(false),
        _ => None,
    }
}

fn digits(s: &str, ok: impl Fn(char) -> bool) -> bool {
    !s.is_empty() && s.chars().all(|c| ok(c) || c == '_')
}

/// `[-+]?` then the rest.
fn split_sign(v: &str) -> (bool, &str) {
    match v.as_bytes().first() {
        Some(b'-') => (true, &v[1..]),
        Some(b'+') => (false, &v[1..]),
        _ => (false, v),
    }
}

/// The int regex.
fn is_int(v: &str) -> bool {
    let (_, s) = split_sign(v);
    if let Some(b) = s.strip_prefix("0b") {
        return digits(b, |c| c == '0' || c == '1');
    }
    if let Some(h) = s.strip_prefix("0x") {
        return digits(h, |c| c.is_ascii_hexdigit());
    }
    if s == "0" {
        return true;
    }
    if let Some(o) = s.strip_prefix('0') {
        return digits(o, |c| ('0'..='7').contains(&c));
    }
    if s.starts_with(|c: char| ('1'..='9').contains(&c)) {
        let mut parts = s.split(':');
        let head = parts.next().unwrap_or_default();
        if !digits(head, |c| c.is_ascii_digit()) {
            return false;
        }
        return parts.all(sexagesimal_part);
    }
    false
}

/// `[0-5]?[0-9]`.
fn sexagesimal_part(p: &str) -> bool {
    match p.as_bytes() {
        [d] => d.is_ascii_digit(),
        [a, b] => (b'0'..=b'5').contains(a) && b.is_ascii_digit(),
        _ => false,
    }
}

/// A base-10^9 natural number, for ints past `i128` (Python's are
/// unbounded).
struct Big(Vec<u32>);

impl Big {
    fn mul_add(&mut self, m: u32, a: u32) {
        let mut carry = u64::from(a);
        for limb in &mut self.0 {
            let v = u64::from(*limb) * u64::from(m) + carry;
            *limb = (v % 1_000_000_000) as u32;
            carry = v / 1_000_000_000;
        }
        while carry > 0 {
            self.0.push((carry % 1_000_000_000) as u32);
            carry /= 1_000_000_000;
        }
    }

    fn decimal(&self) -> String {
        let mut it = self.0.iter().rev();
        let mut out = it.next().map_or("0".into(), u32::to_string);
        for limb in it {
            out.push_str(&format!("{limb:09}"));
        }
        out
    }
}

/// PyYAML's `construct_yaml_int`. An error is Python's `int()` failing.
fn int_of(v: &str) -> Result<Yaml, YamlError> {
    let clean: String = v.chars().filter(|c| *c != '_').collect();
    let (neg, s) = split_sign(&clean);
    let bad = |base: u32| {
        YamlError(format!(
            "invalid literal for int() with base {base}: '{}'",
            s.get(
                if base == 10 || base == 60 {
                    0
                } else {
                    2.min(s.len())
                }..
            )
            .unwrap_or_default()
        ))
    };
    // (digits, base) or base-60 parts.
    let mut big = Big(vec![0]);
    if s == "0" {
    } else if let Some(b) = s.strip_prefix("0b") {
        if b.is_empty() {
            return Err(bad(2));
        }
        for c in b.chars() {
            big.mul_add(2, c.to_digit(2).ok_or_else(|| bad(2))?);
        }
    } else if let Some(h) = s.strip_prefix("0x") {
        if h.is_empty() {
            return Err(bad(16));
        }
        for c in h.chars() {
            big.mul_add(16, c.to_digit(16).ok_or_else(|| bad(16))?);
        }
    } else if let Some(o) = s.strip_prefix('0') {
        for c in o.chars() {
            big.mul_add(8, c.to_digit(8).ok_or_else(|| bad(8))?);
        }
    } else if s.contains(':') {
        for part in s.split(':') {
            let mut p = Big(vec![0]);
            for c in part.chars() {
                p.mul_add(10, c.to_digit(10).ok_or_else(|| bad(10))?);
            }
            // big = big * 60 + p
            big.mul_add(60, 0);
            if big.0.len() < p.0.len() {
                big.0.resize(p.0.len(), 0);
            }
            let mut carry = 0u64;
            for (k, limb) in big.0.iter_mut().enumerate() {
                let v = u64::from(*limb) + u64::from(p.0.get(k).copied().unwrap_or(0)) + carry;
                *limb = (v % 1_000_000_000) as u32;
                carry = v / 1_000_000_000;
            }
            if carry > 0 {
                big.0.push(carry as u32);
            }
        }
    } else {
        if s.is_empty() {
            return Err(bad(10));
        }
        for c in s.chars() {
            big.mul_add(10, c.to_digit(10).ok_or_else(|| bad(10))?);
        }
    }
    let digits = big.decimal();
    let text = format!("{}{digits}", if neg && digits != "0" { "-" } else { "" });
    Ok(text.parse::<i128>().map_or(Yaml::BigInt(text), Yaml::Int))
}

/// The float regex.
fn is_float(v: &str) -> bool {
    let (_, s) = split_sign(v);
    if matches!(s, ".inf" | ".Inf" | ".INF") {
        return true;
    }
    if matches!(v, ".nan" | ".NaN" | ".NAN") {
        return true;
    }
    let exp_ok = |e: &str| {
        e.is_empty()
            || (e.len() >= 3
                && matches!(e.as_bytes()[0], b'e' | b'E')
                && matches!(e.as_bytes()[1], b'+' | b'-')
                && e[2..].bytes().all(|b| b.is_ascii_digit()))
    };
    // `[0-9][0-9_]*(:[0-5]?[0-9])+\.[0-9_]*`
    if s.contains(':') {
        let Some((head, frac)) = s.rsplit_once('.') else {
            return false;
        };
        let mut parts = head.split(':');
        let first = parts.next().unwrap_or_default();
        let parts: Vec<&str> = parts.collect();
        return first.starts_with(|c: char| c.is_ascii_digit())
            && digits(first, |c| c.is_ascii_digit())
            && !parts.is_empty()
            && parts.iter().all(|p| sexagesimal_part(p))
            && frac.chars().all(|c| c.is_ascii_digit() || c == '_');
    }
    // `[0-9][0-9_]*\.[0-9_]*(exp)?` | `\.[0-9][0-9_]*(exp)?`
    let Some(dot) = s.find('.') else { return false };
    let (int, rest) = (&s[..dot], &s[dot + 1..]);
    let exp_at = rest.find(['e', 'E']).unwrap_or(rest.len());
    let (frac, exp) = (&rest[..exp_at], &rest[exp_at..]);
    if !exp_ok(exp) || !frac.chars().all(|c| c.is_ascii_digit() || c == '_') {
        return false;
    }
    if int.is_empty() {
        // `\.[0-9][0-9_]*`, and no sign before it.
        return v.starts_with('.') && frac.starts_with(|c: char| c.is_ascii_digit());
    }
    int.starts_with(|c: char| c.is_ascii_digit()) && digits(int, |c| c.is_ascii_digit())
}

fn float_of(v: &str) -> Option<Yaml> {
    let clean: String = v
        .chars()
        .filter(|c| *c != '_')
        .collect::<String>()
        .to_lowercase();
    let (neg, s) = split_sign(&clean);
    let sign = if neg { -1.0 } else { 1.0 };
    let f = match s {
        ".inf" => f64::INFINITY,
        ".nan" => f64::NAN,
        s if s.contains(':') => {
            let mut n = 0.0;
            for p in s.split(':') {
                n = n * 60.0 + p.parse::<f64>().ok()?;
            }
            n
        }
        s => s.parse::<f64>().ok()?,
    };
    Some(Yaml::Float(sign * f))
}

fn is_null(v: &str) -> bool {
    matches!(v, "" | "~" | "null" | "Null" | "NULL")
}

/// The timestamp regex and constructor. `Ok(None)` if it doesn't match.
fn timestamp_of(v: &str) -> Result<Option<Yaml>, YamlError> {
    let b = v.as_bytes();
    let num = |s: &str| s.parse::<i32>().ok();
    // YYYY-M?M-D?D
    let date_end = {
        let mut parts = 0;
        let mut i = 0;
        let mut ok = true;
        let mut lens = [0usize; 3];
        while i < b.len() && parts < 3 {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            lens[parts] = i - start;
            parts += 1;
            if parts < 3 {
                if i < b.len() && b[i] == b'-' {
                    i += 1;
                } else {
                    ok = false;
                    break;
                }
            }
        }
        let date_only_shape = lens == [4, 2, 2];
        let full_shape = lens[0] == 4 && (1..=2).contains(&lens[1]) && (1..=2).contains(&lens[2]);
        if !ok || parts != 3 || !full_shape {
            return Ok(None);
        }
        if i == b.len() && !date_only_shape {
            return Ok(None);
        }
        i
    };
    let mut ds = v[..date_end].split('-');
    let (y, m, d) = (
        num(ds.next().unwrap_or_default()).unwrap_or(0),
        num(ds.next().unwrap_or_default()).unwrap_or(0),
        num(ds.next().unwrap_or_default()).unwrap_or(0),
    );
    let make_date = || {
        // Python's `date` covers years 1 to 9999.
        if !(1..=9999).contains(&y) {
            return Err(YamlError(format!("year {y} is out of range")));
        }
        jiff::civil::Date::new(y as i16, m as i8, d as i8).map_err(|_| {
            YamlError(if !(1..=12).contains(&m) {
                format!("month must be in 1..12, not {m}")
            } else {
                "day is out of range for month".into()
            })
        })
    };
    let rest = &v[date_end..];
    if rest.is_empty() {
        return Ok(Some(Yaml::Date(make_date()?)));
    }
    // ([Tt]|[ \t]+)H?H:MM:SS(\.frac)?[ \t]*(Z|[-+]H?H(:MM)?)?
    let rest = if let Some(r) = rest.strip_prefix(['T', 't']) {
        r
    } else {
        let r = rest.trim_start_matches([' ', '\t']);
        if r.len() == rest.len() {
            return Ok(None);
        }
        r
    };
    let mut it = rest.splitn(3, ':');
    let (h, mi, tail) = (
        it.next().unwrap_or_default(),
        it.next().unwrap_or_default(),
        it.next().unwrap_or_default(),
    );
    if !(1..=2).contains(&h.len())
        || !h.bytes().all(|c| c.is_ascii_digit())
        || mi.len() != 2
        || !mi.bytes().all(|c| c.is_ascii_digit())
        || tail.len() < 2
        || !tail.as_bytes()[..2].iter().all(u8::is_ascii_digit)
    {
        return Ok(None);
    }
    let sec = &tail[..2];
    let mut tail = &tail[2..];
    let mut frac = "";
    if let Some(t) = tail.strip_prefix('.') {
        let n = t.bytes().take_while(u8::is_ascii_digit).count();
        frac = &t[..n];
        tail = &t[n..];
    }
    let tz = tail.trim_start_matches([' ', '\t']);
    // `(Z|[-+][0-9][0-9]?(:[0-9][0-9])?)?`
    let offset = match tz {
        "" => None,
        "Z" => Some(0),
        tz if tz.starts_with(['+', '-']) => {
            let sign = if tz.starts_with('-') { -1 } else { 1 };
            let body = &tz[1..];
            let (hh, mm) = body.split_once(':').unwrap_or((body, "00"));
            let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
            if !(1..=2).contains(&hh.len()) || mm.len() != 2 || !digits(hh) || !digits(mm) {
                return Ok(None);
            }
            let secs = num(hh).unwrap_or(0) * 3600 + num(mm).unwrap_or(0) * 60;
            // `timezone(timedelta)` needs |offset| < 24 h.
            if secs >= 86_400 {
                return err(
                    "offset must be a timedelta strictly between -timedelta(hours=24) and timedelta(hours=24)",
                );
            }
            Some(sign * secs)
        }
        _ => return Ok(None),
    };
    let us: i32 = format!("{frac:0<6}")[..6].parse().unwrap_or(0);
    let date = make_date()?;
    let time = jiff::civil::Time::new(
        num(h).unwrap_or(0) as i8,
        num(mi).unwrap_or(0) as i8,
        num(sec).unwrap_or(0) as i8,
        us * 1000,
    )
    .map_err(|e| YamlError(e.to_string()))?;
    Ok(Some(Yaml::DateTime(date.to_datetime(time), offset)))
}

/// PyYAML's implicit resolvers for a plain scalar, in their order.
fn resolve_plain(v: &str) -> Result<Yaml, YamlError> {
    if let Some(b) = bool_of(v) {
        return Ok(Yaml::Bool(b));
    }
    // A value the resolver claims and the constructor can't make is an
    // error, as in PyYAML.
    if is_float(v) {
        return float_of(v)
            .ok_or_else(|| YamlError(format!("could not convert string to float: '{v}'")));
    }
    if is_int(v) {
        return int_of(v);
    }
    if v == "<<" {
        return err("could not determine a constructor for the tag 'tag:yaml.org,2002:merge'");
    }
    if is_null(v) {
        return Ok(Yaml::Null);
    }
    if let Some(ts) = timestamp_of(v)? {
        return Ok(ts);
    }
    if v == "=" {
        return err("could not determine a constructor for the tag 'tag:yaml.org,2002:value'");
    }
    Ok(Yaml::Str(v.to_owned()))
}

#[cfg(test)]
mod tests;
