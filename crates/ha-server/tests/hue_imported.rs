//! Drives the real `hars` binary on the store files Home Assistant
//! 2026.6.1 left of an install with a Hue bridge
//! (`scripts/vectors/hue_rows.py`): entities of every kind the user
//! disabled, hid, renamed, moved or gave another unit, rows that never
//! ran, rows with nothing behind them, removed rows and devices. A start
//! and a clean stop must leave the entries, the registries and the areas
//! byte for byte as they were, as a start of HA itself does.
//!
//! The bridge is a listener on 127.0.0.1 that a debug build is pointed at
//! (`HARS_DEV_HUE`); nothing reaches a real bridge.
#![cfg(debug_assertions)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const RESOURCES: &str =
    include_str!("../../ha-integrations/src/hue/vectors/imported/resource.json");
const STORES: [(&str, &str); 4] = [
    (
        "core.config_entries",
        include_str!("../../ha-integrations/src/hue/vectors/imported/core.config_entries"),
    ),
    (
        "core.device_registry",
        include_str!("../../ha-integrations/src/hue/vectors/imported/core.device_registry"),
    ),
    (
        "core.entity_registry",
        include_str!("../../ha-integrations/src/hue/vectors/imported/core.entity_registry"),
    ),
    (
        "core.area_registry",
        include_str!("../../ha-integrations/src/hue/vectors/imported/core.area_registry"),
    ),
];

/// The install's bridge: its resources, and an event stream that says
/// nothing. How often the resources were read.
fn bridge() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = reads.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let reads = counted.clone();
            std::thread::spawn(move || {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if !matches!(stream.read(&mut byte), Ok(1)) {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let path = head.split(' ').nth(1).unwrap_or_default();
                if path.contains("/eventstream/") {
                    let open = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n: hi\n\n";
                    let _ = stream.write_all(open.as_bytes());
                    // Until `hars` goes.
                    while matches!(stream.read(&mut byte), Ok(1)) {}
                    return;
                }
                let (status, body) = match path.ends_with("/clip/v2/resource") {
                    true => (200, RESOURCES),
                    false => (404, r#"{"errors":[{"description":"not found"}],"data":[]}"#),
                };
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
                if status == 200 {
                    reads.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });
    (port, reads)
}

#[cfg(unix)]
#[test]
fn a_start_on_home_assistants_files_leaves_them_byte_for_byte() {
    let (bridge, reads) = bridge();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dir = std::env::temp_dir().join(format!("hars-test-hue-imported-{port}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".storage")).unwrap();
    for (name, text) in STORES {
        std::fs::write(dir.join(".storage").join(name), text).unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_hars"))
        .env_remove("MIMALLOC_ALLOW_THP")
        .env("HARS_CONFIG", &dir)
        .env("HARS_BIND", format!("127.0.0.1:{port}"))
        .env("HARS_DEV_HUE", format!("http://127.0.0.1:{bridge}"))
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Set up (the resources are read), then its event stream connected
    // (they are read again): the bridge's entities run.
    let deadline = Instant::now() + Duration::from_secs(20);
    while reads.load(Ordering::SeqCst) < 2 || TcpStream::connect(("127.0.0.1", port)).is_err() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!(
                "hars did not set the bridge up: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(500));
    // A clean stop writes whatever is pending.
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
