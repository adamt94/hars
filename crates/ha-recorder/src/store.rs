//! The segment store: a directory of `NNNNNNNN.seg` files, the newest
//! open for appends by one writer thread.
//!
//! - Rows are batched for up to `commit_interval` (HA's meaning) and each
//!   batch is one `write()`. There is no fsync per batch: a crash loses at
//!   most the pending batch, and the CRC framing cuts any torn tail on the
//!   next open. Segments are `fdatasync`ed when closed (at UTC midnight and
//!   at stop).
//! - An idle writer blocks on its queue with no timeout: no wakeups.
//! - Readers see a segment up to its published committed length, so never a
//!   partial frame, and never block the writer.
//! - Each process start and each UTC day opens a new segment, so dictionaries
//!   are never reloaded, closed segments are immutable, and purge is an
//!   `unlink`. At open every segment is scanned once (about 1.6 MB for ten
//!   days of the target install) to cut torn tails and build the small
//!   summaries that pick segments for a query.

use crate::format::{self, Dicts, ReadDicts, Rec, Row, StateRow};
use parking_lot::{Mutex, RwLock};
use smol_str::SmolStr;
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const DAY_US: i64 = 86_400_000_000;
/// First retry after a failed write; it doubles up to [`MAX_RETRY`].
const FIRST_RETRY: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    Duration::from_secs(5)
};
const MAX_RETRY: Duration = Duration::from_secs(300);
/// Minimum time between "rows dropped" warnings.
const DROP_LOG_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Config {
    pub dir: PathBuf,
    /// The longest a row waits before it is written (`commit_interval`).
    pub commit_interval: Duration,
    /// Rows queued beyond this are dropped and counted.
    pub max_backlog: usize,
    /// Oldest segments go when the store grows past 90 % of this.
    pub max_bytes: u64,
}

impl Config {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            commit_interval: Duration::from_secs(5),
            max_backlog: 10_000,
            max_bytes: 64 << 20,
        }
    }
}

/// What a query needs to know about a segment.
#[derive(Debug)]
struct Segment {
    seq: u64,
    path: PathBuf,
    base: i64,
    /// Bytes readers may read: whole frames only.
    len: u64,
    /// The rows' time span; `min > max` while empty.
    min: i64,
    max: i64,
    /// Entities with a state row here.
    entities: HashSet<SmolStr>,
}

impl Segment {
    fn overlaps(&self, from: i64, to: i64) -> bool {
        self.min <= self.max && self.max >= from && self.min <= to
    }
}

#[derive(Default)]
struct Shared {
    /// Oldest first.
    segments: RwLock<Vec<Segment>>,
    /// Rows accepted and not yet written.
    backlog: AtomicUsize,
    dropped: AtomicU64,
    last_drop_log: Mutex<Option<Instant>>,
    /// The last write failed; it is being retried.
    failing: AtomicBool,
    running: AtomicBool,
    /// Rows are accepted. Cleared by `close` under the write lock, so no
    /// row is queued behind the `Close` message.
    accepting: RwLock<bool>,
    cap_warned: AtomicBool,
    /// Test hook: writes fail as on a full disk.
    #[cfg(test)]
    fail_writes: AtomicBool,
}

enum Msg {
    Row(Row),
    Flush(tokio::sync::oneshot::Sender<bool>),
    Purge(i64, tokio::sync::oneshot::Sender<()>),
    Close(tokio::sync::oneshot::Sender<()>),
}

/// `recorder/info`'s live fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    pub backlog: usize,
    pub max_backlog: usize,
    pub recording: bool,
    pub thread_running: bool,
}

