//! Drives the real `hars` binary: a stop signal must shut it down cleanly.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn spawn_hars(port: u16) -> Child {
    spawn_hars_with(port, &[])
}

fn spawn_hars_with(port: u16, env: &[(&str, &str)]) -> Child {
    let child = Command::new(env!("CARGO_BIN_EXE_hars"))
        .env_remove("MIMALLOC_ALLOW_THP")
        .envs(env.iter().copied())
        .env("HARS_BIND", format!("127.0.0.1:{port}"))
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "hars did not start listening");
        std::thread::sleep(Duration::from_millis(1));
    }
    child
}

fn signal(child: &Child, sig: &str) {
    let ok = Command::new("kill")
        .args([sig, &child.id().to_string()])
        .status()
        .unwrap()
        .success();
    assert!(ok, "kill {sig} failed");
}

/// Wait for exit, failing if it takes longer than `limit`.
fn wait_with_limit(mut child: Child, limit: Duration) -> (std::process::ExitStatus, String) {
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("hars did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    (status, out)
}

#[cfg(unix)]
#[test]
fn sigterm_stops_cleanly_with_an_idle_keep_alive_connection() {
    let port = free_port();
    let child = spawn_hars(port);

    // A browser-style keep-alive connection that has finished its request.
    let mut conn = TcpStream::connect(("127.0.0.1", port)).unwrap();
    conn.write_all(b"GET /api/ HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let mut buf = [0u8; 512];
    let n = conn.read(&mut buf).unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));

    signal(&child, "-TERM");
    let (status, out) = wait_with_limit(child, Duration::from_secs(5));
    assert!(status.success(), "exit {status}: {out}");
    assert!(out.contains("SIGTERM received, stopping"), "{out}");
    assert!(out.contains("hars stopped"), "{out}");
}

/// A client stuck mid-request must not hold shutdown past the drain
/// timeout, which has to fit in `docker stop`'s 10 s grace period.
#[cfg(unix)]
#[test]
fn stuck_request_is_cut_off_within_dockers_grace_period() {
    let port = free_port();
    let child = spawn_hars(port);
    let mut conn = TcpStream::connect(("127.0.0.1", port)).unwrap();
    conn.write_all(
        b"POST /api/states/sensor.x HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{",
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let start = Instant::now();
    signal(&child, "-TERM");
    let (status, out) = wait_with_limit(child, Duration::from_secs(9));
    assert!(status.success(), "exit {status}: {out}");
    assert!(out.contains("hars stopped"), "{out}");
    assert!(
        start.elapsed() < Duration::from_secs(8),
        "{:?}",
        start.elapsed()
    );
}

#[cfg(unix)]
#[test]
fn sigint_stops_cleanly() {
    let port = free_port();
    let child = spawn_hars(port);
    signal(&child, "-INT");
    let (status, out) = wait_with_limit(child, Duration::from_secs(5));
    assert!(status.success(), "exit {status}: {out}");
    assert!(out.contains("SIGINT received, stopping"), "{out}");
}

/// A supervisor may signal the moment the port answers: the handlers must
/// already be installed by then.
#[cfg(unix)]
#[test]
fn signal_right_after_the_port_opens_still_stops_cleanly() {
    for sig in ["-TERM", "-INT"].repeat(10) {
        let child = spawn_hars(free_port());
        signal(&child, sig);
        let (status, out) = wait_with_limit(child, Duration::from_secs(5));
        assert!(status.success(), "{sig}: exit {status}: {out}");
    }
}

/// THP is off for the whole process (docs/PERF.md): the kernel reports
/// `THP_enabled: 0` and the command line is unchanged by the re-exec, unless
/// `MIMALLOC_ALLOW_THP=1` keeps THP on.
#[cfg(target_os = "linux")]
#[test]
fn transparent_huge_pages_are_disabled() {
    let status = |child: &Child, key: &str| {
        std::fs::read_to_string(format!("/proc/{}/status", child.id()))
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix(key).map(|v| v.trim().to_owned()))
            .unwrap_or_else(|| panic!("kernel reports {key}"))
    };
    let cmdline = |child: &Child| std::fs::read(format!("/proc/{}/cmdline", child.id())).unwrap();

    for (env, expected) in [(&[][..], "0"), (&[("MIMALLOC_ALLOW_THP", "1")][..], "1")] {
        let child = spawn_hars_with(free_port(), env);
        assert_eq!(status(&child, "THP_enabled:"), expected, "{env:?}");
        let argv0 = format!("{}\0", env!("CARGO_BIN_EXE_hars")).into_bytes();
        assert_eq!(cmdline(&child), argv0, "argv[0] survives the re-exec");
        signal(&child, "-TERM");
        let (status, out) = wait_with_limit(child, Duration::from_secs(5));
        assert!(status.success(), "exit {status}: {out}");
    }
}
