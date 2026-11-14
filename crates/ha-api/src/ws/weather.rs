//! `weather/*` commands (`components/weather/websocket_api.py`): the
//! units a weather entity can show, and its forecasts as they change.

use super::{Conn, Invalid, Subscription, TaskSub, only_keys};
use ha_integrations::weather::{self, Kind, SubscribeError};
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// Handle a weather command; `None` if `ty` isn't one.
pub(super) fn dispatch(
    conn: &mut Conn,
    id: u64,
    ty: &str,
    msg: &Map<String, Value>,
) -> Option<Result<(), Invalid>> {
    Some(match ty {
        "weather/convertible_units" => only_keys(msg, &[]).map(|()| {
            conn.out.result(id, weather::convertible_units());
        }),
        "weather/subscribe_forecast" => subscribe_forecast(conn, id, msg),
        _ => return None,
    })
}

/// `cv.entity_domain("weather")`: exactly one valid weather entity id.
fn weather_entity(msg: &Map<String, Value>) -> Result<String, Invalid> {
    let at = " for dictionary value @ data['entity_id']";
    let value = match msg.get("entity_id") {
        Some(Value::String(s)) => s,
        Some(_) => return Err(format!("expected str{at}")),
        None => return Err("required key not provided @ data['entity_id']".into()),
    };
    let ids: Vec<String> = value
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    for id in &ids {
        if ha_core::EntityId::parse(id).is_err() {
            return Err(format!("Entity ID {id} is an invalid entity ID{at}"));
        }
        if !id.starts_with("weather.") {
            return Err(format!(
                "Entity ID '{id}' does not belong to domain 'weather'{at}"
            ));
        }
    }
    match <[String; 1]>::try_from(ids) {
        Ok([id]) => Ok(id),
        Err(ids) => Err(format!("Expected exactly 1 entity, got {}{at}", ids.len())),
    }
}

/// `weather/subscribe_forecast`: the result, the current forecast as an
/// event, then another after every refresh.
fn subscribe_forecast(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(msg, &["entity_id", "forecast_type"])?;
    let entity_id = weather_entity(msg)?;
    let kind = match msg.get("forecast_type") {
        None => return Err("required key not provided @ data['forecast_type']".into()),
        Some(v) => v.as_str().and_then(Kind::parse).ok_or_else(|| {
            "value must be one of ['daily', 'hourly', 'twice_daily'] for dictionary value @ data['forecast_type']"
                .to_owned()
        })?,
    };
    let mut rx = match conn.shared.config.weather.subscribe(&entity_id, kind) {
        Ok(rx) => rx,
        Err(SubscribeError::NotFound) => {
            conn.out.error(
                json!(id),
                "invalid_entity_id",
                &format!("Weather entity not found: {entity_id}"),
            );
            return Ok(());
        }
        Err(SubscribeError::NotSupported) => {
            conn.out.error(
                json!(id),
                "forecast_not_supported",
                &format!(
                    "The weather entity does not support forecast type: {}",
                    kind.as_str()
                ),
            );
            return Ok(());
        }
    };
    let event = move |f: &weather::Forecasts| json!({"type": kind.as_str(), "forecast": f.get(kind).map(|l| &**l)});
    conn.out.result(id, Value::Null);
    conn.out.event(id, &event(&rx.borrow_and_update()));

    let out = conn.out.clone();
    let live = Arc::new(parking_lot::Mutex::new(true));
    let task_live = live.clone();
    let task = tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            let event = event(&rx.borrow_and_update());
            let live = task_live.lock();
            if !*live || !out.event(id, &event) {
                return;
            }
        }
    });
    conn.subs.insert(
        id,
        Subscription::Task(TaskSub {
            abort: task.abort_handle(),
            live,
        }),
    );
    Ok(())
}
