//! CORS against HA's own answers: `vectors/cors.json`, made by
//! `scripts/vectors/http.py`, holds what Home Assistant 2026.6.1 (aiohttp_cors
//! 0.8.1) answered to preflights and requests on `/auth/token` and
//! `/auth/revoke`; these send the same requests to hars and compare the
//! status, `Content-Type`, body and every `Access-Control-*`/`Vary` header.

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;

const VECTORS: &str = include_str!("vectors/cors.json");

#[tokio::test]
async fn matches_home_assistant() {
    let router = crate::router(
        ha_core::Hass::new(),
        crate::test_util::auth(true),
        &crate::ApiConfig::default(),
    );
    let steps: Vec<Value> = serde_json::from_str(VECTORS).unwrap();
    assert_eq!(steps.len(), 19);
    for step in &steps {
        let note = step["note"].as_str().unwrap();
        let r = &step["request"];
        let mut req = Request::builder()
            .method(r["method"].as_str().unwrap())
            .uri(r["path"].as_str().unwrap());
        for (k, v) in r["headers"].as_object().into_iter().flatten() {
            req = req.header(k, v.as_str().unwrap());
        }
        let body = r["body"].as_str().unwrap_or_default().to_owned();
        let mut req = req.body(Body::from(body)).unwrap();
        let ip: std::net::SocketAddr = "127.0.0.1:50000".parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(ip));
        let res = router.clone().oneshot(req).await.unwrap();

        let what = format!("{} {}: {note}", r["method"], r["path"]);
        assert_eq!(res.status().as_u16(), step["status"], "{what}");
        let content_type = res
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap());
        assert_eq!(content_type, step["content_type"].as_str(), "{what}");
        let mut cors = serde_json::Map::new();
        for (k, v) in res.headers() {
            if k.as_str().starts_with("access-control-") || k == "vary" {
                cors.insert(k.as_str().into(), v.to_str().unwrap().into());
            }
        }
        assert_eq!(Value::Object(cors), step["headers"], "{what}");
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        match &step["body"] {
            Value::String(s) => assert_eq!(&text, s, "{what}"),
            json => assert_eq!(
                &serde_json::from_str::<Value>(&text).unwrap(),
                json,
                "{what}"
            ),
        }
    }
}
