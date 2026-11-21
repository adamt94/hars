use super::*;
use crate::Endpoints;
use crate::config_entries::ConfigEntries;
use crate::flow::{FlowResult, Manager as FlowManager, SOURCE_USER};
use crate::light::Profiles;
use ha_core::{Context, Hass};
use ha_registry::{Registries, Registry};
use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("hars-plugin-test-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A real out-of-process mock plugin written in Python 3 (zero third-party
/// deps, stdlib only) that speaks NDJSON JSON-RPC 2.0 over stdin/stdout.
const MOCK_PLUGIN_PY: &str = r#"#!/usr/bin/env python3
import json, sys

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    params = msg.get("params") or {}

    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocol_version": 1, "plugin_version": "0.1.0"}})
    elif method == "flow.step":
        inp = params.get("input")
        if inp is None:
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "type": "form",
                "step_id": "user",
                "schema": [
                    {"name": "host", "required": True, "kind": {"type": "string"}},
                    {"name": "name", "required": False, "kind": {"type": "string"}, "default": "Desk Plug"},
                ],
                "errors": {},
            }})
        elif inp.get("host") == "bad":
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "type": "form",
                "step_id": "user",
                "schema": [
                    {"name": "host", "required": True, "kind": {"type": "string"}},
                    {"name": "name", "required": False, "kind": {"type": "string"}, "default": "Desk Plug"},
                ],
                "errors": {"base": "cannot_connect"},
            }})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "type": "create_entry",
                "title": inp.get("name", "Desk Plug"),
                "unique_id": "plug-" + inp["host"],
                "data": {"host": inp["host"]},
            }})
    elif method == "flow.cancel":
        send({"jsonrpc": "2.0", "id": mid, "result": {"ok": True}})
    elif method == "entry.setup":
        eid = params["entry_id"]
        send({"jsonrpc": "2.0", "method": "device.upsert", "params": {
            "entry_id": eid,
            "identifier": "dev-1",
            "name": params["title"],
            "manufacturer": "DemoCo",
            "model": "Plug P1",
        }})
        send({"jsonrpc": "2.0", "method": "entity.upsert", "params": {
            "entry_id": eid,
            "unique_id": "plug-sw",
            "domain": "switch",
            "device_identifier": "dev-1",
            "name": "Power",
        }})
        send({"jsonrpc": "2.0", "method": "entity.upsert", "params": {
            "entry_id": eid,
            "unique_id": "plug-light",
            "domain": "light",
            "device_identifier": "dev-1",
            "name": "Nightlight",
            "supported_color_modes": ["brightness"],
        }})
        send({"jsonrpc": "2.0", "method": "entity.upsert", "params": {
            "entry_id": eid,
            "unique_id": "plug-btn",
            "domain": "button",
            "device_identifier": "dev-1",
            "name": "Identify",
        }})
        send({"jsonrpc": "2.0", "method": "entity.upsert", "params": {
            "entry_id": eid,
            "unique_id": "plug-watts",
            "domain": "sensor",
            "device_identifier": "dev-1",
            "name": "Watts",
            "unit_of_measurement": "W",
        }})
        # Attempt an undeclared entity domain (must be rejected by hars).
        send({"jsonrpc": "2.0", "id": "denied-1", "method": "entity.upsert", "params": {
            "entry_id": eid,
            "unique_id": "forbidden-scene",
            "domain": "scene",
            "name": "Forbidden",
        }})
        send({"jsonrpc": "2.0", "method": "state.set", "params": {
            "entry_id": eid, "unique_id": "plug-sw", "state": "off", "attributes": {}
        }})
        send({"jsonrpc": "2.0", "method": "state.set", "params": {
            "entry_id": eid, "unique_id": "plug-light", "state": "off", "attributes": {}
        }})
        send({"jsonrpc": "2.0", "method": "state.set", "params": {
            "entry_id": eid, "unique_id": "plug-watts", "state": "12.5", "attributes": {}
        }})
        send({"jsonrpc": "2.0", "method": "entry.status", "params": {
            "entry_id": eid, "state": "loaded"
        }})
        send({"jsonrpc": "2.0", "id": mid, "result": {"ok": True}})
    elif method == "entity.call":
        eid = params["entry_id"]
        uid = params["unique_id"]
        svc = params["service"]
        data = params.get("data") or {}
        if uid == "plug-sw":
            state = "on" if svc == "turn_on" else "off"
            send({"jsonrpc": "2.0", "method": "state.set", "params": {
                "entry_id": eid, "unique_id": uid, "state": state, "attributes": {}
            }})
        elif uid == "plug-light":
            if svc == "turn_on":
                attrs = {"color_mode": "brightness", "brightness": data.get("brightness", 255)}
                send({"jsonrpc": "2.0", "method": "state.set", "params": {
                    "entry_id": eid, "unique_id": uid, "state": "on", "attributes": attrs
                }})
            else:
                send({"jsonrpc": "2.0", "method": "state.set", "params": {
                    "entry_id": eid, "unique_id": uid, "state": "off", "attributes": {}
                }})
        elif uid == "plug-btn":
            send({"jsonrpc": "2.0", "method": "state.set", "params": {
                "entry_id": eid, "unique_id": "plug-watts", "state": "99.0", "attributes": {}
            }})
        send({"jsonrpc": "2.0", "id": mid, "result": {"ok": True}})
    elif method == "entry.unload":
        send({"jsonrpc": "2.0", "id": mid, "result": {"ok": True}})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": mid, "result": {"ok": True}})
        break
