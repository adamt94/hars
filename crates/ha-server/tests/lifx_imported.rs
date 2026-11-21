//! Drives the real `hars` binary on the store files Home Assistant
//! 2026.6.1 left of an install with LIFX bulbs
//! (`scripts/vectors/lifx_rows.py`). A start and a clean stop must leave
//! the entries, the registries and the areas byte for byte as they were,
//! as a start of HA itself does.
//!
//! The bulbs are sockets on 127.0.0.1 that a debug build is pointed at
//! (`HARS_DEV_LIFX`); nothing reaches a real bulb or the network.
#![cfg(debug_assertions)]

use std::io::Read;
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const STORES: [(&str, &str); 4] = [
    (
        "core.config_entries",
        include_str!("../../ha-integrations/src/lifx/vectors/imported/core.config_entries"),
    ),
    (
        "core.device_registry",
        include_str!("../../ha-integrations/src/lifx/vectors/imported/core.device_registry"),
    ),
    (
        "core.entity_registry",
        include_str!("../../ha-integrations/src/lifx/vectors/imported/core.entity_registry"),
    ),
    (
        "core.area_registry",
        include_str!("../../ha-integrations/src/lifx/vectors/imported/core.area_registry"),
    ),
];

fn text(value: &str, size: usize) -> Vec<u8> {
    let mut b = value.as_bytes().to_vec();
    b.resize(size, 0);
    b
}

/// A bulb of the install: it answers what set-up asks, as
/// `lifx_flow.py`'s fake does. How many `LightGet`s it answered.
fn bulb(
    serial: u8,
    label: &'static str,
    group: &'static str,
    product: u32,
    fw: (u32, u32),
) -> (u16, Arc<AtomicUsize>) {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = sock.local_addr().unwrap().port();
    let polls = Arc::new(AtomicUsize::new(0));
    let counted = polls.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            if n < 36 {
                continue;
            }
            let data = &buf[..n];
            let source = &data[4..8];
            let (seq, ty) = (data[23], u16::from_le_bytes([data[32], data[33]]));
            let reply = |kind: u16, body: Vec<u8>| {
                let mut b = Vec::new();
                b.extend_from_slice(&((36 + body.len()) as u16).to_le_bytes());
                b.extend_from_slice(&0x1400u16.to_le_bytes());
                b.extend_from_slice(source);
                b.extend_from_slice(&[0xd0, 0x73, 0xd5, 0, 0, serial, 0, 0]);
                b.extend_from_slice(&[0; 6]);
                b.extend_from_slice(&[0, seq]);
                b.extend_from_slice(&[0; 8]);
                b.extend_from_slice(&kind.to_le_bytes());
                b.extend_from_slice(&[0; 2]);
                b.extend(body);
                b
            };
            let out = match ty {
                14 => {
                    let mut b = vec![0; 16];
                    b.extend_from_slice(&(fw.0 << 16 | fw.1).to_le_bytes());
                    reply(15, b)
                }
                32 => {
                    let mut b = 1u32.to_le_bytes().to_vec();
                    b.extend_from_slice(&product.to_le_bytes());
                    b.extend_from_slice(&[0; 4]);
                    reply(33, b)
                }
                51 => {
                    let mut b = vec![0x11; 16];
                    b.extend(text(group, 32));
                    b.extend_from_slice(&[0; 8]);
                    reply(53, b)
                }
                101 => {
                    counted.fetch_add(1, Ordering::SeqCst);
                    let mut b = Vec::new();
                    for c in [0u16, 0, 39321, 2700] {
                        b.extend_from_slice(&c.to_le_bytes());
                    }
                    b.extend_from_slice(&[0; 2]);
                    b.extend_from_slice(&65535u16.to_le_bytes());
                    b.extend(text(label, 32));
                    b.extend_from_slice(&[0; 8]);
                    reply(107, b)
                }
                _ => continue,
            };
            let _ = sock.send_to(&out, from);
        }
    });
    (port, polls)
}

#[cfg(unix)]
#[test]
fn a_start_on_home_assistants_lifx_files_leaves_them_byte_for_byte() {
    let (colour, colour_polls) = bulb(0x20, "Desk Lamp", "Office", 91, (3, 70));
    let (white, white_polls) = bulb(0x41, "Hall Light", "Hallway", 51, (2, 77));
    // The porch bulb is gone: a socket that answers nothing.
    let porch = UdpSocket::bind("127.0.0.1:0").unwrap();
    let broadcast = UdpSocket::bind("127.0.0.1:0").unwrap();
    let stand_ins = format!(
        "broadcast={},192.0.2.20=127.0.0.1:{colour},192.0.2.21=127.0.0.1:{white},192.0.2.27={}",
        broadcast.local_addr().unwrap(),
        porch.local_addr().unwrap()
    );
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dir = std::env::temp_dir().join(format!("hars-test-lifx-imported-{port}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".storage")).unwrap();
    for (name, text) in STORES {
        std::fs::write(dir.join(".storage").join(name), text).unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_hars"))
        .env_remove("MIMALLOC_ALLOW_THP")
        .env("HARS_CONFIG", &dir)
        .env("HARS_BIND", format!("127.0.0.1:{port}"))
        .env("HARS_DEV_LIFX", stand_ins)
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Both bulbs that answer are set up (their first `LightGet`).
    let deadline = Instant::now() + Duration::from_secs(20);
    while colour_polls.load(Ordering::SeqCst) < 1
        || white_polls.load(Ordering::SeqCst) < 1
        || TcpStream::connect(("127.0.0.1", port)).is_err()
    {
        if Instant::now() > deadline {
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!(
                "hars did not set the bulbs up: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(500));
    let stopped = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(stopped.success());
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "hars did not stop");
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut log = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut log)
        .unwrap();
    assert!(status.success(), "{log}");
    for (name, before) in STORES {
        let after = std::fs::read_to_string(dir.join(".storage").join(name)).unwrap();
        if after != before {
            let changed = before.lines().zip(after.lines()).find(|(b, a)| b != a);
            panic!("{name} was changed by the start: {changed:#?}\n{log}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
