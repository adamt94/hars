//! The recorder's WebSocket commands, present only while the recorder
//! runs: `recorder/info` (`recorder/basic_websocket_api.py`) and the
//! statistics calls the history screens make. hars compiles no long-term
//! statistics (decision D1), so those answer empty, which makes more-info
//! show the history line instead of "No statistics found".

use super::{Conn, Invalid, only_keys};
use jiff::Timestamp;
use serde_json::{Map, Value, json};

/// HA's `MAX_QUEUE_BACKLOG_MIN_VALUE`, which `recorder/info` reports.
const MAX_BACKLOG: u32 = 65_000;

pub(super) fn dispatch(
    conn: &mut Conn,
    id: u64,
    ty: &str,
    msg: &Map<String, Value>,
) -> Option<Result<(), Invalid>> {
    let recorder = conn.shared.config.recorder.clone()?;
    let res = match ty {
        "recorder/info" => only_keys(msg, &[]).map(|()| {
            let info = recorder.info();
            conn.out.result(
                id,
                json!({
                    "backlog": info.backlog,
                    "db_in_default_location": true,
                    "max_backlog": MAX_BACKLOG,
                    "migration_in_progress": false,
                    "migration_is_live": false,
                    "recording": info.recording,
                    "thread_running": info.thread_running,
                }),
            );
        }),
        "recorder/get_statistics_metadata" => only_keys(msg, &["statistic_ids"]).and_then(|()| {
            str_list(msg, "statistic_ids", false)?;
            conn.out.result(id, json!([]));
            Ok(())
        }),
        "recorder/list_statistic_ids" => only_keys(msg, &["statistic_type"]).and_then(|()| {
            one_of(msg, "statistic_type", &["sum", "mean"], false)?;
            conn.out.result(id, json!([]));
            Ok(())
        }),
        "recorder/statistics_during_period" => statistics_during_period(conn, id, msg),
        _ => return None,
    };
    Some(res)
}

/// The schema, then HA's time checks, then `{}`: no statistic has rows.
fn statistics_during_period(
    conn: &mut Conn,
    id: u64,
    msg: &Map<String, Value>,
) -> Result<(), Invalid> {
    only_keys(
        msg,
        &[
            "start_time",
            "end_time",
            "statistic_ids",
            "period",
            "units",
            "types",
        ],
    )?;
    let start = string(msg, "start_time", true)?;
    let end = string(msg, "end_time", false)?;
    if str_list(msg, "statistic_ids", true)?.is_some_and(|n| n == 0) {
        return Err(
            "length of value must be at least 1 for dictionary value @ data['statistic_ids']"
                .into(),
        );
    }
    one_of(
        msg,
        "period",
        &["5minute", "hour", "day", "week", "month", "year"],
        true,
    )?;
    units(msg.get("units"))?;
    if let Some(types) = msg.get("types") {
        let Value::Array(types) = types else {
            return Err("expected list for dictionary value @ data['types']".into());
        };
        const TYPES: [&str; 7] = ["change", "last_reset", "max", "mean", "min", "state", "sum"];
        if let Some(i) = types
            .iter()
            .position(|t| !t.as_str().is_some_and(|t| TYPES.contains(&t)))
        {
            return Err(format!("not a valid value @ data['types'][{i}]"));
        }
    }
    let tz = conn.shared.hass.config().time_zone.clone();
    if start.and_then(|s| parse_datetime(s, &tz)).is_none() {
        conn.out
            .error(json!(id), "invalid_start_time", "Invalid start_time");
        return Ok(());
    }
    if let Some(end) = end.filter(|e| !e.is_empty())
        && parse_datetime(end, &tz).is_none()
    {
        conn.out
            .error(json!(id), "invalid_end_time", "Invalid end_time");
        return Ok(());
    }
    conn.out.result(id, json!({}));
    Ok(())
}

/// HA's `UNIT_SCHEMA`: per unit converter, the units it accepts and
/// voluptuous's message for any other (generated from HA 2026.6.1).
const UNITS: &str = include_str!("statistic_units.json");

