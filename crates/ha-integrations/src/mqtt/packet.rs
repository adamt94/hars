//! MQTT 3.1, 3.1.1 and 5 packets, as paho-mqtt 2.1.0 writes the ones Home
//! Assistant sends and reads the ones a broker sends back. Only what HA's
//! client uses: no topic aliases, no authentication exchange, and of the
//! MQTT 5 properties only those HA sets (subscription identifiers, message
//! expiry) or reads (whether the broker has subscription identifiers).
//!
//! Checked against the packets HA 2026.6.1 sent a broker
//! (`vectors/v5.json`, `vectors/v311.json`).

use std::fmt;

/// The protocol, as the entry's `protocol` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    /// `3.1`: protocol name `MQIsdp`, level 3.
    V31,
    /// `3.1.1`: `MQTT`, level 4.
    V311,
    /// `5`: `MQTT`, level 5, with properties.
    V5,
}

impl Version {
    pub fn is_v5(self) -> bool {
        self == Version::V5
    }

    /// What the entry calls it (and HA's messages quote).
    pub fn as_str(self) -> &'static str {
        match self {
            Version::V31 => "3.1",
            Version::V311 => "3.1.1",
            Version::V5 => "5",
        }
    }
}

/// A packet that doesn't parse: the connection is closed, as paho does
/// (`MQTT_ERR_PROTOCOL`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed;

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("A network protocol error occurred when communicating with the broker.")
    }
}

/// The largest packet the remaining length can say (the spec's limit).
pub const MAX_REMAINING: usize = 268_435_455;

fn varint(out: &mut Vec<u8>, mut n: usize) {
    loop {
        let byte = (n % 128) as u8;
        n /= 128;
        out.push(if n > 0 { byte | 0x80 } else { byte });
        if n == 0 {
            return;
        }
    }
}

fn str16(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s);
}

/// The fixed header and `body`.
fn packet(first: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(first);
    varint(&mut out, body.len());
    out.extend_from_slice(body);
    out
}

/// A will: HA's `will_message` (or its default).
#[derive(Debug, Clone, Copy)]
pub struct Will<'a> {
    pub topic: &'a str,
    pub payload: &'a [u8],
    pub qos: u8,
    pub retain: bool,
}

/// `_send_connect`'s fields.
#[derive(Debug, Clone, Copy)]
pub struct Connect<'a> {
    pub version: Version,
    pub client_id: &'a str,
    pub keepalive: u16,
    /// Clean session (3.1, 3.1.1) or clean start (5): HA asks for it on
    /// every connection.
    pub clean: bool,
    pub will: Option<Will<'a>>,
    pub username: Option<&'a str>,
    /// Sent only with a username, as paho does.
    pub password: Option<&'a str>,
}

/// `CONNECT`.
pub fn connect(c: &Connect) -> Vec<u8> {
    let mut body = Vec::with_capacity(64);
    let (name, level): (&[u8], u8) = match c.version {
        Version::V31 => (b"MQIsdp", 3),
        Version::V311 => (b"MQTT", 4),
        Version::V5 => (b"MQTT", 5),
    };
    str16(&mut body, name);
    body.push(level);
    let mut flags = 0u8;
    if c.clean {
        flags |= 0x02;
    }
    if let Some(w) = &c.will {
        flags |= 0x04 | ((w.qos & 3) << 3) | (u8::from(w.retain) << 5);
    }
    if c.username.is_some() {
        flags |= 0x80;
        if c.password.is_some() {
            flags |= 0x40;
        }
    }
    body.push(flags);
    body.extend_from_slice(&c.keepalive.to_be_bytes());
    if c.version.is_v5() {
        // No properties.
        body.push(0);
    }
    str16(&mut body, c.client_id.as_bytes());
    if let Some(w) = &c.will {
        if c.version.is_v5() {
            body.push(0);
        }
        str16(&mut body, w.topic.as_bytes());
        str16(&mut body, w.payload);
    }
    if let Some(user) = c.username {
        str16(&mut body, user.as_bytes());
        if let Some(password) = c.password {
            str16(&mut body, password.as_bytes());
        }
    }
    packet(0x10, &body)
}

/// An outgoing `PUBLISH`.
#[derive(Debug, Clone, Copy)]
pub struct Out<'a> {
    pub topic: &'a str,
    pub payload: &'a [u8],
    pub qos: u8,
    pub retain: bool,
    /// Sent again after a reconnect (paho's `dup`).
    pub dup: bool,
    /// For QoS 1 and 2.
    pub packet_id: u16,
    /// MQTT 5's Message Expiry Interval.
    pub expiry: Option<u32>,
}

