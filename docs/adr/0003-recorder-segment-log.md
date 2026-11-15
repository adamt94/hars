# 0003: History in an append-only segment log, not SQLite

Status: accepted (2026-11-15)

## Context

Home Assistant records history in SQLite through SQLAlchemy. A SQLite
recorder means a C library in the binary, page-cache memory, and an fsync
pattern that keeps the disk busy. The frontend only needs a few queries:
significant states over a period, and the logbook's rows.

## Decision

The recorder (`ha-recorder`) is an append-only log of segments under
`<config>/.hars/recorder/`, one writer thread, batches of up to 5 s, a
segment synced when it closes. HA's database is never read or written.

The numbered decisions made with it, referred to in the code as D1 to D9:

- **D1:** the statistics calls answer empty, so `state_class` sensors show
  the history line instead of long-term statistics.
- **D2:** attribute-only writes that repeat the previous row, outside HA's
  significant domains, aren't recorded (46 % fewer rows on the reference
  install).
- **D3:** only the event types the logbook shows are recorded.
- **D4:** the logbook's `context_*` fields are worked out when a row is
  written, not when it is read.
- **D5:** history reads flush the pending batch first.
- **D6:** HA's rule that the first row after a restart has no old state is
  kept.
- **D7:** an append-only segment log in place of SQLite.
- **D8:** HA's recorder database is never imported or written; history
  starts fresh at cutover.
- **D9:** retention is 10 days **and** 64 MiB, whichever is reached first.

## Consequences

- No SQL engine in the binary, and writes are one `write()` per batch.
- Every query the frontend makes must be written by hand; arbitrary SQL
  against history is not possible.
- A process kill loses nothing committed (`ha-recorder/tests/crash.rs`);
  power loss can lose the open segment's unsynced tail.
