//! The segment file format.
//!
//! A segment is a 16-byte header (`HSEG0001`, then the base time in µs,
//! i64 LE) followed by frames. A frame is one batch: `u32 LE` payload
//! length, `u32 LE` CRC-32 of the payload, the payload. A torn or corrupt
//! tail frame is cut off on open, so a batch is all or nothing.
//!
//! A payload is tagged records. Entity ids, blobs (JSON text) and event
//! types are declared once per segment by dictionary records and then
//! referenced by number, so each segment reads on its own and purge is an
//! `unlink`. Times are zigzag varint µs from the header's base time.

pub(crate) const MAGIC: &[u8; 8] = b"HSEG0001";
pub(crate) const HEADER: usize = 16;
pub(crate) const FRAME_HEAD: usize = 8;

const T_ENTITY: u8 = 1;
const T_BLOB: u8 = 2;
const T_KIND: u8 = 3;
const T_STATE: u8 = 4;
const T_EVENT: u8 = 5;

/// A state row's `last_changed` differs from its `last_updated`.
const F_LC: u8 = 0x80;

const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// CRC-32 (IEEE), as zlib's.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c = CRC_TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

pub(crate) fn header(base: i64) -> [u8; HEADER] {
    let mut h = [0u8; HEADER];
    h[..8].copy_from_slice(MAGIC);
    h[8..].copy_from_slice(&base.to_le_bytes());
    h
}

/// The base time, if `b` starts with a valid header.
pub(crate) fn read_header(b: &[u8]) -> Option<i64> {
    if b.len() < HEADER || &b[..8] != MAGIC {
        return None;
    }
    Some(i64::from_le_bytes(b[8..16].try_into().ok()?))
}

fn put_var(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

fn put_zig(buf: &mut Vec<u8>, v: i64) {
    put_var(buf, ((v << 1) ^ (v >> 63)) as u64);
}

fn put_str(buf: &mut Vec<u8>, s: &[u8]) {
    put_var(buf, s.len() as u64);
    buf.extend_from_slice(s);
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Option<u8> {
        let x = *self.b.get(self.pos)?;
        self.pos += 1;
        Some(x)
    }

    fn var(&mut self) -> Option<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let x = self.byte()?;
            v |= u64::from(x & 0x7f) << shift;
            if x & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
    }

    fn zig(&mut self) -> Option<i64> {
        let v = self.var()?;
        Some(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(s)
    }

    fn str(&mut self) -> Option<&'a str> {
        let n = usize::try_from(self.var()?).ok()?;
        std::str::from_utf8(self.bytes(n)?).ok()
    }

    fn ctx(&mut self) -> Option<[u8; 16]> {
        self.bytes(16)?.try_into().ok()
    }
}

/// A row to write.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    State(StateRow),
    Event(EventRow),
}

impl Row {
    pub fn ts(&self) -> i64 {
        match self {
            Self::State(s) => s.ts,
            Self::Event(e) => e.ts,
        }
    }
}

/// One `state_changed`, as recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct StateRow {
    pub entity_id: smol_str::SmolStr,
    /// `last_updated`, µs since the epoch.
    pub ts: i64,
    /// `last_changed`, µs.
    pub last_changed: i64,
    pub state: smol_str::SmolStr,
    /// The recorded attributes, as JSON text.
    pub attributes: std::sync::Arc<str>,
    /// [`VISIBLE`], [`HAS_UNIT`].
    pub flags: u8,
    /// The context id, as HA's 16 bytes.
    pub context: [u8; 16],
    /// The logbook's `context_*` fields, worked out at write time (JSON).
    pub aug: Option<std::sync::Arc<str>>,
}

/// An event, as recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct EventRow {
    pub event_type: smol_str::SmolStr,
    /// `time_fired`, µs.
    pub ts: i64,
    /// What the logbook needs of `data` (JSON), if anything.
    pub data: Option<std::sync::Arc<str>>,
    pub context: [u8; 16],
    pub aug: Option<std::sync::Arc<str>>,
}

