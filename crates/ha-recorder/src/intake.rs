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

use crate::format::{EventRow, HAS_UNIT, Row, StateRow, VISIBLE};
use ha_core::{Context, ContextId, EntityId, Event, EventKind, Hass, State};
use smol_str::SmolStr;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
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

/// Turns the recorder's events into rows.
pub(crate) struct Intake {
    hass: Hass,
    last: HashMap<EntityId, Last>,
    contexts: Contexts,
    /// Entities already warned about for oversized attributes.
    oversized: HashSet<EntityId>,
    buf: Vec<u8>,
}

impl Intake {
    pub(crate) fn new(hass: Hass) -> Self {
        Self {
            hass,
            last: HashMap::new(),
            contexts: Contexts::new(CONTEXTS),
            oversized: HashSet::new(),
            buf: Vec::new(),
        }
    }

    /// The row for `event`, if it is recorded.
    pub(crate) fn row(&mut self, event: &Event) -> Option<Row> {
        let ts = event.time_fired.as_microsecond();
        let ctx = &event.context;
        match &event.kind {
            EventKind::StateChanged {
                entity_id,
                old_state,
                new_state,
            } => {
                // A removal (no new state) isn't recorded: hars doesn't
                // remove entities at runtime.
                let new = new_state.as_deref()?;
                self.state_row(entity_id, old_state.is_some(), new)
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
                self.contexts.see(ctx, Origin::Fields(Arc::from(fields)));
                let mut data = String::new();
                field(&mut data, "domain", domain);
                field(&mut data, "service", service);
                if let Some(ids) = service_data.get("entity_id") {
                    let _ = write!(data, ",\"entity_id\":{ids}");
                }
                Some(Row::Event(EventRow {
                    event_type: "call_service".into(),
                    ts,
                    data: Some(Arc::from(format!("{{{data}}}"))),
                    context: ctx.id.to_bytes(),
                    aug: None,
                }))
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
                self.contexts.see(ctx, Origin::Fields(Arc::from(fields)));
                Some(Row::Event(EventRow {
                    event_type: "automation_triggered".into(),
                    ts,
                    data: Some(Arc::from(data.to_string())),
                    context: ctx.id.to_bytes(),
                    aug,
                }))
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
                self.contexts.see(ctx, Origin::Fields(Arc::from(fields)));
                Some(Row::Event(EventRow {
                    event_type: event_type.into(),
                    ts,
                    data: None,
                    context: ctx.id.to_bytes(),
                    aug,
                }))
            }
            _ => None,
        }
    }

    fn state_row(&mut self, entity_id: &EntityId, had_old: bool, new: &State) -> Option<Row> {
        let ts = new.last_updated.as_microsecond();
        let last_changed = new.last_changed.as_microsecond();
        let (text, has_unit) = self.attributes(new);
        let prev = self.last.get(entity_id);
        // Reuse the previous row's text when it repeats.
        let attributes = match prev {
            Some(p) if *p.attributes == *text => p.attributes.clone(),
            _ => Arc::from(text),
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
        self.contexts.see(&new.context, origin());
        self.last.insert(
            entity_id.clone(),
            Last {
                state: new.state.clone(),
                attributes: attributes.clone(),
            },
        );
        let mut flags = 0;
        if visible {
            flags |= VISIBLE;
        }
        if has_unit {
            flags |= HAS_UNIT;
        }
        Some(Row::State(StateRow {
            entity_id: entity_id.as_str().into(),
            ts,
            last_changed,
            state: new.state.clone(),
            attributes,
            flags,
            context: new.context.id.to_bytes(),
            aug,
        }))
    }

    /// The recorded attributes as JSON text, and whether they have a unit.
    fn attributes(&mut self, state: &State) -> (String, bool) {
        let unrecorded = self.hass.unrecorded(&state.entity_id);
        let buf = &mut self.buf;
        buf.clear();
        buf.push(b'{');
        let mut has_unit = false;
        for (k, v) in &state.attributes {
            if unrecorded.excludes(k) {
                continue;
            }
            if buf.len() > 1 {
                buf.push(b',');
            }
            has_unit |= k == "unit_of_measurement";
            let _ = serde_json::to_writer(&mut *buf, k);
            buf.push(b':');
            let _ = serde_json::to_writer(&mut *buf, v);
        }
        buf.push(b'}');
        if buf.len() > MAX_ATTRS_BYTES {
            if self.oversized.insert(state.entity_id.clone()) {
                tracing::warn!(
                    "State attributes for {} exceed maximum size of {MAX_ATTRS_BYTES} bytes. \
                     This can cause database performance issues; Attributes will not be stored",
                    state.entity_id
                );
            }
            return ("{}".into(), false);
        }
        (String::from_utf8_lossy(buf).into_owned(), has_unit)
    }
}

#[cfg(test)]
mod tests;
