//! HA's YAML writer (`homeassistant.util.yaml.dump`, which is
//! annotatedyaml's): PyYAML's `CSafeDumper` with `default_flow_style=False`,
//! `allow_unicode=True` and `sort_keys=False`, then every `: null` at a
//! line's end cut to `:`. What the automation editor rewrites
//! `automations.yaml` with on each save.
//!
//! PyYAML's representer turns a value into nodes (a tag and a text for
//! each scalar) and hands libyaml's emitter one event per node, saying
//! for a scalar whether its text read plain, or read quoted, would give
//! that tag back. The emitter picks the style from that: a string that
//! would read as something else (`on`, `23:00:00`, `123`) is quoted.
//! `libyaml-safer`'s emitter is libyaml's, so the layout (indent 2, width
//! 80, a sequence under a key not indented, `[]` and `{}` for empty ones)
//! and the quoting are the same code. Checked against
//! `vectors/dump.json`, made by `scripts/vectors/yaml_dump.py`.
//!
//! One thing libyaml does that the port doesn't: libyaml's emitter takes
//! a character past U+FFFF (an emoji) for unprintable, so the scalar is
//! double-quoted and the character written `\U0001F600`. Here such a
//! string is given to the emitter double-quoted with a stand-in for each
//! of those characters: three control characters that take the same four
//! bytes (the emitter's limit for a plain key counts bytes) and are
//! written in the same ten columns (where a long scalar folds counts
//! columns). The stand-ins are swapped for the escapes in the text that
//! comes out. A value that already holds every one of the 4000 or so
//! stand-ins there are can't be written (an error, not a wrong file).
//!
//! One difference: HA writes a mapping or sequence that the loaded file
//! named once and used again (`&a`/`*a`) with an anchor and an alias;
//! [`Yaml`] holds each use as its own copy, so it is written out each
//! time. What loads from either is the same.

use super::{Yaml, YamlError, datetime_iso, err, resolve_plain};
use libyaml_safer::{Emitter, Encoding, Event, MappingStyle, ScalarStyle, SequenceStyle};

const STR: &str = "tag:yaml.org,2002:str";
const INT: &str = "tag:yaml.org,2002:int";
const FLOAT: &str = "tag:yaml.org,2002:float";
const BOOL: &str = "tag:yaml.org,2002:bool";
const NULL: &str = "tag:yaml.org,2002:null";
const TIMESTAMP: &str = "tag:yaml.org,2002:timestamp";
const BINARY: &str = "tag:yaml.org,2002:binary";
const SEQ: &str = "tag:yaml.org,2002:seq";
const MAP: &str = "tag:yaml.org,2002:map";
const SET: &str = "tag:yaml.org,2002:set";

/// The tag PyYAML's resolver gives a plain scalar with this text.
fn resolved(text: &str) -> Option<&'static str> {
    Some(match resolve_plain(text) {
        Ok(Yaml::Str(_)) => STR,
        Ok(Yaml::Int(_) | Yaml::BigInt(_)) => INT,
        Ok(Yaml::Float(_)) => FLOAT,
        Ok(Yaml::Bool(_)) => BOOL,
        Ok(Yaml::Null) => NULL,
        Ok(Yaml::Date(_) | Yaml::DateTime(..)) => TIMESTAMP,
        // Claimed by a resolver (merge, value, a number or date that
        // can't be made): some tag that is none of ours.
        _ => return None,
    })
}

/// `SafeRepresenter.represent_float`.
fn float_text(x: f64) -> String {
    if x.is_nan() {
        return ".nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { ".inf" } else { "-.inf" }.into();
    }
    let mut text = crate::py::repr_float(x).to_lowercase();
    // `1e+20` isn't a YAML 1.1 float without a dot.
    if !text.contains('.') && text.contains('e') {
        text = text.replacen('e', ".0e", 1);
    }
    text
}

/// `base64.encodebytes`: 76 characters to a line, each line ended.
fn base64_lines(bytes: &[u8]) -> String {
    use base64::Engine;
    let all = base64::engine::general_purpose::STANDARD.encode(bytes);
    let mut out = String::with_capacity(all.len() + all.len() / 76 + 1);
    for line in all.as_bytes().chunks(76) {
        // Base64 text is ASCII.
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push('\n');
    }
    out
}

/// Whether libyaml's emitter can't print `c`: its `IS_PRINTABLE` stops
/// at U+FFFD.
fn astral(c: char) -> bool {
    c > '\u{ffff}'
}