"#;

fn write_mock_plugin(plugins_dir: &Path, domain: &str) -> PathBuf {
    let dir = plugins_dir.join(domain);
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::create_dir_all(dir.join("translations")).unwrap();
    let manifest = PluginManifest {
        schema_version: 1,
        domain: domain.into(),
        name: "Demo Plug".into(),
        version: "0.1.0".into(),
        iot_class: "local_push".into(),
        config_flow: true,
        documentation: None,
        description: Some("Demo smart plug plugin".into()),
        transport: Transport::Stdio {
            command: "bin/plugin.py".into(),
            args: vec![],
        },
        capabilities: Capabilities {
            entity_domains: vec![
                "switch".into(),
                "light".into(),
                "button".into(),
                "sensor".into(),
            ],
            config_entry_version: 1,
        },
    };
    std::fs::write(
        dir.join("plugin.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let bin = dir.join("bin/plugin.py");
    std::fs::write(&bin, MOCK_PLUGIN_PY).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(
        dir.join("translations/en.json"),
        json!({
            "config": {
                "step": { "user": { "title": "Add Demo Plug", "data": { "host": "Host" } } },
                "error": { "cannot_connect": "Could not reach plug" }
            }
        })
        .to_string(),
    )
    .unwrap();
    dir
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_plugin_full_lifecycle_flow_entities_services_and_removal() {
    let dir = temp_dir("stdio-lifecycle");
    write_mock_plugin(&dir.join("plugins"), "demo_plug");

    let hass = Hass::new();
    let registry = Registry::new(&hass, &dir, Registries::default());
    let entries = ConfigEntries::load(&hass, &dir);
    let components = Components::new(&hass, registry.clone(), Profiles::default());
    let plugins = PluginManager::load(&dir);
    assert_eq!(plugins.list().len(), 1);
    assert_eq!(plugins.list()[0].domain, "demo_plug");

    let flows = FlowManager::new(
        &hass,
        entries.clone(),
        components.clone(),
        Endpoints::default(),
        Vec::new(),
        None,
    )
    .unwrap();
    flows.set_plugins(plugins.clone());

    // 1. Start config flow -> form shown with host + name fields.
    let FlowResult::Form {
        flow_id,
        handler,
        step_id,
        schema,
        ..
    } = flows.init("demo_plug", SOURCE_USER).await.unwrap()
    else {
        panic!("expected form");
    };
    assert_eq!(handler, "demo_plug");
    assert_eq!(step_id, "user");
    assert_eq!(schema.unwrap().len(), 2);

    // 2. Bad host returns form error.
    let FlowResult::Form { errors, .. } = flows
        .configure(&flow_id, Some(vec![("host".into(), json!("bad"))]))
        .await
        .unwrap()
    else {
        panic!("expected form error");
    };
    assert_eq!(errors.unwrap()[0].1, "cannot_connect");

    // 3. Valid host creates entry, spawns plugin, registers device + entities.
    let FlowResult::CreateEntry { entry, runtime, .. } = flows
        .configure(
            &flow_id,
            Some(vec![
                ("host".into(), json!("192.0.2.10")),
                ("name".into(), json!("Office Plug")),
            ]),
        )
        .await
        .unwrap()
    else {
        panic!("expected create_entry");
    };
    let entry_id = entry["entry_id"].as_str().unwrap().to_owned();
    assert_eq!(runtime.unwrap().state.as_str(), "loaded");

    // Wait briefly for the plugin's initial state notifications to settle.
    let mut sw_id = None;
    let mut light_id = None;
    let mut btn_id = None;
    let mut sensor_id = None;
    for _ in 0..40 {
        let snap = registry.snapshot();
        for e in &snap.entities {
            match e.unique_id.as_ref().and_then(Value::as_str).unwrap_or("") {
                "plug-sw" => sw_id = Some(e.entity_id.clone()),
                "plug-light" => light_id = Some(e.entity_id.clone()),
                "plug-btn" => btn_id = Some(e.entity_id.clone()),
                "plug-watts" => sensor_id = Some(e.entity_id.clone()),
                other => panic!("unexpected entity registered: {other}"),
            }
        }
        if sw_id.is_some() && light_id.is_some() && btn_id.is_some() && sensor_id.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let sw_id = sw_id.expect("switch entity");
    let light_id = light_id.expect("light entity");
    let btn_id = btn_id.expect("button entity");
    let sensor_id = sensor_id.expect("sensor entity");

    // Undeclared `scene` entity must NOT exist.
    assert!(
        !registry
            .snapshot()
            .entities
            .iter()
            .any(|e| e.entity_id.starts_with("scene."))
    );
    // Device was created with manufacturer & model.
    assert!(
        registry
            .snapshot()
            .devices
            .iter()
            .any(|d| d.manufacturer.as_deref() == Some("DemoCo")
                && d.model.as_deref() == Some("Plug P1"))
    );

    // 4. Call `switch.turn_on` -> plugin receives `entity.call` and updates state to `on`.
    let mut data = Map::new();
    data.insert("entity_id".into(), json!(sw_id));
    hass.call_service("switch", "turn_on", data, None, false, Some(Context::new()))
        .await
        .unwrap();
    for _ in 0..40 {
        if hass
            .states()
            .get(&ha_core::EntityId::parse(&sw_id).unwrap())
            .is_some_and(|s| s.state.as_str() == "on")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        hass.states()
            .get(&ha_core::EntityId::parse(&sw_id).unwrap())
            .unwrap()
            .state
            .as_str(),
        "on"
    );

    // 5. Call `light.turn_on` with brightness -> plugin receives brightness=128.
    let mut data = Map::new();
    data.insert("entity_id".into(), json!(light_id));
    data.insert("brightness".into(), json!(128));
    hass.call_service("light", "turn_on", data, None, false, Some(Context::new()))
        .await
        .unwrap();
    for _ in 0..40 {
        if hass
            .states()
            .get(&ha_core::EntityId::parse(&light_id).unwrap())
            .is_some_and(|s| s.state.as_str() == "on")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let light_state = hass
        .states()
        .get(&ha_core::EntityId::parse(&light_id).unwrap())
        .unwrap();
    assert_eq!(light_state.state.as_str(), "on");
    assert_eq!(light_state.attributes["brightness"], json!(128));

    // 6. Call `button.press` -> plugin handles it and bumps sensor watts to 99.0.
    let mut data = Map::new();
    data.insert("entity_id".into(), json!(btn_id));
    hass.call_service("button", "press", data, None, false, Some(Context::new()))
        .await
        .unwrap();
    for _ in 0..40 {
        if hass
            .states()
            .get(&ha_core::EntityId::parse(&sensor_id).unwrap())
            .is_some_and(|s| s.state.as_str() == "99.0")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        hass.states()
            .get(&ha_core::EntityId::parse(&sensor_id).unwrap())
            .unwrap()
            .state
            .as_str(),
        "99.0"
    );

    // 7. Duplicate unique_id aborts with `already_configured`.
    let FlowResult::Form { flow_id: f2, .. } = flows.init("demo_plug", SOURCE_USER).await.unwrap()
    else {
        panic!("expected form");
    };
    let FlowResult::Abort { reason, .. } = flows
        .configure(&f2, Some(vec![("host".into(), json!("192.0.2.10"))]))
        .await
        .unwrap()
    else {
        panic!("expected already_configured abort");
    };
    assert_eq!(reason, "already_configured");

    // 8. Removing the entry unloads without requiring restart and clears states.
    let require_restart = flows.remove_entry(&entry_id).await.unwrap();
    assert!(!require_restart);
    assert!(
        hass.states()
            .get(&ha_core::EntityId::parse(&sw_id).unwrap())
            .is_none()
    );

    plugins.shutdown_all().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn unix_socket_sidecar_plugin_works_identically() {
    // Keep path short for macOS 104-byte `SUN_LEN`.
    let n = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(format!("/tmp/hars-ps-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let plugins_dir = dir.join("plugins");
    let run_dir = plugins_dir.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let plugin_dir = plugins_dir.join("sidecar_demo");
    std::fs::create_dir_all(&plugin_dir).unwrap();

    let manifest = PluginManifest {
        schema_version: 1,
        domain: "sidecar_demo".into(),
        name: "Sidecar Demo".into(),
        version: "1.0.0".into(),
        iot_class: "local_push".into(),
        config_flow: true,
        documentation: None,
        description: None,
        transport: Transport::UnixSocket { path: None },
        capabilities: Capabilities {
            entity_domains: vec!["sensor".into()],
            config_entry_version: 1,
        },
    };
    std::fs::write(
        plugin_dir.join("plugin.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();

    // Start a mock sidecar server listening on `<config>/plugins/run/sidecar_demo.sock`.
    let sock_path = run_dir.join("sidecar_demo.sock");
    let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
    let sidecar = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut lines = BufReader::new(r).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let msg: Value = serde_json::from_str(&line).unwrap();
            let id = msg["id"].clone();
            match msg["method"].as_str().unwrap_or("") {
                "initialize" => {
                    let resp =
                        json!({"jsonrpc": "2.0", "id": id, "result": {"protocol_version": 1}});
                    w.write_all(format!("{resp}\n").as_bytes()).await.unwrap();
                }
                "entry.setup" => {
                    let eid = msg["params"]["entry_id"].as_str().unwrap();
                    let reg = json!({"jsonrpc": "2.0", "method": "entity.upsert", "params": {
                        "entry_id": eid, "unique_id": "temp-1", "domain": "sensor", "name": "Temp"
                    }});
                    let st = json!({"jsonrpc": "2.0", "method": "state.set", "params": {
                        "entry_id": eid, "unique_id": "temp-1", "state": "21.4", "attributes": {}
                    }});
                    let ok = json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}});
                    w.write_all(format!("{reg}\n{st}\n{ok}\n").as_bytes())
                        .await
                        .unwrap();
                }
                "shutdown" => {
                    let ok = json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}});
                    w.write_all(format!("{ok}\n").as_bytes()).await.unwrap();
                    break;
                }
                _ => {
                    let ok = json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}});
                    w.write_all(format!("{ok}\n").as_bytes()).await.unwrap();
                }
            }
        }
    });

    let hass = Hass::new();
    let registry = Registry::new(&hass, &dir, Registries::default());
    let entries = ConfigEntries::load(&hass, &dir);
    let entry = entries.new_entry("sidecar_demo", SOURCE_USER, "Sidecar", None, Map::new());
    entries.add(entry.clone());
    let components = Components::new(&hass, registry.clone(), Profiles::default());
    let plugins = PluginManager::load(&dir);

    let mut tasks = Vec::new();
    let started = crate::start_plugin_entries(
        &plugins,
        &components,
        entries.list().as_deref(),
        Some(&entries),
        &mut tasks,
    );
    assert_eq!(started, vec!["sidecar_demo"]);

    for _ in 0..40 {
        if hass
            .states()
            .get(&ha_core::EntityId::parse("sensor.temp").unwrap())
            .is_some_and(|s| s.state.as_str() == "21.4")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        hass.states()
            .get(&ha_core::EntityId::parse("sensor.temp").unwrap())
            .unwrap()
            .state
            .as_str(),
        "21.4"
    );

    plugins.shutdown_all().await;
    for t in tasks {
        t.task.abort();
    }
    let _ = sidecar.await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn store_flow_installs_from_http_catalog_verifies_sha256_and_removes() {
    let dir = temp_dir("store-flow");
    let target = store::current_target();
    let bin_bytes = MOCK_PLUGIN_PY.as_bytes().to_vec();
    let good_sha = store::sha256_hex(&bin_bytes);

    let manifest = PluginManifest {
        schema_version: 1,
        domain: "cat_plug".into(),
        name: "Catalog Plug".into(),
        version: "0.2.0".into(),
        iot_class: "local_push".into(),
        config_flow: true,
        documentation: None,
        description: Some("Installed from catalog".into()),
        transport: Transport::Stdio {
            command: "bin/cat_plug".into(),
            args: vec![],
        },
        capabilities: Capabilities {
            entity_domains: vec!["switch".into()],
            config_entry_version: 1,
        },
    };

    // Start local HTTP server serving catalog + binary + bad-checksum bundle.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());

    let catalog_json = serde_json::to_vec(&Catalog {
        schema_version: 1,
        plugins: vec![CatalogPlugin {
            domain: "cat_plug".into(),
            name: "Catalog Plug".into(),
            description: "Test plug".into(),
            version: "0.2.0".into(),
            artifacts: [(
                target.clone(),
                store::Artifact {
                    url: format!("{base}/cat_plug.bin"),
                    sha256: good_sha,
                },
            )]
            .into_iter()
            .collect(),
            manifest: Some(manifest.clone()),
            manifest_url: None,
            translations_en_url: Some(format!("{base}/en.json")),
            icons_url: None,
            icon_png_url: Some(format!("{base}/icon.png")),
        }],
    })
    .unwrap();
    let bad_bundle_json = serde_json::to_vec(&CatalogPlugin {
        domain: "cat_plug".into(),
        name: "Bad Plug".into(),
        description: String::new(),
        version: "0.2.0".into(),
        artifacts: [(
            target.clone(),
            store::Artifact {
                url: format!("{base}/cat_plug.bin"),
                sha256: "0".repeat(64),
            },
        )]
        .into_iter()
        .collect(),
        manifest: Some(manifest),
        manifest_url: None,
        translations_en_url: None,
        icons_url: None,
        icon_png_url: None,
    })
    .unwrap();
    let en_json = br#"{"config":{"step":{"user":{"title":"Catalog Setup"}}}}"#.to_vec();
    let icon_png = b"FAKEPNG".to_vec();

    let server = tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                break;
            };
            let (cat, bad, bin, en, png) = (
                catalog_json.clone(),
                bad_bundle_json.clone(),
                bin_bytes.clone(),
                en_json.clone(),
                icon_png.clone(),
            );
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split(' ').nth(1).unwrap_or("");
                let (status, body) = match path {
                    "/plugins.json" => ("200 OK", cat),
                    "/bad.json" => ("200 OK", bad),
                    "/cat_plug.bin" => ("200 OK", bin),
                    "/en.json" => ("200 OK", en),
                    "/icon.png" => ("200 OK", png),
                    _ => ("404 Not Found", Vec::new()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes()).await;
                let _ = s.write_all(&body).await;
            });
        }
    });

    let hass = Hass::new();
    let registry = Registry::new(&hass, &dir, Registries::default());
    let entries = ConfigEntries::load(&hass, &dir);
    let components = Components::new(&hass, registry, Profiles::default());
    let plugins = PluginManager::load(&dir);
    plugins.set_catalog_url(format!("{base}/plugins.json"));

    // Plain `http://` and loopback URLs are refused by default.
    let err = plugins
        .install_from_url(&format!("{base}/bad.json"))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidUrl(_)));
    assert!(store::validate_remote_url("https://127.0.0.1/x.json", false).is_err());
    assert!(store::validate_remote_url("https://localhost/x.json", false).is_err());
    assert!(store::validate_remote_url("https://192.168.1.1/x.json", false).is_err());
    assert!(store::validate_remote_url("https://example.com/x.json", false).is_ok());

    // Enable local loopback HTTP for the rest of this loopback test.
    plugins.set_allow_insecure_http(true);

    // Checksum mismatch is rejected before touching disk.
    let err = plugins
        .install_from_url(&format!("{base}/bad.json"))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::ChecksumMismatch { .. }));
    assert!(plugins.get("cat_plug").is_none());

    // Now drive the in-UI `hars_plugins` config flow!
    let flows = FlowManager::new(
        &hass,
        entries,
        components,
        Endpoints::default(),
        Vec::new(),
        None,
    )
    .unwrap();
    flows.set_plugins(plugins.clone());

    let FlowResult::Form { flow_id, .. } = flows.init(STORE_DOMAIN, SOURCE_USER).await.unwrap()
    else {
        panic!("expected store user form");
    };
    let FlowResult::Form { step_id, .. } = flows
        .configure(
            &flow_id,
            Some(vec![("action".into(), json!("install_catalog"))]),
        )
        .await
        .unwrap()
    else {
        panic!("expected catalog pick form");
    };
    assert_eq!(step_id, "install_catalog");

    let FlowResult::Abort { reason, .. } = flows
        .configure(&flow_id, Some(vec![("plugin".into(), json!("cat_plug"))]))
        .await
        .unwrap()
    else {
        panic!("expected plugin_installed abort");
    };
    assert_eq!(reason, "plugin_installed");

    // Plugin is immediately loaded live without restart!
    let installed = plugins.get("cat_plug").expect("plugin hot-loaded");
    assert_eq!(installed.manifest.version, "0.2.0");
    assert_eq!(
        installed.read_brand_image("icon.png").as_deref(),
        Some(b"FAKEPNG".as_slice())
    );
    assert!(
        installed
            .translations_for("config")
            .unwrap()
            .contains("Catalog Setup")
    );

    // Remove it via the UI flow too.
    let FlowResult::Form { flow_id, .. } = flows.init(STORE_DOMAIN, SOURCE_USER).await.unwrap()
    else {
        panic!("expected store user form");
    };
    let FlowResult::Form { step_id, .. } = flows
        .configure(&flow_id, Some(vec![("action".into(), json!("remove"))]))
        .await
        .unwrap()
    else {
        panic!("expected remove form");
    };
    assert_eq!(step_id, "remove");
    let FlowResult::Abort { reason, .. } = flows
        .configure(&flow_id, Some(vec![("plugin".into(), json!("cat_plug"))]))
        .await
        .unwrap()
    else {
        panic!("expected plugin_removed abort");
    };
    assert_eq!(reason, "plugin_removed");
    assert!(plugins.get("cat_plug").is_none());

    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn local_directory_install_and_remove() {
    let src_root = temp_dir("local-src");
    let dst_root = temp_dir("local-dst");
    let src = write_mock_plugin(&src_root, "my_local");

    let installed = store::install_from_dir(&src, &dst_root.join("plugins")).unwrap();
    assert_eq!(installed.domain(), "my_local");
    assert!(dst_root.join("plugins/my_local/plugin.json").is_file());

    let manager = PluginManager::load(&dst_root);
    assert_eq!(manager.list().len(), 1);

    // A broken local upgrade with a missing executable is rejected AND leaves
    // the existing working installation intact on disk.
    let broken_src = temp_dir("broken-local");
    let broken_plug = broken_src.join("my_local");
    std::fs::create_dir_all(&broken_plug).unwrap();
    std::fs::write(
        broken_plug.join("plugin.json"),
        json!({
            "schema_version": 1,
            "domain": "my_local",
            "name": "Broken",
            "version": "0.2.0",
            "transport": { "type": "stdio", "command": "bin/missing" },
            "capabilities": { "entity_domains": ["switch"] }
        })
        .to_string(),
    )
    .unwrap();
    assert!(store::install_from_dir(&broken_plug, &dst_root.join("plugins")).is_err());
    let still_working = InstalledPlugin::load(&dst_root.join("plugins/my_local")).unwrap();
    assert_eq!(still_working.manifest.version, "0.1.0");
    assert!(dst_root.join("plugins/my_local/bin/plugin.py").is_file());

    assert!(store::remove_from_dir(&dst_root.join("plugins"), "my_local").unwrap());
    assert!(!store::remove_from_dir(&dst_root.join("plugins"), "my_local").unwrap());

    // `run` is reserved and can never be deleted by `remove_from_dir`.
    let run_dir = dst_root.join("plugins/run");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("keep.sock"), b"x").unwrap();
    assert!(store::remove_from_dir(&dst_root.join("plugins"), "run").is_err());
    assert!(run_dir.join("keep.sock").is_file());

    let _ = std::fs::remove_dir_all(&broken_src);
    let _ = std::fs::remove_dir_all(&src_root);
    let _ = std::fs::remove_dir_all(&dst_root);
}

