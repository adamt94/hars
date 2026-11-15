//! `kill -9` while the writer is busy: what was committed is all there, in
//! order, and the store opens (a torn last frame is cut, never trusted).
//!
//! The test runs itself as the writer (`HARS_CRASH_DIR` set), reads what the
//! writer says it has committed, kills it with SIGKILL, then reopens the
//! store.
#![cfg(unix)]

use ha_recorder::{Config, Rec, Row, StateRow, Store};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

const ENV: &str = "HARS_CRASH_DIR";
const T0: i64 = 1_768_464_000_000_000;

fn config(dir: PathBuf) -> Config {
    Config {
        commit_interval: Duration::from_millis(50),
        ..Config::new(dir)
    }
}

fn row(n: i64) -> Row {
    Row::State(StateRow {
        entity_id: "sensor.counter".into(),
        ts: T0 + n,
        last_changed: T0 + n,
        state: n.to_string().into(),
        attributes: Arc::from(r#"{"unit_of_measurement":"x"}"#),
        flags: 0,
        context: [1; 16],
        aug: None,
    })
}

/// The writer: rows `0, 1, 2, …` as fast as it can, and after each
/// `flush` a line with the count the store has committed.
#[tokio::test(flavor = "multi_thread")]
async fn crash_writer() {
    let Ok(dir) = std::env::var(ENV) else { return };
    let store = Store::open(config(dir.into())).unwrap();
    let mut n = 0;
    loop {
        for _ in 0..500 {
            store.record(row(n));
            n += 1;
        }
        // `flush` is the commit: once it says so, those rows are written.
        if store.flush().await {
            println!("committed {n}");
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[test]
fn kill_9_loses_at_most_the_uncommitted_batch_and_never_corrupts() {
    if std::env::var(ENV).is_ok() {
        return;
    }
    let root = std::env::temp_dir().join(format!("hars-crash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    // Killed at different moments: after a different number of commits.
    for round in 1..=6 {
        let dir = root.join(round.to_string());
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["crash_writer", "--exact", "--nocapture", "--test-threads=1"])
            .env(ENV, &dir)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let (mut committed, mut seen) = (0i64, 0);
        while seen < round {
            let line = lines.next().expect("the writer died early").unwrap();
            if let Some(n) = line.strip_prefix("committed ") {
                committed = n.parse().unwrap();
                seen += 1;
            }
        }
        // Anywhere in the next batch: between commits, or mid-write.
        std::thread::sleep(Duration::from_micros(round as u64 * 4_321 % 23_000));
        child.kill().unwrap();
        child.wait().unwrap();

        // Reopening cuts a torn tail. What is left is rows `0..n` in order,
        // with no gap, and at least everything reported committed.
        let store = Store::open(config(dir)).unwrap();
        let mut got = Vec::new();
        store.scan(i64::MIN, i64::MAX, |r| {
            if let Rec::State(s) = r {
                got.push((s.ts - T0, s.state.to_owned()));
            }
        });
        assert!(
            got.len() as i64 >= committed,
            "round {round}: {} rows survived, {committed} were committed",
            got.len()
        );
        for (i, (n, state)) in got.iter().enumerate() {
            assert_eq!(*n, i as i64, "round {round}: a gap or a reorder at row {i}");
            assert_eq!(*state, n.to_string(), "round {round}: row {i} is corrupt");
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}
