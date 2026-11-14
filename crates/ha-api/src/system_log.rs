//! `system_log`'s services (`components/system_log`): the frontend reports
//! its own errors through `system_log.write`. `hars` logs them; it keeps no
//! list of recent errors yet (`system_log/list`), so `clear` has nothing to
//! clear.

use ha_core::{Hass, Kind, Schema, SupportsResponse};
use serde_json::Value;

/// Register `system_log.write` and `system_log.clear`.
pub fn register(hass: &Hass) {
    let write = Schema::new()
        .required("message", Kind::String)
        .optional(
            "level",
            Kind::OneOf(&["debug", "info", "warning", "error", "critical"]),
        )
        .optional("logger", Kind::String);
    hass.services().register_with_schema(
        "system_log",
        "write",
        Some(write),
        SupportsResponse::None,
        |call| async move {
            let text = |k: &str| call.data.get(k).and_then(Value::as_str);
            let message = text("message").unwrap_or_default();
            let logger = text("logger").unwrap_or("homeassistant.components.system_log.external");
            match text("level").unwrap_or("error") {
                "debug" => tracing::debug!(logger, "{message}"),
                "info" => tracing::info!(logger, "{message}"),
                "warning" => tracing::warn!(logger, "{message}"),
                // `error` and `critical`.
                _ => tracing::error!(logger, "{message}"),
            }
            Ok(None)
        },
    );
    hass.services().register_with_schema(
        "system_log",
        "clear",
        Some(Schema::new()),
        SupportsResponse::None,
        |_| async { Ok(None) },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn write_checks_its_data_as_has_schema() {
        let hass = Hass::new();
        register(&hass);
        let call = |data: Value| {
            let hass = hass.clone();
            async move {
                let Value::Object(data) = data else { panic!() };
                hass.call_service("system_log", "write", data, None, false, None)
                    .await
            }
        };
        assert_eq!(
            call(json!({"message": "boom", "level": "warning"})).await,
            Ok(None)
        );
        assert_eq!(
            call(json!({"message": "boom", "logger": "frontend.js.latest"})).await,
            Ok(None)
        );
        assert!(
            call(json!({"message": "boom", "level": "loud"}))
                .await
                .is_err()
        );
        assert!(call(json!({"level": "error"})).await.is_err());
        assert!(call(json!({"message": "boom", "extra": 1})).await.is_err());
        let clear = hass
            .call_service("system_log", "clear", Default::default(), None, false, None)
            .await;
        assert_eq!(clear, Ok(None));
    }
}