pub struct Store {
    shared: Arc<Shared>,
    tx: mpsc::Sender<Msg>,
    max_backlog: usize,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// Open (creating) the directory, recover its segments and start the
    /// writer thread. Blocking.
    pub fn open(config: Config) -> std::io::Result<Self> {
        std::fs::create_dir_all(&config.dir)?;
        let (segments, highest) = recover(&config.dir)?;
        let next_seq = highest + 1;
        let shared = Arc::new(Shared {
            segments: RwLock::new(segments),
            running: AtomicBool::new(true),
            accepting: RwLock::new(true),
            ..Shared::default()
        });
        let (tx, rx) = mpsc::channel();
        let writer = Writer {
            shared: shared.clone(),
            dir: config.dir,
            interval: config.commit_interval,
            max_bytes: config.max_bytes,
            next_seq,
            open: None,
            pending: Vec::new(),
            buf: Vec::new(),
            retry: FIRST_RETRY,
            retry_at: None,
        };
        let running = shared.clone();
        std::thread::Builder::new()
            .name("recorder".into())
            .spawn(move || {
                writer.run(rx);
                running.running.store(false, Ordering::Release);
            })?;
        Ok(Self {
            shared,
            tx,
            max_backlog: config.max_backlog,
        })
    }

    /// Queue a row. `false` if it was dropped: the backlog is full (logged
    /// at most once a minute) or the store is closed.
    pub fn record(&self, row: Row) -> bool {
        let accepting = self.shared.accepting.read();
        if !*accepting {
            return false;
        }
        if self.shared.backlog.fetch_add(1, Ordering::AcqRel) >= self.max_backlog {
            self.shared.backlog.fetch_sub(1, Ordering::AcqRel);
            let dropped = self.shared.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            let mut last = self.shared.last_drop_log.lock();
            if last.is_none_or(|t| t.elapsed() >= DROP_LOG_EVERY) {
                *last = Some(Instant::now());
                tracing::error!(
                    "the recorder's backlog is full ({} rows); {dropped} rows dropped so far",
                    self.max_backlog
                );
            }
            return false;
        }
        if self.tx.send(Msg::Row(row)).is_err() {
            self.shared.backlog.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        true
    }

    /// Write the pending batch now (before a read, so it sees every row
    /// recorded so far). `false` if rows are still unwritten: writes are
    /// failing (the retry keeps its schedule) or the writer has stopped.
    pub async fn flush(&self) -> bool {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Msg::Flush(tx)).is_ok() {
            rx.await.unwrap_or(false)
        } else {
            false
        }
    }

