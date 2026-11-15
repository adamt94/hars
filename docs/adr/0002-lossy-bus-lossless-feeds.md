# 0002: A lossy bus for API clients, lossless feeds for internal consumers

Status: accepted (2026-11-13; the event feed added 2026-11-15)

## Context

Home Assistant's event bus calls every listener for every event, holding
memory for as long as a listener is slow. A bounded channel keeps memory
fixed but drops events for a slow reader. API clients can recover from that
(they resync from a snapshot); automations and the recorder cannot, because
a dropped transition is a missed trigger or a hole in history.

## Decision

- `EventBus` is a bounded `broadcast` (4096 events) of `Arc<Event>`. A
  subscriber that lags is told so and resyncs from the state machine. For
  example, a `subscribe_entities` client that falls behind is disconnected,
  as in HA, and the frontend reconnects to a fresh snapshot.
- Internal consumers that must see everything use unbounded, lossless feeds:
  `Hass::subscribe_state_changes` (state changes, with an atomic snapshot)
  and `EventBus::subscribe_lossless` (any events a filter picks).
- Writers notify the feeds and the bus while holding the state lock, so all
  consumers see one order.

## Consequences

- Memory for API clients is bounded whatever they do.
- A lossless consumer must keep up; its backlog is memory. Each one is
  internal code we control and measure.
- The filter of `subscribe_lossless` runs on the writer's thread with locks
  held, so it must be cheap and must not call back into `Hass`.