/// A stand-in for an astral character: three control characters that
/// take 4 bytes, like its UTF-8, and that the emitter writes as three
/// escapes in 10 columns, like `\U0001F600` (`\x01\a\x80`). The first
/// of the 4000 or so there are that `value` holds neither raw nor as
/// written; `None` if it has them all.
fn stand_in(value: &Yaml) -> Option<(String, String)> {
    fn any(v: &Yaml, has: &dyn Fn(&str) -> bool) -> bool {
        match v {
            Yaml::Str(s) | Yaml::Input(s) => has(s),
            Yaml::Seq(items) | Yaml::Set(items) | Yaml::Tuple(items) => {
                items.iter().any(|i| any(i, has))
            }
            Yaml::Map(pairs) => pairs.iter().any(|(k, v)| any(k, has) || any(v, has)),
            _ => false,
        }
    }
    // One byte, written `\xNN`: control characters with no short escape
    // that aren't breaks.
    let first = (0x01..=0x06u8).chain(0x0e..=0x1a).chain(0x1c..=0x1f);
    // One byte, written with a letter.
    const SECOND: [(char, char); 6] = [
        ('\x07', 'a'),
        ('\0', '0'),
        ('\x08', 'b'),
        ('\x0b', 'v'),
        ('\x0c', 'f'),
        ('\x1b', 'e'),
    ];
    for a in first {
        for (b, letter) in SECOND {
            // Two bytes, written `\xNN` (U+0085 is `\N`).
            for c in (0x80..=0x9fu8).filter(|c| *c != 0x85) {
                let raw: String = [char::from(a), b, char::from(c)].iter().collect();
                let written = format!("\\x{a:02X}\\{letter}\\x{c:02X}");
                if !any(value, &|s| s.contains(&raw) || s.contains(&written)) {
                    return Some((raw, written));
                }
            }
        }
    }
    None
}