    /// Unlink segments whose newest row is older than `cutoff` (µs), then
    /// enforce the byte cap.
    pub async fn purge(&self, cutoff: i64) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Msg::Purge(cutoff, tx)).is_ok() {
            let _ = rx.await;
        }
    }

    /// Refuse new rows, write what's pending, `fdatasync` and stop the
    /// writer (the final write). While writes fail this keeps retrying on
    /// the backoff schedule, so the caller bounds it (the final-write
    /// stage's 60 s).
    pub async fn close(&self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let sent = {
            let mut accepting = self.shared.accepting.write();
            *accepting = false;
            self.tx.send(Msg::Close(tx)).is_ok()
        };
        if sent {
            let _ = rx.await;
        }
    }

    pub fn info(&self) -> Info {
        Info {
            backlog: self.shared.backlog.load(Ordering::Acquire),
            max_backlog: self.max_backlog,
            recording: *self.shared.accepting.read()
                && self.shared.running.load(Ordering::Acquire)
                && !self.shared.failing.load(Ordering::Acquire)
                && self.shared.backlog.load(Ordering::Acquire) < self.max_backlog,
            thread_running: self.shared.running.load(Ordering::Acquire),
        }
    }

    /// Rows dropped because the backlog was full.
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// The oldest row's time (µs), HA's `_get_oldest_possible_ts`.
    pub fn oldest_ts(&self) -> Option<i64> {
        let segments = self.shared.segments.read();
        segments
            .iter()
            .filter(|s| s.min <= s.max)
            .map(|s| s.min)
            .min()
    }

    /// Call `f` with every committed row of the segments that may hold rows
    /// in `[from, to]` (µs), in write order. Rows outside the range can
    /// come too: callers filter. Blocking (reads files).
    pub fn scan(&self, from: i64, to: i64, mut f: impl FnMut(Rec<'_>)) {
        for seg in self.pick(|s| s.overlaps(from, to)) {
            seg.scan(&mut f);
        }
    }

    /// For each of `entities`, its newest state row before `before` (µs).
    /// Segments are read newest rows first and only while one could hold
    /// a newer row than found so far: a clock step back can put older
    /// times in a later segment. Blocking.
    pub fn last_before(&self, entities: &[&str], before: i64) -> Vec<StateRow> {
        let wanted: HashSet<&str> = entities.iter().copied().collect();
        // Per entity: (ts, seq, row); ties go to the later write.
        let mut best: std::collections::HashMap<&str, (i64, u64, StateRow)> = Default::default();
        let mut candidates =
            self.pick(|s| s.min < before && wanted.iter().any(|e| s.entities.contains(*e)));
        candidates.sort_unstable_by_key(|c| {
            std::cmp::Reverse((c.max.min(before.saturating_sub(1)), c.seq))
        });
        for seg in candidates {
            let hi = seg.max.min(before.saturating_sub(1));
            let improves = |e: &str| best.get(e).is_none_or(|b| b.0 <= hi);
            if !wanted.iter().any(|e| improves(e)) {
                // Later candidates' rows are older still.
                break;
            }
            if !wanted
                .iter()
                .any(|e| seg.entities.contains(*e) && improves(e))
            {
                continue;
            }
            let seq = seg.seq;
            seg.scan(&mut |r| {
                if let Rec::State(s) = r
                    && s.ts < before
                    && let Some(&e) = wanted.get(s.entity_id)
                    && best.get(e).is_none_or(|b| (b.0, b.1) <= (s.ts, seq))
                {
                    best.insert(e, (s.ts, seq, s.to_owned_row()));
                }
            });
        }
        best.into_values().map(|b| b.2).collect()
    }

    /// The segments `keep` picks, each opened while the index is locked:
    /// a purge after this can unlink them, and the reads still see them.
    fn pick(&self, keep: impl Fn(&Segment) -> bool) -> Vec<Pinned> {
        let segments = self.shared.segments.read();
        segments
            .iter()
            .filter(|s| keep(s))
            .filter_map(|s| match std::fs::File::open(&s.path) {
                Ok(file) => Some(Pinned {
                    file,
                    path: s.path.clone(),
                    seq: s.seq,
                    base: s.base,
                    len: s.len,
                    max: s.max,
                    entities: s.entities.clone(),
                }),
                Err(e) => {
                    tracing::error!("opening {}: {e}", s.path.display());
                    None
                }
            })
            .collect()
    }

    #[cfg(test)]
    fn fail_writes(&self, fail: bool) {
        self.shared.fail_writes.store(fail, Ordering::Release);
    }

    #[cfg(test)]
    fn segment_files(&self) -> Vec<(u64, u64)> {
        self.shared
            .segments
            .read()
            .iter()
            .map(|s| (s.seq, s.len))
            .collect()
    }
}

impl format::StateRec<'_> {
    pub fn to_owned_row(&self) -> StateRow {
        StateRow {
            entity_id: self.entity_id.into(),
            ts: self.ts,
            last_changed: self.last_changed,
            state: self.state.into(),
            attributes: Arc::from(self.attributes),
            flags: self.flags,
            context: self.context,
            aug: self.aug.map(Arc::from),
        }
    }
}

/// A segment picked for a read, held open.
struct Pinned {
    file: std::fs::File,
    path: PathBuf,
    seq: u64,
    base: i64,
    len: u64,
    max: i64,
    entities: HashSet<SmolStr>,
}

