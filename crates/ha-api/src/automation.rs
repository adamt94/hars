//! The automation editor's config API (`components/config/automation.py`
//! on `components/config/view.py`), at
//! `/api/config/automation/config/<id>`, for admins:
//!
//! - `GET` reads `automations.yaml` with HA's loader and answers the entry
//!   as loaded, keys in file order.
//! - `POST` saves one: the body is validated as the automation engine
//!   validates it (`Message malformed: …` if it can't run), put in the
//!   file's list in place of the entry with that id or at its end, and the
//!   whole file is written again with HA's YAML writer. Then
//!   `automation.reload` is called for that id; the reply doesn't wait.
//! - `DELETE` takes the entry out of the file, then the automation's
//!   registry row and entity.
//!
//! The file is always `automations.yaml`, whatever `configuration.yaml`
//! includes, and reads and writes of it take turns, as in HA. Checked by
//! `automation/vectors/editor.json`, made by
//! `scripts/vectors/automation_editor.py`.

use axum::{
    Router,
    body::Bytes,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use ha_core::yaml::Yaml;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
struct Editor {
    hass: ha_core::Hass,
    dir: PathBuf,
    /// HA's `mutation_lock`.
    turn: Arc<tokio::sync::Mutex<()>>,
    automations: Option<ha_automation::Automations>,
}

fn message(status: StatusCode, text: &str) -> Response {
    (status, axum::Json(json!({ "message": text }))).into_response()
}

/// Where HA's view raises.
struct Raised;

/// aiohttp's answer to an exception in a view.
fn crashed() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "500 Internal Server Error\n\nServer got itself in trouble",
    )
        .into_response()
}

fn ok() -> Response {
    axum::Json(json!({"result": "ok"})).into_response()
}

/// `read_config`: the file's entries, none if it is missing or holds
/// nothing. `Err` where HA's view raises: a file that doesn't parse, or
/// isn't a list.
async fn read(dir: &std::path::Path) -> Result<Vec<Yaml>, Raised> {
    let file = dir.join("automations.yaml");
    let loaded = tokio::task::spawn_blocking(move || {
        if !file.is_file() {
            return Ok(None);
        }
        ha_core::yaml::load_file(&file, None).map(Some)
    })
    .await;
    match loaded {
        Ok(Ok(Some(Yaml::Seq(items)))) => Ok(items),
        // Missing or empty: no automations.
        Ok(Ok(None | Some(Yaml::Null))) => Ok(Vec::new()),
        Ok(Ok(Some(v))) if !v.is_truthy() => Ok(Vec::new()),
        // HA iterates whatever loaded: anything else raises.
        Ok(Ok(Some(_))) => Err(Raised),
        Ok(Err(e)) => {
            tracing::error!("automations.yaml: {e}");
            Err(Raised)
        }
        Err(_) => Err(Raised),
    }
}

/// `_write`: the list as HA's YAML, written atomically.
async fn write(dir: &std::path::Path, entries: Vec<Yaml>) -> Result<(), Raised> {
    let file = dir.join("automations.yaml");
    let written = tokio::task::spawn_blocking(move || {
        // Dumped first: a failure leaves the file as it was.
        let text = ha_core::yaml::dump(&Yaml::Seq(entries)).map_err(|e| e.to_string())?;
        ha_core::storage::write_atomic(&file, text.as_bytes(), false).map_err(|e| e.to_string())
    })
    .await;
    match written {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            tracing::error!("Saving file failed: automations.yaml: {e}");
            Err(Raised)
        }
        Err(_) => Err(Raised),
    }
}

/// `_get_value`'s test on one entry: whether its `id` is the URL's text
/// (an int id never is). `Err` for an entry that isn't a mapping, where
/// HA raises.
fn has_id(entry: &Yaml, id: &str) -> Result<bool, Raised> {
    match entry {
        Yaml::Map(_) => Ok(entry.get("id").and_then(Yaml::as_str) == Some(id)),
        _ => Err(Raised),
    }
}