/// `PUBLISH`.
pub fn publish(version: Version, m: &Out) -> Vec<u8> {
    let mut body = Vec::with_capacity(m.topic.len() + m.payload.len() + 10);
    str16(&mut body, m.topic.as_bytes());
    if m.qos > 0 {
        body.extend_from_slice(&m.packet_id.to_be_bytes());
    }
    if version.is_v5() {
        match m.expiry {
            Some(seconds) => {
                body.push(5);
                body.push(0x02);
                body.extend_from_slice(&seconds.to_be_bytes());
            }
            None => body.push(0),
        }
    }
    body.extend_from_slice(m.payload);
    let dup = if m.dup && m.qos > 0 { 0x08 } else { 0 };
    packet(0x30 | dup | (m.qos << 1) | u8::from(m.retain), &body)
}

/// `SUBSCRIBE` for `topics` (filter, QoS), with MQTT 5's subscription
/// identifier when the broker has them.
pub fn subscribe(
    version: Version,
    packet_id: u16,
    topics: &[(&str, u8)],
    identifier: Option<u32>,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(16 + topics.iter().map(|t| t.0.len() + 3).sum::<usize>());
    body.extend_from_slice(&packet_id.to_be_bytes());
    if version.is_v5() {
        match identifier {
            Some(id) => {
                let mut prop = vec![0x0B];
                varint(&mut prop, id as usize);
                varint(&mut body, prop.len());
                body.extend_from_slice(&prop);
            }
            None => body.push(0),
        }
    }
    for (topic, qos) in topics {
        str16(&mut body, topic.as_bytes());
        // MQTT 5's options are the QoS alone: no "no local", "retain as
        // published" or retain handling, as paho's `SubscribeOptions`.
        body.push(*qos);
    }
    packet(0x82, &body)
}

/// `UNSUBSCRIBE`.
pub fn unsubscribe(version: Version, packet_id: u16, topics: &[&str]) -> Vec<u8> {
    let mut body = Vec::with_capacity(8 + topics.iter().map(|t| t.len() + 2).sum::<usize>());
    body.extend_from_slice(&packet_id.to_be_bytes());
    if version.is_v5() {
        body.push(0);
    }
    for topic in topics {
        str16(&mut body, topic.as_bytes());
    }
    packet(0xA2, &body)
}

/// `PUBACK`, `PUBREC`, `PUBREL` or `PUBCOMP` (`0x40`, `0x50`, `0x62`,
/// `0x70`): two bytes of packet id; MQTT 5's success reason code is left
/// out, as paho leaves it.
pub fn ack(first: u8, packet_id: u16) -> [u8; 4] {
    let [hi, lo] = packet_id.to_be_bytes();
    [first, 2, hi, lo]
}

pub const PUBACK: u8 = 0x40;
pub const PUBREC: u8 = 0x50;
pub const PUBREL: u8 = 0x62;
pub const PUBCOMP: u8 = 0x70;
pub const PINGREQ: [u8; 2] = [0xC0, 0];
pub const DISCONNECT: [u8; 2] = [0xE0, 0];

/// A packet from the broker.
#[derive(Debug, PartialEq, Eq)]
pub enum Incoming<'a> {
    ConnAck {
        /// The return code (3.1.1) or reason code (5).
        code: u8,
        /// MQTT 5: whether the broker said it has no subscription
        /// identifiers (`Subscription Identifier Available` 0).
        no_identifiers: bool,
        /// MQTT 5's `Receive Maximum`: how many QoS 1 and 2 messages the
        /// broker takes unacknowledged (65,535 when it doesn't say).
        receive_maximum: u16,
    },
    Publish(Publish<'a>),
    PubAck(u16),
    PubRec(u16),
    PubRel(u16),
    PubComp(u16),
    SubAck(u16),
    UnsubAck(u16),
    PingResp,
    Disconnect,
}

/// A `PUBLISH` from the broker.
#[derive(Debug, PartialEq, Eq)]
pub struct Publish<'a> {
    /// The topic's bytes: paho skips a message whose topic isn't UTF-8.
    pub topic: &'a [u8],
    pub payload: &'a [u8],
    pub qos: u8,
    pub retain: bool,
    pub packet_id: Option<u16>,
    /// MQTT 5's subscription identifiers (each matching subscription's).
    pub identifiers: Ids,
}

