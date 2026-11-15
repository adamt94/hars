//! `config_entries/subscribe` and `config_entries/flow/subscribe`
//! (`components/config/config_entries.py`), which onboarding's
//! integrations step and the integrations page open.
//!
//! Where `hars` differs from HA:
//! - Entries are sent as they are when subscribing; later changes are not
//!   pushed (an entry is only added at runtime by onboarding, before its
//!   integrations step subscribes).
//! - An entry's `state` is `loaded` when its integration runs in `hars`
//!   and `not_loaded` otherwise (or when disabled): `hars` doesn't track
//!   each entry's setup (HA says `setup_retry` while met.no can't be
//!   reached, for one).
//! - No config flow is ever in progress (`hars` has no discovery or config
//!   flows), so the flow subscription's list is empty.

use super::{Conn, Invalid, Subscription, only_keys};
use ha_auth::Permission;
use serde_json::{Map, Value, json};

/// What each config flow handler of an integration `hars` runs supports
/// (HA 2026.6.1): (domain, options flow, `async_unload_entry`).
/// None of them removes devices, reconfigures or has subentries.
const HANDLERS: &[(&str, bool, bool)] = &[
    ("hue", true, true),
    ("lifx", false, true),
    ("met", true, true),
    ("mobile_app", false, true),
    ("sun", false, true),
    ("switchbot_cloud", false, true),
];

pub(super) fn dispatch(
    conn: &mut Conn,
    id: u64,
    ty: &str,
    msg: &Map<String, Value>,
) -> Option<Result<(), Invalid>> {
    Some(match ty {
        "config_entries/subscribe" => subscribe(conn, id, msg),
        "config_entries/flow/subscribe" => only_keys(msg, &[]).map(|()| {
            if conn.principal.permission < Permission::Admin {
                conn.out.error(json!(id), "unauthorized", "Unauthorized");
                return;
            }
            // The flows in progress (none), then the result, as HA sends them.
            conn.out.event(id, &json!([]));
            conn.out.result(id, Value::Null);
            conn.subs.insert(id, Subscription::Idle);
        }),
        _ => return None,
    })
}

/// `vol.Optional("type_filter"): vol.All(cv.ensure_list, [str])`.
fn type_filter(msg: &Map<String, Value>) -> Result<Vec<&str>, Invalid> {
    let items: Vec<&Value> = match msg.get("type_filter") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(a)) => a.iter().collect(),
        Some(v) => vec![v],
    };
    items
        .iter()
        .enumerate()
        .map(|(i, v)| {
            v.as_str()
                .ok_or_else(|| format!("expected str @ data['type_filter'][{i}]"))
        })
        .collect()
}

fn subscribe(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(msg, &["type_filter"])?;
    let filter = type_filter(msg)?;
    let entries = conn
        .shared
        .config
        .config_entries
        .as_ref()
        .and_then(|e| e.list())
        .unwrap_or_default();
    let running = crate::components(&conn.shared.config);
    let list: Vec<Value> = entries
        .iter()
        .filter(|e| matches_filter(e, &filter))
        .map(|e| json!({"type": null, "entry": entry_json(e, &running)}))
        .collect();
    conn.out.result(id, Value::Null);
    conn.out.event(id, &Value::Array(list));
    conn.subs.insert(id, Subscription::Idle);
    Ok(())
}

/// `_async_matching_config_entries_json_fragments`: an entry whose
/// integration's type is in the filter; one of an integration `hars`
/// doesn't know passes too, unless only helpers are asked for.
fn matches_filter(entry: &Map<String, Value>, filter: &[&str]) -> bool {
    if filter.is_empty() {
        return true;
    }
    let domain = entry.get("domain").and_then(Value::as_str).unwrap_or("");
    match super::manifest::integration_type(domain) {
        Some(t) => filter.contains(&t.as_str()),
        None => filter != ["helper"],
    }
}

/// `ConfigEntry.as_json_fragment` for a stored entry.
fn entry_json(e: &Map<String, Value>, running: &[&str]) -> Value {
    let str_of = |k: &str| e.get(k).and_then(Value::as_str);
    let domain = str_of("domain").unwrap_or("");
    let disabled = e.get("disabled_by").is_some_and(|d| !d.is_null());
    let (options, unload) = HANDLERS
        .iter()
        .find(|(d, ..)| *d == domain)
        .map_or((false, false), |&(_, o, u)| (o, u));
    let state = if !disabled && running.contains(&domain) {
        "loaded"
    } else {
        "not_loaded"
    };
    // `datetime.timestamp()` of the stored ISO times.
    let timestamp = |k: &str| {
        str_of(k)
            .and_then(|s| s.parse::<jiff::Timestamp>().ok())
            .map_or(json!(0.0), |t| json!(t.as_microsecond() as f64 / 1e6))
    };
    json!({
        "created_at": timestamp("created_at"),
        "entry_id": str_of("entry_id"),
        "domain": domain,
        "modified_at": timestamp("modified_at"),
        "title": str_of("title"),
        "source": str_of("source"),
        "state": state,
        "supports_options": options,
        "supports_remove_device": false,
        "supports_unload": unload,
        "supports_reconfigure": false,
        "supported_subentry_types": {},
        "pref_disable_new_entities": e.get("pref_disable_new_entities").cloned().unwrap_or(json!(false)),
        "pref_disable_polling": e.get("pref_disable_polling").cloned().unwrap_or(json!(false)),
        "disabled_by": e.get("disabled_by").cloned().unwrap_or(Value::Null),
        "reason": null,
        "error_reason_translation_key": null,
        "error_reason_translation_placeholders": null,
        "num_subentries": e.get("subentries").and_then(Value::as_array).map_or(0, Vec::len),
    })
}
