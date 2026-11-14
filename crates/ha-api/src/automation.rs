//! The automation editor's config API (`components/config/automation.py`):
//! `GET /api/config/automation/config/<id>` reads `automations.yaml` with
//! HA's loader and answers the entry as loaded, keys in file order. Saving
//! and deleting from the editor aren't supported yet.

use axum::{
    Router,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::json;
use std::path::PathBuf;

fn message(status: StatusCode, text: &str) -> Response {
    (status, axum::Json(json!({ "message": text }))).into_response()
}

async fn get_config(State(dir): State<PathBuf>, Path(id): Path<String>) -> Response {
    let file = dir.join("automations.yaml");
    let loaded = tokio::task::spawn_blocking(move || {
        if !file.is_file() {
            return Ok(None);
        }
        ha_core::yaml::load_file(&file, None).map(Some)
    })
    .await;
    use ha_core::yaml::Yaml;
    // aiohttp's answer to an exception in a view.
    let crashed = || {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "500 Internal Server Error\n\nServer got itself in trouble",
        )
            .into_response()
    };
    let entries = match loaded {
        Ok(Ok(Some(Yaml::Seq(items)))) => items,
        // Missing or empty: no automations.
        Ok(Ok(None | Some(Yaml::Null))) => Vec::new(),
        Ok(Ok(Some(v))) if !v.is_truthy() => Vec::new(),
        // HA iterates whatever loaded: anything else raises.
        Ok(Ok(Some(_))) => return crashed(),
        Ok(Err(e)) => {
            tracing::error!("automations.yaml: {e}");
            return crashed();
        }
        Err(_) => return crashed(),
    };
    // `_get_value`: the first entry whose `id` equals the URL's text (an
    // int id never does); an entry that isn't a mapping raises.
    for e in &entries {
        let Yaml::Map(_) = e else { return crashed() };
        if e.get("id").and_then(Yaml::as_str) != Some(id.as_str()) {
            continue;
        }
        return match e.to_json_text() {
            Some(text) => ([(header::CONTENT_TYPE, "application/json")], text).into_response(),
            // `HomeAssistantView.json` failing to encode.
            None => (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "500: Internal Server Error",
            )
                .into_response(),
        };
    }
    message(StatusCode::NOT_FOUND, "Resource not found")
}

async fn not_supported() -> Response {
    message(
        StatusCode::BAD_REQUEST,
        "Editing automations isn't supported by hars yet",
    )
}

/// The routes, for the config directory holding `automations.yaml`.
pub(crate) fn routes(config_dir: PathBuf) -> Router {
    Router::new()
        .route(
            "/api/config/automation/config/{id}",
            get(get_config).post(not_supported).delete(not_supported),
        )
        .with_state(config_dir)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    const FILE: &str = "- id: kitchen_off\n  alias: Kitchen off\n  description: Night.\n  mode: single\n  triggers:\n  - trigger: time\n    at: \"23:30:00\"\n  actions:\n  - action: light.turn_off\n    target:\n      entity_id:\n      - light.kitchen\n    data:\n      transition: 10\n";

    #[tokio::test]
    async fn serves_the_entry_as_loaded_to_admins() {
        let dir = std::env::temp_dir().join(format!("hars-auto-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("automations.yaml"), FILE).unwrap();
        let auth = crate::test_util::auth(true);
        let token = {
            const C: &str = "http://127.0.0.1:8123/";
            let flow = auth.start_login_flow(C, None);
            let user = auth.check_password("adam", "secret").unwrap();
            let code = auth.finish_login_flow(&flow, C, &user, None).unwrap();
            auth.exchange_code(Some(C), Some(&code), None)
                .unwrap()
                .access_token
        };
        let config = crate::ApiConfig {
            config_dir: dir.display().to_string(),
            ..Default::default()
        };
        let router = crate::router(ha_core::Hass::new(), auth, &config);
        let send = |method: &str, uri: &str, bearer: bool| {
            let mut req = Request::builder().method(method).uri(uri);
            if bearer {
                req = req.header("authorization", format!("Bearer {token}"));
            }
            let mut req = req.body(Body::empty()).unwrap();
            req.extensions_mut()
                .insert(ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 1))));
            let router = router.clone();
            async move {
                let res = router.oneshot(req).await.unwrap();
                let status = res.status();
                let body = axum::body::to_bytes(res.into_body(), 1 << 20)
                    .await
                    .unwrap();
                (status, String::from_utf8(body.to_vec()).unwrap())
            }
        };
        let base = "/api/config/automation/config";
        // HA's JSON of the loaded dict: the file's key order.
        assert_eq!(
            send("GET", &format!("{base}/kitchen_off"), true).await,
            (
                StatusCode::OK,
                r#"{"id":"kitchen_off","alias":"Kitchen off","description":"Night.","mode":"single","triggers":[{"trigger":"time","at":"23:30:00"}],"actions":[{"action":"light.turn_off","target":{"entity_id":["light.kitchen"]},"data":{"transition":10}}]}"#.into()
            )
        );
        assert_eq!(
            send("GET", &format!("{base}/nope"), true).await,
            (
                StatusCode::NOT_FOUND,
                r#"{"message":"Resource not found"}"#.into()
            )
        );
        assert_eq!(
            send("GET", &format!("{base}/kitchen_off"), false).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            send("POST", &format!("{base}/kitchen_off"), true).await.0,
            StatusCode::BAD_REQUEST
        );
        // An int id never matches the URL's text; what HA can't encode, or
        // a file that isn't a list of mappings, is a 500.
        std::fs::write(
            dir.join("automations.yaml"),
            "- id: 123\n- {id: bin, value: !!binary aGk=}\n",
        )
        .unwrap();
        assert_eq!(
            send("GET", &format!("{base}/123"), true).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            send("GET", &format!("{base}/bin"), true).await,
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "500: Internal Server Error".into()
            )
        );
        std::fs::write(dir.join("automations.yaml"), "id: x\n").unwrap();
        assert_eq!(
            send("GET", &format!("{base}/x"), true).await,
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "500 Internal Server Error\n\nServer got itself in trouble".into()
            )
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