/// A message's subscription identifiers: inline while there are a few
/// (a broker sends one per subscription the message matches), on the heap
/// past that, so every one is still checked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ids {
    ids: [u32; 8],
    len: u8,
    /// All of them, once there are more than fit inline.
    spilled: Vec<u32>,
}

impl Ids {
    pub fn push(&mut self, id: u32) {
        if !self.spilled.is_empty() {
            self.spilled.push(id);
            return;
        }
        match self.ids.get_mut(usize::from(self.len)) {
            Some(slot) => {
                *slot = id;
                self.len += 1;
            }
            None => {
                self.spilled.extend_from_slice(&self.ids);
                self.spilled.push(id);
            }
        }
    }

    pub fn as_slice(&self) -> &[u32] {
        match self.spilled.is_empty() {
            true => &self.ids[..usize::from(self.len)],
            false => &self.spilled,
        }
    }
}

/// One whole packet at the start of `buf`: its first byte, where its body
/// is, and its length. `Ok(None)` while more bytes are needed.
pub fn frame(buf: &[u8]) -> Result<Option<(u8, std::ops::Range<usize>)>, Malformed> {
    let Some(&first) = buf.first() else {
        return Ok(None);
    };
    let mut length = 0usize;
    let mut shift = 0;
    let mut i = 1;
    loop {
        let Some(&byte) = buf.get(i) else {
            return Ok(None);
        };
        length |= usize::from(byte & 0x7F) << shift;
        i += 1;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 21 {
            return Err(Malformed);
        }
    }
    match buf.len() >= i + length {
        true => Ok(Some((first, i..i + length))),
        false => Ok(None),
    }
}

/// How many bytes the packet at the start of `buf` needs in all, once its
/// header is there.
pub fn needed(buf: &[u8]) -> Option<usize> {
    let mut length = 0usize;
    let mut shift = 0;
    for (i, &byte) in buf.iter().enumerate().skip(1).take(4) {
        length |= usize::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some(i + 1 + length);
        }
        shift += 7;
    }
    None
}

