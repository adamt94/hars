//! `mobile_app/push_notification_channel` and `_confirm`
//! (`mobile_app/websocket_api.py`). `hars` sends no push notifications: a
//! channel is a subscription that never fires (so the app's push provider
//! stays connected instead of retrying), and nothing is ever pending
//! confirmation.

use super::{Conn, Invalid, Subscription};
use ha_integrations::mobile_app::Webhook;
use serde_json::{Map, Value, json};

pub(super) fn dispatch(
    conn: &mut Conn,
    id: u64,
    ty: &str,
    msg: &Map<String, Value>,
) -> Option<Result<(), Invalid>> {
    let res = match ty {
        "mobile_app/push_notification_channel" => channel(conn, id, msg),
        "mobile_app/push_notification_confirm" => confirm(conn, id, msg),
        _ => return None,
    };
    Some(res)
}

/// The schema: `str` fields (strict), `bool` ones, nothing else.
fn schema(msg: &Map<String, Value>, strs: &[&str], bools: &[&str]) -> Result<(), Invalid> {
    let mut errors = Vec::new();
    // voluptuous's path: `data[<repr(key)>]`.
    let at = |k: &str| {
        let mut r = String::new();
        super::py_repr_str(&mut r, k);
        format!("@ data[{r}]")
    };
    for (k, v) in msg {
        let k = k.as_str();
        if matches!(k, "id" | "type") {
            continue;
        }
        if strs.contains(&k) {
            if !v.is_string() {
                errors.push(format!("expected str for dictionary value {}", at(k)));
            }
        } else if bools.contains(&k) {
            if !v.is_boolean() {
                errors.push(format!("expected bool for dictionary value {}", at(k)));
            }
        } else {
            errors.push(format!("extra keys not allowed {}", at(k)));
        }
    }
    for k in strs {
        if !msg.contains_key(*k) {
            errors.push(format!("required key not provided {}", at(k)));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// `_ensure_webhook_access`: the registration exists and belongs to the
/// connection's user. `false` once the error is sent.
fn access(conn: &Conn, id: u64, webhook_id: &str) -> bool {
    let hook = match &conn.shared.config.mobile_app {
        Some(app) => app.webhook(webhook_id),
        None => Webhook::Unknown,
    };
    let Webhook::Registered(reg) = hook else {
        conn.out
            .error(json!(id), "not_found", "Webhook ID not found");
        return false;
    };
    if reg.user_id().as_deref() != Some(conn.user.id.as_str()) {
        conn.out.error(
            json!(id),
            "unauthorized",
            "User not linked to this webhook ID",
        );
        return false;
    }
    true
}

fn channel(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    schema(msg, &["webhook_id"], &["support_confirm"])?;
    let webhook_id = msg["webhook_id"].as_str().unwrap_or_default();
    if !access(conn, id, webhook_id) {
        return Ok(());
    }
    conn.subs.insert(id, Subscription::Idle);
    conn.out.result(id, Value::Null);
    Ok(())
}

fn confirm(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    schema(msg, &["webhook_id", "confirm_id"], &[])?;
    let webhook_id = msg["webhook_id"].as_str().unwrap_or_default();
    if !access(conn, id, webhook_id) {
        return Ok(());
    }
    // No channel ever has a notification waiting for confirmation.
    conn.out.error(
        json!(id),
        "not_found",
        "Push notification channel not found",
    );
    Ok(())
}