async fn get_config(State(editor): State<Editor>, Path(id): Path<String>) -> Response {
    let entries = match read(&editor.dir).await {
        Ok(entries) => entries,
        Err(Raised) => return crashed(),
    };
    // `_get_value`: the first entry with the id.
    for e in &entries {
        match has_id(e, &id) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(Raised) => return crashed(),
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

/// The posted mapping as HA's validators leave it, which is what gets
/// written: `trigger`, `action` and `condition` under their plural names,
/// and each action's `service` as `action`, moved last.
fn migrated(posted: &Yaml) -> Result<Yaml, String> {
    let Yaml::Map(pairs) = posted else {
        return Ok(posted.clone());
    };
    let mut pairs = pairs.clone();
    // `cv.renamed`, in `_backward_compat_schema`'s order: the old key is
    // popped and the new one added last.
    for (old, new) in [
        ("trigger", "triggers"),
        ("action", "actions"),
        ("condition", "conditions"),
    ] {
        let Some(i) = pairs.iter().position(|(k, _)| k.as_str() == Some(old)) else {
            continue;
        };
        if pairs.iter().any(|(k, _)| k.as_str() == Some(new)) {
            return Err(format!(
                "Cannot specify both '{old}' and '{new}'. Please use '{new}' only."
            ));
        }
        let (_, value) = pairs.remove(i);
        pairs.push((Yaml::Str(new.into()), value));
    }
    Ok(ha_automation::migrate_actions(&Yaml::Map(pairs)))
}

/// `EditAutomationConfigView._write_value`'s entry: the URL's id first,
/// then these keys in this order, then the rest as posted (a posted `id`
/// takes the URL's place).
fn ordered(id: &str, posted: Vec<(Yaml, Yaml)>) -> Yaml {
    let mut out = vec![(Yaml::Str("id".into()), Yaml::Str(id.to_owned()))];
    let mut put = |key: &Yaml, value: &Yaml| match out.iter_mut().find(|(k, _)| k == key) {
        Some(pair) => pair.1 = value.clone(),
        None => out.push((key.clone(), value.clone())),
    };
    for key in [
        "alias",
        "description",
        "triggers",
        "trigger",
        "conditions",
        "condition",
        "actions",
        "action",
    ] {
        if let Some((k, v)) = posted.iter().find(|(k, _)| k.as_str() == Some(key)) {
            put(k, v);
        }
    }
    for (k, v) in &posted {
        put(k, v);
    }
    Yaml::Map(out)
}

/// `uuid.uuid4().hex`.
fn uuid4_hex() -> String {
    let random = rand::random::<u128>();
    let versioned = (random & !(0xf << 76)) | (0x4 << 76);
    format!("{:032x}", (versioned & !(0x3 << 62)) | (0x2 << 62))
}

async fn post_config(
    State(editor): State<Editor>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    // `request.json()`.
    let posted = match std::str::from_utf8(&body).map(Yaml::from_json_text) {
        Ok(Ok(posted)) => posted,
        _ => return message(StatusCode::BAD_REQUEST, "Invalid JSON specified"),
    };
    let malformed = |why: &str| {
        message(
            StatusCode::BAD_REQUEST,
            &format!("Message malformed: {why}"),
        )
    };
    match &posted {
        Yaml::Map(_) => {}
        // The schema's answer to what `in` works on but isn't a mapping.
        Yaml::Seq(_) | Yaml::Str(_) => return malformed("expected dictionary"),
        // `"use_blueprint" in 5` raises.
        _ => return crashed(),
    }
    let posted = match migrated(&posted) {
        Ok(posted) => posted,
        Err(why) => return malformed(&why),
    };
    if let Err(why) = ha_automation::validate(&posted.to_json()).await {
        return malformed(&why);
    }
    let Yaml::Map(pairs) = posted else {
        return crashed();
    };
    let updated = ordered(&id, pairs);
    {
        let _turn = editor.turn.lock().await;
        let mut entries = match read(&editor.dir).await {
            Ok(entries) => entries,
            Err(Raised) => return crashed(),
        };
        let mut replaced = false;
        for entry in &mut entries {
            let Yaml::Map(pairs) = entry else {
                return crashed();
            };
            // "When people copy paste their automations to the config
            // file, they sometimes forget to add IDs. Fix it here."
            match pairs.iter().find(|(k, _)| k.as_str() == Some("id")) {
                None => pairs.push((Yaml::Str("id".into()), Yaml::Str(uuid4_hex()))),
                Some((_, Yaml::Str(have))) if *have == id => {
                    *entry = updated.clone();
                    replaced = true;
                }
                Some(_) => {}
            }
        }
        if !replaced {
            entries.push(updated);
        }
        if let Err(Raised) = write(&editor.dir, entries).await {
            return crashed();
        }
    }
    // The hook, as a task: `automation.reload` for this id.
    let hass = editor.hass.clone();
    tokio::spawn(async move {
        let mut data = serde_json::Map::new();
        data.insert("id".into(), json!(id));
        if let Err(e) = hass
            .call_service(ha_automation::DOMAIN, "reload", data, None, false, None)
            .await
        {
            tracing::error!("reloading automation {id} after a save: {e}");
        }
    });
    ok()
}

async fn delete_config(State(editor): State<Editor>, Path(id): Path<String>) -> Response {
    {
        let _turn = editor.turn.lock().await;
        let mut entries = match read(&editor.dir).await {
            Ok(entries) => entries,
            Err(Raised) => return crashed(),
        };
        // `_get_value`, then `_delete_value`: the first entry with the id.
        let mut found = None;
        for (i, entry) in entries.iter().enumerate() {
            match has_id(entry, &id) {
                Ok(true) => {
                    found = Some(i);
                    break;
                }
                Ok(false) => {}
                Err(Raised) => return crashed(),
            }
        }
        let Some(found) = found else {
            return message(StatusCode::BAD_REQUEST, "Resource not found");
        };
        entries.remove(found);
        if let Err(Raised) = write(&editor.dir, entries).await {
            return crashed();
        }
    }
    // The hook, as a task: the registry row goes, and the entity with it.
    if let Some(automations) = editor.automations {
        tokio::spawn(async move { automations.delete(&id).await });
    }
    ok()
}

/// The routes, for the config directory holding `automations.yaml`.
pub(crate) fn routes(
    hass: ha_core::Hass,
    config_dir: PathBuf,
    automations: Option<ha_automation::Automations>,
) -> Router {
    Router::new()
        .route(
            "/api/config/automation/config/{id}",
            get(get_config).post(post_config).delete(delete_config),
        )
        .with_state(Editor {
            hass,
            dir: config_dir,
            turn: Arc::default(),
            automations,
        })
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

    /// Our text for a file HA wrote: the id HA gave an entry without one
    /// is random.
    fn scrub_ids(text: &str) -> String {
        text.lines()
            .map(|line| match line.split_once("id: ") {
                Some((before, id))
                    if id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) =>
                {
                    format!("{before}id: <hex32>\n")
                }
                _ => format!("{line}\n"),
            })
            .collect()
    }

    fn state_json(s: Option<&std::sync::Arc<ha_core::State>>) -> serde_json::Value {
        use serde_json::json;
        let Some(s) = s else {
            return serde_json::Value::Null;
        };
        let mut attributes = s.attributes.clone();
        if attributes
            .get("last_triggered")
            .is_some_and(|t| !t.is_null())
        {
            attributes.insert("last_triggered".into(), json!("<time>"));
        }
        json!({"state": s.state, "attributes": attributes})
    }

    /// What `automations.yaml` holds, as `automation: !include` loads it.
    fn on_disk(dir: &std::path::Path) -> Result<Vec<ha_core::yaml::Yaml>, String> {
        use ha_core::yaml::Yaml;
        let list =
            ha_core::yaml::load_file(&dir.join("automations.yaml"), None).map_err(|e| e.0)?;
        Ok(ha_automation::from_config_yaml(&Yaml::Map(vec![(
            Yaml::Str("automation".into()),
            list,
        )])))
    }

    /// Against `automation/vectors/editor.json`, made by
    /// `scripts/vectors/automation_editor.py` sending the editor's requests
    /// to HA 2026.6.1 over HTTP: each reply (status, content type, body),
    /// `automations.yaml` after it byte for byte, and the automations'
    /// state changes and the registry and reload events that follow, in
    /// order; then the registry rows and states at the end.
    #[tokio::test]
    async fn saves_and_deletes_as_ha() {
        use ha_core::EventKind;
        use serde_json::{Value, json};
        let all: Value =
            serde_json::from_str(include_str!("automation/vectors/editor.json")).unwrap();
        let mut failures = Vec::new();
        for (n, scenario) in all["scenarios"].as_array().unwrap().iter().enumerate() {
            let name = scenario["name"].as_str().unwrap();
            let dir = std::env::temp_dir().join(format!("hars-editor-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join("automations.yaml");
            if let Some(text) = scenario["file"].as_str() {
                std::fs::write(&file, text).unwrap();
            }
            let hass = ha_core::Hass::new();
            let mut bus = hass.bus().subscribe();
            let registry = ha_registry::Registry::in_memory(&hass, Default::default());
            let items = on_disk(&dir).unwrap_or_default();
            let automations =
                ha_automation::Automations::setup_live(&hass, registry.clone(), &items).await;
            {
                let dir = dir.clone();
                automations.set_loader(std::sync::Arc::new(move || {
                    let now = on_disk(&dir);
                    Box::pin(async move { now })
                }));
            }
            hass.start();
            automations.started();
            let auth = crate::test_util::auth(true);
            const C: &str = "http://127.0.0.1:8123/";
            let token = |name: &str, password: &str| {
                let flow = auth.start_login_flow(C, None);
                let user = auth.check_password(name, password).unwrap();
                let code = auth.finish_login_flow(&flow, C, &user, None).unwrap();
                auth.exchange_code(Some(C), Some(&code), None)
                    .unwrap()
                    .access_token
            };
            auth.add_user("user", "pw").unwrap();
            let (admin, user) = (token("adam", "secret"), token("user", "pw"));
            let user_id = auth.validate_access_token(&user, None).unwrap().user_id;
            auth.set_user_groups(&user_id, &[ha_auth::GROUP_ID_USER]);
            let config = crate::ApiConfig {
                config_dir: dir.display().to_string(),
                automations: Some(automations.clone()),
                ..Default::default()
            };
            let router = crate::router(hass.clone(), auth.clone(), &config);
            // What happened since the last call, as the vectors list it.
            let events = |bus: &mut tokio::sync::broadcast::Receiver<
                std::sync::Arc<ha_core::Event>,
            >| {
                let mut out = Vec::new();
                while let Ok(e) = bus.try_recv() {
                    match &e.kind {
                        EventKind::StateChanged {
                            entity_id,
                            old_state,
                            new_state,
                        } if entity_id.as_str().starts_with("automation.") => out.push(json!({
                            "kind": "state", "entity_id": entity_id.as_str(),
                            "old": state_json(old_state.as_ref()),
                            "new": state_json(new_state.as_ref()),
                        })),
                        EventKind::Custom { name, data } if name == "entity_registry_updated" => {
                            out.push(json!({"kind": "registry", "data": data}));
                        }
                        EventKind::Custom { name, data } if name == "automation_reloaded" => {
                            out.push(json!({
                                "kind": "reloaded", "data": data,
                                "user": e.context.user_id.is_some(),
                            }));
                        }
                        _ => {}
                    }
                }
                out
            };
            let mut report = String::new();
            let start = events(&mut bus);
            if json!(start) != scenario["start"] {
                report += &format!(
                    "  start:\n    HA:   {}\n    hars: {}\n",
                    scenario["start"],
                    json!(start)
                );
            }
            for (i, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
                let request = &step["request"];
                let before = std::fs::read_to_string(&file).ok();
                let mut got = json!({});
                if let Some(service) = request["service"].as_str() {
                    let data = request["data"].as_object().cloned().unwrap_or_default();
                    hass.call_service("automation", service, data, None, false, None)
                        .await
                        .unwrap();
                } else {
                    let uri = format!(
                        "/api/config/automation/config/{}",
                        request["id"].as_str().unwrap()
                    );
                    let mut req = Request::builder()
                        .method(request["method"].as_str().unwrap())
                        .uri(uri);
                    match request["user"].as_str() {
                        Some("admin") => {
                            req = req.header("authorization", format!("Bearer {admin}"))
                        }
                        Some(_) => req = req.header("authorization", format!("Bearer {user}")),
                        None => {}
                    }
                    let body = request["sent"].as_str().unwrap_or_default().to_owned();
                    let mut req = req.body(Body::from(body)).unwrap();
                    req.extensions_mut()
                        .insert(ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 1))));
                    let res = router.clone().oneshot(req).await.unwrap();
                    got["status"] = json!(res.status().as_u16());
                    got["content_type"] = json!(
                        res.headers()
                            .get("content-type")
                            .and_then(|v| v.to_str().ok())
                    );
                    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
                        .await
                        .unwrap();
                    got["body"] = json!(String::from_utf8(body.to_vec()).unwrap());
                }
                // The hooks run as tasks: until what HA saw has been seen
                // (or 3 s), then a little more for anything extra.
                let wanted = step["events"].as_array().unwrap();
                let mut seen = Vec::new();
                for _ in 0..300 {
                    seen.extend(events(&mut bus));
                    if seen.len() >= wanted.len() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                seen.extend(events(&mut bus));
                got["events"] = json!(seen);
                let after = std::fs::read_to_string(&file).ok();
                if after != before {
                    got["file"] = json!(after.as_deref().map(scrub_ids));
                }
                let mut want = step.clone();
                want.as_object_mut().unwrap().remove("request");
                if got != want {
                    let differ = |key: &str| got.get(key) != want.get(key);
                    report += &format!("  step {i} {}:\n", request);
                    for key in ["status", "content_type", "body", "events"] {
                        if differ(key) {
                            report += &format!(
                                "    {key}:\n      HA:   {}\n      hars: {}\n",
                                want[key], got[key]
                            );
                        }
                    }
                    if differ("file") {
                        report += &format!(
                            "    file:\n      HA:\n{}\n      hars:\n{}\n",
                            want["file"].as_str().unwrap_or("<unchanged>"),
                            got["file"].as_str().unwrap_or("<unchanged>")
                        );
                    }
                }
            }
            let rows: Vec<Value> = registry
                .snapshot()
                .entities
                .iter()
                .filter(|e| e.platform == "automation")
                .map(|e| {
                    json!({
                        "entity_id": e.entity_id, "unique_id": e.unique_id, "name": e.name,
                        "original_name": e.original_name, "object_id_base": e.object_id_base,
                        "capabilities": e.capabilities.as_ref().map(|c| Value::Object(c.object())),
                        "options": Value::Object(e.options.object()),
                    })
                })
                .collect();
            if json!(rows) != scenario["registry"] {
                report += &format!(
                    "  registry:\n    HA:   {}\n    hars: {}\n",
                    scenario["registry"],
                    json!(rows)
                );
            }
            let states: serde_json::Map<String, Value> = hass
                .states()
                .all()
                .iter()
                .filter(|s| s.entity_id.as_str().starts_with("automation."))
                .map(|s| (s.entity_id.to_string(), state_json(Some(s))))
                .collect();
            if Value::Object(states.clone()) != scenario["final"] {
                report += &format!(
                    "  final:\n    HA:   {}\n    hars: {}\n",
                    scenario["final"],
                    Value::Object(states)
                );
            }
            if !report.is_empty() {
                failures.push(format!("{name}:\n{report}"));
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
        assert!(
            failures.is_empty(),
            "{} of {} scenarios differ from HA:\n{}",
            failures.len(),
            all["scenarios"].as_array().unwrap().len(),
            failures.join("\n")
        );
    }
}