/// The logbook shows this state row (its previous recorded state differed,
/// and the state itself changed).
pub const VISIBLE: u8 = 1;
/// The recorded attributes have a `unit_of_measurement` (a continuous
/// sensor, which the logbook skips).
pub const HAS_UNIT: u8 = 2;

/// A recorded row, borrowed from a scanned segment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Rec<'a> {
    State(StateRec<'a>),
    Event(EventRec<'a>),
}

impl Rec<'_> {
    pub fn ts(&self) -> i64 {
        match self {
            Self::State(s) => s.ts,
            Self::Event(e) => e.ts,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StateRec<'a> {
    pub entity_id: &'a str,
    pub ts: i64,
    pub last_changed: i64,
    pub state: &'a str,
    pub attributes: &'a str,
    pub flags: u8,
    pub context: [u8; 16],
    pub aug: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EventRec<'a> {
    pub event_type: &'a str,
    pub ts: i64,
    pub data: Option<&'a str>,
    pub context: [u8; 16],
    pub aug: Option<&'a str>,
}

impl StateRow {
    pub fn as_rec(&self) -> StateRec<'_> {
        StateRec {
            entity_id: &self.entity_id,
            ts: self.ts,
            last_changed: self.last_changed,
            state: &self.state,
            attributes: &self.attributes,
            flags: self.flags,
            context: self.context,
            aug: self.aug.as_deref(),
        }
    }
}

/// The writer's per-segment dictionaries. Ids start at 1; a blob id of 0
/// means none.
#[derive(Default)]
pub(crate) struct Dicts {
    entities: std::collections::HashMap<smol_str::SmolStr, u32>,
    blobs: std::collections::HashMap<std::sync::Arc<str>, u32>,
    kinds: std::collections::HashMap<smol_str::SmolStr, u32>,
}

fn intern<K: std::hash::Hash + Eq + Clone + AsRef<str>>(
    map: &mut std::collections::HashMap<K, u32>,
    key: &K,
    tag: u8,
    out: &mut Vec<u8>,
) -> u32 {
    if let Some(&id) = map.get(key) {
        return id;
    }
    let id = map.len() as u32 + 1;
    map.insert(key.clone(), id);
    out.push(tag);
    put_var(out, u64::from(id));
    put_str(out, key.as_ref().as_bytes());
    id
}

impl Dicts {
    fn blob(&mut self, blob: Option<&std::sync::Arc<str>>, out: &mut Vec<u8>) -> u32 {
        blob.map_or(0, |b| intern(&mut self.blobs, b, T_BLOB, out))
    }

    /// Append `row`'s records (any new dictionary entries first) to `out`.
    pub(crate) fn encode(&mut self, base: i64, row: &Row, out: &mut Vec<u8>) {
        match row {
            Row::State(s) => {
                let entity = intern(&mut self.entities, &s.entity_id, T_ENTITY, out);
                let attrs = self.blob(Some(&s.attributes), out);
                let aug = self.blob(s.aug.as_ref(), out);
                out.push(T_STATE);
                put_var(out, u64::from(entity));
                put_zig(out, s.ts.wrapping_sub(base));
                put_str(out, s.state.as_bytes());
                put_var(out, u64::from(attrs));
                let lc = s.last_changed != s.ts;
                out.push((s.flags & !F_LC) | if lc { F_LC } else { 0 });
                if lc {
                    put_zig(out, s.ts.wrapping_sub(s.last_changed));
                }
                out.extend_from_slice(&s.context);
                put_var(out, u64::from(aug));
            }
            Row::Event(e) => {
                let kind = intern(&mut self.kinds, &e.event_type, T_KIND, out);
                let data = self.blob(e.data.as_ref(), out);
                let aug = self.blob(e.aug.as_ref(), out);
                out.push(T_EVENT);
                put_var(out, u64::from(kind));
                put_zig(out, e.ts.wrapping_sub(base));
                put_var(out, u64::from(data));
                out.extend_from_slice(&e.context);
                put_var(out, u64::from(aug));
            }
        }
    }
}

/// A frame around `payload`.
pub(crate) fn frame(payload: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32(payload).to_le_bytes());
    out.extend_from_slice(payload);
}