impl Pinned {
    fn scan(self, f: &mut dyn FnMut(Rec<'_>)) {
        let mut buf = Vec::with_capacity(self.len as usize);
        if let Err(e) = self.file.take(self.len).read_to_end(&mut buf) {
            tracing::error!("reading {}: {e}", self.path.display());
            return;
        }
        let mut dicts = ReadDicts::default();
        format::frames(&buf, |payload| {
            let ok = dicts.decode(self.base, payload, &mut *f);
            if !ok {
                tracing::error!(
                    "{}: a malformed record; skipping the rest",
                    self.path.display()
                );
            }
            ok
        });
    }
}

fn seq_of(path: &Path) -> Option<u64> {
    if path.extension()? != "seg" {
        return None;
    }
    path.file_stem()?.to_str()?.parse().ok()
}

fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Err(e) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
        tracing::warn!("syncing {}: {e}", dir.display());
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Scan every segment: cut torn or corrupt tails, move unreadable files
/// aside as HA does (`<name>.corrupt.<UTC ISO>`), drop empty ones, and
/// summarise the rest. Also the highest segment number seen, so a new one
/// never reuses a name.
fn recover(dir: &Path) -> std::io::Result<(Vec<Segment>, u64)> {
    let mut paths: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)?
        .filter_map(|e| {
            let path = e.ok()?.path();
            Some((seq_of(&path)?, path))
        })
        .collect();
    paths.sort_unstable();
    let highest = paths.last().map_or(0, |p| p.0);
    let mut segments = Vec::new();
    for (seq, path) in paths {
        let bytes = std::fs::read(&path)?;
        let Some(base) = format::read_header(&bytes) else {
            let n = bytes.len().min(8);
            if bytes.len() < format::HEADER
                && (bytes.iter().all(|&b| b == 0) || format::MAGIC.starts_with(&bytes[..n]))
            {
                // A crash before the header reached the disk.
                std::fs::remove_file(&path)?;
                continue;
            }
            let aside = format!(
                "{}.corrupt.{}",
                path.display(),
                ha_core::isoformat(jiff::Timestamp::now())
            );
            tracing::error!(
                "{} is not a recorder segment; moved to {aside}",
                path.display()
            );
            std::fs::rename(&path, &aside)?;
            continue;
        };
        let mut seg = Segment {
            seq,
            path,
            base,
            len: 0,
            min: i64::MAX,
            max: i64::MIN,
            entities: HashSet::new(),
        };
        let mut dicts = ReadDicts::default();
        let (mut min, mut max) = (i64::MAX, i64::MIN);
        let mut entities: Vec<SmolStr> = Vec::new();
        // A frame counts only if all of it decodes: one that doesn't (a
        // CRC-valid but malformed batch) ends the valid prefix like a torn
        // one, and none of its rows reach the summary.
        let valid = format::frames(&bytes, |payload| {
            let (mut lo, mut hi) = (i64::MAX, i64::MIN);
            entities.clear();
            let ok = dicts.decode(base, payload, |r| {
                lo = lo.min(r.ts());
                hi = hi.max(r.ts());
                if let Rec::State(s) = r
                    && !seg.entities.contains(s.entity_id)
                {
                    entities.push(s.entity_id.into());
                }
            });
            if ok {
                min = min.min(lo);
                max = max.max(hi);
                seg.entities.extend(entities.drain(..));
            }
            ok
        });
        (seg.min, seg.max) = (min, max);
        if valid < bytes.len() {
            tracing::warn!(
                "{}: cut {} bytes of a torn or corrupt tail",
                seg.path.display(),
                bytes.len() - valid
            );
            std::fs::OpenOptions::new()
                .write(true)
                .open(&seg.path)?
                .set_len(valid as u64)?;
        }
        seg.len = valid as u64;
        if seg.min > seg.max {
            std::fs::remove_file(&seg.path)?;
            continue;
        }
        segments.push(seg);
    }
    Ok((segments, highest))
}

struct OpenSegment {
    file: std::fs::File,
    seq: u64,
    base: i64,
    dicts: Dicts,
    len: u64,
}

struct Writer {
    shared: Arc<Shared>,
    dir: PathBuf,
    interval: Duration,
    max_bytes: u64,
    next_seq: u64,
    open: Option<OpenSegment>,
    pending: Vec<Row>,
    buf: Vec<u8>,
    /// The wait before the next retry of a failed write.
    retry: Duration,
    /// While writes fail: the next attempt. Nothing writes before it, not
    /// even a flush, so each failure (which frees disk by dropping the
    /// oldest segment) keeps the backoff's pace.
    retry_at: Option<Instant>,
}

