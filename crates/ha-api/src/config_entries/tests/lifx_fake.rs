//! The fake bulbs of `scripts/vectors/lifx_flow.py` (`Bulb`, `Lifx`), on
//! 127.0.0.1: one socket for each made-up bulb address, one for the
//! broadcast. Each keeps what it got as that script records what HA sent.

use ha_integrations::lifx;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;

pub const COLOUR: &str = "192.0.2.20";
pub const WHITE: &str = "192.0.2.21";
pub const SWITCH: &str = "192.0.2.23";
pub const SILENT: &str = "192.0.2.24";
/// The colour bulb's new address.
pub const ALIAS: &str = "192.0.2.25";
pub const GHOST: &str = "192.0.2.26";
const OTHER_SERIAL: [u8; 6] = [0xd0, 0x73, 0xd5, 0, 0, 0x2f];

/// A bulb's state, shared by the addresses that reach it.
struct State {
    serial: [u8; 6],
    label: &'static str,
    group: &'static str,
    product: u32,
    firmware: (u32, u32),
    /// Whether it answers what is sent to its address.
    unicast: bool,
    silent: bool,
    power: u16,
    color: [u16; 4],
    /// Once it has told its label, another bulb answers there.
    swaps: bool,
    swapped: bool,
}

struct Bulb {
    ip: &'static str,
    sock: Arc<UdpSocket>,
    state: Arc<Mutex<State>>,
}

pub struct LifxFake {
    bulbs: Vec<Bulb>,
    /// The addresses of the bulbs that answer a broadcast.
    discovering: Mutex<Vec<&'static str>>,
    frames: Arc<Mutex<Vec<Value>>>,
    /// Where `hars` is pointed.
    pub at: lifx::Where,
}

fn frame(source: u32, target: [u8; 6], seq: u8, ty: u16, payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&((36 + payload.len()) as u16).to_le_bytes());
    b.extend_from_slice(&0x1400u16.to_le_bytes());
    b.extend_from_slice(&source.to_le_bytes());
    b.extend_from_slice(&target);
    b.extend_from_slice(&[0; 8]);
    b.push(0);
    b.push(seq);
    b.extend_from_slice(&[0; 8]);
    b.extend_from_slice(&ty.to_le_bytes());
    b.extend_from_slice(&[0; 2]);
    b.extend_from_slice(payload);
    b
}

fn text(value: &str, size: usize) -> Vec<u8> {
    let mut b = value.as_bytes().to_vec();
    b.resize(size, 0);
    b
}

/// What the generator records of a datagram.
fn record(to: &str, data: &[u8]) -> Option<Value> {
    if data.len() < 36 {
        return None;
    }
    let size = u16::from_le_bytes([data[0], data[1]]) as usize;
    let proto = u16::from_le_bytes([data[2], data[3]]);
    let ty = u16::from_le_bytes([data[32], data[33]]);
    let tagged = proto & 0x2000 != 0;
    // The coordinator's polls: when they fall depends on the steps.
    if ty == 101 && !tagged {
        return None;
    }
    let target: Vec<String> = data[8..14].iter().map(|b| format!("{b:02x}")).collect();
    let payload: String = data[36..size.min(data.len())]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Some(
        json!({"to": to, "tagged": tagged, "target": target.join(":"),
                "ack": data[22] & 2 != 0, "res": data[22] & 1 != 0, "seq": data[23],
                "type": ty, "payload": payload}),
    )
}

/// `Bulb.reply`: what the bulb sends back.
fn reply(state: &Mutex<State>, data: &[u8]) -> Vec<Vec<u8>> {
    let mut s = state.lock();
    if s.silent || data.len() < 36 {
        return Vec::new();
    }
    let source = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let (seq, ty) = (data[23], u16::from_le_bytes([data[32], data[33]]));
    let payload = &data[36..];
    let serial = if s.swapped { OTHER_SERIAL } else { s.serial };
    let mut out = Vec::new();
    if data[22] & 2 != 0 {
        out.push(frame(source, serial, seq, 45, &[]));
    }
    let (major, minor) = s.firmware;
    let mut state = |kind: u16, body: Vec<u8>| out.push(frame(source, serial, seq, kind, &body));
    match ty {
        2 => state(3, [vec![1], 56700u32.to_le_bytes().to_vec()].concat()),
        14 => {
            let mut b = vec![0; 16];
            b.extend_from_slice(&(major << 16 | minor).to_le_bytes());
            state(15, b);
        }
        16 => {
            let mut b = 1e-5f32.to_le_bytes().to_vec();
            b.extend_from_slice(&[0; 10]);
            state(17, b);
        }
        23 => {
            state(25, text(s.label, 32));
            if s.swaps {
                s.swapped = true;
            }
        }
        32 => {
            let mut b = 1u32.to_le_bytes().to_vec();
            b.extend_from_slice(&s.product.to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes());
            state(33, b);
        }
        51 => {
            let mut b = vec![0x11; 16];
            b.extend(text(s.group, 32));
            b.extend_from_slice(&[0; 8]);
            state(53, b);
        }
        101 => {
            let mut b = Vec::new();
            for c in s.color {
                b.extend_from_slice(&c.to_le_bytes());
            }
            b.extend_from_slice(&[0; 2]);
            b.extend_from_slice(&s.power.to_le_bytes());
            b.extend(text(s.label, 32));
            b.extend_from_slice(&[0; 8]);
            state(107, b);
        }
        117 if payload.len() >= 2 => {
            s.power = u16::from_le_bytes([payload[0], payload[1]]);
        }
        119 if payload.len() >= 21 => {
            for i in 0..4 {
                if payload[17 + i] != 0 {
                    s.color[i] = u16::from_le_bytes([payload[2 + 2 * i], payload[3 + 2 * i]]);
                }
            }
        }
        _ => {}
    }
    out
}