/// Walk `b`'s frames from the header on, verifying each CRC, and return
/// the length of the valid prefix: a short or corrupt frame ends it.
pub(crate) fn frames<'a>(b: &'a [u8], mut f: impl FnMut(&'a [u8])) -> usize {
    let mut pos = HEADER;
    while let Some(head) = b.get(pos..pos + FRAME_HEAD) {
        let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let crc = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
        let Some(payload) = b.get(pos + FRAME_HEAD..pos + FRAME_HEAD + len) else {
            break;
        };
        if crc32(payload) != crc {
            break;
        }
        f(payload);
        pos += FRAME_HEAD + len;
    }
    pos.min(b.len())
}

/// A reader's view of one segment's dictionaries, as slices of the
/// segment's bytes.
#[derive(Default)]
pub(crate) struct ReadDicts<'a> {
    entities: Vec<&'a str>,
    blobs: Vec<&'a str>,
    kinds: Vec<&'a str>,
}

fn lookup<'a>(v: &[&'a str], id: u64) -> Option<&'a str> {
    v.get(usize::try_from(id).ok()?.checked_sub(1)?).copied()
}

impl<'a> ReadDicts<'a> {
    fn blob(&self, id: u64) -> Option<Option<&'a str>> {
        if id == 0 {
            return Some(None);
        }
        lookup(&self.blobs, id).map(Some)
    }

    /// Decode one payload's rows, calling `f` with each. `false` if the
    /// payload is malformed (rows before the fault were delivered).
    pub(crate) fn decode(
        &mut self,
        base: i64,
        payload: &'a [u8],
        mut f: impl FnMut(Rec<'a>),
    ) -> bool {
        let mut r = Reader { b: payload, pos: 0 };
        while r.pos < payload.len() {
            if self.record(base, &mut r, &mut f).is_none() {
                return false;
            }
        }
        true
    }

    fn record(&mut self, base: i64, r: &mut Reader<'a>, f: &mut impl FnMut(Rec<'a>)) -> Option<()> {
        match r.byte()? {
            tag @ (T_ENTITY | T_BLOB | T_KIND) => {
                let id = r.var()?;
                let s = r.str()?;
                let v = match tag {
                    T_ENTITY => &mut self.entities,
                    T_BLOB => &mut self.blobs,
                    _ => &mut self.kinds,
                };
                // Ids are declared in order.
                if id != v.len() as u64 + 1 {
                    return None;
                }
                v.push(s);
            }
            T_STATE => {
                let entity_id = lookup(&self.entities, r.var()?)?;
                let ts = base.wrapping_add(r.zig()?);
                let state = r.str()?;
                let attributes = self.blob(r.var()?)?.unwrap_or("{}");
                let flags = r.byte()?;
                let last_changed = if flags & F_LC != 0 {
                    ts.wrapping_sub(r.zig()?)
                } else {
                    ts
                };
                let context = r.ctx()?;
                let aug = self.blob(r.var()?)?;
                f(Rec::State(StateRec {
                    entity_id,
                    ts,
                    last_changed,
                    state,
                    attributes,
                    flags: flags & !F_LC,
                    context,
                    aug,
                }));
            }
            T_EVENT => {
                let event_type = lookup(&self.kinds, r.var()?)?;
                let ts = base.wrapping_add(r.zig()?);
                let data = self.blob(r.var()?)?;
                let context = r.ctx()?;
                let aug = self.blob(r.var()?)?;
                f(Rec::Event(EventRec {
                    event_type,
                    ts,
                    data,
                    context,
                    aug,
                }));
            }
            _ => return None,
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    pub(crate) fn state(entity: &str, ts: i64, state: &str, attrs: &str) -> Row {
        Row::State(StateRow {
            entity_id: entity.into(),
            ts,
            last_changed: ts,
            state: state.into(),
            attributes: Arc::from(attrs),
            flags: 0,
            context: [7; 16],
            aug: None,
        })
    }

    #[test]
    fn crc_is_zlibs() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    /// Every field comes back, across many frames sharing dictionaries,
    /// with times on both sides of the base and odd strings.
    #[test]
    fn rows_round_trip() {
        let base = 1_768_435_200_000_000;
        let mut rows = Vec::new();
        for i in 0..500i64 {
            let ts = base - 3_600_000_000 + i * 997_331;
            rows.push(match i % 4 {
                0 => state(
                    "sensor.t",
                    ts,
                    &format!("{}.5", i % 7),
                    r#"{"unit_of_measurement":"°C"}"#,
                ),
                1 => Row::State(StateRow {
                    entity_id: "climate.ac".into(),
                    ts,
                    last_changed: ts - 86_400_000_000 * (i % 3),
                    state: "".into(),
                    attributes: Arc::from(format!(r#"{{"t":{}}}"#, i % 5)),
                    flags: VISIBLE | HAS_UNIT,
                    context: (i as u128).to_be_bytes(),
                    aug: (i % 2 == 0).then(|| Arc::from(r#"{"context_user_id":"x"}"#)),
                }),
                2 => Row::Event(EventRow {
                    event_type: "automation_triggered".into(),
                    ts,
                    data: Some(Arc::from(r#"{"name":"ü"}"#)),
                    context: [i as u8; 16],
                    aug: None,
                }),
                _ => Row::Event(EventRow {
                    event_type: "homeassistant_start".into(),
                    ts: i64::MIN / 4,
                    data: None,
                    context: [0; 16],
                    aug: Some(Arc::from("{}")),
                }),
            });
        }
        let mut dicts = Dicts::default();
        let mut file = header(base).to_vec();
        for chunk in rows.chunks(37) {
            let mut payload = Vec::new();
            for r in chunk {
                dicts.encode(base, r, &mut payload);
            }
            frame(&payload, &mut file);
        }
        assert_eq!(read_header(&file), Some(base));
        let mut rd = ReadDicts::default();
        let mut got = Vec::new();
        let mut ok = true;
        let end = frames(&file, |p| ok &= rd.decode(base, p, |r| got.push(r)));
        assert!(ok);
        assert_eq!(end, file.len());
        assert_eq!(got.len(), rows.len());
        for (want, got) in rows.iter().zip(&got) {
            match (want, got) {
                (Row::State(w), Rec::State(g)) => assert_eq!(w.as_rec(), *g),
                (Row::Event(w), Rec::Event(g)) => {
                    assert_eq!(
                        (
                            w.event_type.as_str(),
                            w.ts,
                            w.data.as_deref(),
                            w.context,
                            w.aug.as_deref()
                        ),
                        (g.event_type, g.ts, g.data, g.context, g.aug)
                    );
                }
                _ => panic!("kind differs"),
            }
        }
    }

    #[test]
    fn a_torn_or_corrupt_tail_ends_the_valid_prefix() {
        let base = 0;
        let mut dicts = Dicts::default();
        let mut file = header(base).to_vec();
        let mut ends = Vec::new();
        for i in 0..3 {
            let mut payload = Vec::new();
            dicts.encode(base, &state("a.b", i, "x", "{}"), &mut payload);
            frame(&payload, &mut file);
            ends.push(file.len());
        }
        let count = |b: &[u8]| {
            let mut n = 0;
            (frames(b, |_| n += 1), n)
        };
        assert_eq!(count(&file), (ends[2], 3));
        assert_eq!(count(&file[..ends[2] - 1]), (ends[1], 2), "torn");
        let mut bad = file.clone();
        bad[ends[1] + FRAME_HEAD] ^= 1;
        assert_eq!(count(&bad), (ends[1], 2), "corrupt");
        assert_eq!(count(&file[..HEADER]), (HEADER, 0));
        assert_eq!(read_header(b"HSEG0002\0\0\0\0\0\0\0\0"), None);
    }
}