impl Writer {
    fn run(mut self, rx: mpsc::Receiver<Msg>) {
        let mut deadline: Option<Instant> = None;
        loop {
            let msg = match deadline {
                // Idle: block with no timeout, so no wakeups.
                None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
            };
            match msg {
                Ok(Msg::Row(row)) => {
                    self.pending.push(row);
                    deadline.get_or_insert_with(|| Instant::now() + self.interval);
                }
                Ok(Msg::Flush(reply)) => {
                    deadline = self.commit();
                    let _ = reply.send(deadline.is_none());
                }
                Ok(Msg::Purge(cutoff, reply)) => {
                    deadline = self.commit();
                    self.purge(cutoff);
                    let _ = reply.send(());
                }
                Ok(Msg::Close(reply)) => {
                    // `close` stopped admissions before sending this, so
                    // every accepted row is pending now. Keep retrying:
                    // the caller's timeout decides when to give up.
                    while let Some(at) = self.commit() {
                        std::thread::sleep(at.saturating_duration_since(Instant::now()));
                    }
                    self.close_segment();
                    let _ = reply.send(());
                    return;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => deadline = self.commit(),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // Dropped without `close`: one last try.
                    self.retry_at = None;
                    if self.commit().is_some() {
                        tracing::error!(
                            "the recorder stopped with {} rows unwritten",
                            self.pending.len()
                        );
                    }
                    self.close_segment();
                    return;
                }
            }
        }
    }

    /// Write the pending batch as one frame. On failure the batch is kept
    /// and the next attempt's time returned; before that time this only
    /// returns it.
    fn commit(&mut self) -> Option<Instant> {
        if self.pending.is_empty() {
            return None;
        }
        if let Some(at) = self.retry_at
            && Instant::now() < at
        {
            return Some(at);
        }
        match self.try_commit() {
            Ok(()) => {
                let n = self.pending.len();
                self.pending.clear();
                self.shared.backlog.fetch_sub(n, Ordering::AcqRel);
                if self.shared.failing.swap(false, Ordering::AcqRel) {
                    tracing::info!("the recorder is writing again");
                }
                self.retry = FIRST_RETRY;
                self.retry_at = None;
                None
            }
            Err(e) => {
                if !self.shared.failing.swap(true, Ordering::AcqRel) {
                    tracing::error!(
                        "the recorder can't write ({e}); keeping {} rows and retrying",
                        self.pending.len()
                    );
                }
                // Give the disk room: the oldest closed segment goes.
                self.drop_oldest_closed();
                let at = Instant::now() + self.retry;
                self.retry = (self.retry * 2).min(MAX_RETRY);
                self.retry_at = Some(at);
                Some(at)
            }
        }
    }

    fn try_commit(&mut self) -> std::io::Result<()> {
        let first = self.pending[0].ts();
        // A new UTC day starts a new segment; a row dated earlier (a clock
        // step back) stays in the open one: scans go by time, not file.
        if self
            .open
            .as_ref()
            .is_some_and(|o| first.div_euclid(DAY_US) > o.base.div_euclid(DAY_US))
        {
            self.close_segment();
            self.enforce_cap();
        }
        if self.open.is_none() {
            self.open = Some(self.new_segment(first.div_euclid(DAY_US) * DAY_US)?);
        }
        let Some(open) = self.open.as_mut() else {
            unreachable!("opened above")
        };
        // One reused buffer: the frame head's room, then the payload.
        let frame = &mut self.buf;
        frame.clear();
        frame.resize(format::FRAME_HEAD, 0);
        for row in &self.pending {
            open.dicts.encode(open.base, row, frame);
        }
        format::seal_frame(frame);
        let frame = &self.buf;
        let written = (|| {
            #[cfg(test)]
            if self.shared.fail_writes.load(Ordering::Acquire) {
                return Err(std::io::Error::other("no space left on device (test)"));
            }
            open.file.seek(SeekFrom::Start(open.len))?;
            open.file.write_all(frame)
        })();
        if let Err(e) = written {
            // No half frame stays behind, and the dictionaries now hold
            // entries the file doesn't: the next attempt starts a new
            // segment.
            let _ = open.file.set_len(open.len);
            self.open = None;
            return Err(e);
        }
        open.len += frame.len() as u64;
        let mut segments = self.shared.segments.write();
        if let Some(seg) = segments.iter_mut().find(|s| s.seq == open.seq) {
            seg.len = open.len;
            for row in &self.pending {
                seg.min = seg.min.min(row.ts());
                seg.max = seg.max.max(row.ts());
                if let Row::State(s) = row
                    && !seg.entities.contains(&s.entity_id)
                {
                    seg.entities.insert(s.entity_id.clone());
                }
            }
        }
        Ok(())
    }

