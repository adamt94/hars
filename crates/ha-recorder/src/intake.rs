//! The intake: bus events (from the lossless feed) to rows.
//!
//! For each event it does the cheap work and hands the writer a ready row:
//!
//! - **States.** The recorded attributes (the entity's unrecorded ones
//!   left out, JSON over 16 KiB stored as `{}`, as HA's
//!   `shared_attrs_bytes_from_event`). The logbook flags: [`VISIBLE`] when
//!   the previous row this process wrote for the entity had another state
//!   and `last_changed == last_updated` (HA's `old_state_id` rule, so the
//!   first row after a start never shows), [`HAS_UNIT`] when the recorded
//!   attributes have a unit.
//! - **D2.** An attribute-only write whose state and recorded attributes
//!   repeat the entity's previous row, outside HA's significant domains,
//!   writes nothing.
//! - **D3.** Of the events, only what the logbook reads: `automation_triggered`,
//!   `call_service`, `homeassistant_start` and `homeassistant_stop`.
//! - **D4.** A logbook row's `context_*` fields are worked out now, from a
//!   bounded map of recent contexts, instead of by query-time joins
//!   (`logbook/processor.py`'s `ContextAugmenter`).
//! - **Live rows.** While a logbook stream follows, the rows it would show
//!   are also handed to it ([`crate::live`]), whether or not the store takes
//!   them.

use crate::format::{EventRow, HAS_UNIT, REMOVED, Row, StateRow, VISIBLE};
use crate::live::Live;
use ha_core::{Context, ContextId, EntityId, Event, EventKind, Hass, State};
use smol_str::SmolStr;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::Arc;

/// HA's `MAX_STATE_ATTRS_BYTES`: recorded attributes past this are `{}`.
const MAX_ATTRS_BYTES: usize = 16_384;
/// How many recent contexts the augmentation can see.
pub(crate) const CONTEXTS: usize = 4096;
/// `history/const.py`'s `SIGNIFICANT_DOMAINS`: their attribute-only
/// writes are history (the climate chart's temperatures).
const SIGNIFICANT_DOMAINS: [&str; 5] = [
    "climate",
    "device_tracker",
    "humidifier",
    "thermostat",
    "water_heater",
];

/// The events the recorder reads off the bus, `homeassistant_final_write`
/// included: the intake stops at it.
pub(crate) fn recorded(e: &Event) -> bool {
    match &e.kind {
        EventKind::StateChanged { .. }
        | EventKind::CallService { .. }
        | EventKind::HomeassistantStart {}
        | EventKind::HomeassistantStop {}
        | EventKind::HomeassistantFinalWrite {} => true,
        EventKind::Custom { name, .. } => name == "automation_triggered",
        _ => false,
    }
}

/// The entity's previous recorded row.
struct Last {
    state: SmolStr,
    attributes: Arc<str>,
}

/// What a context's first row was, as the augmentation needs it.
enum Origin {
    State {
        entity_id: EntityId,
        state: SmolStr,
    },
    /// The `context_*` fields, ready to splice (`call_service` and the
    /// described events).
    Fields(Arc<str>),
}

struct Seen {
    origin: Origin,
    user_id: Option<SmolStr>,
}

/// The first row of each recent context, oldest dropped first.
struct Contexts {
    map: HashMap<ContextId, Seen>,
    order: VecDeque<ContextId>,
    cap: usize,
}

impl Contexts {
    fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    /// Remember `ctx`'s first row; a later one with the same context
    /// changes nothing (the processor's `context_lookup` keeps the first).
    fn see(&mut self, ctx: &Context, origin: Origin) {
        if self.map.contains_key(&ctx.id) {
            return;
        }
        if self.order.len() == self.cap
            && let Some(old) = self.order.pop_front()
        {
            self.map.remove(&old);
        }
        self.order.push_back(ctx.id);
        self.map.insert(
            ctx.id,
            Seen {
                origin,
                user_id: ctx.user_id.clone(),
            },
        );
    }

