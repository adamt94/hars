//! `history/history_during_period` and `history/stream`'s query: HA's
//! `get_significant_states` over the segment store, answered as the
//! compressed-state JSON the frontend reads.
//!
//! Per entity (in the order asked, those with no states left out): the
//! state it had at `start` (the newest row before it, stamped `start`, as
//! HA's start-time-state subquery returns it), then the rows with
//! `start < last_updated < end`. With `significant_only`, only rows whose
//! state changed (`last_changed == last_updated`) and every row of HA's
//! significant domains. With `minimal`, all but the first row carry just
//! the state and `lu`, and a row repeating the previous state is skipped
//! (`_sorted_states_to_dict`).

use crate::Store;
use crate::format::{REMOVED, StateRow};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

/// `history/const.py`'s `SIGNIFICANT_DOMAINS`.
pub const SIGNIFICANT_DOMAINS: [&str; 5] = [
    "climate",
    "device_tracker",
    "humidifier",
    "thermostat",
    "water_heater",
];
/// `history/const.py`'s `NEED_ATTRIBUTE_DOMAINS`.
pub const NEED_ATTRIBUTE_DOMAINS: [&str; 5] = [
    "climate",
    "humidifier",
    "input_datetime",
    "thermostat",
    "water_heater",
];

pub struct Query<'a> {
    /// µs since the epoch.
    pub start: i64,
    pub end: Option<i64>,
    pub entity_ids: &'a [String],
    pub include_start_state: bool,
    pub significant_only: bool,
    pub minimal: bool,
    pub no_attributes: bool,
}

pub struct Answer {
    /// `{"entity_id": [{"s": …, "a": …, "lu": …}, …], …}`.
    pub json: String,
    /// The newest `lu` in the answer (µs), if there is any state.
    pub last_updated: Option<i64>,
}

fn domain(entity_id: &str) -> &str {
    entity_id.split_once('.').map_or(entity_id, |(d, _)| d)
}

/// Seconds as Python's float, as JSON: `1768465800.5`, `1768465800.0`.
pub fn seconds(us: i64) -> f64 {
    us as f64 / 1e6
}

/// One row, in the order and fields HA sends it.
struct Item {
    state: Option<String>,
    attributes: String,
    lu: i64,
    /// Only when it differs from `lu` and the query reads it.
    lc: Option<i64>,
}

/// HA's `get_significant_states`. Blocking: reads segment files.
pub fn significant_states(store: &Store, q: &Query<'_>) -> Answer {
    // The entities, once each, in the order asked.
    let mut seen = HashSet::new();
    let order: Vec<&str> = q
        .entity_ids
        .iter()
        .map(String::as_str)
        .filter(|e| seen.insert(*e))
        .collect();
    let mut rows: HashMap<&str, Vec<Item>> = HashMap::new();
    let start_state = q.include_start_state && store.oldest_ts().is_some_and(|t| t < q.start);
    if start_state {
        for row in store.last_before(&order, q.start) {
            let Some(&e) = order.iter().find(|e| **e == &*row.entity_id) else {
                continue;
            };
            rows.entry(e)
                .or_default()
                .push(item(&row, q, Some(q.start)));
        }
    }
    let wanted: HashSet<&str> = order.iter().copied().collect();
    let mut found: Vec<(i64, &str, Item)> = Vec::new();
    store.scan(q.start, q.end.unwrap_or(i64::MAX), |rec| {
        let crate::Rec::State(s) = rec else { return };
        if s.ts <= q.start || q.end.is_some_and(|end| s.ts >= end) {
            return;
        }
        let Some(&e) = wanted.get(s.entity_id) else {
            return;
        };
        if q.significant_only && s.last_changed != s.ts && !SIGNIFICANT_DOMAINS.contains(&domain(e))
        {
            return;
        }
        found.push((s.ts, e, item(&s.to_owned_row(), q, None)));
    });
    // Write order breaks ties (`sort_by_key` is stable).
    found.sort_by_key(|(ts, ..)| *ts);
    for (_, e, it) in found {
        rows.entry(e).or_default().push(it);
    }

    let mut json = String::from("{");
    let mut last = None;
    for e in order {
        let Some(items) = rows.get(e).filter(|r| !r.is_empty()) else {
            continue;
        };
        if json.len() > 1 {
            json.push(',');
        }
        push_str(&mut json, e);
        json.push_str(":[");
        let minimal = q.minimal && !NEED_ATTRIBUTE_DOMAINS.contains(&domain(e));
        let mut prev: Option<&Option<String>> = None;
        let mut written = false;
        for it in items {
            if minimal && prev.is_some_and(|p| *p == it.state) {
                continue;
            }
            if written {
                json.push(',');
            }
            // Under `minimal`, only the first row is a whole state; the
            // rest are the state and its time.
            if !minimal {
                compressed(&mut json, it, true, true);
            } else if prev.is_none() {
                compressed(&mut json, it, !q.no_attributes, true);
            } else {
                compressed(&mut json, it, false, false);
            }
            written = true;
            prev = Some(&it.state);
            last = last.max(Some(it.lu));
        }
        json.push(']');
    }
    json.push('}');
    Answer {
        json,
        last_updated: last,
    }
}

fn item(row: &StateRow, q: &Query<'_>, stamp: Option<i64>) -> Item {
    let lu = stamp.unwrap_or(row.ts);
    // Without `significant_only` the query reads `last_changed`; the
    // start-time state has none.
    let lc = (!q.significant_only && stamp.is_none() && row.last_changed != row.ts)
        .then_some(row.last_changed);
    Item {
        state: (row.flags & REMOVED == 0).then(|| row.state.to_string()),
        attributes: if q.no_attributes {
            String::new()
        } else {
            row.attributes.to_string()
        },
        lu,
        lc,
    }
}

/// `row_to_compressed_state`. `attributes`: with `a`, which is `{}` when
/// the query left the attributes out (a plain row's are then NULL);
/// `changed`: with `lc` where it has one.
fn compressed(out: &mut String, it: &Item, attributes: bool, changed: bool) {
    out.push_str("{\"s\":");
    match &it.state {
        Some(s) => push_str(out, s),
        None => out.push_str("null"),
    }
    if attributes {
        out.push_str(",\"a\":");
        out.push_str(if it.attributes.is_empty() {
            "{}"
        } else {
            &it.attributes
        });
    }
    let _ = write!(out, ",\"lu\":{:?}", seconds(it.lu));
    if let Some(lc) = it.lc.filter(|_| changed) {
        let _ = write!(out, ",\"lc\":{:?}", seconds(lc));
    }
    out.push('}');
}

fn push_str(out: &mut String, s: &str) {
    match serde_json::to_string(s) {
        Ok(j) => out.push_str(&j),
        Err(_) => out.push_str("\"\""),
    }
}

#[cfg(test)]
mod tests;