    fn new_segment(&mut self, base: i64) -> std::io::Result<OpenSegment> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let path = self.dir.join(format!("{seq:08}.seg"));
        let created = (|| {
            #[cfg(test)]
            if self.shared.fail_writes.load(Ordering::Acquire) {
                return Err(std::io::Error::other("no space left on device (test)"));
            }
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)?;
            file.write_all(&format::header(base))?;
            Ok(file)
        })();
        let file = match created {
            Ok(f) => f,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
        };
        sync_dir(&self.dir);
        self.shared.segments.write().push(Segment {
            seq,
            path,
            base,
            len: format::HEADER as u64,
            min: i64::MAX,
            max: i64::MIN,
            entities: HashSet::new(),
        });
        Ok(OpenSegment {
            file,
            seq,
            base,
            dicts: Dicts::default(),
            len: format::HEADER as u64,
        })
    }

    fn close_segment(&mut self) {
        if let Some(open) = self.open.take()
            && let Err(e) = open.file.sync_data()
        {
            tracing::error!("syncing the recorder segment: {e}");
        }
    }

    fn open_seq(&self) -> Option<u64> {
        self.open.as_ref().map(|o| o.seq)
    }

    /// Unlink a segment, then forget it. `false` if the unlink failed: it
    /// stays listed and readable, and its bytes still count.
    fn remove(&self, seq: u64) -> bool {
        let Some(path) = self
            .shared
            .segments
            .read()
            .iter()
            .find(|s| s.seq == seq)
            .map(|s| s.path.clone())
        else {
            return true;
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::error!("removing {}: {e}", path.display());
                return false;
            }
        }
        self.shared.segments.write().retain(|s| s.seq != seq);
        true
    }

    fn drop_oldest_closed(&mut self) {
        let open = self.open_seq();
        let oldest = {
            let segments = self.shared.segments.read();
            (segments.len() > 1)
                .then(|| segments.iter().find(|s| Some(s.seq) != open).map(|s| s.seq))
                .flatten()
        };
        if let Some(seq) = oldest {
            self.remove(seq);
        }
    }

    fn purge(&mut self, cutoff: i64) {
        let stale: Vec<u64> = self
            .shared
            .segments
            .read()
            .iter()
            .filter(|s| s.min <= s.max && s.max < cutoff)
            .map(|s| s.seq)
            .collect();
        for seq in stale {
            if Some(seq) == self.open_seq() {
                self.close_segment();
            }
            if !self.remove(seq) {
                break;
            }
        }
        self.enforce_cap();
        sync_dir(&self.dir);
    }

    /// Unlink the oldest closed segments while the store is over 90 % of
    /// the cap.
    fn enforce_cap(&mut self) {
        let limit = self.max_bytes / 10 * 9;
        loop {
            let (total, oldest) = {
                let segments = self.shared.segments.read();
                let total: u64 = segments.iter().map(|s| s.len).sum();
                let open = self.open_seq();
                (
                    total,
                    segments.iter().find(|s| Some(s.seq) != open).map(|s| s.seq),
                )
            };
            let Some(seq) = oldest.filter(|_| total > limit) else {
                return;
            };
            if !self.shared.cap_warned.swap(true, Ordering::AcqRel) {
                tracing::warn!(
                    "the recorder store is over {} bytes; removing the oldest days",
                    limit
                );
            }
            if !self.remove(seq) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests;
