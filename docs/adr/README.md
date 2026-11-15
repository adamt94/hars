# Architecture decision records

Short records of decisions that shape hars, with the reasons, so they are
not re-argued without new information. Each has a status: *accepted*, or
*superseded by* a later record. To change a decision, add a new record that
supersedes the old one; don't rewrite history.

| # | Decision | Date |
|---|---|---|
| [0001](0001-match-a-pinned-home-assistant.md) | Match one pinned Home Assistant release, its APIs and its files | 2026-11-13 |
| [0002](0002-lossy-bus-lossless-feeds.md) | A lossy bus for API clients, lossless feeds for internal consumers | 2026-11-13 |
| [0003](0003-recorder-segment-log.md) | History in an append-only segment log, not SQLite (D1–D9) | 2026-11-15 |
| [0004](0004-frontend-from-disk.md) | Serve the official frontend from disk, not embedded | 2026-11-14 |
| [0005](0005-integrations-compiled-in.md) | Integrations are compiled in, in one crate | 2026-11-13 |
| [0006](0006-compiled-in-tls-roots.md) | TLS roots compiled into the binary | 2026-11-14 |
| [0007](0007-transparent-huge-pages-off.md) | Turn transparent huge pages off by re-exec | 2026-11-13 |
| [0008](0008-vectors-from-home-assistant.md) | Expected values come from running Home Assistant's code | 2026-11-15 |
| [0009](0009-mvp-reference-install.md) | The MVP is a reference install; cutover after a shadow week | 2026-11-13 |

Template for a new record (`NNNN-short-name.md`):

```markdown
# NNNN: Title

Status: accepted (YYYY-MM-DD)

## Context
What forces the decision. Numbers if performance is involved.

## Decision
What we do.

## Consequences
What gets easier, what gets harder, what would make us revisit it.
```