    /// The `context_*` fields for a logbook row with context `ctx`, as
    /// HA's `ContextAugmenter.augment`, as JSON object text. Call before
    /// [`Contexts::see`] for the row itself.
    fn augment(&self, ctx: &Context) -> Option<Arc<str>> {
        let mut out = String::new();
        let mut user = ctx.user_id.as_deref();
        if let Some(u) = user {
            field(&mut out, "context_user_id", u);
        }
        // The context's first row, or (if this row is it) its parent's.
        let origin = self
            .map
            .get(&ctx.id)
            .or_else(|| self.map.get(&ctx.parent_id?));
        match origin.map(|s| &s.origin) {
            Some(Origin::State { entity_id, state }) => {
                field(&mut out, "context_state", state);
                field(&mut out, "context_entity_id", entity_id.as_str());
            }
            Some(Origin::Fields(fields)) => {
                if !out.is_empty() {
                    out.push(',');
                }
                out.push_str(fields);
            }
            None => {}
        }
        if user.is_none()
            && let Some(parent) = ctx.parent_id
        {
            user = self.map.get(&parent).and_then(|s| s.user_id.as_deref());
            if let Some(u) = user {
                // HA adds it last when it comes from the parent.
                field(&mut out, "context_user_id", u);
            }
        }
        (!out.is_empty()).then(|| Arc::from(format!("{{{out}}}")))
    }
}

/// Append `"key":"value"` (JSON-escaped) to an object's fields.
fn field(out: &mut String, key: &str, value: &str) {
    if !out.is_empty() {
        out.push(',');
    }
    let _ = write!(out, "\"{key}\":");
    push_json_str(out, value);
}

fn push_json_str(out: &mut String, s: &str) {
    match serde_json::to_string(s) {
        Ok(j) => out.push_str(&j),
        Err(_) => out.push_str("\"\""),
    }
}

/// What admitting a row changes for the rows after it.
struct Effects {
    see: Option<(Context, Origin)>,
    last: Option<(EntityId, Last)>,
}

/// A row to write, what admitting it changes, and the row the live
/// streams get (if they follow and it is one they show).
struct Prepared {
    row: Row,
    effects: Effects,
    live: Option<Row>,
}

/// Turns the recorder's events into rows.
pub(crate) struct Intake {
    hass: Hass,
    last: HashMap<EntityId, Last>,
    contexts: Contexts,
    /// Entities already warned about for oversized attributes.
    oversized: HashSet<EntityId>,
    buf: Vec<u8>,
    live: Arc<Live>,
}

impl Intake {
    pub(crate) fn new(hass: Hass) -> Self {
        Self {
            hass,
            last: HashMap::new(),
            contexts: Contexts::new(CONTEXTS),
            oversized: HashSet::new(),
            buf: Vec::new(),
            live: Arc::default(),
        }
    }

    /// Where the live streams subscribe.
    pub(crate) fn live(&self) -> Arc<Live> {
        self.live.clone()
    }

    /// Record `event`'s row, if it has one, through `admit`. What later
    /// rows are compared with (the entity's previous row, the context's
    /// first row) changes only if `admit` takes the row: a row the store
    /// drops (its backlog full) leaves no trace.
    pub(crate) fn record(&mut self, event: &Event, admit: impl FnOnce(Row) -> bool) {
        let Some(Prepared { row, effects, live }) = self.prepare(event) else {
            return;
        };
        if let Some(live) = live {
            self.live.publish(live);
        }
        if admit(row) {
            if let Some((ctx, origin)) = effects.see {
                self.contexts.see(&ctx, origin);
            }
            if let Some((entity_id, last)) = effects.last {
                self.last.insert(entity_id, last);
            }
        }
    }

    /// The row `record` would admit (tests).
    #[cfg(test)]
    pub(crate) fn row(&mut self, event: &Event) -> Option<Row> {
        let mut out = None;
        self.record(event, |row| {
            out = Some(row);
            true
        });
        out
    }

