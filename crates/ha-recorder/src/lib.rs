//! The recorder: state and event history for the history and logbook
//! screens, kept in an append-only segment log under
//! `<config>/.hars/recorder/` (never HA's `home-assistant_v2.db`).
//!
//! A crate of its own: it owns a thread and a file format, with their own
//! failure modes (a full disk, a torn write, a crash), and its queries are
//! shared by the history and logbook commands.

pub mod format;
mod intake;
mod recorder;
mod store;

pub use format::{EventRec, EventRow, HAS_UNIT, Rec, Row, StateRec, StateRow, VISIBLE};
pub use recorder::{Recorder, Settings};
pub use store::{Config, Info, Store};
