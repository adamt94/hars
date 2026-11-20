//! The config entry commands (`components/config/config_entries.py`):
//! `config_entries/get`, `config_entries/subscribe`,
//! `config_entries/flow/progress` and `config_entries/flow/subscribe`,
//! which onboarding's integrations step, the integrations page and the Add
//! integration dialog use.
//!
//! Where `hars` differs from HA:
//! - An entry's `state` is its setup's when the setup reports it (an entry
//!   a config flow added). For the others it is `loaded` when their
//!   integration runs in `hars` and `not_loaded` otherwise (or when
//!   disabled): HA says `setup_retry` while met.no can't be reached, for
//!   one.
//! - Every flow is one a user started (`hars` has no discovery or reauth),
//!   and those the two flow commands leave out, as HA does: the progress
//!   list and the flow subscription are empty.
//! - `switchbot_cloud` never gets a webhook id (its module says why), so
//!   the `updated` HA sends for that change of the entry's data is not
//!   sent.

use super::{Conn, Invalid, Subscription, only_keys};
use crate::ApiConfig;
use ha_auth::Permission;
use ha_integrations::config_entries::{Change, Listener};
use ha_integrations::flow;
use serde_json::{Map, Value, json};
use std::sync::Arc;

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
        "config_entries/get" => get(conn, id, msg),
        "config_entries/subscribe" => subscribe(conn, id, msg),
        // `require_admin`: the flows in progress that no user started.
        "config_entries/flow/progress" => only_keys(msg, &[]).map(|()| {
            if conn.principal.permission < Permission::Admin {
                conn.out.error(json!(id), "unauthorized", "Unauthorized");
                return;
            }
            let flows: Vec<Value> = conn
                .shared
                .config
                .flows
                .iter()
                .flat_map(|f| f.progress())
                .filter(|f| ![flow::SOURCE_USER, flow::SOURCE_RECONFIGURE].contains(&f.source))
                .map(|f| {
                    json!({"flow_id": f.flow_id, "handler": f.handler,
                           "context": {"source": f.source}, "step_id": f.step_id})
                })
                .collect();
            conn.out.result(id, Value::Array(flows));
        }),
        "config_entries/flow/subscribe" => only_keys(msg, &[]).map(|()| {
            if conn.principal.permission < Permission::Admin {
                conn.out.error(json!(id), "unauthorized", "Unauthorized");
                return;
            }
            // The flows in progress (none a user didn't start), then the
            // result, as HA sends them.
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

/// `_async_matching_config_entries_json_fragments`.
fn matching(config: &ApiConfig, filter: &[&str], domain: Option<&str>) -> Vec<Value> {
    let entries = config
        .config_entries
        .as_ref()
        .and_then(|e| e.list())
        .unwrap_or_default();
    entries
        .iter()
        .filter(|e| {
            // `if domain:` — an empty one asks for every entry.
            domain
                .is_none_or(|d| d.is_empty() || e.get("domain").and_then(Value::as_str) == Some(d))
        })
        .filter(|e| matches_filter(e, filter))
        .map(|e| entry_json(config, e))
        .collect()
}

/// `config_entries/get`: the entries, of one domain or of some
/// integration types. Anyone signed in may ask, as in HA.
fn get(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(msg, &["type_filter", "domain"])?;
    let filter = type_filter(msg)?;
    let domain = match msg.get("domain") {
        None => None,
        Some(Value::String(d)) => Some(d.as_str()),
        Some(_) => return Err("expected str for dictionary value @ data['domain']".into()),
    };
    let list = matching(&conn.shared.config, &filter, domain);
    conn.out.result(id, Value::Array(list));
    Ok(())
}

/// `config_entries/subscribe`: the result, the entries now, then each
/// change (`SIGNAL_CONFIG_ENTRY_CHANGED`) of an entry the filter lets by.
fn subscribe(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(msg, &["type_filter"])?;
    let filter = type_filter(msg)?;
    let config = conn.shared.config.clone();
    let now = |config: &ApiConfig| -> Value {
        matching(config, &filter, None)
            .into_iter()
            .map(|entry| json!({"type": null, "entry": entry}))
            .collect()
    };
    let Some(entries) = config.config_entries.clone() else {
        conn.out.result(id, Value::Null);
        conn.out.event(id, &now(&config));
        conn.subs.insert(id, Subscription::Idle);
        return Ok(());
    };
    let live = Arc::new(parking_lot::Mutex::new(true));
    let listener: Listener = {
        // Weak: the entries own the listener, and the config owns them.
        let (out, live, config) = (conn.out.clone(), live.clone(), Arc::downgrade(&config));
        let filter: Vec<String> = filter.iter().map(|f| (*f).to_owned()).collect();
        Box::new(move |change: Change, entry: &Map<String, Value>| {
            let (live, Some(config)) = (live.lock(), config.upgrade()) else {
                return false;
            };
            if !*live {
                return false;
            }
            let filter: Vec<&str> = filter.iter().map(String::as_str).collect();
            if !matches_filter(entry, &filter) {
                return true;
            }
            out.event(
                id,
                &json!([{"type": change.as_str(), "entry": entry_json(&config, entry)}]),
            )
        })
    };
    // The result and the entries as they are, queued before any change
    // can be told: no change is missed, and none is sent twice.
    entries.subscribe(live.clone(), listener, || {
        conn.out.result(id, Value::Null);
        conn.out.event(id, &now(&config));
    });
    conn.subs.insert(id, Subscription::Entries(live));
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
pub(crate) fn entry_json(config: &ApiConfig, e: &Map<String, Value>) -> Value {
    let str_of = |k: &str| e.get(k).and_then(Value::as_str);
    let domain = str_of("domain").unwrap_or("");
    let disabled = e.get("disabled_by").is_some_and(|d| !d.is_null());
    let (options, unload) = HANDLERS
        .iter()
        .find(|(d, ..)| *d == domain)
        .map_or((false, false), |&(_, o, u)| (o, u));
    // Where its setup is, if the setup says; else whether it runs.
    let runtime = config
        .config_entries
        .as_ref()
        .zip(str_of("entry_id"))
        .and_then(|(entries, id)| entries.runtime(id));
    let (state, reason, unload) = match runtime {
        // HA learns `supports_unload` when it loads the integration for
        // the entry's setup.
        Some(r) => (r.state.as_str(), r.reason, unload && r.integration_loaded),
        None if !disabled && crate::components(config).contains(&domain) => {
            ("loaded", None, unload)
        }
        None => ("not_loaded", None, unload),
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
        "reason": reason,
        "error_reason_translation_key": null,
        "error_reason_translation_placeholders": null,
        "num_subentries": e.get("subentries").and_then(Value::as_array).map_or(0, Vec::len),
    })
}
