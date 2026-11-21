//! One bulb's UDP connection: aiolifx's request matching (sequence, source
//! and expected type) under HA's retry layer, where each try is a new
//! message and a reply to any try completes the request.
//!
//! aiolifx also sends each message again on its own ([`Resend`]): 3 times,
//! 0.5 s apart, when nothing has told it otherwise (HA's config flow); once,
//! kept for 18 s, once HA's coordinator has set `retry_count` and `timeout`.
//!
//! A connection can be stopped ([`Conn::stop`]): a request on its way, or
//! waiting to be tried again, ends there, and nothing is sent after. A
//! removed entry's commands, which a service call may still hold, can't
//! reach the bulb afterwards.

use super::proto::{self, Header, Out, ty};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::Notify;

pub const PORT: u16 = 56700;

/// How aiolifx sends one message: `sends` times, `every` apart, until it
/// is answered; after the last it waits `every` once more and forgets the
/// message (a reply after that doesn't count).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resend {
    pub every: Duration,
    pub sends: u32,
}

impl Resend {
    /// As HA's coordinator sets the device (`MESSAGE_TIMEOUT`,
    /// `MESSAGE_RETRIES`).
    pub const COORDINATOR: Resend = Resend {
        every: Duration::from_secs(18),
        sends: 1,
    };
    /// aiolifx's own (`DEFAULT_TIMEOUT`, `DEFAULT_ATTEMPTS`), which HA's
    /// config flow leaves as they are.
    pub const LIBRARY: Resend = Resend {
        every: Duration::from_millis(500),
        sends: 3,
    };
}

/// One request of a multi-request try.
#[derive(Debug, Clone)]
pub struct Op {
    pub ty: u16,
    pub payload: Vec<u8>,
    /// The reply that completes it: an `Acknowledgement` or a `State*`.
    pub expect: u16,
}

impl Op {
    /// A request answered by a state message.
    pub fn get(ty: u16, expect: u16) -> Self {
        Self {
            ty,
            payload: Vec::new(),
            expect,
        }
    }

    /// A command answered by an acknowledgement.
    pub fn set(ty: u16, payload: Vec<u8>) -> Self {
        Self {
            ty,
            payload,
            expect: ty::ACK,
        }
    }
}

/// A request's reply, once it came.
type Slot = Arc<Mutex<Option<(Header, Vec<u8>)>>>;

struct Pending {
    /// aiolifx forgets a request 18 s after sending it; a later reply
    /// doesn't count.
    expires: tokio::time::Instant,
    expect: u16,
    slot: Slot,
    notify: Arc<Notify>,
}

