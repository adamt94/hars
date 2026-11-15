use super::*;
use crate::format::{EventRow, StateRow};

const T0: i64 = 1_768_464_000_000_000; // 2026-01-15T08:00:00Z

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("hars-rec-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Self(p)
    }

    fn config(&self) -> Config {
        Config {
            commit_interval: Duration::from_millis(200),
            ..Config::new(self.0.clone())
        }
    }

    fn files(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn state(entity: &str, ts: i64, value: &str) -> Row {
    Row::State(StateRow {
        entity_id: entity.into(),
        ts,
        last_changed: ts,
        state: value.into(),
        attributes: Arc::from(r#"{"friendly_name":"X"}"#),
        flags: 0,
        context: [1; 16],
        aug: None,
    })
}

fn all(store: &Store) -> Vec<(String, i64, String)> {
    let mut v = Vec::new();
    store.scan(i64::MIN, i64::MAX, |r| match r {
        Rec::State(s) => v.push((s.entity_id.to_owned(), s.ts, s.state.to_owned())),
        Rec::Event(e) => v.push((e.event_type.to_owned(), e.ts, String::new())),
    });
    v
}

/// Flush until every row is written (failed writes retry on a backoff).
async fn flush_until_written(store: &Store) {
    let start = Instant::now();
    while !store.flush().await {
        assert!(start.elapsed() < Duration::from_secs(10), "never written");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn frames_in(path: &Path) -> usize {
    let bytes = std::fs::read(path).unwrap();
    let mut n = 0;
    format::frames(&bytes, |_| {
        n += 1;
        true
    });
    n
}

/// One row is written a commit interval later, not before, and a 71-row
/// burst (the target install's busiest 5 s) is one frame.
#[tokio::test]
async fn rows_wait_for_the_commit_interval_then_go_as_one_frame() {
    let dir = Dir::new("batch");
    let store = Store::open(dir.config()).unwrap();
    assert!(store.record(state("sensor.a", T0, "1")));
    assert_eq!(store.info().backlog, 1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(all(&store).is_empty(), "not before the interval");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(all(&store).len(), 1);
    assert_eq!(store.info().backlog, 0);

    for i in 0..71 {
        store.record(state("sensor.a", T0 + 1 + i, &i.to_string()));
    }
    store.flush().await;
    assert_eq!(all(&store).len(), 72);
    assert_eq!(frames_in(&dir.0.join("00000001.seg")), 2);
    assert_eq!(
        store.info(),
        Info {
            backlog: 0,
            max_backlog: 10_000,
            recording: true,
            thread_running: true
        }
    );
}

/// Rows survive a restart, which opens a new segment; a UTC day opens a
/// new one too.
#[tokio::test]
async fn rows_survive_a_restart_and_days_split_segments() {
    let dir = Dir::new("restart");
    {
        let store = Store::open(dir.config()).unwrap();
        store.record(state("sensor.a", T0, "1"));
        store.flush().await;
        store.record(state("sensor.a", T0 + DAY_US, "2"));
        store.record(Row::Event(EventRow {
            event_type: "homeassistant_stop".into(),
            ts: T0 + DAY_US + 1,
            data: None,
            context: [2; 16],
            aug: None,
        }));
        store.close().await;
        assert!(!store.record(state("sensor.a", T0 + DAY_US + 2, "3")));
        assert!(!store.info().thread_running || store.info().backlog == 0);
    }
    assert_eq!(dir.files(), ["00000001.seg", "00000002.seg"]);
    let store = Store::open(dir.config()).unwrap();
    assert_eq!(store.oldest_ts(), Some(T0));
    store.record(state("sensor.a", T0 + DAY_US + 10, "4"));
    store.flush().await;
    assert_eq!(
        dir.files(),
        ["00000001.seg", "00000002.seg", "00000003.seg"]
    );
    assert_eq!(
        all(&store),
        [
            ("sensor.a".into(), T0, "1".into()),
            ("sensor.a".into(), T0 + DAY_US, "2".into()),
            ("homeassistant_stop".into(), T0 + DAY_US + 1, String::new()),
            ("sensor.a".into(), T0 + DAY_US + 10, "4".into()),
        ]
    );
    // Scans pick segments by time.
    let mut n = 0;
    store.scan(T0 + DAY_US, i64::MAX, |_| n += 1);
    assert_eq!(n, 3);
}

/// Open: a torn tail is cut, a file that isn't a segment is moved aside
/// (HA's `.corrupt.<time>` naming), and an empty one is removed.
#[tokio::test]
async fn recovery_cuts_tails_and_moves_bad_files_aside() {
    let dir = Dir::new("recover");
    {
        let store = Store::open(dir.config()).unwrap();
        for i in 0..3 {
            store.record(state("sensor.a", T0 + i, &i.to_string()));
            store.flush().await;
        }
        store.close().await;
    }
    let seg = dir.0.join("00000001.seg");
    let whole = std::fs::read(&seg).unwrap();
    // A torn last frame: half of it.
    let mut torn = whole.clone();
    torn.truncate(whole.len() - 5);
    std::fs::write(&seg, &torn).unwrap();
    std::fs::write(dir.0.join("00000002.seg"), b"not a segment at all").unwrap();
    std::fs::write(dir.0.join("00000003.seg"), b"HSEG").unwrap();
    std::fs::write(dir.0.join("00000004.seg"), format::header(T0)).unwrap();
    std::fs::write(dir.0.join("notes.txt"), b"kept").unwrap();

    let store = Store::open(dir.config()).unwrap();
    assert_eq!(all(&store).len(), 2, "the torn frame is gone");
    assert!(std::fs::metadata(&seg).unwrap().len() < torn.len() as u64);
    let files = dir.files();
    assert_eq!(files.len(), 3, "{files:?}");
    assert_eq!(files[0], "00000001.seg");
    assert!(files[1].starts_with("00000002.seg.corrupt.20"), "{files:?}");
    assert_eq!(files[2], "notes.txt");
    // New rows go after the highest number seen, even a removed one.
    store.record(state("sensor.a", T0 + 9, "9"));
    store.flush().await;
    assert!(dir.0.join("00000005.seg").exists());
}

/// The start-row lookup walks back across segments, per entity.
#[tokio::test]
async fn last_before_walks_back_per_entity() {
    let dir = Dir::new("last");
    let store = Store::open(dir.config()).unwrap();
    store.record(state("sensor.old", T0, "a"));
    store.record(state("sensor.both", T0 + 1, "b1"));
    store.flush().await;
    store.record(state("sensor.both", T0 + DAY_US, "b2"));
    store.record(state("sensor.both", T0 + DAY_US + 5, "b3"));
    store.flush().await;
    let mut got: Vec<_> = store
        .last_before(
            &["sensor.old", "sensor.both", "sensor.never"],
            T0 + DAY_US + 5,
        )
        .into_iter()
        .map(|r| (r.entity_id.to_string(), r.state.to_string()))
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            ("sensor.both".into(), "b2".into()),
            ("sensor.old".into(), "a".into())
        ]
    );
    assert!(store.last_before(&["sensor.old"], T0).is_empty());
}

/// Purge unlinks whole segments older than the cutoff; the byte cap
/// removes the oldest closed ones.
#[tokio::test]
async fn purge_and_the_byte_cap_unlink_whole_segments() {
    let dir = Dir::new("purge");
    let store = Store::open(dir.config()).unwrap();
    for day in 0..4 {
        store.record(state("sensor.a", T0 + day * DAY_US, "x"));
        store.flush().await;
    }
    assert_eq!(store.segment_files().len(), 4);
    store.purge(T0 + 2 * DAY_US).await;
    assert_eq!(
        store
            .segment_files()
            .iter()
            .map(|s| s.0)
            .collect::<Vec<_>>(),
        [3, 4]
    );
    assert_eq!(store.oldest_ts(), Some(T0 + 2 * DAY_US));
    drop(store);

    let mut config = dir.config();
    let one_segment = std::fs::metadata(dir.0.join("00000003.seg")).unwrap().len();
    // Room for about one and a half segments.
    config.max_bytes = one_segment * 2 * 10 / 9 * 3 / 4;
    let store = Store::open(config).unwrap();
    store.record(state("sensor.a", T0 + 5 * DAY_US, "y"));
    store.flush().await;
    store.purge(i64::MIN).await;
    assert_eq!(
        store
            .segment_files()
            .iter()
            .map(|s| s.0)
            .collect::<Vec<_>>(),
        [5],
        "the open segment is never removed for the cap"
    );
}

/// A full disk: rows are kept and retried, `recording` is false, nothing
/// half-written stays, the backlog is bounded; then writing resumes.
#[tokio::test]
async fn a_full_disk_keeps_rows_and_recovers() {
    let dir = Dir::new("full");
    let mut config = dir.config();
    config.max_backlog = 5;
    let store = Store::open(config).unwrap();
    store.record(state("sensor.a", T0, "0"));
    store.flush().await;
    let before = std::fs::metadata(dir.0.join("00000001.seg")).unwrap().len();

    store.fail_writes(true);
    for i in 1..=7 {
        let kept = store.record(state("sensor.a", T0 + i, &i.to_string()));
        assert_eq!(kept, i <= 5, "row {i}");
    }
    assert_eq!(store.dropped(), 2);
    assert!(!store.flush().await, "the flush reports the unwritten rows");
    assert!(!store.info().recording);
    assert_eq!(store.info().backlog, 5);
    assert_eq!(all(&store).len(), 1, "reads still answer");
    assert_eq!(
        std::fs::metadata(dir.0.join("00000001.seg")).unwrap().len(),
        before,
        "no half frame"
    );

    store.fail_writes(false);
    flush_until_written(&store).await;
    assert!(store.info().recording);
    assert_eq!(store.info().backlog, 0);
    let states: Vec<String> = all(&store).into_iter().map(|r| r.2).collect();
    assert_eq!(states, ["0", "1", "2", "3", "4", "5"]);
}

/// Flushes during a write failure keep to the retry schedule: one failure,
/// so one dropped segment, not one per read.
#[tokio::test]
async fn flushes_wait_for_the_retry_while_writes_fail() {
    let dir = Dir::new("backoff");
    let store = Store::open(dir.config()).unwrap();
    for day in 0..4 {
        store.record(state("sensor.a", T0 + day * DAY_US, "x"));
        store.flush().await;
    }
    store.fail_writes(true);
    store.record(state("sensor.a", T0 + 4 * DAY_US, "y"));
    for _ in 0..20 {
        assert!(!store.flush().await);
    }
    assert_eq!(store.segment_files().len(), 3, "one failure, one segment");
    store.fail_writes(false);
    flush_until_written(&store).await;
    assert_eq!(all(&store).last().unwrap().2, "y");
}

/// Close keeps retrying a failing final write instead of dropping it, and
/// refuses rows from the moment it is called.
#[tokio::test]
async fn close_retries_the_final_write() {
    let dir = Dir::new("close");
    let store = Arc::new(Store::open(dir.config()).unwrap());
    store.fail_writes(true);
    store.record(state("sensor.a", T0, "kept"));
    let closing = tokio::spawn({
        let store = store.clone();
        async move { store.close().await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!closing.is_finished(), "still retrying");
    assert!(!store.record(state("sensor.a", T0 + 1, "late")));
    assert!(!store.info().recording);
    store.fail_writes(false);
    tokio::time::timeout(Duration::from_secs(10), closing)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(store.info().backlog, 0);
    assert!(!store.info().recording, "a closed store isn't recording");
    drop(store);
    let store = Store::open(dir.config()).unwrap();
    let states: Vec<String> = all(&store).into_iter().map(|r| r.2).collect();
    assert_eq!(states, ["kept"]);
}

/// A clock stepped back across a restart puts an older row in a later
/// segment; the lookup still finds the newest.
#[tokio::test]
async fn last_before_survives_a_clock_step_back() {
    let dir = Dir::new("stepback");
    let hour = 3_600_000_000;
    {
        let store = Store::open(dir.config()).unwrap();
        store.record(state("sensor.a", T0 + 4 * hour, "noon"));
        store.close().await;
    }
    let store = Store::open(dir.config()).unwrap();
    store.record(state("sensor.a", T0 + 3 * hour, "eleven"));
    store.flush().await;
    assert_eq!(store.segment_files().len(), 2);
    let got = store.last_before(&["sensor.a"], T0 + 5 * hour);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].state, "noon");
}

/// Segments a read has picked stay readable when a purge unlinks them.
#[tokio::test]
async fn picked_segments_outlive_a_purge() {
    let dir = Dir::new("pinned");
    let store = Store::open(dir.config()).unwrap();
    store.record(state("sensor.a", T0, "old"));
    store.flush().await;
    store.record(state("sensor.a", T0 + DAY_US, "new"));
    store.flush().await;
    let picked = store.pick(|_| true);
    store.purge(T0 + 1).await;
    assert_eq!(store.segment_files().len(), 1);
    let mut states = Vec::new();
    for seg in picked {
        seg.scan(&mut |r| {
            if let Rec::State(s) = r {
                states.push(s.state.to_owned());
            }
        });
    }
    assert_eq!(states, ["old", "new"]);
}

/// A frame whose CRC is right but whose contents don't decode ends the
/// valid prefix: it is cut, and none of its rows count.
#[tokio::test]
async fn a_malformed_frame_is_cut_whole() {
    let dir = Dir::new("malformed");
    std::fs::create_dir_all(&dir.0).unwrap();
    let base = T0.div_euclid(DAY_US) * DAY_US;
    let mut dicts = format::Dicts::default();
    let mut file = format::header(base).to_vec();
    let mut payload = Vec::new();
    dicts.encode(base, &state("sensor.a", T0, "good"), &mut payload);
    format::frame(&payload, &mut file);
    let good_len = file.len();
    payload.clear();
    dicts.encode(base, &state("sensor.a", T0 - 100, "early"), &mut payload);
    payload.push(0x7f); // no such tag
    format::frame(&payload, &mut file);
    payload.clear();
    dicts.encode(base, &state("sensor.a", T0 + 100, "after"), &mut payload);
    format::frame(&payload, &mut file);
    let path = dir.0.join("00000001.seg");
    std::fs::write(&path, &file).unwrap();

    let store = Store::open(dir.config()).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), good_len as u64);
    assert_eq!(
        store.oldest_ts(),
        Some(T0),
        "the bad frame's row isn't summarised"
    );
    let states: Vec<String> = all(&store).into_iter().map(|r| r.2).collect();
    assert_eq!(states, ["good"]);
}

/// An unlink that fails leaves the segment listed and readable; the next
/// purge removes it.
#[cfg(unix)]
#[tokio::test]
async fn a_failed_unlink_keeps_the_segment() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Dir::new("unlink");
    let store = Store::open(dir.config()).unwrap();
    store.record(state("sensor.a", T0, "old"));
    store.flush().await;
    store.record(state("sensor.a", T0 + DAY_US, "new"));
    store.flush().await;
    let mode = |m| std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(m)).unwrap();
    mode(0o555);
    let probe = std::fs::File::create(dir.0.join("probe")).is_ok();
    if probe {
        // Running as root: permissions don't stop an unlink.
        mode(0o755);
        return;
    }
    store.purge(T0 + 1).await;
    mode(0o755);
    assert_eq!(store.segment_files().len(), 2, "still listed");
    assert_eq!(all(&store).len(), 2, "still readable");
    store.purge(T0 + 1).await;
    assert_eq!(store.segment_files().len(), 1);
    assert_eq!(dir.files(), ["00000002.seg"]);
}
