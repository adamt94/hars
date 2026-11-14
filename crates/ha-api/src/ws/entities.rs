//! `subscribe_entities`' compressed states and diffs
//! (`State.as_compressed_state`, `websocket_api/messages.py::_state_diff_event`).

use ha_core::{Context, EventKind, State};
use jiff::Timestamp;
use serde_json::{Map, Value, json};

/// Epoch seconds as a float, like `last_changed_timestamp`.
fn epoch(t: Timestamp) -> Value {
    json!(t.as_microsecond() as f64 / 1e6)
}

/// A bare id string when there is no parent or user, else the full context.
fn context(c: &Context) -> Value {
    if c.parent_id.is_none() && c.user_id.is_none() {
        json!(c.id)
    } else {
        json!(c)
    }
}

/// `{"s", "a", "c", "lc"[, "lu"]}`; `lu` only when it differs from `lc`.
pub(crate) fn compressed(state: &State) -> Value {
    let mut m = Map::new();
    m.insert("s".into(), json!(state.state));
    m.insert("a".into(), Value::Object(state.attributes.clone()));
    m.insert("c".into(), context(&state.context));
    m.insert("lc".into(), epoch(state.last_changed));
    if state.last_updated != state.last_changed {
        m.insert("lu".into(), epoch(state.last_updated));
    }
    Value::Object(m)
}

