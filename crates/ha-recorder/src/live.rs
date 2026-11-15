//! The rows the logbook's live streams follow.
//!
//! HA builds a live logbook row from the event itself and augments it from
//! the context's first event (`Context.origin_event`). hars's intake already
//! keeps the first row of each recent context and works the `context_*`
//! fields out ([`crate::intake`], decision D4), so it hands the same rows to
//! the streams: one pass over each event however many clients follow, and
//! the live row can't differ from the stored one. A row comes only when the
//! logbook would show it *live* (HA's `_is_state_filtered`, which looks at
//! the event's old state rather than the previous row we wrote, so the first
//! change after a start shows live though it isn't [`VISIBLE`] in history),
//! and only while a stream is subscribed.
//!
//! [`VISIBLE`]: crate::VISIBLE

use crate::format::Row;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;

/// The subscribers, as the intake sees them.
#[derive(Default)]
pub(crate) struct Live {
    senders: Mutex<Vec<mpsc::UnboundedSender<Arc<Row>>>>,
    /// How many there are, so the intake can skip the work (and the lock)
    /// while nobody follows.
    count: AtomicUsize,
}

impl Live {
    pub(crate) fn subscribe(&self) -> LiveFeed {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut senders = self.senders.lock();
        senders.retain(|s| !s.is_closed());
        senders.push(tx);
        self.count.store(senders.len(), Ordering::Release);
        LiveFeed { rx }
    }

    /// Whether anyone follows. A feed that was dropped still counts until
    /// the next row goes out and finds it closed.
    pub(crate) fn wanted(&self) -> bool {
        self.count.load(Ordering::Acquire) > 0
    }

    pub(crate) fn publish(&self, row: Row) {
        let row = Arc::new(row);
        let mut senders = self.senders.lock();
        senders.retain(|s| s.send(row.clone()).is_ok());
        self.count.store(senders.len(), Ordering::Release);
    }
}

/// A live feed of logbook rows, from [`crate::Recorder::subscribe_live`].
/// Nothing is dropped: the queue grows while the consumer is behind, so the
/// consumer bounds it with [`LiveFeed::backlog`]. Dropping it unsubscribes.
pub struct LiveFeed {
    rx: mpsc::UnboundedReceiver<Arc<Row>>,
}

impl LiveFeed {
    /// The next row, in the order the recorder saw the events. `None` once
    /// the recorder is gone.
    pub async fn recv(&mut self) -> Option<Arc<Row>> {
        self.rx.recv().await
    }

    #[cfg(test)]
    pub(crate) fn try_recv(&mut self) -> Option<Arc<Row>> {
        self.rx.try_recv().ok()
    }

    /// Rows queued and not yet received.
    pub fn backlog(&self) -> usize {
        self.rx.len()
    }
}