    fn prepare(&mut self, event: &Event) -> Option<Prepared> {
        let ts = event.time_fired.as_microsecond();
        let ctx = &event.context;
        match &event.kind {
            EventKind::StateChanged {
                entity_id,
                old_state,
                new_state,
            } => {
                // A removal is HA's row with a NULL state (stored as an empty
                // state with the [`REMOVED`] flag), so history ends the entity's line there.
                // The entity may have a line from before this process
                // started, so every removal writes one. The entity is
                // forgotten: a later one with this id starts over.
                let Some(new) = new_state.as_deref() else {
                    self.oversized.remove(entity_id);
                    self.last.remove(entity_id);
                    if old_state.is_none() {
                        return None;
                    }
                    let row = Row::State(StateRow {
                        entity_id: entity_id.as_str().into(),
                        ts,
                        last_changed: ts,
                        state: SmolStr::default(),
                        attributes: Arc::from("{}"),
                        flags: REMOVED,
                        context: ctx.id.to_bytes(),
                        aug: None,
                    });
                    return Some(Prepared {
                        row,
                        effects: Effects {
                            see: None,
                            last: None,
                        },
                        live: None,
                    });
                };
                self.state_row(entity_id, old_state.as_deref(), new)
            }
            EventKind::CallService {
                domain,
                service,
                service_data,
            } => {
                let mut fields = String::new();
                field(&mut fields, "context_domain", domain);
                field(&mut fields, "context_service", service);
                field(&mut fields, "context_event_type", "call_service");
                let see = Some((ctx.clone(), Origin::Fields(Arc::from(fields))));
                let mut data = String::new();
                field(&mut data, "domain", domain);
                field(&mut data, "service", service);
                if let Some(ids) = service_data.get("entity_id") {
                    let _ = write!(data, ",\"entity_id\":{ids}");
                }
                let row = Row::Event(EventRow {
                    event_type: "call_service".into(),
                    ts,
                    data: Some(Arc::from(format!("{{{data}}}"))),
                    context: ctx.id.to_bytes(),
                    aug: None,
                });
                let effects = Effects { see, last: None };
                // Never a logbook row: it only links contexts.
                Some(Prepared {
                    row,
                    effects,
                    live: None,
                })
            }
            EventKind::Custom { name, data } if name == "automation_triggered" => {
                let aug = self.contexts.augment(ctx);
                // The describer's values; empty ones are left out.
                let text = |k: &str| {
                    data.get(k)
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                };
                let source = text("source");
                let mut fields = String::new();
                field(&mut fields, "context_event_type", "automation_triggered");
                field(&mut fields, "context_domain", "automation");
                if let Some(n) = text("name") {
                    field(&mut fields, "context_name", n);
                }
                let message = match data.get("source").and_then(|v| v.as_str()) {
                    Some(s) => format!("triggered by {s}"),
                    None => "triggered".into(),
                };
                field(&mut fields, "context_message", &message);
                if let Some(s) = source {
                    field(&mut fields, "context_source", s);
                }
                if let Some(e) = text("entity_id") {
                    field(&mut fields, "context_entity_id", e);
                }
                let row = Row::Event(EventRow {
                    event_type: "automation_triggered".into(),
                    ts,
                    data: Some(Arc::from(data.to_string())),
                    context: ctx.id.to_bytes(),
                    aug,
                });
                let see = Some((ctx.clone(), Origin::Fields(Arc::from(fields))));
                Some(self.shown(row, Effects { see, last: None }))
            }
            EventKind::HomeassistantStart {} | EventKind::HomeassistantStop {} => {
                let (event_type, message) = match event.kind {
                    EventKind::HomeassistantStart {} => ("homeassistant_start", "started"),
                    _ => ("homeassistant_stop", "stopped"),
                };
                let aug = self.contexts.augment(ctx);
                // `homeassistant/logbook.py` describes these.
                let mut fields = String::new();
                field(&mut fields, "context_event_type", event_type);
                field(&mut fields, "context_domain", "homeassistant");
                field(&mut fields, "context_name", "Home Assistant");
                field(&mut fields, "context_message", message);
                let row = Row::Event(EventRow {
                    event_type: event_type.into(),
                    ts,
                    data: None,
                    context: ctx.id.to_bytes(),
                    aug,
                });
                let see = Some((ctx.clone(), Origin::Fields(Arc::from(fields))));
                Some(self.shown(row, Effects { see, last: None }))
            }
            _ => None,
        }
    }

