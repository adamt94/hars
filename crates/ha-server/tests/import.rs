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

/// An automation's entity registry row, as HA 2026.6.1 writes it.
fn automation_row(id: &str, alias: &str) -> Value {
    json!({
        "aliases": [], "aliases_v2": [null], "area_id": null, "categories": {},
        "capabilities": {"id": id}, "config_entry_id": null, "config_subentry_id": null,
        "created_at": "2026-06-06T19:42:30.014344+00:00", "device_class": null,
        "device_id": null, "disabled_by": null, "entity_category": null,
        "entity_id": format!("automation.{}", alias.to_lowercase()), "hidden_by": null,
        "icon": null, "id": format!("{id:0>32}"), "has_entity_name": false, "labels": [],
        "modified_at": "2026-07-25T20:31:50.773133+00:00", "name": null,
        "object_id_base": alias, "options": {"conversation": {"should_expose": false}},
        "original_device_class": null, "original_icon": null, "original_name": alias,
        "platform": "automation", "suggested_object_id": null, "supported_features": 0,
        "translation_key": null, "unique_id": id, "previous_unique_id": null,
        "unit_of_measurement": null
    })
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
            {"entry_id": "e2", "domain": "met", "title": "Home", "disabled_by": "user", "data": {}},
            {"entry_id": "e3", "domain": "radio_browser", "title": "Radio Browser", "disabled_by": null, "data": {}}
        ]}),
    );
    write_store(
        dir,
        "core.entity_registry",
        1,
        22,
        // The rows HA made for the two automations below: hars finds
        // them as they are, so the file is not written again.
        json!({"entities": [automation_row("ok", "Fine"), automation_row("wait", "Waiting")],
               "deleted_entities": []}),
    );
    write_store(
        dir,
        "lovelace.map",
        1,
        1,
        json!({"config": {"strategy": {"type": "map"}}}),
    );
    std::fs::write(
        dir.join("configuration.yaml"),
        "default_config:\nautomation: !include automations.yaml\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("automations.yaml"),
        "- id: ok\n  alias: Fine\n  triggers: [{trigger: time, at: '23:30:00'}]\n  actions: [{action: light.turn_off}]\n\
         - id: wait\n  alias: Waiting\n  triggers: [{trigger: time, at: '23:30:00'}]\n  actions: [{delay: '{{ 5 }}'}]\n",
    )
    .unwrap();
    // Not used by any file: never copied.
    std::fs::write(dir.join("secrets.yaml"), "pw: secret-value\n").unwrap();
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
    assert!(String::from_utf8_lossy(&out.stdout).contains("would import 8 store(s)"));
    assert!(!dest.join(".storage").exists() && !dest.join("import-report.json").exists());

    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("integrations not in hars yet: radio_browser (Radio Browser)\n"),
        "{stdout}"
    );

    // Byte for byte, with HA's file modes.
    for key in [
        "auth",
        "auth_provider.homeassistant",
        "onboarding",
        "core.config",
        "core.entity_registry",
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
    assert_eq!(report["importer_version"], 2);
    assert_eq!(report["ha_version"], "2026.6.1");
    let mut imported = vec![
        "auth",
        "auth_provider.homeassistant",
        "core.config_entries",
        "core.config",
        "core.entity_registry",
        "lovelace.map",
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
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(
        report["config_entries"][0],
        json!({"entry_id": "e1", "domain": "hue", "title": "Hue Bridge", "disabled": false, "supported": true})
    );
    assert_eq!(report["config_entries"][1]["disabled"], true);
    // The YAML, byte for byte; secrets.yaml only when something uses it.
    assert_eq!(
        report["yaml"],
        json!(["automations.yaml", "configuration.yaml"])
    );
    for f in ["automations.yaml", "configuration.yaml"] {
        assert_eq!(
            std::fs::read(src.join(f)).unwrap(),
            std::fs::read(dest.join(f)).unwrap(),
            "{f}"
        );
    }
    assert!(!dest.join("secrets.yaml").exists());
    assert_eq!(
        report["automations"][0],
        json!({"id": "ok", "alias": "Fine", "status": "ok"})
    );
    assert_eq!(report["automations"][1]["status"], "unavailable");
    assert!(
        stdout.contains(
            "automation Waiting will be unavailable: the 'delay' action is not supported"
        ),
        "{stdout}"
    );
    assert!(stdout.contains("automations: 1 of 2 supported"), "{stdout}");
    assert!(!report.to_string().contains("secret-value"));
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
    // `zone.home` comes from the imported location.
    let (status, states) = http(
        port,
        "GET",
        "/api/states/zone.home",
        &[("Authorization", &format!("Bearer {access}"))],
        "",
    );
    assert_eq!(status, 200, "{states}");
    let home: Value = serde_json::from_str(&states).unwrap();
    assert_eq!(home["attributes"]["latitude"], 52.37);
    assert_eq!(home["attributes"]["friendly_name"], "Home");
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

/// The destination's supported stores end up exactly the source's: a store
/// the source doesn't have is removed (with --force), not left stale.
#[test]
fn stores_missing_from_the_source_are_removed() {
    let src = temp("partial");
    ha_config(&src);
    std::fs::remove_file(src.join(".storage/onboarding")).unwrap();
    let dest = temp("partial-dest");
    assert!(hars(&dest, &["auth", "add", "bob", "pw"]).status.success()); // an older owner

    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("onboarding (removed)"),
        "{out:?}"
    );
    // Nothing changed.
    assert!(read_json(&dest.join(".storage/auth"))["data"]["users"][0]["name"] == "bob");

    let out = hars(&dest, &["import", src.to_str().unwrap(), "--force"]);
    assert!(out.status.success(), "{out:?}");
    let report = read_json(&dest.join("import-report.json"));
    assert_eq!(report["removed"], json!(["onboarding"]));
    assert!(!dest.join(".storage/onboarding").exists());
    assert_eq!(
        read_json(&dest.join(".storage/auth"))["data"]["users"][0]["name"],
        "adam"
    );
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[cfg(unix)]
#[test]
fn a_symlinked_store_is_refused() {
    let src = temp("symlink");
    ha_config(&src);
    std::fs::rename(src.join(".storage/auth"), src.join("auth-bytes")).unwrap();
    std::os::unix::fs::symlink("../auth-bytes", src.join(".storage/auth")).unwrap();
    let dest = temp("symlink-dest");
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("auth in the source is not a regular file"),
        "{out:?}"
    );
    assert!(!dest.join(".storage").exists());
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// Frontend settings that don't load are skipped, with a reason that
/// doesn't quote the file.
#[test]
fn bad_frontend_settings_are_skipped_without_quoting_them() {
    let src = temp("bad-frontend");
    let owner = ha_config(&src);
    write_store(&src, "frontend.system_data", 2, 1, json!({}));
    let user_data = format!("frontend.user_data_{owner}");
    std::fs::write(
        src.join(".storage").join(&user_data),
        r#"{"version": 1, "key": "x", "data": "SYNTHETIC_SECRET_MARKER"}"#,
    )
    .unwrap();
    let dest = temp("bad-frontend-dest");
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    let report = std::fs::read_to_string(dest.join("import-report.json")).unwrap();
    assert!(!String::from_utf8_lossy(&out.stdout).contains("SYNTHETIC"));
    assert!(!report.contains("SYNTHETIC"));
    let report: Value = serde_json::from_str(&report).unwrap();
    let skipped: Vec<&str> = report["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["key"].as_str().unwrap())
        .collect();
    assert!(
        skipped.contains(&"frontend.system_data") && skipped.contains(&user_data.as_str()),
        "{skipped:?}"
    );
    assert!(!dest.join(".storage/frontend.system_data").exists());
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[cfg(unix)]
#[test]
fn an_unreadable_source_is_an_error_not_an_empty_install() {
    use std::os::unix::fs::PermissionsExt;
    let src = temp("unreadable");
    ha_config(&src);
    let dest = temp("unreadable-dest");
    assert!(hars(&dest, &["auth", "add", "bob", "pw"]).status.success());
    let storage = src.join(".storage");
    std::fs::set_permissions(&storage, std::fs::Permissions::from_mode(0o300)).unwrap();
    let out = hars(&dest, &["import", src.to_str().unwrap(), "--force"]);
    std::fs::set_permissions(&storage, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!out.status.success(), "{out:?}");
    assert_eq!(
        read_json(&dest.join(".storage/auth"))["data"]["users"][0]["name"],
        "bob"
    );

    // No .storage at all: refused too.
    let empty = temp("no-storage");
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::write(empty.join(".HA_VERSION"), "2026.6.1").unwrap();
    let out = hars(&dest, &["import", empty.to_str().unwrap(), "--force"]);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no .storage directory"),
        "{out:?}"
    );
    assert!(dest.join(".storage/auth").exists());
    for d in [src, dest, empty] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// A backup left by a rollback that failed is the only copy of the old
/// files: a later import must not delete it.
#[test]
fn a_leftover_backup_blocks_the_next_import() {
    let src = temp("leftover");
    ha_config(&src);
    let dest = temp("leftover-dest");
    let backup = dest.join(".import-backup");
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::write(backup.join("auth"), "previous auth").unwrap();
    let out = hars(&dest, &["import", src.to_str().unwrap(), "--force"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("could not roll back"),
        "{out:?}"
    );
    assert_eq!(
        std::fs::read_to_string(backup.join("auth")).unwrap(),
        "previous auth"
    );
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// `!secret` brings `secrets.yaml` (owner-only); an include leaving the
/// config directory is refused.
#[test]
fn yaml_secrets_and_includes() {
    let src = temp("yaml-src");
    let dest = temp("yaml-dest");
    ha_config(&src);
    std::fs::write(
        src.join("configuration.yaml"),
        "http:\n  key: !secret pw\nautomation: !include_dir_merge_list automations\n",
    )
    .unwrap();
    std::fs::create_dir_all(src.join("automations/sub")).unwrap();
    std::fs::write(src.join("automations/sub/a.yaml"), "- id: a\n").unwrap();
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    let report = read_json(&dest.join("import-report.json"));
    assert_eq!(
        report["yaml"],
        json!([
            "automations/sub/a.yaml",
            "configuration.yaml",
            "secrets.yaml"
        ])
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dest.join("secrets.yaml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    assert!(!String::from_utf8_lossy(&out.stdout).contains("secret-value"));
    // Loosened in the destination: a re-import makes it owner-only again.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = dest.join("secrets.yaml");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let out = hars(&dest, &["import", src.to_str().unwrap()]);
        assert!(out.status.success(), "{out:?}");
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    std::fs::write(src.parent().unwrap().join("outside.yaml"), "1\n").unwrap();
    std::fs::write(
        src.join("configuration.yaml"),
        "x: !include ../outside.yaml\n",
    )
    .unwrap();
    let dest2 = temp("yaml-dest2");
    let out = hars(&dest2, &["import", src.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("outside the config directory"));
}

/// The files the loader really follows (multi-line and flow includes;
/// not comments or quoted text), secrets beside included files, and
/// secrets never printed.
#[test]
fn yaml_found_as_the_loader_finds_it() {
    let src = temp("yaml-loader-src");
    let dest = temp("yaml-loader-dest");
    ha_config(&src);
    std::fs::create_dir_all(src.join("parts")).unwrap();
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(
        src.join("configuration.yaml"),
        "# !include /etc/passwd and !secret nothing\nnote: \"!include ../nope.yaml\"\nautomation: !include_dir_merge_list\n  parts\nother: [!include sub/a.yaml]\n",
    )
    .unwrap();
    std::fs::write(
        src.join("parts/a.yaml"),
        "- id: a\n  alias: !secret name\n  triggers: [{trigger: time, at: '10:00'}]\n  actions: [{action: light.turn_on}]\n",
    )
    .unwrap();
    std::fs::write(src.join("sub/a.yaml"), "pw: !secret pw\n").unwrap();
    std::fs::write(src.join("sub/secrets.yaml"), "pw: SUBSECRET\n").unwrap();
    std::fs::write(src.join("secrets.yaml"), "name: TOPNAME\npw: ROOTPW\n").unwrap();
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    let report = read_json(&dest.join("import-report.json"));
    assert_eq!(
        report["yaml"],
        json!([
            "configuration.yaml",
            "parts/a.yaml",
            "secrets.yaml",
            "sub/a.yaml",
            "sub/secrets.yaml"
        ])
    );
    let text = format!(
        "{report}{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for secret in ["TOPNAME", "SUBSECRET", "ROOTPW"] {
        assert!(!text.contains(secret), "{secret} leaked");
    }
    assert_eq!(report["automations"][0]["alias"], "<secret>");
}

/// Symlinks, in the source or the destination, and the importer's own
/// paths are refused.
#[cfg(unix)]
#[test]
fn yaml_paths_are_checked() {
    let src = temp("yaml-paths-src");
    let outside = temp("yaml-paths-outside");
    ha_config(&src);
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("a.yaml"), "1\n").unwrap();
    std::os::unix::fs::symlink(&outside, src.join("linked")).unwrap();
    std::fs::write(
        src.join("configuration.yaml"),
        "x: !include linked/a.yaml\n",
    )
    .unwrap();
    let out = hars(&temp("yaml-paths-d1"), &["import", src.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("symlink"),
        "{out:?}"
    );

    std::fs::remove_file(src.join("linked")).unwrap();
    std::fs::create_dir_all(src.join("linked")).unwrap();
    std::fs::write(src.join("linked/a.yaml"), "1\n").unwrap();
    let dest = temp("yaml-paths-d2");
    std::fs::create_dir_all(&dest).unwrap();
    std::os::unix::fs::symlink(&outside, dest.join("linked")).unwrap();
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("symlink"),
        "{out:?}"
    );

    std::fs::create_dir_all(src.join(".import-backup")).unwrap();
    std::fs::write(src.join(".import-backup/a.yaml"), "42\n").unwrap();
    std::fs::write(
        src.join("configuration.yaml"),
        "x: !include .import-backup/a.yaml\n",
    )
    .unwrap();
    let out = hars(&temp("yaml-paths-d3"), &["import", src.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("keeps for itself"),
        "{out:?}"
    );
}