/// The event payload for one `state_changed`: `{"r": [id]}` for a removal,
/// `{"a": {id: compressed}}` for an addition, else `{"c": {id: diff}}`.
pub(crate) fn diff_event(kind: &EventKind) -> Option<Value> {
    let EventKind::StateChanged {
        entity_id,
        old_state,
        new_state,
    } = kind
    else {
        return None;
    };
    let Some(new) = new_state else {
        return Some(json!({ "r": [entity_id] }));
    };
    let Some(old) = old_state else {
        return Some(json!({ "a": { new.entity_id.as_str(): compressed(new) } }));
    };

    let mut add = Map::new();
    if old.state != new.state {
        add.insert("s".into(), json!(new.state));
    }
    if old.last_changed != new.last_changed {
        add.insert("lc".into(), epoch(new.last_changed));
    } else if old.last_updated != new.last_updated {
        add.insert("lu".into(), epoch(new.last_updated));
    }
    let (oc, nc) = (&old.context, &new.context);
    let mut ctx: Option<Value> = None;
    if oc.parent_id != nc.parent_id {
        ctx = Some(json!({ "parent_id": nc.parent_id }));
    }
    if oc.user_id != nc.user_id {
        match &mut ctx {
            Some(Value::Object(m)) => {
                m.insert("user_id".into(), json!(nc.user_id));
            }
            _ => ctx = Some(json!({ "user_id": nc.user_id })),
        }
    }
    if oc.id != nc.id {
        match &mut ctx {
            Some(Value::Object(m)) => {
                m.insert("id".into(), json!(nc.id));
            }
            _ => ctx = Some(json!(nc.id)),
        }
    }
    if let Some(c) = ctx {
        add.insert("c".into(), c);
    }

    let mut diff = Map::new();
    if old.attributes != new.attributes {
        let changed: Map<String, Value> = new
            .attributes
            .iter()
            .filter(|(k, v)| old.attributes.get(*k) != Some(*v))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !changed.is_empty() {
            add.insert("a".into(), Value::Object(changed));
        }
        let removed: Vec<&String> = old
            .attributes
            .keys()
            .filter(|k| !new.attributes.contains_key(*k))
            .collect();
        if !removed.is_empty() {
            diff.insert("-".into(), json!({ "a": removed }));
        }
    }
    diff.insert("+".into(), Value::Object(add));
    Some(json!({ "c": { new.entity_id.as_str(): diff } }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ha_core::{Attributes, EntityId};
    use std::sync::Arc;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn state(value: &str, attrs: Value, lc: &str, lu: &str, ctx: Context) -> Arc<State> {
        let Value::Object(attributes) = attrs else {
            panic!()
        };
        Arc::new(State {
            entity_id: EntityId::parse("light.k").unwrap(),
            state: value.into(),
            attributes: attributes as Attributes,
            last_changed: ts(lc),
            last_reported: ha_core::Reported::new(ts(lu)),
            last_updated: ts(lu),
            context: ctx,
        })
    }

    fn changed(old: Option<Arc<State>>, new: Option<Arc<State>>) -> Value {
        diff_event(&EventKind::StateChanged {
            entity_id: EntityId::parse("light.k").unwrap(),
            old_state: old,
            new_state: new,
        })
        .unwrap()
    }

    #[test]
    fn compressed_state_matches_ha() {
        let ctx = Context::at(ts("2026-06-01T10:00:00Z"));
        let id = serde_json::to_value(ctx.id).unwrap();
        let s = state(
            "on",
            json!({"brightness": 3}),
            "2026-06-01T10:00:00.000001Z",
            "2026-06-01T10:00:00.000001Z",
            ctx.clone(),
        );
        assert_eq!(
            compressed(&s),
            json!({"s": "on", "a": {"brightness": 3}, "c": id, "lc": 1780308000.000001})
        );
        let user_ctx = ctx.clone().with_user("u1");
        let s = state(
            "on",
            json!({}),
            "2026-06-01T10:00:00Z",
            "2026-06-01T10:05:00Z",
            user_ctx,
        );
        assert_eq!(
            compressed(&s),
            json!({"s": "on", "a": {}, "lc": 1780308000.0, "lu": 1780308300.0,
                   "c": {"id": id, "parent_id": null, "user_id": "u1"}})
        );
    }

    #[test]
    fn diffs_match_ha() {
        let c1 = Context::at(ts("2026-06-01T10:00:00Z"));
        let c2 = Context::at(ts("2026-06-01T10:01:00Z"));
        let id2 = serde_json::to_value(c2.id).unwrap();
        let t0 = "2026-06-01T10:00:00Z";
        let t1 = "2026-06-01T10:01:00Z";
        let old = state(
            "on",
            json!({"brightness": 3, "color_mode": "hs", "same": 1}),
            t0,
            t0,
            c1.clone(),
        );

        // Attribute-only update: `lu`, a new context id as a bare string,
        // changed attributes in `+`, removed ones in `-`.
        let new = state(
            "on",
            json!({"brightness": 4, "same": 1, "new": true}),
            t0,
            t1,
            c2.clone(),
        );
        assert_eq!(
            changed(Some(old.clone()), Some(new)),
            json!({"c": {"light.k": {
                "+": {"lu": 1780308060.0, "c": id2, "a": {"brightness": 4, "new": true}},
                "-": {"a": ["color_mode"]}}}})
        );

        // State change by a user: `s`, `lc` (never `lu` too), and a context
        // object with `user_id` then `id`.
        let new = state(
            "off",
            json!({"brightness": 3, "color_mode": "hs", "same": 1}),
            t1,
            t1,
            c2.clone().with_user("u1"),
        );
        assert_eq!(
            changed(Some(old.clone()), Some(new)),
            json!({"c": {"light.k": {"+": {"s": "off", "lc": 1780308060.0,
                                           "c": {"user_id": "u1", "id": id2}}}}})
        );

        // Nothing but a report: "+" is still present.
        let same = state(
            "on",
            json!({"brightness": 3, "color_mode": "hs", "same": 1}),
            t0,
            t0,
            c1,
        );
        assert_eq!(
            changed(Some(old.clone()), Some(same)),
            json!({"c": {"light.k": {"+": {}}}})
        );

        assert_eq!(changed(Some(old.clone()), None), json!({"r": ["light.k"]}));
        let added = changed(None, Some(old.clone()));
        assert_eq!(added["a"]["light.k"], compressed(&old));
    }
}