    fn state_row(
        &mut self,
        entity_id: &EntityId,
        old: Option<&State>,
        new: &State,
    ) -> Option<Prepared> {
        let had_old = old.is_some();
        let ts = new.last_updated.as_microsecond();
        let last_changed = new.last_changed.as_microsecond();
        let has_unit = self.attributes(new);
        let text = String::from_utf8_lossy(&self.buf);
        let prev = self.last.get(entity_id);
        // Reuse the previous row's text when it repeats; copy it out of
        // the scratch buffer only when it changed.
        let attributes = match prev {
            Some(p) if *p.attributes == *text => p.attributes.clone(),
            _ => Arc::from(&*text),
        };
        let origin = || Origin::State {
            entity_id: entity_id.clone(),
            state: new.state.clone(),
        };
        // D2: an attribute-only write that records nothing new.
        if had_old
            && last_changed != ts
            && !SIGNIFICANT_DOMAINS.contains(&entity_id.domain())
            && prev.is_some_and(|p| p.state == new.state && Arc::ptr_eq(&p.attributes, &attributes))
        {
            self.contexts.see(&new.context, origin());
            return None;
        }
        let visible = had_old && last_changed == ts && prev.is_some_and(|p| p.state != new.state);
        let aug = if visible {
            self.contexts.augment(&new.context)
        } else {
            None
        };
        let effects = Effects {
            see: Some((new.context.clone(), origin())),
            last: Some((
                entity_id.clone(),
                Last {
                    state: new.state.clone(),
                    attributes: attributes.clone(),
                },
            )),
        };
        // Live, HA looks at the event's old state, not our previous row
        // (`_is_state_filtered`): the first change after a start shows.
        let live = (self.live.wanted()
            && old.is_some_and(|o| o.state != new.state)
            && last_changed == ts
            && !crate::logbook::continuous(entity_id.domain(), &new.attributes))
        .then(|| {
            Row::State(StateRow {
                entity_id: entity_id.as_str().into(),
                ts,
                last_changed,
                state: new.state.clone(),
                attributes: attributes.clone(),
                flags: VISIBLE,
                context: new.context.id.to_bytes(),
                aug: if visible {
                    aug.clone()
                } else {
                    self.contexts.augment(&new.context)
                },
            })
        });
        let mut flags = 0;
        if visible {
            flags |= VISIBLE;
        }
        if has_unit {
            flags |= HAS_UNIT;
        }
        let row = Row::State(StateRow {
            entity_id: entity_id.as_str().into(),
            ts,
            last_changed,
            state: new.state.clone(),
            attributes,
            flags,
            context: new.context.id.to_bytes(),
            aug,
        });
        Some(Prepared { row, effects, live })
    }

    /// `row` as a [`Prepared`] that live streams get too, if any follow.
    fn shown(&self, row: Row, effects: Effects) -> Prepared {
        let live = self.live.wanted().then(|| row.clone());
        Prepared { row, effects, live }
    }

    /// Encode the recorded attributes as JSON text into `self.buf`;
    /// whether they have a unit.
    fn attributes(&mut self, state: &State) -> bool {
        let unrecorded = self.hass.unrecorded(&state.entity_id);
        let mut out = Capped(&mut self.buf);
        out.0.clear();
        let mut has_unit = false;
        let mut first = true;
        let fits = (|| -> std::io::Result<()> {
            out.write_all(b"{")?;
            for (k, v) in &state.attributes {
                if unrecorded.excludes(k) {
                    continue;
                }
                if !std::mem::take(&mut first) {
                    out.write_all(b",")?;
                }
                has_unit |= k == "unit_of_measurement";
                serde_json::to_writer(&mut out, k)?;
                out.write_all(b":")?;
                serde_json::to_writer(&mut out, v)?;
            }
            out.write_all(b"}")
        })()
        .is_ok();
        let buf = out.0;
        if !fits {
            buf.clear();
            buf.extend_from_slice(b"{}");
            if self.oversized.insert(state.entity_id.clone()) {
                tracing::warn!(
                    "State attributes for {} exceed maximum size of {MAX_ATTRS_BYTES} bytes. \
                     This can cause database performance issues; Attributes will not be stored",
                    state.entity_id
                );
            }
            return false;
        }
        has_unit
    }
}

/// A writer that refuses to grow past [`MAX_ATTRS_BYTES`], so an
/// oversized attribute is never encoded whole (or kept as capacity).
struct Capped<'a>(&'a mut Vec<u8>);

impl std::io::Write for Capped<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.0.len() + data.len() > MAX_ATTRS_BYTES {
            return Err(std::io::Error::other("over the attribute limit"));
        }
        self.0.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
