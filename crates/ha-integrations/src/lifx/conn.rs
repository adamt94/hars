//! One bulb's UDP connection: aiolifx's request matching (sequence, source
//! and expected type) under HA's retry layer, where each try is a new
//! message and a reply to any try completes the request.

use super::proto::{self, Header, Out, ty};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::Notify;

pub const PORT: u16 = 56700;

/// aiolifx's per-request timeout as HA sets it.
const REQUEST_LIFETIME: Duration = Duration::from_secs(18);

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
    lifetime: Duration,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Conn {
    /// A socket bound to an ephemeral port and connected to the bulb.
    pub async fn open(host: &str, port: u16) -> std::io::Result<Self> {
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
            lifetime: REQUEST_LIFETIME,
        })
    }

    #[cfg(test)]
    pub(super) fn with_lifetime(mut self, d: Duration) -> Self {
        self.lifetime = d;
        self
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
    /// request not yet answered (a new sequence number each), then waits
    /// `per_try` for all of them. `Err` holds the unanswered indices.
    pub async fn execute(
        &self,
        ops: &[Op],
        attempts: u32,
        per_try: Duration,
    ) -> Result<Vec<(Header, Vec<u8>)>, Vec<usize>> {
        let notify = Arc::new(Notify::new());
        let slots: Vec<Slot> = ops.iter().map(|_| Slot::default()).collect();
        let mut seqs = Vec::new();
        let done = |i: usize| slots[i].lock().is_some();
        for _ in 0..attempts {
            for (i, op) in ops.iter().enumerate() {
                if done(i) {
                    continue;
                }
                let seq = self.next_seq();
                self.pending.lock().insert(
                    seq,
                    Pending {
                        expires: tokio::time::Instant::now() + self.lifetime,
                        expect: op.expect,
                        slot: slots[i].clone(),
                        notify: notify.clone(),
                    },
                );
                seqs.push(seq);
                // A send error is a lost datagram: the try times out.
                let _ = self.sock.send(&self.frame(op, seq)).await;
            }
            let all = async {
                loop {
                    let n = notify.notified();
                    if (0..ops.len()).all(done) {
                        return;
                    }
                    n.await;
                }
            };
            if tokio::time::timeout(per_try, all).await.is_ok() {
                break;
            }
        }
        {
            // Our entries go when the request ends (aiolifx's linger 18 s,
            // which nothing can observe).
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