/// `text` as the emitter wrote it, with each stand-in (`written`) made
/// the escape of the character it stood for, in the order written.
/// Escapes are read one at a time: a backslash the emitter wrote for a
/// backslash (`\\`) is not the start of the next one, so `\\x01\a\x80`
/// (the text `\x01`, then two control characters) is left alone.
fn swap_stand_ins(text: &str, written: &str, stood_for: &[char]) -> Result<String, YamlError> {
    let mut out = String::with_capacity(text.len() + stood_for.len() * 2);
    let mut chars = stood_for.iter();
    let mut rest = text;
    while let Some(i) = rest.find('\\') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        if rest.starts_with("\\\\") {
            out.push_str("\\\\");
            rest = &rest[2..];
        } else if rest.starts_with(written) {
            let c = chars
                .next()
                .ok_or_else(|| YamlError("a stand-in was written twice".into()))?;
            out.push_str(&format!("\\U{:08X}", u32::from(*c)));
            rest = &rest[written.len()..];
        } else {
            out.push('\\');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    match chars.next() {
        Some(_) => err("a stand-in was not written"),
        None => Ok(out),
    }
}

struct Writer<'a, 'w> {
    emitter: &'a mut Emitter<'w>,
    /// The stand-in for an astral character, if the value has any.
    stand_in: Option<String>,
    /// The astral characters stood in for, in the order written.
    astral: Vec<char>,
}

impl Writer<'_, '_> {
    fn emit(&mut self, event: Event) -> Result<(), YamlError> {
        self.emitter
            .emit(event)
            .map_err(|e| YamlError(e.to_string()))
    }

    /// A scalar node, as `CEmitter._serialize_node` hands it over: its
    /// tag, and whether a plain or a quoted scalar with this text would
    /// resolve to that tag.
    fn scalar(&mut self, tag: &str, text: &str, style: ScalarStyle) -> Result<(), YamlError> {
        let plain_implicit = resolved(text) == Some(tag);
        let quoted_implicit = tag == STR;
        self.emit(Event::scalar(
            None,
            Some(tag),
            text,
            plain_implicit,
            quoted_implicit,
            style,
        ))
    }

    fn sequence(&mut self, items: &[Yaml]) -> Result<(), YamlError> {
        self.emit(Event::sequence_start(
            None,
            Some(SEQ),
            true,
            SequenceStyle::Block,
        ))?;
        for item in items {
            self.node(item)?;
        }
        self.emit(Event::sequence_end())
    }

    fn node(&mut self, value: &Yaml) -> Result<(), YamlError> {
        let plain = ScalarStyle::Plain;
        match value {
            Yaml::Null => self.scalar(NULL, "null", plain),
            Yaml::Bool(b) => self.scalar(BOOL, if *b { "true" } else { "false" }, plain),
            Yaml::Int(i) => self.scalar(INT, &i.to_string(), plain),
            Yaml::BigInt(text) => self.scalar(INT, text, plain),
            Yaml::Float(x) => self.scalar(FLOAT, &float_text(*x), plain),
            Yaml::Str(s) => match &self.stand_in {
                Some(stand_in) if s.contains(astral) => {
                    self.astral.extend(s.chars().filter(|c| astral(*c)));
                    let text = s.replace(astral, stand_in);
                    self.scalar(STR, &text, ScalarStyle::DoubleQuoted)
                }
                _ => self.scalar(STR, s, plain),
            },
            Yaml::Bytes(bytes) => self.scalar(BINARY, &base64_lines(bytes), ScalarStyle::Literal),
            Yaml::Date(d) => self.scalar(TIMESTAMP, &d.to_string(), plain),
            // `datetime.isoformat(' ')`.
            Yaml::DateTime(dt, offset) => self.scalar(
                TIMESTAMP,
                &datetime_iso(dt, *offset).replacen('T', " ", 1),
                plain,
            ),
            // annotatedyaml's representer for a blueprint's `!input`.
            Yaml::Input(name) => {
                let (text, style) = match &self.stand_in {
                    Some(stand_in) if name.contains(astral) => {
                        self.astral.extend(name.chars().filter(|c| astral(*c)));
                        (name.replace(astral, stand_in), ScalarStyle::DoubleQuoted)
                    }
                    _ => (name.clone(), plain),
                };
                self.emit(Event::scalar(
                    None,
                    Some("!input"),
                    &text,
                    false,
                    false,
                    style,
                ))
            }
            Yaml::Seq(items) | Yaml::Tuple(items) => self.sequence(items),
            // `represent_set`: a `!!set` mapping of its members to null.
            Yaml::Set(items) => {
                self.emit(Event::mapping_start(
                    None,
                    Some(SET),
                    false,
                    MappingStyle::Block,
                ))?;
                for item in items {
                    self.node(item)?;
                    self.node(&Yaml::Null)?;
                }
                self.emit(Event::mapping_end())
            }
            Yaml::Map(pairs) => {
                self.emit(Event::mapping_start(
                    None,
                    Some(MAP),
                    true,
                    MappingStyle::Block,
                ))?;
                for (k, v) in pairs {
                    self.node(k)?;
                    self.node(v)?;
                }
                self.emit(Event::mapping_end())
            }
        }
    }
}

/// Write `value` as HA writes a YAML file.
pub fn dump(value: &Yaml) -> Result<String, YamlError> {
    fn has_astral(v: &Yaml) -> bool {
        match v {
            Yaml::Str(s) | Yaml::Input(s) => s.contains(astral),
            Yaml::Seq(items) | Yaml::Set(items) | Yaml::Tuple(items) => {
                items.iter().any(has_astral)
            }
            Yaml::Map(pairs) => pairs.iter().any(|(k, v)| has_astral(k) || has_astral(v)),
            _ => false,
        }
    }
    let stand_in = match has_astral(value) {
        true => Some(stand_in(value).ok_or_else(|| {
            YamlError("can't write characters past U+FFFF beside these control characters".into())
        })?),
        false => None,
    };
    let mut out = Vec::new();
    let astral_written;
    {
        let mut emitter = Emitter::new();
        emitter.set_output_string(&mut out);
        emitter.set_unicode(true);
        let mut w = Writer {
            emitter: &mut emitter,
            stand_in: stand_in.as_ref().map(|(raw, _)| raw.clone()),
            astral: Vec::new(),
        };
        w.emit(Event::stream_start(Encoding::Utf8))?;
        w.emit(Event::document_start(None, &[], true))?;
        w.node(value)?;
        w.emit(Event::document_end(true))?;
        w.emit(Event::stream_end())?;
        astral_written = w.astral;
        emitter.flush().map_err(|e| YamlError(e.to_string()))?;
    }
    let mut text = String::from_utf8(out).map_err(|e| YamlError(e.to_string()))?;
    if let Some((_, written)) = &stand_in {
        text = swap_stand_ins(&text, written, &astral_written)?;
    }
    Ok(text.replace(": null\n", ":\n"))
}
