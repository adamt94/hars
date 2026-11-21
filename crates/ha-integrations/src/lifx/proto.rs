//! The LIFX LAN protocol as aiolifx 1.2.1 frames it: a 36-byte
//! little-endian header, then the payload.

/// Message types `hars` sends or reads.
pub mod ty {
    pub const GET_SERVICE: u16 = 2;
    pub const STATE_SERVICE: u16 = 3;
    pub const GET_HOST_FIRMWARE: u16 = 14;
    pub const STATE_HOST_FIRMWARE: u16 = 15;
    pub const GET_VERSION: u16 = 32;
    pub const STATE_VERSION: u16 = 33;
    pub const GET_LABEL: u16 = 23;
    pub const STATE_LABEL: u16 = 25;
    pub const SET_REBOOT: u16 = 38;
    pub const ACK: u16 = 45;
    pub const GET_GROUP: u16 = 51;
    pub const STATE_GROUP: u16 = 53;
    pub const LIGHT_GET: u16 = 101;
    pub const LIGHT_STATE: u16 = 107;
    pub const LIGHT_SET_POWER: u16 = 117;
    pub const LIGHT_SET_WAVEFORM_OPTIONAL: u16 = 119;
}

/// The header fields that matter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub source: u32,
    /// The target's serial (the first 6 of its 8 bytes).
    pub target: [u8; 6],
    pub seq: u8,
    pub ty: u16,
}

/// A message to send. A target of all zeros is sent `tagged`.
#[derive(Debug, Clone, PartialEq)]
pub struct Out<'a> {
    pub source: u32,
    pub target: [u8; 6],
    pub seq: u8,
    pub ack: bool,
    pub res: bool,
    pub ty: u16,
    pub payload: &'a [u8],
}

impl Out<'_> {
    pub fn encode(&self) -> Vec<u8> {
        let size = 36 + self.payload.len();
        let mut b = Vec::with_capacity(size);
        b.extend_from_slice(&(size as u16).to_le_bytes());
        // origin 0, addressable, protocol 1024; tagged for "any".
        let tagged = self.target == [0; 6];
        let proto: u16 = 0x1400 | if tagged { 0x2000 } else { 0 };
        b.extend_from_slice(&proto.to_le_bytes());
        b.extend_from_slice(&self.source.to_le_bytes());
        b.extend_from_slice(&self.target);
        b.extend_from_slice(&[0; 2]);
        b.extend_from_slice(&[0; 6]);
        b.push(u8::from(self.ack) << 1 | u8::from(self.res));
        b.push(self.seq);
        b.extend_from_slice(&[0; 8]);
        b.extend_from_slice(&self.ty.to_le_bytes());
        b.extend_from_slice(&[0; 2]);
        b.extend_from_slice(self.payload);
        b
    }
}

/// A received datagram's header and payload, if it holds a whole one.
pub fn decode(b: &[u8]) -> Option<(Header, &[u8])> {
    if b.len() < 36 {
        return None;
    }
    let size = u16::from_le_bytes([b[0], b[1]]) as usize;
    if size < 36 || b.len() < size {
        return None;
    }
    let mut target = [0; 6];
    target.copy_from_slice(&b[8..14]);
    Some((
        Header {
            source: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            target,
            seq: b[23],
            ty: u16::from_le_bytes([b[32], b[33]]),
        },
        &b[36..size],
    ))
}

/// `"d0:73:d5:01:02:03"`.
pub fn mac(serial: &[u8; 6]) -> String {
    serial
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn u16_at(p: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(p.get(i..i + 2)?.try_into().ok()?))
}

fn u32_at(p: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(p.get(i..i + 4)?.try_into().ok()?))
}

/// A label: NULs removed anywhere (aiolifx's `.replace("\x00", "")`).
fn label(p: &[u8]) -> String {
    String::from_utf8_lossy(p).replace('\0', "")
}

/// `LightState`: HSBK, power and label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LightState {
    pub color: [u16; 4],
    pub power: u16,
    pub label: String,
}

impl LightState {
    pub fn parse(p: &[u8]) -> Option<Self> {
        Some(Self {
            color: [u16_at(p, 0)?, u16_at(p, 2)?, u16_at(p, 4)?, u16_at(p, 6)?],
            power: u16_at(p, 10)?,
            label: label(p.get(12..44)?),
        })
    }
}

/// `StateHostFirmware`'s version as aiolifx shows it (`"1.22"`).
pub fn host_firmware(p: &[u8]) -> Option<String> {
    let v = u32_at(p, 16)?;
    Some(format!("{}.{}", v >> 16, v & 0xffff))
}

/// `StateVersion`'s product.
pub fn product(p: &[u8]) -> Option<u32> {
    u32_at(p, 4)
}

/// `StateLabel`'s label.
pub fn state_label(p: &[u8]) -> Option<String> {
    Some(label(p.get(..32)?))
}