pub struct Conn {
    sock: Arc<UdpSocket>,
    source: u32,
    pub(super) seq: Mutex<u8>,
    /// The bulb's serial once known; all zeros (tagged) before.
    target: Mutex<[u8; 6]>,
    pending: Arc<Mutex<HashMap<u8, Pending>>>,
    reader: tokio::task::JoinHandle<()>,
    resend: Resend,
    /// True once stopped.
    stopped: tokio::sync::watch::Sender<bool>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Conn {
    /// A socket bound to an ephemeral port and connected to the bulb, as
    /// HA's coordinator uses it.
    pub async fn open(host: &str, port: u16) -> std::io::Result<Self> {
        Self::open_with(host, port, Resend::COORDINATOR).await
    }

    /// [`Conn::open`], with aiolifx sending each message as `resend` says.
    pub async fn open_with(host: &str, port: u16, resend: Resend) -> std::io::Result<Self> {
        let sock = UdpSocket::bind("0.0.0.0:0").await?;
        sock.connect((host, port)).await?;
        let sock = Arc::new(sock);
        // A random source, never 0 (aiolifx's may be; unobservable).
        let source = rand::random_range(1..=u32::MAX);
        let pending: Arc<Mutex<HashMap<u8, Pending>>> = Arc::default();
        let reader = tokio::spawn(read(sock.clone(), source, pending.clone()));
        Ok(Self {
            sock,
            source,
            seq: Mutex::new(0),
            target: Mutex::new([0; 6]),
            pending,
            reader,
            resend,
            stopped: tokio::sync::watch::Sender::new(false),
        })
    }

    #[cfg(test)]
    pub(super) fn with_lifetime(mut self, d: Duration) -> Self {
        self.resend.every = d;
        self
    }

    /// The entry is stopped: what is on its way ends now, unanswered, and
    /// nothing is sent from here on.
    pub fn stop(&self) {
        self.stopped.send_replace(true);
    }

    fn is_stopped(&self) -> bool {
        *self.stopped.borrow()
    }

    /// The address the socket is connected to.
    pub fn peer(&self) -> Option<std::net::SocketAddr> {
        self.sock.peer_addr().ok()
    }

    pub fn set_target(&self, serial: [u8; 6]) {
        *self.target.lock() = serial;
    }

    /// `seq_next`: 1, 2, … 127, 0, 1, …
    pub(super) fn next_seq(&self) -> u8 {
        let mut s = self.seq.lock();
        *s = (*s + 1) % 128;
        *s
    }

    fn frame(&self, op: &Op, seq: u8) -> Vec<u8> {
        Out {
            source: self.source,
            target: *self.target.lock(),
            seq,
            ack: op.expect == ty::ACK,
            res: op.expect != ty::ACK,
            ty: op.ty,
            payload: &op.payload,
        }
        .encode()
    }

    /// `async_multi_execute_lifx_with_retries`: each try sends every
    /// request not yet answered (a new sequence number each, which aiolifx
    /// sends as [`Resend`] says), then waits `per_try` for all of them.
    /// `Err` holds the unanswered indices: all of them once the connection
    /// is stopped.
    pub async fn execute(
        &self,
        ops: &[Op],
        attempts: u32,
        per_try: Duration,
    ) -> Result<Vec<(Header, Vec<u8>)>, Vec<usize>> {
        let mut stopped = self.stopped.subscribe();
        let run = self.execute_now(ops, attempts, per_try);
        let missing = || (0..ops.len()).collect::<Vec<_>>();
        if self.is_stopped() {
            return Err(missing());
        }
        tokio::select! {
            biased;
            _ = stopped.wait_for(|s| *s) => Err(missing()),
            done = run => done,
        }
    }

    async fn execute_now(
        &self,
        ops: &[Op],
        attempts: u32,
        per_try: Duration,
    ) -> Result<Vec<(Header, Vec<u8>)>, Vec<usize>> {
        let notify = Arc::new(Notify::new());
        let slots: Vec<Slot> = ops.iter().map(|_| Slot::default()).collect();
        let mut seqs = Vec::new();
        let done = |i: usize| slots[i].lock().is_some();
        let all_done = || (0..ops.len()).all(done);
        // Woken by any reply; true once every request is answered.
        let answered = |until: tokio::time::Instant| {
            let notify = notify.clone();
            let all_done = &all_done;
            async move {
                loop {
                    let n = notify.notified();
                    if all_done() {
                        return true;
                    }
                    if tokio::time::timeout_at(until, n).await.is_err() {
                        return all_done();
                    }
                }
            }
        };
        for _ in 0..attempts {
            let start = tokio::time::Instant::now();
            // This try's messages: (index, sequence, frame).
            let mut sent = Vec::new();
            for (i, op) in ops.iter().enumerate() {
                if done(i) {
                    continue;
                }
                let seq = self.next_seq();
                let lifetime = self.resend.every * self.resend.sends;
                self.pending.lock().insert(
                    seq,
                    Pending {
                        expires: start + lifetime,
                        expect: op.expect,
                        slot: slots[i].clone(),
                        notify: notify.clone(),
                    },
                );
                seqs.push(seq);
                sent.push((i, seq, self.frame(op, seq)));
            }
            // aiolifx's sends of each, `every` apart, while unanswered.
            let mut finished = false;
            for k in 0..self.resend.sends {
                for (i, _, frame) in &sent {
                    if !done(*i) {
                        // A send error is a lost datagram: the try times out.
                        let _ = self.sock.send(frame).await;
                    }
                }
                let next = start + self.resend.every * (k + 1);
                let until = next.min(start + per_try);
                if answered(until).await {
                    finished = true;
                    break;
                }
                if until == start + per_try {
                    break;
                }
            }
            // aiolifx has given the messages up; HA waits out its try.
            if !finished && answered(start + per_try).await {
                finished = true;
            }
            // A reply after a message's lifetime doesn't count (see `read`);
            // one within it does, though HA has gone on to its next try.
            if finished {
                break;
            }
        }
        {
            let mut pending = self.pending.lock();
            for s in seqs {
                if pending
                    .get(&s)
                    .is_some_and(|p| Arc::ptr_eq(&p.notify, &notify))
                {
                    pending.remove(&s);
                }
            }
        }
        let missing: Vec<usize> = (0..ops.len()).filter(|i| !done(*i)).collect();
        if !missing.is_empty() {
            return Err(missing);
        }
        Ok(slots
            .iter()
            .map(|s| s.lock().take().unwrap_or_else(|| unreachable!()))
            .collect())
    }

    /// A message nothing answers (`fire_and_forget`: `SetReboot`): seq 0,
    /// neither ack nor response asked for.
    pub async fn fire_and_forget(&self, ty: u16) {
        if self.is_stopped() {
            return;
        }
        let frame = Out {
            source: self.source,
            target: *self.target.lock(),
            seq: 0,
            ack: false,
            res: false,
            ty,
            payload: &[],
        }
        .encode();
        let _ = self.sock.send(&frame).await;
    }
}

/// Replies: a pending sequence with our source and the expected type
/// completes it; an `Acknowledgement` to a request that wants a state is
/// ignored; anything else on a pending sequence drops it, as aiolifx does.
async fn read(sock: Arc<UdpSocket>, source: u32, pending: Arc<Mutex<HashMap<u8, Pending>>>) {
    let mut buf = vec![0u8; 1024];
    loop {
        let Ok(n) = sock.recv(&mut buf).await else {
            // An ICMP error on a connected socket; keep listening.
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let Some((h, payload)) = proto::decode(&buf[..n]) else {
            continue;
        };
        let mut pending = pending.lock();
        let Some(p) = pending.get(&h.seq) else {
            continue;
        };
        if p.expires <= tokio::time::Instant::now() {
            pending.remove(&h.seq);
            continue;
        }
        if h.source == source && h.ty == p.expect {
            let p = pending.remove(&h.seq).unwrap_or_else(|| unreachable!());
            *p.slot.lock() = Some((h, payload.to_vec()));
            p.notify.notify_one();
        } else if h.ty == ty::ACK && p.expect != ty::ACK {
        } else {
            pending.remove(&h.seq);
        }
    }
}