fn u16_at(body: &[u8], i: usize) -> Result<u16, Malformed> {
    body.get(i..i + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .ok_or(Malformed)
}

fn read_varint(body: &[u8], i: &mut usize) -> Result<u32, Malformed> {
    let mut value = 0u32;
    for shift in [0, 7, 14, 21] {
        let byte = *body.get(*i).ok_or(Malformed)?;
        *i += 1;
        value |= u32::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(Malformed)
}

/// MQTT 5 properties at `i`: each one is handed to `seen` (its identifier
/// and, for the integer ones, the value); `i` moves past them.
fn properties(body: &[u8], i: &mut usize, mut seen: impl FnMut(u8, u32)) -> Result<(), Malformed> {
    let length = read_varint(body, i)? as usize;
    let end = i.checked_add(length).ok_or(Malformed)?;
    if end > body.len() {
        return Err(Malformed);
    }
    while *i < end {
        let id = body[*i];
        *i += 1;
        let value = match id {
            // Bytes.
            0x01 | 0x17 | 0x19 | 0x24 | 0x25 | 0x28 | 0x29 | 0x2A => {
                let v = *body.get(*i).ok_or(Malformed)?;
                *i += 1;
                u32::from(v)
            }
            // Two-byte integers.
            0x13 | 0x21 | 0x22 | 0x23 => {
                let v = u16_at(body, *i)?;
                *i += 2;
                u32::from(v)
            }
            // Four-byte integers.
            0x02 | 0x11 | 0x18 | 0x27 => {
                let b = body.get(*i..*i + 4).ok_or(Malformed)?;
                *i += 4;
                u32::from_be_bytes([b[0], b[1], b[2], b[3]])
            }
            0x0B => read_varint(body, i)?,
            // Strings and binary data.
            0x03 | 0x08 | 0x09 | 0x12 | 0x15 | 0x16 | 0x1A | 0x1C | 0x1F => {
                let n = usize::from(u16_at(body, *i)?);
                *i += 2 + n;
                0
            }
            // A string pair.
            0x26 => {
                for _ in 0..2 {
                    let n = usize::from(u16_at(body, *i)?);
                    *i += 2 + n;
                }
                0
            }
            _ => return Err(Malformed),
        };
        seen(id, value);
    }
    match *i == end {
        true => Ok(()),
        false => Err(Malformed),
    }
}

/// The packet `first` with `body`, from a broker speaking `version`.
pub fn parse(first: u8, body: &[u8], version: Version) -> Result<Incoming<'_>, Malformed> {
    let v5 = version.is_v5();
    match first >> 4 {
        2 => {
            let code = *body.get(1).ok_or(Malformed)?;
            let mut no_identifiers = false;
            let mut receive_maximum = u16::MAX;
            // A broker without MQTT 5 answers a v5 `CONNECT` in 3.1.1's
            // two bytes (paho: "Unsupported protocol version").
            if v5 && body.len() > 2 {
                let mut i = 2;
                properties(body, &mut i, |id, value| {
                    if id == 0x29 && value == 0 {
                        no_identifiers = true;
                    }
                    if id == 0x21 {
                        receive_maximum = u16::try_from(value).unwrap_or(u16::MAX).max(1);
                    }
                })?;
            }
            Ok(Incoming::ConnAck {
                code,
                no_identifiers,
                receive_maximum,
            })
        }
        3 => {
            let qos = (first >> 1) & 3;
            if qos == 3 {
                return Err(Malformed);
            }
            let n = usize::from(u16_at(body, 0)?);
            let topic = body.get(2..2 + n).ok_or(Malformed)?;
            let mut i = 2 + n;
            let packet_id = match qos {
                0 => None,
                _ => {
                    let id = u16_at(body, i)?;
                    i += 2;
                    Some(id)
                }
            };
            let mut identifiers = Ids::default();
            if v5 {
                properties(body, &mut i, |id, value| {
                    if id == 0x0B {
                        identifiers.push(value);
                    }
                })?;
            }
            Ok(Incoming::Publish(Publish {
                topic,
                payload: &body[i..],
                qos,
                retain: first & 1 == 1,
                packet_id,
                identifiers,
            }))
        }
        4 => Ok(Incoming::PubAck(u16_at(body, 0)?)),
        5 => Ok(Incoming::PubRec(u16_at(body, 0)?)),
        6 => Ok(Incoming::PubRel(u16_at(body, 0)?)),
        7 => Ok(Incoming::PubComp(u16_at(body, 0)?)),
        9 => Ok(Incoming::SubAck(u16_at(body, 0)?)),
        11 => Ok(Incoming::UnsubAck(u16_at(body, 0)?)),
        13 => Ok(Incoming::PingResp),
        14 => Ok(Incoming::Disconnect),
        _ => Err(Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_length_takes_up_to_four_bytes() {
        for n in [
            0,
            127,
            128,
            16_383,
            16_384,
            2_097_151,
            2_097_152,
            MAX_REMAINING,
        ] {
            let mut out = vec![0x30];
            varint(&mut out, n);
            assert_eq!(needed(&out), Some(out.len() + n), "{n}");
        }
        // A fifth length byte is refused.
        assert_eq!(frame(&[0x30, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]), Err(Malformed));
        assert_eq!(frame(&[0x30, 0x80]), Ok(None));
    }

    #[test]
    fn a_v5_publish_carries_its_subscription_identifiers() {
        // Topic "a/b", QoS 1, id 7, properties: identifiers 2 and 300.
        let body = [
            0, 3, b'a', b'/', b'b', 0, 7, 5, 0x0B, 2, 0x0B, 0xAC, 0x02, b'h', b'i',
        ];
        let Incoming::Publish(p) = parse(0x32, &body, Version::V5).unwrap() else {
            panic!()
        };
        assert_eq!(p.topic, b"a/b");
        assert_eq!(p.packet_id, Some(7));
        assert_eq!(p.identifiers.as_slice(), [2, 300]);
        let mut many = Ids::default();
        for id in 1..=10 {
            many.push(id);
        }
        assert_eq!(many.as_slice(), (1..=10).collect::<Vec<_>>());
        assert_eq!(p.payload, b"hi");
        // The same bytes from a 3.1.1 broker: no properties.
        let Incoming::Publish(p) = parse(0x32, &body, Version::V311).unwrap() else {
            panic!()
        };
        assert_eq!(p.payload, &body[7..]);
    }

    #[test]
    fn connack_without_v5_properties_is_a_refusal_from_an_older_broker() {
        assert_eq!(
            parse(0x20, &[0, 1], Version::V5),
            Ok(Incoming::ConnAck {
                code: 1,
                no_identifiers: false,
                receive_maximum: u16::MAX
            })
        );
        assert_eq!(
            parse(0x20, &[0, 0, 5, 0x29, 0, 0x21, 0, 1], Version::V5),
            Ok(Incoming::ConnAck {
                code: 0,
                no_identifiers: true,
                receive_maximum: 1
            })
        );
    }
}