fn units(units: Option<&Value>) -> Result<(), Invalid> {
    let Some(units) = units else { return Ok(()) };
    let Value::Object(units) = units else {
        return Err("expected a dictionary for dictionary value @ data['units']".into());
    };
    if units.is_empty() {
        return Ok(());
    }
    // Parsed per call: the command is rare, and nothing stays in memory.
    let table: Map<String, Value> = serde_json::from_str(UNITS).unwrap_or_default();
    for (k, v) in units {
        let Some(allowed) = table.get(k) else {
            return Err(format!("extra keys not allowed @ data['units']['{k}']"));
        };
        let ok = match v {
            Value::Null => allowed["null"] == true,
            Value::String(u) => allowed["units"]
                .as_array()
                .is_some_and(|a| a.iter().any(|x| x == u)),
            _ => false,
        };
        if !ok {
            let msg = allowed["msg"].as_str().unwrap_or("not a valid value");
            return Err(format!("{msg} for dictionary value @ data['units']['{k}']"));
        }
    }
    Ok(())
}

fn string<'a>(
    msg: &'a Map<String, Value>,
    key: &str,
    required: bool,
) -> Result<Option<&'a str>, Invalid> {
    match msg.get(key) {
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(format!("expected str for dictionary value @ data['{key}']")),
        None if required => Err(format!("required key not provided @ data['{key}']")),
        None => Ok(None),
    }
}

/// A `[str]` field: its length.
fn str_list(msg: &Map<String, Value>, key: &str, required: bool) -> Result<Option<usize>, Invalid> {
    match msg.get(key) {
        Some(Value::Array(items)) => match items.iter().position(|v| !v.is_string()) {
            Some(i) => Err(format!("expected str @ data['{key}'][{i}]")),
            None => Ok(Some(items.len())),
        },
        Some(_) => Err(format!(
            "expected list for dictionary value @ data['{key}']"
        )),
        None if required => Err(format!("required key not provided @ data['{key}']")),
        None => Ok(None),
    }
}

/// A `vol.Any` of literal strings.
fn one_of(
    msg: &Map<String, Value>,
    key: &str,
    values: &[&str],
    required: bool,
) -> Result<(), Invalid> {
    match msg.get(key) {
        Some(v) if v.as_str().is_some_and(|s| values.contains(&s)) => Ok(()),
        Some(_) => Err(format!(
            "not a valid value for dictionary value @ data['{key}']"
        )),
        None if required => Err(format!("required key not provided @ data['{key}']")),
        None => Ok(()),
    }
}

/// HA's `dt_util.parse_datetime` then `as_utc`: ISO 8601 with an offset,
/// or without one in the configured time zone; a bare date is midnight.
pub(crate) fn parse_datetime(s: &str, time_zone: &str) -> Option<Timestamp> {
    if let Ok(t) = s.parse::<Timestamp>() {
        return Some(t);
    }
    let tz = jiff::tz::TimeZone::get(time_zone).unwrap_or(jiff::tz::TimeZone::UTC);
    let civil = s.parse::<jiff::civil::DateTime>().ok().or_else(|| {
        s.parse::<jiff::civil::Date>()
            .ok()
            .map(|d| d.at(0, 0, 0, 0))
    })?;
    civil.to_zoned(tz).ok().map(|z| z.timestamp())
}

#[cfg(test)]
mod tests {
    use super::parse_datetime;

    #[test]
    fn datetimes_parse_as_has() {
        let t = |s: &str| parse_datetime(s, "Europe/London").map(|t| t.to_string());
        assert_eq!(
            t("2026-01-15T08:30:00.000Z").as_deref(),
            Some("2026-01-15T08:30:00Z")
        );
        assert_eq!(
            t("2026-07-01T10:00:00+02:00").as_deref(),
            Some("2026-07-01T08:00:00Z")
        );
        // No offset: local time.
        assert_eq!(
            t("2026-07-01T10:00:00").as_deref(),
            Some("2026-07-01T09:00:00Z")
        );
        assert_eq!(t("2026-07-01").as_deref(), Some("2026-06-30T23:00:00Z"));
        assert_eq!(t("yesterday"), None);
    }
}
