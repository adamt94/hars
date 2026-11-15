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
use crate::logbook::Filter;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;

/// `MAX_PENDING_LOGBOOK_EVENTS`: rows a follower may have queued. One that
/// falls further behind is dropped, as HA drops a client whose queue fills.
pub const MAX_PENDING: usize = 2048;

/// A subscriber: the rows it follows, and its queue.
type Follower = (Arc<Filter>, mpsc::Sender<Arc<Row>>);

/// The subscribers, as the intake sees them.
#[derive(Default)]
pub(crate) struct Live {
    senders: Mutex<Vec<Follower>>,
    /// How many there are, so the intake can skip the work (and the lock)
    /// while nobody follows.
    count: AtomicUsize,
}

impl Live {
    pub(crate) fn subscribe(&self, filter: Arc<Filter>) -> LiveFeed {
        let (tx, rx) = mpsc::channel(MAX_PENDING);
        let mut senders = self.senders.lock();
        senders.retain(|(_, s)| !s.is_closed());
        senders.push((filter, tx));
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
        // Rows a follower doesn't want never queue, so they can't fill its
        // bound. A full queue drops the follower; it drains what it has,
        // then sees the end.
        senders.retain(|(filter, tx)| {
            if tx.is_closed() {
                return false;
            }
            !filter.follows(&row.as_rec()) || tx.try_send(row.clone()).is_ok()
        });
        self.count.store(senders.len(), Ordering::Release);
    }
}

/// A live feed of the logbook rows its filter follows, from
/// [`crate::Recorder::subscribe_live`]. At most [`MAX_PENDING`] queue; past
/// that the feed ends once drained. Dropping it unsubscribes.
pub struct LiveFeed {
    rx: mpsc::Receiver<Arc<Row>>,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::StateRow;

    fn row(entity: &str) -> Row {
        Row::State(StateRow {
            entity_id: entity.into(),
            ts: 1,
            last_changed: 1,
            state: "on".into(),
            attributes: Arc::from("{}"),
            flags: crate::VISIBLE,
            context: [1; 16],
            aug: None,
        })
    }

    /// A follower holds at most `MAX_PENDING` rows of what it follows, and
    /// rows it doesn't follow never count against it.
    #[tokio::test]
    async fn a_follower_is_bounded_by_what_it_follows() {
        let live = Live::default();
        let filter = Arc::new(Filter::new(&["light.a".to_owned()], &[], None));
        let mut feed = live.subscribe(filter);
        for _ in 0..MAX_PENDING * 2 {
            live.publish(row("light.b"));
        }
        assert!(live.wanted(), "unrelated rows don't drop the follower");
        for _ in 0..MAX_PENDING {
            live.publish(row("light.a"));
        }
        assert!(live.wanted(), "a full queue is still a follower");
        // One more than it can hold: dropped, after what it has.
        live.publish(row("light.a"));
        assert!(!live.wanted());
        let mut got = 0;
        while feed.recv().await.is_some() {
            got += 1;
        }
        assert_eq!(got, MAX_PENDING);
    }
}