impl LifxFake {
    pub async fn start() -> Arc<Self> {
        let bulb = |serial: u8, label, group, product, firmware| {
            Arc::new(Mutex::new(State {
                serial: [0xd0, 0x73, 0xd5, 0, 0, serial],
                label,
                group,
                product,
                firmware,
                unicast: true,
                silent: false,
                power: 65535,
                color: [0, 0, 39321, 2700],
                swaps: false,
                swapped: false,
            }))
        };
        let colour = bulb(0x20, "Desk Lamp", "Office", 91, (3, 70));
        let ghost = bulb(0x26, "Ghost", "Attic", 91, (3, 70));
        ghost.lock().unicast = false;
        let silent = bulb(0x24, "Nobody", "Office", 91, (3, 70));
        silent.lock().silent = true;
        let states = [
            (COLOUR, colour.clone()),
            (WHITE, bulb(0x21, "Hall Light", "Hallway", 51, (2, 77))),
            (SWITCH, bulb(0x23, "Wall Switch", "Office", 70, (3, 70))),
            (SILENT, silent),
            (ALIAS, colour),
            (GHOST, ghost),
        ];
        let mut bulbs = Vec::new();
        for (ip, state) in states {
            let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            bulbs.push(Bulb {
                ip,
                sock: Arc::new(sock),
                state,
            });
        }
        let broadcast = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let stand_ins: Vec<(String, SocketAddr)> = bulbs
            .iter()
            .map(|b| (b.ip.to_owned(), b.sock.local_addr().unwrap()))
            .collect();
        let fake = Arc::new(Self {
            bulbs,
            discovering: Mutex::new(vec![COLOUR, WHITE]),
            frames: Arc::default(),
            at: lifx::Where {
                broadcast: broadcast.local_addr().unwrap(),
                stand_ins,
                // HA's waits, shorter, in the same proportions.
                flow: lifx::config_flow::Timing {
                    resend: lifx::conn::Resend {
                        every: Duration::from_millis(25),
                        sends: 3,
                    },
                    per_try: Duration::from_millis(150),
                    discovery: Duration::from_millis(300),
                },
            },
        });
        for i in 0..fake.bulbs.len() {
            let this = fake.clone();
            tokio::spawn(async move {
                let bulb = &this.bulbs[i];
                let mut buf = vec![0u8; 1024];
                while let Ok((n, from)) = bulb.sock.recv_from(&mut buf).await {
                    this.frames.lock().extend(record(bulb.ip, &buf[..n]));
                    if !bulb.state.lock().unicast {
                        continue;
                    }
                    for out in reply(&bulb.state, &buf[..n]) {
                        let _ = bulb.sock.send_to(&out, from).await;
                    }
                }
            });
        }
        let this = fake.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 1024];
            while let Ok((n, from)) = broadcast.recv_from(&mut buf).await {
                this.frames.lock().extend(record("broadcast", &buf[..n]));
                let discovering = this.discovering.lock().clone();
                for ip in discovering {
                    let bulb = this.bulbs.iter().find(|b| b.ip == ip).unwrap();
                    for out in reply(&bulb.state, &buf[..n]) {
                        let _ = bulb.sock.send_to(&out, from).await;
                    }
                }
            }
        });
        fake
    }

    /// `Lifx.set`: true if the mode was one of the bulbs'.
    pub fn set(&self, mode: &str) -> bool {
        match mode {
            "discovery_none" => *self.discovering.lock() = Vec::new(),
            "discovery_ghost" => *self.discovering.lock() = vec![GHOST],
            "colour_swaps" => {
                let bulb = self.bulbs.iter().find(|b| b.ip == COLOUR).unwrap();
                bulb.state.lock().swaps = true;
            }
            _ => return false,
        }
        true
    }

    /// The bulb at `ip` says it is another product.
    pub fn set_product(&self, ip: &str, product: u32) {
        let bulb = self.bulbs.iter().find(|b| b.ip == ip).unwrap();
        bulb.state.lock().product = product;
    }

    pub fn take(&self) -> Vec<Value> {
        std::mem::take(&mut *self.frames.lock())
    }
}