/// `StateGroup`'s label.
pub fn group(p: &[u8]) -> Option<String> {
    Some(label(p.get(16..48)?))
}

/// `StateService`: (service, port).
pub fn service(p: &[u8]) -> Option<(u8, u32)> {
    Some((*p.first()?, u32_at(p, 1)?))
}

/// `LightSetPower`.
pub fn set_power(level: u16, duration_ms: u32) -> Vec<u8> {
    let mut b = level.to_le_bytes().to_vec();
    b.extend_from_slice(&duration_ms.to_le_bytes());
    b
}

/// `LightSetWaveformOptional`.
#[derive(Debug, Clone, PartialEq)]
pub struct Waveform {
    pub transient: bool,
    pub color: [u16; 4],
    pub period_ms: u32,
    pub cycles: f32,
    pub skew_ratio: i16,
    pub waveform: u8,
    /// Which of H, S, B, K to set.
    pub set: [bool; 4],
}

impl Waveform {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![0, u8::from(self.transient)];
        for c in self.color {
            b.extend_from_slice(&c.to_le_bytes());
        }
        b.extend_from_slice(&self.period_ms.to_le_bytes());
        b.extend_from_slice(&self.cycles.to_le_bytes());
        b.extend_from_slice(&self.skew_ratio.to_le_bytes());
        b.push(self.waveform);
        b.extend(self.set.map(u8::from));
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// The real bulb's replies to aiolifx's requests.
    #[test]
    fn codec_against_the_real_bulb() {
        let probe: serde_json::Value =
            serde_json::from_str(include_str!("vectors/probe.json")).unwrap();
        let mut checked = 0;
        for r in probe["records"].as_array().unwrap() {
            let sent = hex(r["sent_hex"].as_str().unwrap());
            let (h, payload) = decode(&sent).unwrap();
            let ty = r["request_type"].as_u64().unwrap() as u16;
            assert_eq!(h.ty, ty);
            // Re-encoded as aiolifx sent it (res required, no ack).
            let target = h.target;
            let flags = sent[22];
            let out = Out {
                source: h.source,
                target,
                seq: h.seq,
                ack: flags & 2 != 0,
                res: flags & 1 != 0,
                ty,
                payload,
            };
            // The probe script didn't tag its zero-target requests;
            // aiolifx (and so `hars`) does: only that bit may differ.
            let mut enc = out.encode();
            assert_eq!(enc[3] & !0x20, sent[3] & !0x20);
            assert_eq!(enc[3] & 0x20 != 0, target == [0; 6]);
            enc[3] = sent[3];
            assert_eq!(enc, sent, "{}", r["request"]);
            for reply in r["replies"].as_array().unwrap() {
                let b = hex(reply["hex"].as_str().unwrap());
                let (h, p) = decode(&b).unwrap();
                let d = &reply["decoded"];
                assert_eq!(h.ty as u64, d["type"].as_u64().unwrap());
                assert_eq!(h.seq as u64, d["seq_num"].as_u64().unwrap());
                assert_eq!(h.source as u64, d["source_id"].as_u64().unwrap());
                assert_eq!(mac(&h.target), d["target_addr"].as_str().unwrap());
                match h.ty {
                    ty::LIGHT_STATE => {
                        let s = LightState::parse(p).unwrap();
                        assert_eq!(s.color, [0, 0, 39321, 2700]);
                        assert_eq!(s.power, 65535);
                        assert_eq!(s.label, "Kitchen Light");
                    }
                    ty::STATE_HOST_FIRMWARE => assert_eq!(host_firmware(p).unwrap(), "1.22"),
                    ty::STATE_VERSION => assert_eq!(product(p), Some(22)),
                    ty::STATE_GROUP => assert_eq!(group(p).unwrap(), "Kitchen"),
                    ty::STATE_SERVICE => {
                        assert_eq!(
                            Some(d["service"].as_u64().unwrap() as u8),
                            service(p).map(|s| s.0)
                        )
                    }
                    _ => {}
                }
                checked += 1;
            }
        }
        assert!(checked >= 12, "{checked}");
    }

    #[test]
    fn encodes_what_ha_sends() {
        // LightSetPower(65535, 0), seq 6, to the serial (vector 1).
        let out = Out {
            source: u32::from_le_bytes([0x71, 0x42, 0xbd, 0xa2]),
            target: [0xd0, 0x73, 0xd5, 0x01, 0x02, 0x03],
            seq: 6,
            ack: true,
            res: false,
            ty: ty::LIGHT_SET_POWER,
            payload: &set_power(65535, 0),
        };
        assert_eq!(
            out.encode(),
            hex(
                "2a0000147142bda2d073d501020300000000000000000206000000000000000075000000ffff00000000"
            )
        );
    }
}