#[tokio::test]
async fn entity_store_guards_foreign_devices_duplicates_and_attribute_growth() {
    let dir = temp_dir("entity-guards");
    let plugin_dir = write_mock_plugin(&dir.join("plugins"), "guard_plug");
    let installed = Arc::new(InstalledPlugin::load(&plugin_dir).unwrap());

    let hass = Hass::new();
    let registry = Registry::new(&hass, &dir, Registries::default());
    // Seed a Hue device owning MAC aa:bb:cc:dd:ee:ff.
    let hue_dev = registry.get_or_create_device(ha_registry::DeviceInfo {
        config_entry_id: "hue-entry".into(),
        identifiers: vec![json!(["hue", "bridge-1"])],
        connections: vec![json!(["mac", "aa:bb:cc:dd:ee:ff"])],
        entry_type: None,
        manufacturer: Some("Signify".into()),
        model: Some("BSB002".into()),
        model_id: None,
        name: Some("Hue Bridge".into()),
        sw_version: None,
        via_device: None,
        suggested_area: None,
    });

    let components = Components::new(&hass, registry.clone(), Profiles::default());
    let call_fn: entities::CallPluginFn = Arc::new(|_| Box::pin(async { Ok(()) }));
    let store = entities::PluginEntityStore::new(installed, components, call_fn);
    store.activate_entry("e1");
    store.activate_entry("e2");

    // 1. Even if a plugin passes a MAC connection owned by Hue, `upsert_device`
    //    matches exclusively on namespaced identifiers and never attaches to
    //    or modifies Hue's device row.
    let plugin_dev_id = store
        .upsert_device(protocol::DeviceUpsertParams {
            entry_id: "e1".into(),
            identifier: "spoof".into(),
            connections: vec![("mac".into(), "AA-BB-CC-DD-EE-FF".into())],
            name: Some("Plugin Device".into()),
            manufacturer: Some("OtherCo".into()),
            model: None,
            model_id: None,
            sw_version: None,
            suggested_area: None,
        })
        .unwrap();
    assert_ne!(plugin_dev_id, hue_dev.id);
    let hue_after = registry
        .snapshot()
        .devices
        .iter()
        .find(|d| d.id == hue_dev.id)
        .unwrap()
        .clone();
    assert_eq!(hue_after.name.as_deref(), Some("Hue Bridge"));
    assert_eq!(hue_after.config_entries, vec!["hue-entry".to_owned()]);

    // 2. Duplicate (domain, unique_id) across two entries is rejected.
    let id1 = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "dup-uid".into(),
            domain: "sensor".into(),
            device_identifier: None,
            name: Some("Meter".into()),
            object_id_base: None,
            original_icon: None,
            device_class: None,
            unit_of_measurement: Some("W".into()),
            entity_category: None,
            supported_features: 0,
            supported_color_modes: vec![],
            effect_list: None,
            min_color_temp_kelvin: None,
            max_color_temp_kelvin: None,
        })
        .unwrap();
    let dup = store.upsert_entity(protocol::EntityUpsertParams {
        entry_id: "e2".into(),
        unique_id: "dup-uid".into(),
        domain: "sensor".into(),
        device_identifier: None,
        name: Some("Meter 2".into()),
        object_id_base: None,
        original_icon: None,
        device_class: None,
        unit_of_measurement: None,
        entity_category: None,
        supported_features: 0,
        supported_color_modes: vec![],
        effect_list: None,
        min_color_temp_kelvin: None,
        max_color_temp_kelvin: None,
    });
    assert!(dup.is_err());

    // 3. State attributes replace per frame instead of accumulating old keys.
    let mut a1 = Map::new();
    a1.insert("ephemeral_a".into(), json!(1));
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "dup-uid".into(),
            state: "10".into(),
            attributes: a1,
        })
        .unwrap();
    let mut a2 = Map::new();
    a2.insert("ephemeral_b".into(), json!(2));
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "dup-uid".into(),
            state: "20".into(),
            attributes: a2,
        })
        .unwrap();
    let st = hass
        .states()
        .get(&ha_core::EntityId::parse(&id1).unwrap())
        .unwrap();
    assert_eq!(st.attributes.get("ephemeral_a"), None);
    assert_eq!(st.attributes.get("ephemeral_b"), Some(&json!(2)));
    assert_eq!(st.attributes.get("unit_of_measurement"), Some(&json!("W")));

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn reload_migrates_active_entries_and_remove_cleans_up_and_preserves_data() {
    let dir = temp_dir("reload-remove");
    let src_dir = temp_dir("reload-src");
    let src = write_mock_plugin(&src_dir, "mig_plug");
    let plugins = PluginManager::load(&dir);
    plugins.install_from_dir(&src).await.unwrap();

    // Seed persistent data under `.data/mig_plug/token.txt` and ensure reload preserves it.
    let data_file = dir.join("plugins/.data/mig_plug/token.txt");
    std::fs::create_dir_all(data_file.parent().unwrap()).unwrap();
    std::fs::write(&data_file, b"keep-me").unwrap();

    let hass = Hass::new();
    let registry = Registry::new(&hass, &dir, Registries::default());
    let entries = ConfigEntries::load(&hass, &dir);
    let components = Components::new(&hass, registry.clone(), Profiles::default());
    let flows = FlowManager::new(
        &hass,
        entries.clone(),
        components.clone(),
        Endpoints::default(),
        Vec::new(),
        None,
    )
    .unwrap();
    flows.set_plugins(plugins.clone());

    let FlowResult::Form { flow_id, .. } = flows.init("mig_plug", SOURCE_USER).await.unwrap()
    else {
        panic!("expected form");
    };
    let FlowResult::CreateEntry { entry, .. } = flows
        .configure(&flow_id, Some(vec![("host".into(), json!("192.0.2.20"))]))
        .await
        .unwrap()
    else {
        panic!("expected create_entry");
    };
    let entry_id = entry["entry_id"].as_str().unwrap().to_owned();

    // Wait for switch entity to appear.
    let mut sw_id = None;
    for _ in 0..40 {
        if let Some(e) = registry
            .snapshot()
            .entities
            .iter()
            .find(|e| e.unique_id.as_ref().and_then(Value::as_str) == Some("plug-sw"))
        {
            sw_id = Some(e.entity_id.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let sw_id = sw_id.expect("switch entity");

    // Re-install (upgrade) plugin while entry is active: data stays & switch still works!
    plugins.install_from_dir(&src).await.unwrap();
    assert_eq!(std::fs::read(&data_file).unwrap(), b"keep-me");

    let mut data = Map::new();
    data.insert("entity_id".into(), json!(sw_id));
    hass.call_service("switch", "turn_on", data, None, false, Some(Context::new()))
        .await
        .unwrap();
    for _ in 0..40 {
        if hass
            .states()
            .get(&ha_core::EntityId::parse(&sw_id).unwrap())
            .is_some_and(|s| s.state.as_str() == "on")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        hass.states()
            .get(&ha_core::EntityId::parse(&sw_id).unwrap())
            .unwrap()
            .state
            .as_str(),
        "on"
    );

    // Removing the plugin while active deactivates its entity & marks entry NotLoaded.
    assert!(plugins.remove_plugin("mig_plug").await.unwrap());
    assert!(
        hass.states()
            .get(&ha_core::EntityId::parse(&sw_id).unwrap())
            .is_none()
    );
    assert_eq!(
        entries.runtime(&entry_id).unwrap().state.as_str(),
        "not_loaded"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&src_dir);
}

#[cfg(unix)]
#[tokio::test]
async fn version_2_plugin_entry_starts_at_boot_and_respects_setup_error() {
    let dir = temp_dir("v2-boot");
    let plugin_dir = dir.join("plugins/v2_plug");
    std::fs::create_dir_all(plugin_dir.join("bin")).unwrap();
    let manifest = PluginManifest {
        schema_version: 1,
        domain: "v2_plug".into(),
        name: "V2 Plug".into(),
        version: "2.0.0".into(),
        iot_class: "local_push".into(),
        config_flow: true,
        documentation: None,
        description: None,
        transport: Transport::Stdio {
            command: "bin/plug.py".into(),
            args: vec![],
        },
        capabilities: Capabilities {
            entity_domains: vec!["sensor".into()],
            config_entry_version: 2,
        },
    };
    std::fs::write(
        plugin_dir.join("plugin.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let bin = plugin_dir.join("bin/plug.py");
    std::fs::write(
        &bin,
        r#"#!/usr/bin/env python3
import json, sys
def send(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    m = json.loads(line)
    mid, meth, p = m.get("id"), m.get("method"), m.get("params") or {}
    if meth == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocol_version": 1}})
    elif meth == "entry.setup":
        send({"jsonrpc": "2.0", "method": "entry.status", "params": {
            "entry_id": p["entry_id"], "state": "setup_error", "reason": "bad key"
        }})
        send({"jsonrpc": "2.0", "id": mid, "result": {"ok": True}})
    elif meth == "shutdown":
        send({"jsonrpc": "2.0", "id": mid, "result": {"ok": True}})
        break
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let hass = Hass::new();
    let registry = Registry::new(&hass, &dir, Registries::default());
    let entries = ConfigEntries::load(&hass, &dir);
    let mut entry = entries.new_entry("v2_plug", SOURCE_USER, "V2", None, Map::new());
    entry.insert("version".into(), json!(2));
    let entry_id = entry["entry_id"].as_str().unwrap().to_owned();
    entries.add(entry);

    let components = Components::new(&hass, registry, Profiles::default());
    let plugins = PluginManager::load(&dir);
    let mut tasks = Vec::new();
    let started = crate::start_plugin_entries(
        &plugins,
        &components,
        entries.list().as_deref(),
        Some(&entries),
        &mut tasks,
    );
    assert_eq!(started, vec!["v2_plug"]);

    for _ in 0..40 {
        if entries
            .runtime(&entry_id)
            .is_some_and(|r| r.state.as_str() == "setup_error")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let rt = entries.runtime(&entry_id).expect("runtime set");
    assert_eq!(rt.state.as_str(), "setup_error");
    assert_eq!(rt.reason.as_deref(), Some("bad key"));

    plugins.shutdown_all().await;
    for t in tasks {
        t.task.abort();
    }
    let _ = std::fs::remove_dir_all(&dir);
}
