//! `hars import`: an HA 2026.6 config directory taken over, sessions intact.

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn hars(config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hars"))
        .env("HARS_CONFIG", config)
        .args(args)
        .output()
        .unwrap()
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hars-import-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn write_store(dir: &Path, key: &str, version: u32, minor: u32, data: Value) {
    let v = json!({"version": version, "minor_version": minor, "key": key, "data": data});
    std::fs::write(
        dir.join(".storage").join(key),
        serde_json::to_vec_pretty(&v).unwrap(),
    )
    .unwrap();
}

const CLIENT: &str = "http://127.0.0.1:8123/";

/// An HA 2026.6.1 config directory: an owner with a browser session, a
/// location, the owner's frontend settings, two integrations, things hars
/// doesn't import yet, and YAML.
fn ha_config(dir: &Path) -> String {
    // HA's own formats; `hars auth add` writes the same files HA does.
    assert!(
        hars(dir, &["auth", "add", "adam", "secret"])
            .status
            .success()
    );
    std::fs::write(dir.join(".HA_VERSION"), "2026.6.1").unwrap();
    let mut auth = read_json(&dir.join(".storage/auth"));
    let owner = auth["data"]["users"][0]["id"].as_str().unwrap().to_owned();
    let cred = auth["data"]["credentials"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    auth["data"]["refresh_tokens"] = json!([{
        "id": "browser-session", "user_id": owner, "client_id": CLIENT, "client_name": null,
        "client_icon": null, "token_type": "normal", "created_at": "2026-06-10T12:34:56.789012+00:00",
        "access_token_expiration": 1800.0, "token": "a".repeat(128), "jwt_key": "b".repeat(128),
        "last_used_at": null, "last_used_ip": null, "expire_at": 4102444800.0,
        "credential_id": cred, "version": "2026.6.1"
    }]);
    std::fs::write(
        dir.join(".storage/auth"),
        serde_json::to_vec_pretty(&auth).unwrap(),
    )
    .unwrap();
    write_store(
        dir,
        "core.config",
        1,
        4,
        json!({
            "latitude": 52.37, "longitude": 4.89, "elevation": 3, "unit_system_v2": "metric",
            "location_name": "Home", "time_zone": "Europe/Amsterdam", "external_url": null,
            "internal_url": null, "currency": "EUR", "country": "NL", "language": "en", "radius": 100
        }),
    );
    write_store(
        dir,
        &format!("frontend.user_data_{owner}"),
        1,
        1,
        json!({"core": {"default_panel": "home"}}),
    );
    write_store(
        dir,
        "core.config_entries",
        1,
        5,
        json!({"entries": [
            {"entry_id": "e1", "domain": "hue", "title": "Hue Bridge", "disabled_by": null, "data": {"host": "10.0.0.2", "api_key": "secret-key"}},
            {"entry_id": "e2", "domain": "met", "title": "Home", "disabled_by": "user", "data": {}}
        ]}),
    );
    write_store(
        dir,
        "core.entity_registry",
        1,
        22,
        json!({"entities": [], "deleted_entities": []}),
    );
    write_store(
        dir,
        "lovelace.map",
        1,
        1,
        json!({"config": {"strategy": {"type": "map"}}}),
    );
    for f in ["configuration.yaml", "automations.yaml", "secrets.yaml"] {
        std::fs::write(dir.join(f), "# yaml\n").unwrap();
    }
    owner
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A tiny HTTP/1.1 client: status and body.
fn http(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out[9..12].parse().unwrap();
    (status, out.split_once("\r\n\r\n").unwrap().1.to_owned())
}

#[test]
fn imports_an_ha_install_and_keeps_its_sessions() {
    let src = temp("src");
    let dest = temp("dest");
    let owner = ha_config(&src);
    let user_data = format!("frontend.user_data_{owner}");

    // A dry run writes nothing.
    let out = hars(&dest, &["import", src.to_str().unwrap(), "--dry-run"]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("would import 5 store(s)"));
    assert!(!dest.join(".storage").exists() && !dest.join("import-report.json").exists());

    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("integrations not in hars yet: hue (Hue Bridge), met (Home)"),
        "{stdout}"
    );

    // Byte for byte, with HA's file modes.
    for key in [
        "auth",
        "auth_provider.homeassistant",
        "onboarding",
        "core.config",
        user_data.as_str(),
    ] {
        let a = std::fs::read(src.join(".storage").join(key)).unwrap();
        let b = std::fs::read(dest.join(".storage").join(key)).unwrap();
        assert_eq!(a, b, "{key}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |k: &str| {
            std::fs::metadata(dest.join(".storage").join(k))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("auth"), 0o600);
        assert_eq!(mode(&user_data), 0o644);
    }
    let report = read_json(&dest.join("import-report.json"));
    assert_eq!(report["importer_version"], 1);
    assert_eq!(report["ha_version"], "2026.6.1");
    let mut imported = vec![
        "auth",
        "auth_provider.homeassistant",
        "core.config",
        "onboarding",
        user_data.as_str(),
    ];
    imported.sort();
    assert_eq!(report["imported"], json!(imported));
    let skipped: Vec<&str> = report["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        skipped,
        [
            "core.config_entries",
            "core.entity_registry",
            "lovelace.map"
        ]
    );
    assert_eq!(
        report["config_entries"][0],
        json!({"entry_id": "e1", "domain": "hue", "title": "Hue Bridge", "disabled": false, "supported": false})
    );
    assert_eq!(report["config_entries"][1]["disabled"], true);
    // secrets.yaml is never listed (nor read).
    assert_eq!(
        report["yaml"],
        json!(["configuration.yaml", "automations.yaml"])
    );
    assert!(
        !report.to_string().contains("secret-key"),
        "integration secrets stay out of the report"
    );

    // Idempotent: the same import again changes nothing.
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        read_json(&dest.join("import-report.json"))["replaced"],
        json!([])
    );

    // The browser's HA session works against hars.
    let port = free_port();
    let mut server = Command::new(env!("CARGO_BIN_EXE_hars"))
        .env("HARS_CONFIG", &dest)
        .env("HARS_BIND", format!("127.0.0.1:{port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "hars did not start");
        std::thread::sleep(Duration::from_millis(5));
    }
    let form = format!(
        "grant_type=refresh_token&client_id={}&refresh_token={}",
        "http%3A%2F%2F127.0.0.1%3A8123%2F",
        "a".repeat(128)
    );
    let (status, body) = http(
        port,
        "POST",
        "/auth/token",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        &form,
    );
    assert_eq!(status, 200, "{body}");
    let access = serde_json::from_str::<Value>(&body).unwrap()["access_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, body) = http(
        port,
        "GET",
        "/api/config",
        &[("Authorization", &format!("Bearer {access}"))],
        "",
    );
    assert_eq!(status, 200, "{body}");
    let config: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (config["latitude"].clone(), config["time_zone"].clone()),
        (json!(52.37), json!("Europe/Amsterdam"))
    );
    let _ = Command::new("kill")
        .args(["-TERM", &server.id().to_string()])
        .status();
    server.wait().unwrap();

    // hars has written its own state since: a re-import needs --force.
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--force"),
        "{out:?}"
    );
    let out = hars(&dest, &["import", src.to_str().unwrap(), "--force"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        read_json(&dest.join("import-report.json"))["replaced"],
        json!(["auth"])
    );

    let _ = std::fs::remove_dir_all(src);
    let _ = std::fs::remove_dir_all(dest);
}

#[test]
fn refuses_other_ha_versions_and_its_own_directory() {
    let src = temp("old");
    ha_config(&src);
    std::fs::write(src.join(".HA_VERSION"), "2025.12.4").unwrap();
    let dest = temp("old-dest");
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Home Assistant 2025.12.4"),
        "{out:?}"
    );
    assert!(!dest.join(".storage").exists());

    std::fs::write(src.join(".HA_VERSION"), "2026.6.1").unwrap();
    let out = hars(&src, &["import", src.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("same directory"));

    let no_ha = temp("not-ha");
    std::fs::create_dir_all(&no_ha).unwrap();
    let out = hars(&dest, &["import", no_ha.to_str().unwrap()]);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(".HA_VERSION"),
        "{out:?}"
    );
    for d in [src, dest, no_ha] {
        let _ = std::fs::remove_dir_all(d);
    }
}
