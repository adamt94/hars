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
    assert_eq!(started, vec!["sidecar_demo", "sensor"]);

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
            fan_speed_list: None,
            ..Default::default()
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
        fan_speed_list: None,
        ..Default::default()
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
    assert_eq!(started, vec!["v2_plug", "sensor"]);

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

#[cfg(unix)]
#[tokio::test]
async fn stdio_plugin_vacuum_entities_and_services() {
    use crate::vacuum::feature;

    let dir = temp_dir("vacuum-plugin");
    let plugin_dir = dir.join("plugins/mock_vac");
    std::fs::create_dir_all(plugin_dir.join("bin")).unwrap();
    let features = feature::START
        | feature::PAUSE
        | feature::STOP
        | feature::RETURN_HOME
        | feature::CLEAN_SPOT
        | feature::LOCATE
        | feature::FAN_SPEED
        | feature::SEND_COMMAND
        | feature::STATE;
    let manifest = PluginManifest {
        schema_version: 1,
        domain: "mock_vac".into(),
        name: "Mock Vacuum".into(),
        version: "0.1.0".into(),
        iot_class: "local_polling".into(),
        config_flow: true,
        documentation: None,
        description: None,
        transport: Transport::Stdio {
            command: "bin/vac.py".into(),
            args: vec![],
        },
        capabilities: Capabilities {
            entity_domains: vec!["vacuum".into(), "sensor".into()],
            config_entry_version: 1,
        },
    };
    std::fs::write(
        plugin_dir.join("plugin.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let bin = plugin_dir.join("bin/vac.py");
    std::fs::write(
        &bin,
        format!(
            r#"#!/usr/bin/env python3
import json, sys
def send(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    m = json.loads(line)
    mid, meth, p = m.get("id"), m.get("method"), m.get("params") or {{}}
    if meth == "initialize":
        send({{"jsonrpc": "2.0", "id": mid, "result": {{"protocol_version": 1}}}})
    elif meth == "entry.setup":
        eid = p["entry_id"]
        send({{"jsonrpc": "2.0", "method": "device.upsert", "params": {{
            "entry_id": eid, "identifier": "rr-1", "name": "Roborock S8",
            "manufacturer": "Roborock", "model": "roborock.vacuum.a70"
        }}}})
        send({{"jsonrpc": "2.0", "method": "entity.upsert", "params": {{
            "entry_id": eid, "unique_id": "rr_1", "domain": "vacuum",
            "device_identifier": "rr-1",
            "supported_features": {features},
            "fan_speed_list": ["quiet", "balanced", "turbo", "max"]
        }}}})
        send({{"jsonrpc": "2.0", "method": "state.set", "params": {{
            "entry_id": eid, "unique_id": "rr_1", "state": "docked",
            "attributes": {{"fan_speed": "balanced"}}
        }}}})
        send({{"jsonrpc": "2.0", "method": "entry.status", "params": {{
            "entry_id": eid, "state": "loaded"
        }}}})
        send({{"jsonrpc": "2.0", "id": mid, "result": {{"ok": True}}}})
    elif meth == "entity.call":
        eid, uid, svc, data = p["entry_id"], p["unique_id"], p["service"], p.get("data") or {{}}
        state_map = {{
            "start": "cleaning",
            "pause": "paused",
            "stop": "idle",
            "return_to_base": "returning",
            "clean_spot": "cleaning",
            "locate": "docked",
        }}
        new_state = state_map.get(svc, "cleaning")
        attrs = {{"last_service": svc, "last_data": data}}
        if svc == "set_fan_speed":
            attrs["fan_speed"] = data["fan_speed"]
        send({{"jsonrpc": "2.0", "method": "state.set", "params": {{
            "entry_id": eid, "unique_id": uid, "state": new_state, "attributes": attrs
        }}}})
        send({{"jsonrpc": "2.0", "id": mid, "result": {{"ok": True}}}})
    elif meth == "entry.unload":
        send({{"jsonrpc": "2.0", "id": mid, "result": {{"ok": True}}}})
    elif meth == "shutdown":
        send({{"jsonrpc": "2.0", "id": mid, "result": {{"ok": True}}}})
        break
"#
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let hass = Hass::new();
    let registry = Registry::new(&hass, &dir, Registries::default());
    let entries = ConfigEntries::load(&hass, &dir);
    let entry = entries.new_entry("mock_vac", SOURCE_USER, "Roborock", None, Map::new());
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
    assert_eq!(started, vec!["mock_vac", "vacuum", "sensor"]);

    let vac_id = ha_core::EntityId::parse("vacuum.roborock_s8").unwrap();
    for _ in 0..50 {
        if hass.states().get(&vac_id).is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let st = hass.states().get(&vac_id).expect("vacuum state present");
    assert_eq!(st.state.as_str(), "docked");
    assert_eq!(st.attributes["friendly_name"], json!("Roborock S8"));
    assert_eq!(st.attributes["supported_features"], json!(features));
    assert_eq!(
        st.attributes["fan_speed_list"],
        json!(["quiet", "balanced", "turbo", "max"])
    );
    assert_eq!(st.attributes["fan_speed"], json!("balanced"));

    // Start, set_fan_speed, send_command, return_to_base.
    hass.call_service(
        "vacuum",
        "start",
        json!({"entity_id": "vacuum.roborock_s8"})
            .as_object()
            .cloned()
            .unwrap(),
        None,
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        hass.states().get(&vac_id).unwrap().state.as_str(),
        "cleaning"
    );

    hass.call_service(
        "vacuum",
        "set_fan_speed",
        json!({"entity_id": "vacuum.roborock_s8", "fan_speed": "turbo"})
            .as_object()
            .cloned()
            .unwrap(),
        None,
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        hass.states().get(&vac_id).unwrap().attributes["fan_speed"],
        json!("turbo")
    );

    hass.call_service(
        "vacuum",
        "send_command",
        json!({
            "entity_id": "vacuum.roborock_s8",
            "command": "app_segment_clean",
            "params": [{"segments": [16]}],
        })
        .as_object()
        .cloned()
        .unwrap(),
        None,
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        hass.states().get(&vac_id).unwrap().attributes["last_data"]["command"],
        json!("app_segment_clean")
    );

    hass.call_service(
        "vacuum",
        "return_to_base",
        json!({"entity_id": "vacuum.roborock_s8"})
            .as_object()
            .cloned()
            .unwrap(),
        None,
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        hass.states().get(&vac_id).unwrap().state.as_str(),
        "returning"
    );

    plugins.shutdown_all().await;
    for t in tasks {
        t.task.abort();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bundled_roborock_plugin_manifest_loads_and_validates() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/roborock");
    let loaded = InstalledPlugin::load(&manifest_dir).expect("roborock plugin loads");
    assert_eq!(loaded.domain(), "roborock");
    assert!(loaded.allows_entity_domain("vacuum"));
    assert!(loaded.allows_entity_domain("sensor"));
    assert!(loaded.allows_entity_domain("binary_sensor"));
    assert!(loaded.allows_entity_domain("switch"));
    assert!(loaded.allows_entity_domain("button"));
    assert!(loaded.translations_for("config").is_some());
    assert!(loaded.icons_for("entity").is_some());
}

#[tokio::test]
async fn user_disabled_plugin_vacuum_cannot_be_commanded() {
    use crate::vacuum::feature;
    use std::sync::atomic::AtomicUsize;

    let dir = temp_dir("disabled-vacuum");
    let plugin_dir = dir.join("plugins/roborock");
    std::fs::create_dir_all(plugin_dir.join("bin")).unwrap();
    let manifest = PluginManifest {
        schema_version: 1,
        domain: "roborock".into(),
        name: "Roborock".into(),
        version: "0.1.0".into(),
        iot_class: "local_polling".into(),
        config_flow: true,
        documentation: None,
        description: None,
        transport: Transport::Stdio {
            command: "bin/rr".into(),
            args: vec![],
        },
        capabilities: Capabilities {
            entity_domains: vec!["vacuum".into(), "button".into()],
            config_entry_version: 1,
        },
    };
    std::fs::write(
        plugin_dir.join("plugin.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    std::fs::write(plugin_dir.join("bin/rr"), b"").unwrap();
    let installed = Arc::new(InstalledPlugin::load(&plugin_dir).unwrap());

    let hass = Hass::new();
    let registry = Registry::new(&hass, &dir, Registries::default());
    let components = Components::new(&hass, registry.clone(), Profiles::default());
    // Ensure `vacuum.*` services are registered.
    let _ = components.vacuums();

    // Pre-create a user-disabled entity row as `hars import` leaves it.
    let pre = registry.get_or_create_entity(
        "vacuum",
        "roborock",
        "duid_disabled",
        Some("user".into()),
        ha_registry::EntityPatch {
            config_entry_id: Some(Some("e1".into())),
            has_entity_name: Some(true),
            original_name: Some(Some("Disabled Vac".into())),
            object_id_base: Some(Some("Disabled Vac".into())),
            ..Default::default()
        },
    );
    assert_eq!(pre.disabled_by.as_deref(), Some("user"));

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_ref = calls.clone();
    let store = entities::PluginEntityStore::new(
        installed,
        components,
        Arc::new(move |_| {
            calls_ref.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }),
    );
    store.activate_entry("e1");
    let id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "duid_disabled".into(),
            domain: "vacuum".into(),
            device_identifier: None,
            name: Some("Disabled Vac".into()),
            object_id_base: None,
            original_icon: None,
            device_class: None,
            unit_of_measurement: None,
            entity_category: None,
            supported_features: feature::START | feature::STATE,
            supported_color_modes: vec![],
            effect_list: None,
            min_color_temp_kelvin: None,
            max_color_temp_kelvin: None,
            fan_speed_list: None,
            ..Default::default()
        })
        .unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "duid_disabled".into(),
            state: "docked".into(),
            attributes: Map::new(),
        })
        .unwrap();

    // Disabled entity has no state and receives no service calls.
    assert!(
        hass.states()
            .get(&ha_core::EntityId::parse(&id).unwrap())
            .is_none()
    );
    hass.call_service(
        "vacuum",
        "start",
        json!({"entity_id": id}).as_object().cloned().unwrap(),
        None,
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // Pressed button retains its last-press timestamp across an outage.
    let btn_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "empty_dustbin".into(),
            domain: "button".into(),
            device_identifier: None,
            name: Some("Empty dustbin".into()),
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
            fan_speed_list: None,
            ..Default::default()
        })
        .unwrap();
    let btn_eid = ha_core::EntityId::parse(&btn_id).unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "empty_dustbin".into(),
            state: "unknown".into(),
            attributes: Map::new(),
        })
        .unwrap();
    hass.call_service(
        "button",
        "press",
        json!({"entity_id": btn_id}).as_object().cloned().unwrap(),
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let pressed_ts = hass.states().get(&btn_eid).unwrap().state.to_string();
    assert_ne!(pressed_ts, "unknown");
    // Outage -> unavailable -> recovery ("unknown") restores pressed timestamp.
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "empty_dustbin".into(),
            state: "unavailable".into(),
            attributes: Map::new(),
        })
        .unwrap();
    assert_eq!(
        hass.states().get(&btn_eid).unwrap().state.as_str(),
        "unavailable"
    );
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "empty_dustbin".into(),
            state: "unknown".into(),
            attributes: Map::new(),
        })
        .unwrap();
    assert_eq!(
        hass.states().get(&btn_eid).unwrap().state.as_str(),
        pressed_ts
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn plugin_supports_select_number_cover_lock_and_fan() {
    use crate::{cover, fan, lock};

    let dir = temp_dir("foundation-domains");
    let plugin_dir = dir.join("plugins/multi_dev");
    std::fs::create_dir_all(plugin_dir.join("bin")).unwrap();
    let manifest = PluginManifest {
        schema_version: 1,
        domain: "multi_dev".into(),
        name: "Multi Device".into(),
        version: "0.1.0".into(),
        iot_class: "local_push".into(),
        config_flow: true,
        documentation: None,
        description: None,
        transport: Transport::Stdio {
            command: "bin/multi".into(),
            args: vec![],
        },
        capabilities: Capabilities {
            entity_domains: vec![
                "select".into(),
                "number".into(),
                "cover".into(),
                "lock".into(),
                "fan".into(),
            ],
            config_entry_version: 1,
        },
    };
    std::fs::write(
        plugin_dir.join("plugin.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    std::fs::write(plugin_dir.join("bin/multi"), b"").unwrap();
    let installed = Arc::new(InstalledPlugin::load(&plugin_dir).unwrap());

    let hass = Hass::new();
    let registry = Registry::new(&hass, &dir, Registries::default());
    let components = Components::new(&hass, registry, Profiles::default());

    let recorded: Arc<parking_lot::Mutex<Vec<protocol::EntityCallParams>>> = Default::default();
    let rec = recorded.clone();
    let store = entities::PluginEntityStore::new(
        installed,
        components.clone(),
        Arc::new(move |params| {
            rec.lock().push(params);
            Box::pin(async { Ok(()) })
        }),
    );
    store.activate_entry("e1");

    // 1. select
    let sel_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "sel_1".into(),
            domain: "select".into(),
            name: Some("Mode".into()),
            options: Some(vec!["eco".into(), "normal".into(), "turbo".into()]),
            ..Default::default()
        })
        .unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "sel_1".into(),
            state: "eco".into(),
            attributes: Map::new(),
        })
        .unwrap();

    // 2. number
    let num_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "num_1".into(),
            domain: "number".into(),
            name: Some("Brightness".into()),
            min_value: Some(0.0),
            max_value: Some(50.0),
            step: Some(5.0),
            mode: Some("slider".into()),
            ..Default::default()
        })
        .unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "num_1".into(),
            state: "25".into(),
            attributes: Map::new(),
        })
        .unwrap();

    // 3. cover
    let cov_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "cov_1".into(),
            domain: "cover".into(),
            name: Some("Blinds".into()),
            supported_features: cover::feature::OPEN
                | cover::feature::CLOSE
                | cover::feature::SET_POSITION
                | cover::feature::STOP,
            ..Default::default()
        })
        .unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "cov_1".into(),
            state: "closed".into(),
            attributes: json!({"current_position": 0}).as_object().cloned().unwrap(),
        })
        .unwrap();

    // 4. lock
    let lck_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "lck_1".into(),
            domain: "lock".into(),
            name: Some("Front Door".into()),
            supported_features: lock::feature::OPEN,
            code_format: Some(r"^\d{4}$".into()),
            ..Default::default()
        })
        .unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "lck_1".into(),
            state: "locked".into(),
            attributes: Map::new(),
        })
        .unwrap();

    // 5. fan
    let fan_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "fan_1".into(),
            domain: "fan".into(),
            name: Some("Ceiling Fan".into()),
            supported_features: fan::feature::TURN_ON
                | fan::feature::TURN_OFF
                | fan::feature::SET_SPEED
                | fan::feature::PRESET_MODE,
            preset_modes: Some(vec!["auto".into(), "sleep".into()]),
            speed_count: Some(3),
            ..Default::default()
        })
        .unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "fan_1".into(),
            state: "on".into(),
            attributes: json!({"percentage": 33}).as_object().cloned().unwrap(),
        })
        .unwrap();

    let call = |domain: &'static str, svc: &'static str, v: Value| {
        let hass = hass.clone();
        async move {
            hass.call_service(
                domain,
                svc,
                v.as_object().cloned().unwrap(),
                None,
                false,
                None,
            )
            .await
        }
    };
    call("select", "select_next", json!({"entity_id": sel_id}))
        .await
        .unwrap();
    call(
        "number",
        "set_value",
        json!({"entity_id": num_id, "value": 35}),
    )
    .await
    .unwrap();
    call("cover", "toggle", json!({"entity_id": cov_id}))
        .await
        .unwrap();
    call(
        "lock",
        "unlock",
        json!({"entity_id": lck_id, "code": "1234"}),
    )
    .await
    .unwrap();
    call("fan", "increase_speed", json!({"entity_id": fan_id}))
        .await
        .unwrap();

    // Validation failures do not reach the plugin.
    assert!(
        call(
            "number",
            "set_value",
            json!({"entity_id": num_id, "value": 99})
        )
        .await
        .is_err()
    );
    assert!(
        call(
            "lock",
            "unlock",
            json!({"entity_id": lck_id, "code": "bad"})
        )
        .await
        .is_err()
    );

    let calls: Vec<(String, String, Value)> = recorded
        .lock()
        .iter()
        .map(|c| {
            (
                c.domain.clone(),
                c.service.clone(),
                Value::Object(c.data.clone()),
            )
        })
        .collect();
    assert_eq!(
        calls,
        vec![
            (
                "select".into(),
                "select_option".into(),
                json!({"option": "normal"})
            ),
            ("number".into(), "set_value".into(), json!({"value": 35.0})),
            ("cover".into(), "open_cover".into(), json!({})),
            ("lock".into(), "unlock".into(), json!({"code": "1234"})),
            (
                "fan".into(),
                "set_percentage".into(),
                json!({"percentage": 66})
            ),
        ]
    );

    // Crash marks all five unavailable while preserving capabilities & units,
    // and deactivate_entry unregisters them.
    store.mark_all_unavailable();
    for id in [&sel_id, &num_id, &cov_id, &lck_id, &fan_id] {
        assert_eq!(
            hass.states()
                .get(&ha_core::EntityId::parse(id).unwrap())
                .unwrap()
                .state
                .as_str(),
            "unavailable"
        );
    }
    let sel_st = hass
        .states()
        .get(&ha_core::EntityId::parse(&sel_id).unwrap())
        .unwrap();
    assert_eq!(
        sel_st.attributes["options"],
        json!(["eco", "normal", "turbo"])
    );
    let num_st = hass
        .states()
        .get(&ha_core::EntityId::parse(&num_id).unwrap())
        .unwrap();
    assert_eq!(num_st.attributes["min"], json!(0.0));
    assert_eq!(num_st.attributes["max"], json!(50.0));
    assert_eq!(num_st.attributes["step"], json!(5.0));
    assert_eq!(num_st.attributes["mode"], json!("slider"));
    let lck_st = hass
        .states()
        .get(&ha_core::EntityId::parse(&lck_id).unwrap())
        .unwrap();
    // `code_format` is a state_attribute in HA's LockEntity, omitted when unavailable.
    assert!(lck_st.attributes.get("code_format").is_none());
    let fan_st = hass
        .states()
        .get(&ha_core::EntityId::parse(&fan_id).unwrap())
        .unwrap();
    assert_eq!(fan_st.attributes["preset_modes"], json!(["auto", "sleep"]));
    assert!(fan_st.attributes.get("percentage_step").is_none());

    store.deactivate_entry("e1");
    for id in [&sel_id, &num_id, &cov_id, &lck_id, &fan_id] {
        assert!(
            hass.states()
                .get(&ha_core::EntityId::parse(id).unwrap())
                .is_none()
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn plugin_round1_edge_cases_default_code_reupsert_dynamic_attrs_and_dropped_pct() {
    use crate::{cover, fan, lock};

    let dir = temp_dir("foundation-edge-cases");
    let plugin_dir = dir.join("plugins/edge_dev");
    std::fs::create_dir_all(plugin_dir.join("bin")).unwrap();
    let manifest = PluginManifest {
        schema_version: 1,
        domain: "edge_dev".into(),
        name: "Edge Device".into(),
        version: "0.1.0".into(),
        iot_class: "local_push".into(),
        config_flow: true,
        documentation: None,
        description: None,
        transport: Transport::Stdio {
            command: "bin/edge".into(),
            args: vec![],
        },
        capabilities: Capabilities {
            entity_domains: vec![
                "cover".into(),
                "lock".into(),
                "fan".into(),
                "light".into(),
                "select".into(),
            ],
            config_entry_version: 1,
        },
    };
    std::fs::write(
        plugin_dir.join("plugin.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    std::fs::write(plugin_dir.join("bin/edge"), b"").unwrap();
    let installed = Arc::new(InstalledPlugin::load(&plugin_dir).unwrap());

    let hass = Hass::new();
    // Seed an imported lock row carrying `options.lock.default_code`.
    let seeded_row: ha_registry::Entity = serde_json::from_value(json!({
        "entity_id": "lock.imported_door",
        "platform": "edge_dev",
        "unique_id": "lck_imported",
        "config_entry_id": "e1",
        "options": { "lock": { "default_code": "4321" } }
    }))
    .unwrap();
    let initial = Registries {
        entities: vec![Arc::new(seeded_row)],
        ..Default::default()
    };
    let registry = Registry::new(&hass, &dir, initial);
    let components = Components::new(&hass, registry, Profiles::default());

    let recorded: Arc<parking_lot::Mutex<Vec<protocol::EntityCallParams>>> = Default::default();
    let rec = recorded.clone();
    let store = entities::PluginEntityStore::new(
        installed,
        components.clone(),
        Arc::new(move |params| {
            rec.lock().push(params);
            Box::pin(async { Ok(()) })
        }),
    );
    store.activate_entry("e1");

    // 1. Imported lock uses `options.lock.default_code` when `code` is omitted.
    let lck_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "lck_imported".into(),
            domain: "lock".into(),
            name: Some("Imported Door".into()),
            supported_features: lock::feature::OPEN,
            code_format: Some(r"^\d{4}$".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(lck_id, "lock.imported_door");
    hass.call_service(
        "lock",
        "unlock",
        json!({"entity_id": lck_id}).as_object().cloned().unwrap(),
        None,
        false,
        None,
    )
    .await
    .unwrap();
    // Dynamic code_format override persists across later frames without it.
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "lck_imported".into(),
            state: "unlocked".into(),
            attributes: json!({"code_format": r"^\d{6}$"})
                .as_object()
                .cloned()
                .unwrap(),
        })
        .unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "lck_imported".into(),
            state: "locked".into(),
            attributes: Map::new(),
        })
        .unwrap();
    let lck_eid = ha_core::EntityId::parse(&lck_id).unwrap();
    assert_eq!(
        hass.states().get(&lck_eid).unwrap().attributes["code_format"],
        json!(r"^\d{6}$")
    );
    // Old 4-digit default_code no longer matches `^\d{6}$`, so unlock without code fails.
    assert!(
        hass.call_service(
            "lock",
            "unlock",
            json!({"entity_id": lck_id}).as_object().cloned().unwrap(),
            None,
            false,
            None,
        )
        .await
        .is_err()
    );
    // Clearing `code_format` still forwards the saved default_code, matching HA.
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "lck_imported".into(),
            state: "locked".into(),
            attributes: json!({"code_format": null}).as_object().cloned().unwrap(),
        })
        .unwrap();
    hass.call_service(
        "lock",
        "unlock",
        json!({"entity_id": lck_id}).as_object().cloned().unwrap(),
        None,
        false,
        None,
    )
    .await
    .unwrap();

    // 2. Re-upsert preserves live fan `is_on` and cover `is_opening` state;
    //    dropped fan `percentage` resets `increase_speed` to 33%; dynamic
    //    `preset_modes` stay synced with published attributes.
    let fan_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "fan_edge".into(),
            domain: "fan".into(),
            name: Some("Edge Fan".into()),
            supported_features: fan::feature::TURN_ON
                | fan::feature::TURN_OFF
                | fan::feature::SET_SPEED
                | fan::feature::PRESET_MODE,
            preset_modes: Some(vec!["auto".into()]),
            speed_count: Some(3),
            ..Default::default()
        })
        .unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "fan_edge".into(),
            state: "on".into(),
            attributes: json!({"percentage": 66, "preset_modes": ["sleep"]})
                .as_object()
                .cloned()
                .unwrap(),
        })
        .unwrap();
    // Subsequent state frame omitting preset_modes still advertises ["sleep"].
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "fan_edge".into(),
            state: "on".into(),
            attributes: json!({"percentage": 66}).as_object().cloned().unwrap(),
        })
        .unwrap();
    let fan_eid = ha_core::EntityId::parse(&fan_id).unwrap();
    assert_eq!(
        hass.states().get(&fan_eid).unwrap().attributes["preset_modes"],
        json!(["sleep"])
    );
    // Re-upserting metadata preserves `is_on = true`, so toggle sends `turn_off`.
    store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "fan_edge".into(),
            domain: "fan".into(),
            name: Some("Edge Fan".into()),
            supported_features: fan::feature::TURN_ON
                | fan::feature::TURN_OFF
                | fan::feature::SET_SPEED
                | fan::feature::PRESET_MODE,
            preset_modes: Some(vec!["sleep".into()]),
            speed_count: Some(3),
            ..Default::default()
        })
        .unwrap();
    hass.call_service(
        "fan",
        "toggle",
        json!({"entity_id": fan_id}).as_object().cloned().unwrap(),
        None,
        false,
        None,
    )
    .await
    .unwrap();
    // Turning off without `percentage` drops cached percentage -> increase_speed sends 33%.
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "fan_edge".into(),
            state: "off".into(),
            attributes: Map::new(),
        })
        .unwrap();
    hass.call_service(
        "fan",
        "increase_speed",
        json!({"entity_id": fan_id}).as_object().cloned().unwrap(),
        None,
        false,
        None,
    )
    .await
    .unwrap();

    // 3. Re-upserting a moving cover preserves `is_opening` -> toggle sends `stop_cover`.
    let cov_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "cov_edge".into(),
            domain: "cover".into(),
            name: Some("Edge Blinds".into()),
            supported_features: cover::feature::OPEN | cover::feature::CLOSE | cover::feature::STOP,
            ..Default::default()
        })
        .unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "cov_edge".into(),
            state: "opening".into(),
            attributes: Map::new(),
        })
        .unwrap();
    store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "cov_edge".into(),
            domain: "cover".into(),
            name: Some("Edge Blinds".into()),
            supported_features: cover::feature::OPEN | cover::feature::CLOSE | cover::feature::STOP,
            ..Default::default()
        })
        .unwrap();
    hass.call_service(
        "cover",
        "toggle",
        json!({"entity_id": cov_id}).as_object().cloned().unwrap(),
        None,
        false,
        None,
    )
    .await
    .unwrap();

    // 4. Re-upserting a color_temp light as brightness-only drops the stale
    //    unsupported color_mode; disconnect preserves light capabilities.
    let light_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "light_edge".into(),
            domain: "light".into(),
            name: Some("Edge Lamp".into()),
            supported_color_modes: vec!["color_temp".into()],
            min_color_temp_kelvin: Some(2200),
            max_color_temp_kelvin: Some(6000),
            ..Default::default()
        })
        .unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "light_edge".into(),
            state: "on".into(),
            attributes: json!({"color_mode": "color_temp", "color_temp_kelvin": 3000})
                .as_object()
                .cloned()
                .unwrap(),
        })
        .unwrap();
    store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "light_edge".into(),
            domain: "light".into(),
            name: Some("Edge Lamp".into()),
            supported_color_modes: vec!["brightness".into()],
            ..Default::default()
        })
        .unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "light_edge".into(),
            state: "on".into(),
            attributes: json!({"brightness": 128}).as_object().cloned().unwrap(),
        })
        .unwrap();
    let light_eid = ha_core::EntityId::parse(&light_id).unwrap();
    let light_st = hass.states().get(&light_eid).unwrap();
    assert_eq!(light_st.attributes["color_mode"], json!("brightness"));
    assert_eq!(
        light_st.attributes["supported_color_modes"],
        json!(["brightness"])
    );
    store.mark_all_unavailable();
    assert_eq!(
        hass.states().get(&light_eid).unwrap().attributes["supported_color_modes"],
        json!(["brightness"])
    );

    // 4b. Re-upserting an `hs` light while an effect is active retains `brightness`
    // (HA permits `onoff`/`brightness` during active effects).
    let fx_light_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "light_fx".into(),
            domain: "light".into(),
            name: Some("FX Lamp".into()),
            supported_color_modes: vec!["hs".into()],
            supported_features: crate::light::FEATURE_EFFECT,
            effect_list: Some(vec!["rainbow".into()]),
            ..Default::default()
        })
        .unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "light_fx".into(),
            state: "on".into(),
            attributes: json!({
                "color_mode": "brightness",
                "brightness": 200,
                "effect": "rainbow"
            })
            .as_object()
            .cloned()
            .unwrap(),
        })
        .unwrap();
    store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "light_fx".into(),
            domain: "light".into(),
            name: Some("FX Lamp".into()),
            supported_color_modes: vec!["hs".into()],
            supported_features: crate::light::FEATURE_EFFECT,
            effect_list: Some(vec!["rainbow".into()]),
            ..Default::default()
        })
        .unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "light_fx".into(),
            state: "on".into(),
            attributes: json!({"brightness": 180}).as_object().cloned().unwrap(),
        })
        .unwrap();
    let fx_eid = ha_core::EntityId::parse(&fx_light_id).unwrap();
    let fx_st = hass.states().get(&fx_eid).unwrap();
    assert_eq!(fx_st.attributes["color_mode"], json!("brightness"));
    assert_eq!(fx_st.attributes["effect"], json!("rainbow"));

    // 5. Re-upserting a select with omitted options clears them (matches registry `[]`).
    let sel_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "sel_edge".into(),
            domain: "select".into(),
            name: Some("Edge Select".into()),
            options: Some(vec!["a".into(), "b".into()]),
            ..Default::default()
        })
        .unwrap();
    store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "sel_edge".into(),
            domain: "select".into(),
            name: Some("Edge Select".into()),
            options: None,
            ..Default::default()
        })
        .unwrap();
    assert!(
        hass.call_service(
            "select",
            "select_option",
            json!({"entity_id": sel_id, "option": "a"})
                .as_object()
                .cloned()
                .unwrap(),
            None,
            false,
            None,
        )
        .await
        .is_err()
    );

    let calls: Vec<(String, String, Value)> = recorded
        .lock()
        .iter()
        .map(|c| {
            (
                c.domain.clone(),
                c.service.clone(),
                Value::Object(c.data.clone()),
            )
        })
        .collect();
    assert_eq!(
        calls,
        vec![
            ("lock".into(), "unlock".into(), json!({"code": "4321"})),
            ("lock".into(), "unlock".into(), json!({"code": "4321"})),
            ("fan".into(), "turn_off".into(), json!({})),
            (
                "fan".into(),
                "set_percentage".into(),
                json!({"percentage": 33})
            ),
            ("cover".into(), "stop_cover".into(), json!({})),
        ]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn plugin_media_player_and_remote_lifecycle_and_services() {
    let dir = std::env::temp_dir().join(format!("hars-plugin-mp-rem-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let plug_dir = dir.join("plugins").join("tv_plug");
    std::fs::create_dir_all(&plug_dir).unwrap();
    std::fs::write(
        plug_dir.join("plugin.json"),
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "domain": "tv_plug",
            "name": "TV Plugin",
            "version": "1.0.0",
            "transport": { "type": "unix_socket" },
            "capabilities": {
                "entity_domains": ["media_player", "remote"]
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let installed = Arc::new(manifest::InstalledPlugin::load(&plug_dir).unwrap());
    let hass = ha_core::Hass::new();
    let reg = Arc::new(ha_registry::Registries::default());
    let components = crate::components::Components::new(&hass, reg.clone(), Default::default());

    let recorded: Arc<parking_lot::Mutex<Vec<protocol::EntityCallParams>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let rec2 = recorded.clone();
    let call_fn: entities::CallPluginFn = Arc::new(move |params| {
        rec2.lock().push(params);
        Box::pin(async { Ok(()) })
    });
    let store = entities::PluginEntityStore::new(installed, components, call_fn);
    store.activate_entry("e1");

    // 1. Register a full media_player (VOLUME_SET only, no VOLUME_STEP) and a VOLUME_STEP-only player.
    let mp_feats = crate::media_player::feature::TURN_ON
        | crate::media_player::feature::TURN_OFF
        | crate::media_player::feature::PLAY
        | crate::media_player::feature::PAUSE
        | crate::media_player::feature::VOLUME_SET
        | crate::media_player::feature::VOLUME_MUTE
        | crate::media_player::feature::SEEK
        | crate::media_player::feature::PLAY_MEDIA
        | crate::media_player::feature::SELECT_SOURCE
        | crate::media_player::feature::SELECT_SOUND_MODE
        | crate::media_player::feature::SHUFFLE_SET
        | crate::media_player::feature::REPEAT_SET
        | crate::media_player::feature::GROUPING
        | crate::media_player::feature::CLEAR_PLAYLIST;
    let mp_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "mp_main".into(),
            domain: "media_player".into(),
            name: Some("Living Room TV".into()),
            supported_features: mp_feats,
            source_list: Some(vec!["HDMI 1".into(), "Netflix".into()]),
            sound_mode_list: Some(vec!["Movie".into()]),
            volume_step: Some(0.2),
            ..Default::default()
        })
        .unwrap();
    let mp_step_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "mp_step".into(),
            domain: "media_player".into(),
            name: Some("Step Speaker".into()),
            supported_features: crate::media_player::feature::VOLUME_STEP,
            ..Default::default()
        })
        .unwrap();
    let rem_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "rem_main".into(),
            domain: "remote".into(),
            name: Some("Living Room Remote".into()),
            supported_features: crate::remote::feature::ACTIVITY,
            ..Default::default()
        })
        .unwrap();

    // Push live state to both entities.
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "mp_main".into(),
            state: "playing".into(),
            attributes: json!({"volume_level": 0.5, "media_title": "Intro"})
                .as_object()
                .cloned()
                .unwrap(),
        })
        .unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "mp_step".into(),
            state: "idle".into(),
            attributes: Default::default(),
        })
        .unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "rem_main".into(),
            state: "on".into(),
            attributes: json!({"activity_list": ["TV"], "current_activity": "TV"})
                .as_object()
                .cloned()
                .unwrap(),
        })
        .unwrap();

    // Re-upsert preserves live state/volume_level on media_player and is_on on remote!
    store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "mp_main".into(),
            domain: "media_player".into(),
            name: Some("Living Room TV".into()),
            supported_features: mp_feats,
            source_list: Some(vec!["HDMI 1".into(), "Netflix".into()]),
            sound_mode_list: Some(vec!["Movie".into()]),
            volume_step: Some(0.2),
            ..Default::default()
        })
        .unwrap();
    store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "rem_main".into(),
            domain: "remote".into(),
            name: Some("Living Room Remote".into()),
            supported_features: crate::remote::feature::ACTIVITY,
            ..Default::default()
        })
        .unwrap();

    // Exercise media_player services across re-upsert.
    for (service, data) in [
        ("toggle", json!({"entity_id": mp_id})),
        ("media_play_pause", json!({"entity_id": mp_id})),
        ("volume_up", json!({"entity_id": mp_id})),
        ("volume_down", json!({"entity_id": mp_step_id})),
        (
            "play_media",
            json!({
                "entity_id": mp_id,
                "media": {
                    "media_content_type": "music",
                    "media_content_id": "http://example.com/song.mp3"
                },
                "enqueue": true,
                "extra": {"thumb": "ok"}
            }),
        ),
        (
            "join",
            json!({"entity_id": mp_id, "group_members": "media_player.Step_Speaker"}),
        ),
        (
            "select_source",
            json!({"entity_id": mp_id, "source": "Netflix"}),
        ),
        ("repeat_set", json!({"entity_id": mp_id, "repeat": "all"})),
    ] {
        hass.call_service(
            "media_player",
            service,
            data.as_object().cloned().unwrap(),
            None,
            false,
            None,
        )
        .await
        .unwrap();
    }

    // Exercise remote services across re-upsert (toggle while on -> turn_off).
    for (service, data) in [
        ("toggle", json!({"entity_id": rem_id, "activity": "TV"})),
        (
            "send_command",
            json!({"entity_id": rem_id, "command": "Power", "device": "tv"}),
        ),
        (
            "learn_command",
            json!({"entity_id": rem_id, "device": "tv", "command": "Mute", "timeout": 15}),
        ),
        (
            "delete_command",
            json!({"entity_id": rem_id, "command": ["Mute"], "device": "tv"}),
        ),
    ] {
        hass.call_service(
            "remote",
            service,
            data.as_object().cloned().unwrap(),
            None,
            false,
            None,
        )
        .await
        .unwrap();
    }

    let calls: Vec<(String, String, Value)> = recorded
        .lock()
        .iter()
        .map(|c| {
            (
                c.domain.clone(),
                c.service.clone(),
                Value::Object(c.data.clone()),
            )
        })
        .collect();
    assert_eq!(
        calls,
        vec![
            ("media_player".into(), "turn_off".into(), json!({})),
            ("media_player".into(), "media_pause".into(), json!({})),
            (
                "media_player".into(),
                "volume_set".into(),
                json!({"volume_level": 0.7})
            ),
            ("media_player".into(), "volume_down".into(), json!({})),
            (
                "media_player".into(),
                "play_media".into(),
                json!({
                    "media_content_type": "music",
                    "media_content_id": "http://example.com/song.mp3",
                    "enqueue": "add",
                    "extra": {"thumb": "ok"}
                })
            ),
            (
                "media_player".into(),
                "join".into(),
                json!({"group_members": ["media_player.step_speaker"]})
            ),
            (
                "media_player".into(),
                "select_source".into(),
                json!({"source": "Netflix"})
            ),
            (
                "media_player".into(),
                "repeat_set".into(),
                json!({"repeat": "all"})
            ),
            (
                "remote".into(),
                "turn_off".into(),
                json!({"activity": "TV"})
            ),
            (
                "remote".into(),
                "send_command".into(),
                json!({
                    "command": ["Power"],
                    "device": "tv",
                    "num_repeats": 1,
                    "hold_secs": 0.0
                })
            ),
            (
                "remote".into(),
                "learn_command".into(),
                json!({
                    "device": "tv",
                    "command": ["Mute"],
                    "timeout": 15
                })
            ),
            (
                "remote".into(),
                "delete_command".into(),
                json!({
                    "command": ["Mute"],
                    "device": "tv"
                })
            ),
        ]
    );

    // Disconnect (`mark_all_unavailable`) preserves media_player capability_attributes
    // (`source_list`, `sound_mode_list`) and omits state_attributes (`media_title`,
    // `volume_level`, and remote's `activity_list`/`current_activity`).
    store.mark_all_unavailable();
    let mp_eid = ha_core::EntityId::parse(&mp_id).unwrap();
    let mp_unavail = hass.states().get(&mp_eid).unwrap();
    assert_eq!(mp_unavail.state.as_str(), "unavailable");
    assert_eq!(
        mp_unavail.attributes["source_list"],
        json!(["HDMI 1", "Netflix"])
    );
    assert_eq!(mp_unavail.attributes["sound_mode_list"], json!(["Movie"]));
    assert!(!mp_unavail.attributes.contains_key("media_title"));
    assert!(!mp_unavail.attributes.contains_key("volume_level"));

    let rem_eid = ha_core::EntityId::parse(&rem_id).unwrap();
    let rem_unavail = hass.states().get(&rem_eid).unwrap();
    assert_eq!(rem_unavail.state.as_str(), "unavailable");
    assert!(!rem_unavail.attributes.contains_key("activity_list"));
    assert!(!rem_unavail.attributes.contains_key("current_activity"));

    store.deactivate_entry("e1");
    assert!(hass.states().get(&mp_eid).is_none());
    assert!(hass.states().get(&rem_eid).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn plugin_media_player_and_remote_round1_edge_cases() {
    let dir = std::env::temp_dir().join(format!("hars-plugin-mp-edge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let plug_dir = dir.join("plugins").join("tv_edge");
    std::fs::create_dir_all(&plug_dir).unwrap();
    std::fs::write(
        plug_dir.join("plugin.json"),
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "domain": "tv_edge",
            "name": "TV Edge",
            "version": "1.0.0",
            "transport": { "type": "unix_socket" },
            "capabilities": {
                "entity_domains": ["media_player", "remote"]
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let installed = Arc::new(manifest::InstalledPlugin::load(&plug_dir).unwrap());
    let hass = ha_core::Hass::new();
    let registry = ha_registry::Registry::in_memory(&hass, ha_registry::Registries::default());
    let components =
        crate::components::Components::new(&hass, registry.clone(), Default::default());
    let call_fn: entities::CallPluginFn = Arc::new(move |_| Box::pin(async { Ok(()) }));
    let store = entities::PluginEntityStore::new(installed, components, call_fn);
    store.activate_entry("e1");

    let mp_feats = crate::media_player::feature::SELECT_SOURCE
        | crate::media_player::feature::SELECT_SOUND_MODE;
    let mp_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "shared_uid".into(),
            domain: "media_player".into(),
            name: Some("TV".into()),
            supported_features: mp_feats,
            ..Default::default()
        })
        .unwrap();

    // 1. Same-entry cross-domain unique_id collision is rejected without touching the first entity.
    let err = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "shared_uid".into(),
            domain: "remote".into(),
            name: Some("TV Remote".into()),
            ..Default::default()
        })
        .unwrap_err();
    assert!(err.contains("already registered in domain"), "{err}");

    // 2. Live `state.set` capability updates sync to entity registry and survive omitted re-upsert.
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "shared_uid".into(),
            state: "playing".into(),
            attributes: json!({
                "source_list": ["HDMI 1", "Spotify"],
                "sound_mode_list": ["Movie"],
                "media_title": "Song",
                "volume_level": 0.5
            })
            .as_object()
            .cloned()
            .unwrap(),
        })
        .unwrap();
    let snap = registry.snapshot();
    let row = snap.entities.iter().find(|e| e.entity_id == mp_id).unwrap();
    assert_eq!(
        serde_json::to_value(&row.capabilities).unwrap(),
        json!({
            "source_list": ["HDMI 1", "Spotify"],
            "sound_mode_list": ["Movie"]
        })
    );

    // Re-upsert omitting source_list / sound_mode_list retains the live capabilities in registry & state.
    store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "shared_uid".into(),
            domain: "media_player".into(),
            name: Some("TV".into()),
            supported_features: mp_feats,
            ..Default::default()
        })
        .unwrap();
    let snap2 = registry.snapshot();
    let row2 = snap2
        .entities
        .iter()
        .find(|e| e.entity_id == mp_id)
        .unwrap();
    assert_eq!(
        serde_json::to_value(&row2.capabilities).unwrap(),
        json!({
            "source_list": ["HDMI 1", "Spotify"],
            "sound_mode_list": ["Movie"]
        })
    );

    // 3. Explicit `state.set("unavailable", ...)` strips state_attributes while keeping
    // capabilities and HA base entity attributes (`attribution`, `assumed_state`, etc.).
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "shared_uid".into(),
            state: "unavailable".into(),
            attributes: json!({
                "media_title": "Stale",
                "volume_level": 0.5,
                "attribution": "Data provided by TV",
                "assumed_state": true
            })
            .as_object()
            .cloned()
            .unwrap(),
        })
        .unwrap();
    let mp_eid = ha_core::EntityId::parse(&mp_id).unwrap();
    let mp_st = hass.states().get(&mp_eid).unwrap();
    assert_eq!(mp_st.state.as_str(), "unavailable");
    assert_eq!(
        mp_st.attributes["source_list"],
        json!(["HDMI 1", "Spotify"])
    );
    assert_eq!(mp_st.attributes["sound_mode_list"], json!(["Movie"]));
    assert_eq!(
        mp_st.attributes["attribution"],
        json!("Data provided by TV")
    );
    assert_eq!(mp_st.attributes["assumed_state"], json!(true));
    assert!(!mp_st.attributes.contains_key("media_title"));
    assert!(!mp_st.attributes.contains_key("volume_level"));

    // Clearing all live capability lists writes `capabilities: {}` (matching HA's dict return).
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "shared_uid".into(),
            state: "idle".into(),
            attributes: json!({"source_list": [], "sound_mode_list": []})
                .as_object()
                .cloned()
                .unwrap(),
        })
        .unwrap();
    let snap3 = registry.snapshot();
    let row3 = snap3
        .entities
        .iter()
        .find(|e| e.entity_id == mp_id)
        .unwrap();
    assert_eq!(serde_json::to_value(&row3.capabilities).unwrap(), json!({}));

    // Explicit `state.set("unavailable", ...)` on remote strips `current_activity` & `activity_list`,
    // while retaining HA base entity attributes (`attribution`, `assumed_state`).
    let rem_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "rem_uid".into(),
            domain: "remote".into(),
            name: Some("TV Remote".into()),
            supported_features: crate::remote::feature::ACTIVITY,
            ..Default::default()
        })
        .unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "rem_uid".into(),
            state: "unavailable".into(),
            attributes: json!({
                "current_activity": "TV",
                "activity_list": ["TV"],
                "attribution": "IR Hub",
                "assumed_state": true
            })
            .as_object()
            .cloned()
            .unwrap(),
        })
        .unwrap();
    let rem_eid = ha_core::EntityId::parse(&rem_id).unwrap();
    let rem_st = hass.states().get(&rem_eid).unwrap();
    assert_eq!(rem_st.state.as_str(), "unavailable");
    assert_eq!(rem_st.attributes["attribution"], json!("IR Hub"));
    assert_eq!(rem_st.attributes["assumed_state"], json!(true));
    assert!(!rem_st.attributes.contains_key("current_activity"));
    assert!(!rem_st.attributes.contains_key("activity_list"));

    // Falsy `assumed_state` values (`0`, `""`, `false`) are omitted when unavailable, matching HA.
    for falsy in [json!(0), json!(""), json!(false)] {
        store
            .set_state(protocol::StateSetParams {
                entry_id: "e1".into(),
                unique_id: "rem_uid".into(),
                state: "unavailable".into(),
                attributes: json!({"assumed_state": falsy})
                    .as_object()
                    .cloned()
                    .unwrap(),
            })
            .unwrap();
        let st = hass.states().get(&rem_eid).unwrap();
        assert!(
            !st.attributes.contains_key("assumed_state"),
            "falsy {falsy} should be omitted"
        );
    }

    store.deactivate_entry("e1");
    assert!(hass.states().get(&mp_eid).is_none());
    assert!(hass.states().get(&rem_eid).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn plugin_climate_all_services_capabilities_and_unavailable() {
    use crate::climate::feature as cf;

    let dir = temp_dir("plugin-climate");
    let plug_dir = dir.join("plugins").join("hvac_plug");
    std::fs::create_dir_all(&plug_dir).unwrap();
    std::fs::write(
        plug_dir.join("plugin.json"),
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "domain": "hvac_plug",
            "name": "HVAC Plug",
            "version": "1.0.0",
            "transport": { "type": "unix_socket" },
            "capabilities": { "entity_domains": ["climate"] }
        }))
        .unwrap(),
    )
    .unwrap();
    let installed = Arc::new(manifest::InstalledPlugin::load(&plug_dir).unwrap());
    let hass = Hass::new();
    let registry = Registry::in_memory(&hass, Registries::default());
    let components =
        crate::components::Components::new(&hass, registry.clone(), Default::default());
    let recorded: Arc<parking_lot::Mutex<Vec<protocol::EntityCallParams>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let recorded_cb = recorded.clone();
    let store = entities::PluginEntityStore::new(
        installed,
        components.clone(),
        Arc::new(move |call| {
            recorded_cb.lock().push(call);
            Box::pin(async { Ok(()) })
        }),
    );
    store.activate_entry("e1");

    let all_features = cf::TARGET_TEMPERATURE
        | cf::TARGET_TEMPERATURE_RANGE
        | cf::TARGET_HUMIDITY
        | cf::FAN_MODE
        | cf::PRESET_MODE
        | cf::SWING_MODE
        | cf::TURN_OFF
        | cf::TURN_ON
        | cf::SWING_HORIZONTAL_MODE;

    let eid_str = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "thermo-1".into(),
            domain: "climate".into(),
            name: Some("Hall Thermostat".into()),
            supported_features: all_features,
            hvac_modes: Some(vec![
                "off".into(),
                "heat".into(),
                "cool".into(),
                "heat_cool".into(),
            ]),
            fan_modes: Some(vec!["auto".into(), "low".into(), "high".into()]),
            preset_modes: Some(vec!["none".into(), "eco".into(), "away".into()]),
            swing_modes: Some(vec!["off".into(), "vertical".into()]),
            swing_horizontal_modes: Some(vec!["off".into(), "left".into(), "right".into()]),
            min_temp: json!(10).as_number().cloned(),
            max_temp: json!(30).as_number().cloned(),
            target_temp_step: json!(0.5).as_number().cloned(),
            min_humidity: json!(25).as_number().cloned(),
            max_humidity: json!(80).as_number().cloned(),
            target_humidity_step: json!(5).as_number().cloned(),
            ..Default::default()
        })
        .unwrap();
    let eid = ha_core::EntityId::parse(&eid_str).unwrap();

    // Registry capabilities snapshot matches HA's ClimateEntity.capability_attributes.
    let snap = registry.snapshot();
    let row = snap
        .entities
        .iter()
        .find(|e| e.entity_id == eid_str)
        .unwrap();
    assert_eq!(
        serde_json::to_value(&row.capabilities).unwrap(),
        json!({
            "hvac_modes": ["off", "heat", "cool", "heat_cool"],
            "min_temp": 10,
            "max_temp": 30,
            "target_temp_step": 0.5,
            "min_humidity": 25,
            "max_humidity": 80,
            "target_humidity_step": 5,
            "fan_modes": ["auto", "low", "high"],
            "preset_modes": ["none", "eco", "away"],
            "swing_modes": ["off", "vertical"],
            "swing_horizontal_modes": ["off", "left", "right"],
        })
    );

    // Publish initial state "heat".
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "thermo-1".into(),
            state: "heat".into(),
            attributes: json!({"current_temperature": 21.0, "temperature": 22.0})
                .as_object()
                .cloned()
                .unwrap(),
        })
        .unwrap();

    let call = |service: &'static str, mut extra: Map<String, Value>| {
        let hass = hass.clone();
        let eid_str = eid_str.clone();
        async move {
            extra.insert("entity_id".into(), json!(eid_str));
            hass.call_service("climate", service, extra, None, false, None)
                .await
        }
    };

    // Call all 10 climate services (all 10 reach the plugin with distinct RPC names).
    call("turn_on", Map::new()).await.unwrap();
    call("turn_off", Map::new()).await.unwrap();
    call("toggle", Map::new()).await.unwrap();
    call(
        "set_hvac_mode",
        json!({"hvac_mode": "cool"}).as_object().cloned().unwrap(),
    )
    .await
    .unwrap();
    call(
        "set_fan_mode",
        json!({"fan_mode": "high"}).as_object().cloned().unwrap(),
    )
    .await
    .unwrap();
    call(
        "set_preset_mode",
        json!({"preset_mode": "eco"}).as_object().cloned().unwrap(),
    )
    .await
    .unwrap();
    call(
        "set_humidity",
        json!({"humidity": 55}).as_object().cloned().unwrap(),
    )
    .await
    .unwrap();
    call(
        "set_swing_mode",
        json!({"swing_mode": "vertical"})
            .as_object()
            .cloned()
            .unwrap(),
    )
    .await
    .unwrap();
    call(
        "set_swing_horizontal_mode",
        json!({"swing_horizontal_mode": "left"})
            .as_object()
            .cloned()
            .unwrap(),
    )
    .await
    .unwrap();
    call(
        "set_temperature",
        json!({"temperature": 23.5, "hvac_mode": "heat"})
            .as_object()
            .cloned()
            .unwrap(),
    )
    .await
    .unwrap();

    // Validation rejections must not reach the plugin.
    assert!(
        call(
            "set_preset_mode",
            json!({"preset_mode": "bogus"})
                .as_object()
                .cloned()
                .unwrap()
        )
        .await
        .is_err()
    );
    assert!(
        call(
            "set_humidity",
            json!({"humidity": 90}).as_object().cloned().unwrap()
        )
        .await
        .is_err()
    );
    assert!(
        call(
            "set_swing_mode",
            json!({"swing_mode": "bogus"}).as_object().cloned().unwrap()
        )
        .await
        .is_err()
    );
    assert!(
        call(
            "set_swing_horizontal_mode",
            json!({"swing_horizontal_mode": "bogus"})
                .as_object()
                .cloned()
                .unwrap()
        )
        .await
        .is_err()
    );

    {
        let guard = recorded.lock();
        let calls: Vec<(&str, Value)> = guard
            .iter()
            .map(|c| {
                assert_eq!(c.domain, "climate");
                assert_eq!(c.unique_id, "thermo-1");
                (c.service.as_str(), Value::Object(c.data.clone()))
            })
            .collect();
        assert_eq!(calls.len(), 10);
        assert_eq!(calls[0], ("turn_on", json!({})));
        assert_eq!(calls[1], ("turn_off", json!({})));
        assert_eq!(calls[2], ("toggle", json!({})));
        assert_eq!(calls[3], ("set_hvac_mode", json!({"hvac_mode": "cool"})));
        assert_eq!(calls[4], ("set_fan_mode", json!({"fan_mode": "high"})));
        assert_eq!(calls[5], ("set_preset_mode", json!({"preset_mode": "eco"})));
        assert_eq!(calls[6], ("set_humidity", json!({"humidity": 55})));
        assert_eq!(
            calls[7],
            ("set_swing_mode", json!({"swing_mode": "vertical"}))
        );
        assert_eq!(
            calls[8],
            (
                "set_swing_horizontal_mode",
                json!({"swing_horizontal_mode": "left"})
            )
        );
        assert_eq!(
            calls[9],
            (
                "set_temperature",
                json!({"temperature": 23.5, "hvac_mode": "heat"})
            )
        );
    }

    // Live capability update via state.set propagates to registry and survives omitted re-upsert.
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "thermo-1".into(),
            state: "cool".into(),
            attributes: json!({
                "preset_modes": ["none", "boost"],
                "max_humidity": 90,
                "target_humidity_step": 1,
                "current_temperature": 22.0,
            })
            .as_object()
            .cloned()
            .unwrap(),
        })
        .unwrap();
    store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "thermo-1".into(),
            domain: "climate".into(),
            name: Some("Hall Thermostat".into()),
            supported_features: all_features,
            ..Default::default()
        })
        .unwrap();
    let snap2 = registry.snapshot();
    let row2 = snap2
        .entities
        .iter()
        .find(|e| e.entity_id == eid_str)
        .unwrap();
    let caps2 = serde_json::to_value(&row2.capabilities).unwrap();
    assert_eq!(caps2["preset_modes"], json!(["none", "boost"]));
    assert_eq!(caps2["max_humidity"], json!(90));
    assert_eq!(caps2["target_humidity_step"], json!(1));
    assert_eq!(
        caps2["hvac_modes"],
        json!(["off", "heat", "cool", "heat_cool"])
    );

    // Newly allowed preset & humidity now succeed.
    call(
        "set_preset_mode",
        json!({"preset_mode": "boost"})
            .as_object()
            .cloned()
            .unwrap(),
    )
    .await
    .unwrap();
    call(
        "set_humidity",
        json!({"humidity": 88}).as_object().cloned().unwrap(),
    )
    .await
    .unwrap();

    // Explicit unavailable strips state_attributes while keeping capabilities + base entity attrs.
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "thermo-1".into(),
            state: "unavailable".into(),
            attributes: json!({
                "current_temperature": 22.0,
                "temperature": 23.5,
                "attribution": "HVAC Cloud",
            })
            .as_object()
            .cloned()
            .unwrap(),
        })
        .unwrap();
    let st = hass.states().get(&eid).unwrap();
    assert_eq!(st.state.as_str(), "unavailable");
    assert_eq!(st.attributes["attribution"], json!("HVAC Cloud"));
    assert_eq!(
        st.attributes["hvac_modes"],
        json!(["off", "heat", "cool", "heat_cool"])
    );
    assert_eq!(st.attributes["preset_modes"], json!(["none", "boost"]));
    assert!(!st.attributes.contains_key("current_temperature"));
    assert!(!st.attributes.contains_key("temperature"));

    // Disconnect mark_all_unavailable keeps capability attributes; deactivate removes entity.
    store.mark_all_unavailable();
    let st2 = hass.states().get(&eid).unwrap();
    assert_eq!(st2.state.as_str(), "unavailable");
    assert_eq!(st2.attributes["min_temp"], json!(10));
    assert_eq!(st2.attributes["max_humidity"], json!(90));

    store.deactivate_entry("e1");
    assert!(hass.states().get(&eid).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn shared_notify_component_fans_out_mobile_app_and_plugin_entities() {
    #[derive(Debug)]
    struct MockRelay {
        sent: parking_lot::Mutex<Vec<(String, Value)>>,
    }

    impl crate::mobile_app::notify::Relay for MockRelay {
        fn post(&self, url: &str, body: String) -> crate::mobile_app::notify::RelayFuture {
            let parsed: Value = serde_json::from_str(&body).unwrap();
            self.sent.lock().push((url.to_owned(), parsed));
            Box::pin(async {
                Ok(crate::mobile_app::notify::RelayAnswer {
                    status: 201,
                    content_type: "application/json".into(),
                    body: br#"{"rateLimits":{"attempts":1,"successful":1,"errors":0,"total":1,"maximum":500,"remaining":499,"resetsAt":"2026-01-01T00:00:00.000Z"}}"#.to_vec(),
                })
            })
        }
    }

    let dir = temp_dir("shared-notify");
    let plug_dir = dir.join("plugins").join("ntfy_plug");
    std::fs::create_dir_all(&plug_dir).unwrap();
    std::fs::write(
        plug_dir.join("plugin.json"),
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "domain": "ntfy_plug",
            "name": "Ntfy Plug",
            "version": "1.0.0",
            "transport": { "type": "unix_socket" },
            "capabilities": { "entity_domains": ["notify"] }
        }))
        .unwrap(),
    )
    .unwrap();
    let installed = Arc::new(manifest::InstalledPlugin::load(&plug_dir).unwrap());
    let hass = Hass::new();
    let registry = Registry::in_memory(
        &hass,
        Registries {
            entities: vec![Arc::new(ha_registry::Entity {
                entity_id: "notify.test_phone".into(),
                platform: "mobile_app".into(),
                unique_id: Some(json!("phone-1")),
                config_entry_id: Some("ma-entry-1".into()),
                ..Default::default()
            })],
            ..Default::default()
        },
    );
    let components =
        crate::components::Components::new(&hass, registry.clone(), Default::default());
    let config_entries = ConfigEntries::load(&hass, &dir);

    // 1. Set up a mobile_app registration wired onto the shared Components.
    let entry_map = json!({
        "entry_id": "ma-entry-1",
        "domain": "mobile_app",
        "title": "Test Phone",
        "data": {
            "webhook_id": "wh-1",
            "device_id": "phone-1",
            "device_name": "Test Phone",
            "app_id": "io.home-assistant.test",
            "app_version": "1.0",
            "os_version": "18.0",
            "app_data": {
                "push_url": "https://relay.example.test/send",
                "push_token": "tok-123"
            }
        }
    })
    .as_object()
    .cloned()
    .unwrap();
    let relay = Arc::new(MockRelay {
        sent: parking_lot::Mutex::new(Vec::new()),
    });
    let mobile_app = crate::mobile_app::MobileApp::new(&[entry_map], vec![]);
    mobile_app.set_entities(components.clone());
    mobile_app.set_relay(relay.clone());
    mobile_app.setup(&hass, registry.clone(), config_entries, None);

    // 2. Register two plugin notify entities (one supporting TITLE, one not).
    let plugin_calls: Arc<parking_lot::Mutex<Vec<protocol::EntityCallParams>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let plugin_calls_cb = plugin_calls.clone();
    let store = entities::PluginEntityStore::new(
        installed,
        components.clone(),
        Arc::new(move |call| {
            plugin_calls_cb.lock().push(call);
            Box::pin(async { Ok(()) })
        }),
    );
    store.activate_entry("p1");
    let titled_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "p1".into(),
            unique_id: "topic-titled".into(),
            domain: "notify".into(),
            name: Some("Alerts".into()),
            supported_features: crate::notify::feature::TITLE,
            ..Default::default()
        })
        .unwrap();
    let plain_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "p1".into(),
            unique_id: "topic-plain".into(),
            domain: "notify".into(),
            name: Some("Plain".into()),
            supported_features: 0,
            ..Default::default()
        })
        .unwrap();

    // 3. Single `notify.send_message` with `entity_id: "all"` hits mobile_app + both plugins.
    hass.call_service(
        "notify",
        "send_message",
        json!({"entity_id": "all", "message": "hello all", "title": "Heads up"})
            .as_object()
            .cloned()
            .unwrap(),
        None,
        false,
        None,
    )
    .await
    .unwrap();

    // Verify mobile_app relay received it.
    {
        let sent = relay.sent.lock();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1["message"], json!("hello all"));
        assert_eq!(sent[0].1["title"], json!("Heads up"));
    }

    // Verify both plugin notify entities received it, and TITLE was stripped for the plain one.
    {
        let calls = plugin_calls.lock();
        assert_eq!(calls.len(), 2);
        let titled = calls
            .iter()
            .find(|c| c.unique_id == "topic-titled")
            .unwrap();
        assert_eq!(titled.domain, "notify");
        assert_eq!(titled.service, "send_message");
        assert_eq!(
            Value::Object(titled.data.clone()),
            json!({"message": "hello all", "title": "Heads up"})
        );
        let plain = calls.iter().find(|c| c.unique_id == "topic-plain").unwrap();
        assert_eq!(
            Value::Object(plain.data.clone()),
            json!({"message": "hello all"})
        );
    }

    // Plugin notify entities recorded ISO timestamp state + friendly_name + supported_features.
    let titled_eid = ha_core::EntityId::parse(&titled_id).unwrap();
    let st = hass.states().get(&titled_eid).unwrap();
    let recorded_ts = st.state.as_str().to_owned();
    assert!(
        recorded_ts.contains('T'),
        "expected ISO timestamp, got {recorded_ts}"
    );
    assert_eq!(st.attributes["friendly_name"], json!("Alerts"));
    assert_eq!(st.attributes["supported_features"], json!(1));

    // Preserves last_notified across unavailable -> unknown recovery and re-upsert.
    store
        .set_state(protocol::StateSetParams {
            entry_id: "p1".into(),
            unique_id: "topic-titled".into(),
            state: "unavailable".into(),
            attributes: Map::new(),
        })
        .unwrap();
    assert_eq!(
        hass.states().get(&titled_eid).unwrap().state.as_str(),
        "unavailable"
    );
    store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "p1".into(),
            unique_id: "topic-titled".into(),
            domain: "notify".into(),
            name: Some("Alerts".into()),
            supported_features: crate::notify::feature::TITLE,
            ..Default::default()
        })
        .unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "p1".into(),
            unique_id: "topic-titled".into(),
            state: "unknown".into(),
            attributes: Map::new(),
        })
        .unwrap();
    assert_eq!(
        hass.states().get(&titled_eid).unwrap().state.as_str(),
        recorded_ts
    );

    // Deactivating the plugin entry detaches both plugin notify entities while mobile_app stays live.
    store.deactivate_entry("p1");
    let plain_eid = ha_core::EntityId::parse(&plain_id).unwrap();
    assert!(hass.states().get(&titled_eid).is_none());
    assert!(hass.states().get(&plain_eid).is_none());

    hass.call_service(
        "notify",
        "send_message",
        json!({"entity_id": "all", "message": "only phone"})
            .as_object()
            .cloned()
            .unwrap(),
        None,
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(relay.sent.lock().len(), 2);
    assert_eq!(plugin_calls.lock().len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(start_paused = true)]
async fn climate_and_notify_round1_regressions() {
    use crate::climate::feature as cf;
    use ha_core::ServiceError;

    let dir = temp_dir("climate-notify-r1");
    let plug_dir = dir.join("plugins").join("r1_plug");
    std::fs::create_dir_all(&plug_dir).unwrap();
    std::fs::write(
        plug_dir.join("plugin.json"),
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "domain": "r1_plug",
            "name": "R1 Plug",
            "version": "1.0.0",
            "transport": { "type": "unix_socket" },
            "capabilities": { "entity_domains": ["climate", "notify"] }
        }))
        .unwrap(),
    )
    .unwrap();
    let installed = Arc::new(manifest::InstalledPlugin::load(&plug_dir).unwrap());
    let hass = Hass::new();
    let registry = Registry::in_memory(&hass, Registries::default());
    let components =
        crate::components::Components::new(&hass, registry.clone(), Default::default());

    let gate = Arc::new(tokio::sync::Notify::new());
    let hold_first = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen_calls: Arc<parking_lot::Mutex<Vec<protocol::EntityCallParams>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let gate_cb = gate.clone();
    let hold_cb = hold_first.clone();
    let seen_cb = seen_calls.clone();
    let store = entities::PluginEntityStore::new(
        installed,
        components.clone(),
        Arc::new(move |call| {
            let should_hold = hold_cb.swap(false, Ordering::SeqCst);
            seen_cb.lock().push(call);
            let gate = gate_cb.clone();
            Box::pin(async move {
                if should_hold {
                    gate.notified().await;
                }
                Ok(())
            })
        }),
    );
    store.activate_entry("e1");

    // 1. Climate display rounding (7.25 -> 7.2), float min_humidity (30.0 -> "30.0"),
    //    Unicode digit & big-int humidity coercion, and cleared steps staying absent when unavailable.
    let clim_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "c1".into(),
            domain: "climate".into(),
            name: Some("Rounding Stat".into()),
            supported_features: cf::TARGET_TEMPERATURE | cf::TARGET_HUMIDITY,
            hvac_modes: Some(vec!["off".into(), "heat".into()]),
            min_temp: json!(7.25).as_number().cloned(),
            max_temp: json!(30).as_number().cloned(),
            target_temp_step: json!(0.5).as_number().cloned(),
            min_humidity: json!(30.0).as_number().cloned(),
            max_humidity: json!(80).as_number().cloned(),
            target_humidity_step: json!(1).as_number().cloned(),
            ..Default::default()
        })
        .unwrap();
    let clim_eid = ha_core::EntityId::parse(&clim_id).unwrap();
    let snap = registry.snapshot();
    let row = snap
        .entities
        .iter()
        .find(|e| e.entity_id == clim_id)
        .unwrap();
    let caps = serde_json::to_value(&row.capabilities).unwrap();
    assert_eq!(caps["min_temp"], json!(7.2));
    assert_eq!(caps["max_temp"], json!(30));

    // `humidity: true` coerces to 1 and fails with `"min_humidity": "30.0"`.
    let err = hass
        .call_service(
            "climate",
            "set_humidity",
            json!({"entity_id": clim_id, "humidity": true})
                .as_object()
                .cloned()
                .unwrap(),
            None,
            false,
            None,
        )
        .await
        .unwrap_err();
    match err {
        ServiceError::Validation(r) => {
            assert_eq!(r.key, "humidity_out_of_range");
            let p = r.placeholders.as_ref().unwrap();
            assert_eq!(p["humidity"], "1");
            assert_eq!(p["min_humidity"], "30.0");
            assert_eq!(p["max_humidity"], "80");
        }
        other => panic!("expected validation error, got {other:?}"),
    }

    // Unicode fullwidth digits `"５５"` coerce to 55 and reach the plugin.
    hass.call_service(
        "climate",
        "set_humidity",
        json!({"entity_id": clim_id, "humidity": "５５"})
            .as_object()
            .cloned()
            .unwrap(),
        None,
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        seen_calls.lock().last().unwrap().data["humidity"],
        json!(55)
    );

    // Arbitrary-precision integer string `"18446744073709551616"` reaches range check.
    let err_big = hass
        .call_service(
            "climate",
            "set_humidity",
            json!({"entity_id": clim_id, "humidity": "18446744073709551616"})
                .as_object()
                .cloned()
                .unwrap(),
            None,
            false,
            None,
        )
        .await
        .unwrap_err();
    match err_big {
        ServiceError::Validation(r) => {
            assert_eq!(r.key, "humidity_out_of_range");
            assert_eq!(
                r.placeholders.as_ref().unwrap()["humidity"],
                "18446744073709551616"
            );
        }
        other => panic!("expected validation error, got {other:?}"),
    }

    // Clear target_temp_step & target_humidity_step with null; verify they do NOT resurrect on unavailable.
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "c1".into(),
            state: "heat".into(),
            attributes: json!({"target_temp_step": null, "target_humidity_step": null})
                .as_object()
                .cloned()
                .unwrap(),
        })
        .unwrap();
    store
        .set_state(protocol::StateSetParams {
            entry_id: "e1".into(),
            unique_id: "c1".into(),
            state: "unavailable".into(),
            attributes: Map::new(),
        })
        .unwrap();
    let st_unavail = hass.states().get(&clim_eid).unwrap();
    assert!(!st_unavail.attributes.contains_key("target_temp_step"));
    assert!(!st_unavail.attributes.contains_key("target_humidity_step"));
    store.mark_all_unavailable();
    let st_disc = hass.states().get(&clim_eid).unwrap();
    assert!(!st_disc.attributes.contains_key("target_temp_step"));
    assert!(!st_disc.attributes.contains_key("target_humidity_step"));

    // 2. Plugin notify: initial "unknown", in-flight re-upsert timestamp preservation,
    //    5s context expiry, restore across reload, and 33-target concurrent fan-out.
    let n_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "n1".into(),
            domain: "notify".into(),
            name: Some("Slow Notify".into()),
            supported_features: crate::notify::feature::TITLE,
            ..Default::default()
        })
        .unwrap();
    let n_eid = ha_core::EntityId::parse(&n_id).unwrap();
    assert_eq!(hass.states().get(&n_eid).unwrap().state.as_str(), "unknown");

    // Hold send_message in flight, re-upsert same entity, advance clock 6s (past 5s context window), then release.
    hold_first.store(true, Ordering::SeqCst);
    let orig_ctx = Context::new();
    let orig_ctx_id = orig_ctx.id;
    let hass_send = hass.clone();
    let n_id_send = n_id.clone();
    let send_task = tokio::spawn(async move {
        hass_send
            .call_service(
                "notify",
                "send_message",
                json!({"entity_id": n_id_send, "message": "delayed"})
                    .as_object()
                    .cloned()
                    .unwrap(),
                None,
                false,
                Some(orig_ctx),
            )
            .await
    });
    tokio::task::yield_now().await;
    // Re-upsert while send is in flight with a new name AND cleared TITLE feature mask.
    store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "n1".into(),
            domain: "notify".into(),
            name: Some("Renamed Notify".into()),
            supported_features: 0,
            ..Default::default()
        })
        .unwrap();
    tokio::time::advance(Duration::from_secs(6)).await;
    gate.notify_one();
    send_task.await.unwrap().unwrap();

    let after_send = hass.states().get(&n_eid).unwrap();
    let saved_ts = after_send.state.as_str().to_owned();
    assert!(saved_ts.contains('T'), "expected timestamp, got {saved_ts}");
    assert_eq!(
        after_send.attributes["friendly_name"],
        json!("Renamed Notify")
    );
    assert_eq!(
        after_send.attributes["supported_features"],
        json!(0),
        "completion must publish the latest re-upserted supported_features"
    );
    assert_ne!(
        after_send.context.id, orig_ctx_id,
        "context must expire after >5s"
    );

    // Unload & reload restores the saved timestamp via `hass.restore()`.
    store.deactivate_entry("e1");
    assert!(hass.states().get(&n_eid).is_none());
    store.activate_entry("e1");
    store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "n1".into(),
            domain: "notify".into(),
            name: Some("Renamed Notify".into()),
            supported_features: crate::notify::feature::TITLE,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hass.states().get(&n_eid).unwrap().state.as_str(), saved_ts);

    // 33-target concurrent fan-out: target 33 starts even while target 1 is held open!
    for i in 2..=33 {
        store
            .upsert_entity(protocol::EntityUpsertParams {
                entry_id: "e1".into(),
                unique_id: format!("n{i}"),
                domain: "notify".into(),
                name: Some(format!("Notify {i}")),
                ..Default::default()
            })
            .unwrap();
    }
    seen_calls.lock().clear();
    hold_first.store(true, Ordering::SeqCst);
    let hass_fan = hass.clone();
    let fan_task = tokio::spawn(async move {
        hass_fan
            .call_service(
                "notify",
                "send_message",
                json!({"entity_id": "all", "message": "fanout"})
                    .as_object()
                    .cloned()
                    .unwrap(),
                None,
                false,
                None,
            )
            .await
    });
    for _ in 0..20 {
        tokio::task::yield_now().await;
        if seen_calls.lock().len() == 33 {
            break;
        }
    }
    assert_eq!(
        seen_calls.lock().len(),
        33,
        "all 33 targets should start concurrently while target 1 is held"
    );
    gate.notify_one();
    fan_task.await.unwrap().unwrap();

    // 3. Exact arbitrary-precision humidity check: 18446744073709551617 > 18446744073709551616.0
    //    must fail range validation instead of rounding equal in f64.
    let big_clim_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "c_big".into(),
            domain: "climate".into(),
            name: Some("Big Bound Stat".into()),
            supported_features: cf::TARGET_HUMIDITY,
            hvac_modes: Some(vec!["off".into(), "heat".into()]),
            min_humidity: json!(0).as_number().cloned(),
            max_humidity: json!(18446744073709551616.0_f64).as_number().cloned(),
            ..Default::default()
        })
        .unwrap();
    let err_exact = hass
        .call_service(
            "climate",
            "set_humidity",
            json!({"entity_id": big_clim_id, "humidity": "18446744073709551617"})
                .as_object()
                .cloned()
                .unwrap(),
            None,
            false,
            None,
        )
        .await
        .unwrap_err();
    match err_exact {
        ServiceError::Validation(r) => {
            assert_eq!(r.key, "humidity_out_of_range");
            assert_eq!(
                r.placeholders.as_ref().unwrap()["humidity"],
                "18446744073709551617"
            );
        }
        other => panic!("expected validation error, got {other:?}"),
    }

    // 4. US customary (°F) uses whole-degree precision for climate min_temp/max_temp (68.6 -> 69).
    let mut us_cfg = (*hass.config()).clone();
    us_cfg.unit_system_v2 = "us_customary".into();
    hass.set_config(us_cfg);
    let us_clim_id = store
        .upsert_entity(protocol::EntityUpsertParams {
            entry_id: "e1".into(),
            unique_id: "c_us".into(),
            domain: "climate".into(),
            name: Some("US Stat".into()),
            supported_features: cf::TARGET_TEMPERATURE,
            hvac_modes: Some(vec!["off".into(), "heat".into()]),
            min_temp: json!(68.6).as_number().cloned(),
            max_temp: json!(80.4).as_number().cloned(),
            ..Default::default()
        })
        .unwrap();
    let us_snap = registry.snapshot();
    let us_row = us_snap
        .entities
        .iter()
        .find(|e| e.entity_id == us_clim_id)
        .unwrap();
    let us_caps = serde_json::to_value(&us_row.capabilities).unwrap();
    assert_eq!(us_caps["min_temp"], json!(69));
    assert_eq!(us_caps["max_temp"], json!(80));

    // 5. Deactivating a plugin entry before a constructed notify future is polled skips the RPC.
    seen_calls.lock().clear();
    let deferred_fut = {
        let hass_d = hass.clone();
        let n_id_d = n_id.clone();
        async move {
            hass_d
                .call_service(
                    "notify",
                    "send_message",
                    json!({"entity_id": n_id_d, "message": "should skip"})
                        .as_object()
                        .cloned()
                        .unwrap(),
                    None,
                    false,
                    None,
                )
                .await
        }
    };
    store.deactivate_entry("e1");
    deferred_fut.await.unwrap();
    assert!(
        seen_calls.lock().is_empty(),
        "deactivated notify entry must not receive RPC"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
