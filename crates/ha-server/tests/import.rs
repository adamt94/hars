//! `hars import`: a Home Assistant config directory taken over, sessions
//! intact.
//!
//! The tests from `release` on use config directories made by each Home
//! Assistant release's own code, and what 2026.6.1 itself does with them
//! (`tests/vectors/import/`, made by `scripts/vectors/import_releases.py`).

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
        hars(dir, &["auth", "add", "alex", "secret"])
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
    assert_eq!(report["importer_version"], 4);
    assert_eq!(report["ha_version"], "2026.6.1");
    assert_eq!(report["ha_version_tested"], true);
    assert_eq!(report["behaves_like"], "2026.6.1");
    assert_eq!(report["newer_stores"], json!([]));
    for nothing in ["converted", "converted_entries", "dropped", "split_devices"] {
        assert_eq!(report[nothing], json!([]), "{nothing}");
    }
    // The pinned release itself: nothing to expect differently.
    assert!(!stdout.contains("behaves like"), "{stdout}");
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
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(
            "this is Home Assistant 2025.12.4; hars imports 2026.6 to 2026.11. \
             Update Home Assistant to 2026.11 first (not to a later release)"
        ),
        "{stderr}"
    );
    assert!(!dest.join(".storage").exists());
    for (version, says) in [
        ("2026.5.4", "this is Home Assistant 2026.5.4"),
        ("garbage", "not a Home Assistant version"),
        ("2026", "not a Home Assistant version"),
    ] {
        std::fs::write(src.join(".HA_VERSION"), version).unwrap();
        let out = hars(&dest, &["import", src.to_str().unwrap()]);
        assert!(!out.status.success(), "{version}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(says),
            "{out:?}"
        );
        assert!(!dest.join(".storage").exists(), "{version}");
    }

    std::fs::write(src.join(".HA_VERSION"), "2026.6.1").unwrap();
    let out = hars(&src, &["import", src.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("or is the source"));

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
        "alex"
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
    // A newer major version is not "settings that don't load": it refuses
    // the import, as for any store.
    write_store(&src, "frontend.system_data", 2, 1, json!({}));
    let dest = temp("newer-frontend-dest");
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("frontend.system_data is storage version 2.1"),
        "{out:?}"
    );
    assert!(!dest.join(".storage").exists());
    write_store(&src, "frontend.system_data", 1, 1, json!("not a mapping"));
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

    // Settings left out because they don't load are not "the source has
    // none": a destination keeps the settings it has, with --force too.
    let kept = temp("bad-frontend-kept");
    std::fs::create_dir_all(kept.join(".storage")).unwrap();
    write_store(&kept, "frontend.system_data", 1, 1, json!({"mine": 1}));
    let mine = std::fs::read(kept.join(".storage/frontend.system_data")).unwrap();
    let out = hars(&kept, &["import", src.to_str().unwrap(), "--force"]);
    assert!(out.status.success(), "{out:?}");
    let report = read_json(&kept.join("import-report.json"));
    assert_eq!(report["removed"], json!([]));
    assert_eq!(
        std::fs::read(kept.join(".storage/frontend.system_data")).unwrap(),
        mine
    );
    for d in [src, dest, kept] {
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

// ----- Config directories made by Home Assistant releases -----

const VECTORS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/import");

fn vector(name: &str) -> Value {
    read_json(&Path::new(VECTORS).join(name))
}

/// The config directory Home Assistant `version` left, written to `dir`.
/// `swap` takes these files from another release's directory instead.
fn release(version: &str, dir: &Path, swap: &[(&str, &str)]) -> Value {
    let mut fixture = vector(&format!("{version}.json"));
    assert_eq!(fixture["ha_version"], version);
    for (file, from) in swap {
        fixture["files"][file] = vector(&format!("{from}.json"))["files"][file].clone();
    }
    for (rel, text) in fixture["files"].as_object().unwrap() {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text.as_str().unwrap()).unwrap();
    }
    fixture
}

fn stores(fixture: &Value) -> Vec<String> {
    fixture["files"]
        .as_object()
        .unwrap()
        .keys()
        .filter_map(|k| k.strip_prefix(".storage/"))
        .map(str::to_owned)
        .collect()
}

/// A refused import changed nothing: no stores, no report, no staging in
/// the destination (the config directory `hars` makes when it starts).
fn nothing_imported(dest: &Path) {
    let left: Vec<_> = std::fs::read_dir(dest)
        .map(|d| d.map(|e| e.unwrap().file_name()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "{left:?}");
}

/// One store of a config directory, changed.
fn edit_store(dir: &Path, key: &str, edit: impl FnOnce(&mut Value)) {
    let path = dir.join(".storage").join(key);
    let mut store = read_json(&path);
    edit(&mut store);
    std::fs::write(path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();
}

/// What `hars import` converts in each release's directory: the stores, with
/// the storage version each had, and the config entries.
fn converted_in(version: &str) -> (Vec<(&'static str, &'static str)>, Vec<&'static str>) {
    let device = "core.device_registry";
    let entity = "core.entity_registry";
    match version {
        "2026.8.3" => (vec![(device, "3.2"), (entity, "1.23")], vec![]),
        "2026.9.4" => (vec![(device, "3.4"), (entity, "1.23")], vec![]),
        "2026.10.4" => (vec![(device, "3.4"), (entity, "1.23")], vec!["lifx Desk"]),
        "2026.11.2" => (
            vec![
                (device, "3.4"),
                (entity, "1.24"),
                ("core.restore_state", "1.1"),
            ],
            vec!["met Home", "met Cabin", "lifx Desk"],
        ),
        _ => (vec![], vec![]),
    }
}

/// Every release fixture imports. What Home Assistant 2026.6.1's own base
/// bootstrap loads (`downgrade.json`: its
/// `bootstrap.async_load_base_functionality` on each directory) is copied
/// byte for byte. The directories it refuses, 2026.8's and later, are the
/// ones with a store hars converts first: those stores arrive in 2026.6.1's
/// form, every other one byte for byte, and the source is left as it was.
#[test]
fn imports_every_release() {
    let cases = vector("downgrade.json");
    assert_eq!(cases["ha_version"], "2026.6.1");
    let cases = cases["cases"].as_array().unwrap();
    let versions: Vec<&str> = cases.iter().map(|c| c["from"].as_str().unwrap()).collect();
    assert_eq!(
        versions,
        [
            "2026.6.1",
            "2026.6.4",
            "2026.7.0",
            "2026.7.4",
            "2026.8.3",
            "2026.9.4",
            "2026.10.4",
            "2026.11.2"
        ]
    );
    for case in cases {
        let version = case["from"].as_str().unwrap();
        let month: u32 = version.split('.').nth(1).unwrap().parse().unwrap();
        let src = temp(&format!("release-{version}"));
        let dest = temp(&format!("release-{version}-dest"));
        let fixture = release(version, &src, &[]);
        let (converted, converted_entries) = converted_in(version);
        // 2026.6.1 itself refuses exactly the directories with a store to
        // convert: "Storage file core.device_registry was created by a
        // newer version of Home Assistant (storage version 3 > 1)".
        assert_eq!(
            case["loads"].as_bool().unwrap(),
            converted.is_empty(),
            "{version}"
        );
        if !converted.is_empty() {
            assert!(
                case["errors"][0]
                    .as_str()
                    .unwrap()
                    .contains("core.device_registry was created by a newer version"),
                "{case}"
            );
        }

        // A dry run says the same and writes nothing.
        let out = hars(&dest, &["import", src.to_str().unwrap(), "--dry-run"]);
        assert!(out.status.success(), "{version}: {out:?}");
        let dry = String::from_utf8_lossy(&out.stdout).replace("would import", "imported");
        nothing_imported(&dest);

        let out = hars(&dest, &["import", src.to_str().unwrap()]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{version}: {stderr}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            stdout
                .lines()
                .filter(|l| !l.starts_with("Report:"))
                .collect::<Vec<_>>(),
            dry.lines().collect::<Vec<_>>(),
            "{version}"
        );

        let report = read_json(&dest.join("import-report.json"));
        assert_eq!(report["importer_version"], 4);
        assert_eq!(report["ha_version"], version);
        assert_eq!(report["ha_version_tested"], true, "{version}");
        assert_eq!(report["behaves_like"], "2026.6.1");
        let was: Vec<(&str, &str)> = report["converted"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["key"].as_str().unwrap(), c["from"].as_str().unwrap()))
            .collect();
        assert_eq!(was, converted, "{version}");
        let entries: Vec<String> = report["converted_entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                assert_eq!((&e["from"], &e["to"]), (&json!("2.1"), &json!("1.1")));
                format!(
                    "{} {}",
                    e["domain"].as_str().unwrap(),
                    e["title"].as_str().unwrap()
                )
            })
            .collect();
        assert_eq!(entries, converted_entries, "{version}");

        // Every store but `http.auth` (the user media is served as) and
        // the repairs registry is one hars reads.
        let mut imported = stores(&fixture);
        imported.retain(|k| k != "http.auth" && k != "repairs.issue_registry");
        assert_eq!(report["imported"], json!(imported), "{version}");
        let skipped: Vec<&str> = report["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["key"].as_str().unwrap())
            .collect();
        let expected: &[&str] = if month >= 8 {
            &["http.auth", "repairs.issue_registry"]
        } else {
            &["http.auth"]
        };
        assert_eq!(skipped, expected, "{version}");
        for key in &imported {
            let written = std::fs::read_to_string(dest.join(".storage").join(key)).unwrap();
            let theirs = fixture["files"][format!(".storage/{key}")]
                .as_str()
                .unwrap();
            let is_converted = converted.iter().any(|(k, _)| k == key)
                || (key == "core.config_entries" && !converted_entries.is_empty());
            if is_converted {
                // In the versions hars reads and writes.
                assert_ne!(written, theirs, "{version} {key}");
                let head: Value = serde_json::from_str(&written).unwrap();
                let reads = match key.as_str() {
                    "core.device_registry" => (1, 12),
                    "core.entity_registry" => (1, 22),
                    "core.config_entries" => (1, 5),
                    _ => (1, 1),
                };
                assert_eq!(
                    (&head["version"], &head["minor_version"]),
                    (&json!(reads.0), &json!(reads.1)),
                    "{version} {key}"
                );
            } else {
                // Byte for byte.
                assert_eq!(written, theirs, "{version} {key}");
            }
        }
        // Converted or not, the entries (hosts, keys, tokens) are owner-only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let entries = dest.join(".storage/core.config_entries");
            let mode = std::fs::metadata(entries).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{version}");
        }
        // The source is only read.
        for (rel, text) in fixture["files"].as_object().unwrap() {
            assert_eq!(
                std::fs::read_to_string(src.join(rel)).unwrap(),
                text.as_str().unwrap(),
                "{version} {rel}"
            );
        }
        assert!(!src.join(".import-staging").exists());

        // 2026.7 writes `mobile_app`'s store at 1.2.
        let newer = if month >= 7 {
            json!([{"key": "mobile_app", "version": "1.2", "hars_writes": "1.1"}])
        } else {
            json!([])
        };
        assert_eq!(report["newer_stores"], newer, "{version}");
        // The integrations hars runs, the companion app's among them.
        let unsupported: Vec<&str> = report["config_entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["supported"] == false)
            .map(|e| e["domain"].as_str().unwrap())
            .collect();
        assert_eq!(unsupported, ["radio_browser", "homekit_controller", "mqtt"]);
        // 2026.10 removes the `lifx` entry from before one entry per bulb.
        let entries = if month >= 10 { 10 } else { 11 };
        assert_eq!(report["config_entries"].as_array().unwrap().len(), entries);

        assert_eq!(
            stdout.contains(
                "hars behaves like Home Assistant 2026.6.1 and shows the 2026.6 app: what \
                 Home Assistant added after that is not there"
            ),
            month >= 7,
            "{stdout}"
        );
        assert!(!stdout.contains("not tested"), "{stdout}");
        assert_eq!(stdout.contains("converted "), month >= 8, "{stdout}");
        // The device two integrations shared: 2026.8 made it two, and the
        // automation that names it by the old id reaches neither.
        if month >= 8 {
            let split = &report["split_devices"];
            assert_eq!(split.as_array().unwrap().len(), 1, "{split}");
            assert_eq!(split[0]["name"], "Desk lamp");
            assert_eq!(split[0]["new_ids"].as_array().unwrap().len(), 2);
            assert_eq!(split[0]["used_in"], json!(["automations.yaml"]));
            let old = split[0]["old_id"].as_str().unwrap();
            assert!(
                fixture["files"]["automations.yaml"]
                    .as_str()
                    .unwrap()
                    .contains(old),
                "{old}"
            );
            assert!(
                stdout.contains(&format!(
                    "automations.yaml still uses the id {old} of a device that Home Assistant \
                     2026.8 replaced with two"
                )),
                "{stdout}"
            );
        } else {
            assert_eq!(report["split_devices"], json!([]));
        }
        // It loads, and says what it will do differently.
        let by_id = &report["automations"][2];
        assert_eq!(
            (&by_id["alias"], &by_id["status"]),
            (&json!("Desk off at night"), &json!("ok"))
        );
        assert_eq!(by_id["warning"].is_string(), month >= 8, "{by_id}");
        assert_eq!(
            stdout.contains("automation Desk off at night loads, but it names a device by the id"),
            month >= 8,
            "{stdout}"
        );
        assert_eq!(report["parent_devices"], json!([]), "{version}");
        assert_eq!(report["dropped"], json!([]), "{version}");
        // An automation the 2026.7 editor can save and 2026.6 can't run.
        let automations = report["automations"].as_array().unwrap();
        if month >= 7 {
            assert_eq!(automations.len(), 4);
            assert_eq!(automations[3]["alias"], "When the hall light turns on");
            assert_eq!(automations[3]["status"], "unavailable");
            assert!(
                stdout.contains("automation When the hall light turns on will be unavailable"),
                "{stdout}"
            );
        } else {
            assert_eq!(automations.len(), 3);
        }
        assert_eq!(automations[0]["status"], "ok");

        // The same import again changes nothing.
        let out = hars(&dest, &["import", src.to_str().unwrap()]);
        assert!(out.status.success(), "{version}: {out:?}");
        let again = read_json(&dest.join("import-report.json"));
        assert_eq!(again["replaced"], json!([]), "{version}");
        assert_eq!(again["converted"], report["converted"], "{version}");
        for d in [src, dest] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// A directory from Home Assistant 2026.7.4 runs: the session it issued
/// logs in, and what its stores hold is served.
#[test]
fn a_2026_7_install_runs() {
    let src = temp("runs-src");
    let dest = temp("runs-dest");
    let fixture = release("2026.7.4", &src, &[]);
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");

    // No test reaches the network: the integrations that would call a
    // bridge, a bulb or a cloud stay off.
    edit_store(&dest, "core.config_entries", |store| {
        for entry in store["data"]["entries"].as_array_mut().unwrap() {
            let domain = entry["domain"].as_str().unwrap();
            if ["met", "hue", "lifx", "switchbot_cloud"].contains(&domain) {
                entry["disabled_by"] = json!("user");
            }
        }
    });

    let auth: Value =
        serde_json::from_str(fixture["files"][".storage/auth"].as_str().unwrap()).unwrap();
    // The session Home Assistant 2026.7.4 itself created.
    let session = auth["data"]["refresh_tokens"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["client_id"] == "http://127.0.0.1:8123/2026.7.4")
        .unwrap();
    assert_eq!(session["version"], "2026.7.4");

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
        "http%3A%2F%2F127.0.0.1%3A8123%2F2026.7.4",
        session["token"].as_str().unwrap()
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
    let bearer = format!("Bearer {access}");
    let get = |path: &str| {
        let (status, body) = http(port, "GET", path, &[("Authorization", &bearer)], "");
        assert_eq!(status, 200, "{path}: {body}");
        serde_json::from_str::<Value>(&body).unwrap()
    };
    // `core.config`, as 2026.7.4 wrote it.
    let config = get("/api/config");
    assert_eq!(config["time_zone"], "Europe/Amsterdam");
    assert_eq!(config["elevation"], 7);
    // The zone and the person 2026.7.4 added, and the ones from 2026.6.1.
    for (entity, name) in [
        ("zone.office", "Office"),
        ("zone.zone_2026_7_4", "Zone 2026.7.4"),
        ("person.test_owner", "Test Owner"),
        ("person.person_2026_7_4", "Person 2026.7.4"),
        ("automation.hall_light_at_dusk", "Hall light at dusk"),
    ] {
        let state = get(&format!("/api/states/{entity}"));
        assert_eq!(state["attributes"]["friendly_name"], name, "{state}");
    }
    // 2026.6 can't run what the 2026.7 editor saved here.
    let new = get("/api/states/automation.when_the_hall_light_turns_on");
    assert_eq!(new["state"], "unavailable");
    let _ = Command::new("kill")
        .args(["-TERM", &server.id().to_string()])
        .status();
    server.wait().unwrap();
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// A release hars has no fixture for imports when every store passes, and
/// the report and the terminal say it is untested.
#[test]
fn an_untested_release_is_imported_and_flagged() {
    let src = temp("untested-src");
    let dest = temp("untested-dest");
    release("2026.7.4", &src, &[]);
    std::fs::write(src.join(".HA_VERSION"), "2026.12.0").unwrap();
    let out = hars(&dest, &["import", src.to_str().unwrap(), "--dry-run"]);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(
            "hars is not tested with Home Assistant 2026.12.0 (it is with 2026.6 to 2026.11)"
        ),
        "{stdout}"
    );
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    let report = read_json(&dest.join("import-report.json"));
    assert_eq!(report["ha_version"], "2026.12.0");
    assert_eq!(report["ha_version_tested"], false);
    assert_eq!(report["behaves_like"], "2026.6.1");
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// What hars has no rule for is refused whole, each on Home Assistant
/// 2026.11.2's directory with one thing changed: a storage version or a
/// config entry version past the ones converted, and data in a convertible
/// store that has no 2026.6.1 meaning. Nothing is written, with `--dry-run`
/// and `--force` too.
#[test]
fn what_hars_cannot_convert_is_refused() {
    type Change = fn(&Path);
    fn entry(src: &Path, domain: &str, edit: impl Fn(&mut Value)) {
        edit_store(src, "core.config_entries", |s| {
            let entries = s["data"]["entries"].as_array_mut().unwrap();
            edit(entries.iter_mut().rfind(|e| e["domain"] == domain).unwrap());
        });
    }
    fn device(src: &Path, list: &str, name: &str, edit: impl Fn(&mut Value)) {
        edit_store(src, "core.device_registry", |s| {
            let rows = s["data"][list].as_array_mut().unwrap();
            edit(rows.iter_mut().find(|d| d["name"] == name).unwrap());
        });
    }
    let past = "hars imports Home Assistant 2026.6 to 2026.11, and none of those releases \
                writes this: import a copy or backup of this config directory made while it ran \
                one of them. Nothing was imported";
    let cases: Vec<(&str, Change, String)> =
        vec![
        // A minor version after 2026.11's: a later release's format.
        (
            "device-3.5",
            |src| edit_store(src, "core.device_registry", |s| s["minor_version"] = json!(5)),
            format!(
                "core.device_registry is storage version 3.5, in a config directory of Home \
                 Assistant 2026.11.2; hars reads version 1.12, as Home Assistant 2026.6.1 does, \
                 and converts versions 3.2 and 3.4 to it (Home Assistant 2026.8 to 2026.11). \
                 {past}"
            ),
        ),
        // 3.3 was in no release, so there is no fixture for it.
        (
            "device-3.3",
            |src| edit_store(src, "core.device_registry", |s| s["minor_version"] = json!(3)),
            "core.device_registry is storage version 3.3".into(),
        ),
        (
            "device-4",
            |src| edit_store(src, "core.device_registry", |s| s["version"] = json!(4)),
            "core.device_registry is storage version 4.4".into(),
        ),
        (
            "entity-1.25",
            |src| edit_store(src, "core.entity_registry", |s| s["minor_version"] = json!(25)),
            format!(
                "core.entity_registry is storage version 1.25, in a config directory of Home \
                 Assistant 2026.11.2; hars reads version 1.22, as Home Assistant 2026.6.1 does, \
                 and converts versions 1.23 and 1.24 to it (Home Assistant 2026.8 to 2026.11). \
                 {past}"
            ),
        ),
        (
            "entity-2",
            |src| edit_store(src, "core.entity_registry", |s| s["version"] = json!(2)),
            "core.entity_registry is storage version 2.24".into(),
        ),
        // Config entries past the versions with a rule.
        (
            "met-2.2",
            |src| entry(src, "met", |e| e["minor_version"] = json!(2)),
            format!(
                "the met integration's config entry \"Cabin\" is version 2.2, in a config \
                 directory of Home Assistant 2026.11.2; hars runs version 1 of it, as Home \
                 Assistant 2026.6.1 does, and has no rule to bring this one back to it. {past}"
            ),
        ),
        (
            "met-3",
            |src| entry(src, "met", |e| e["version"] = json!(3)),
            "the met integration's config entry \"Cabin\" is version 3.1".into(),
        ),
        (
            "lifx-2.2",
            |src| entry(src, "lifx", |e| e["minor_version"] = json!(2)),
            "the lifx integration's config entry \"Desk\" is version 2.2".into(),
        ),
        (
            "hue-2",
            |src| entry(src, "hue", |e| e["version"] = json!(2)),
            "the hue integration's config entry \"Hue Bridge\" is version 2.2".into(),
        ),
        // Entries at a version with a rule, holding what the rule can't
        // take back.
        (
            "lifx-unique-id",
            |src| entry(src, "lifx", |e| e["unique_id"] = json!("not-a-serial")),
            "the lifx integration's config entry \"Desk\" is version 2.1 and its unique id is \
             not a LIFX serial, so hars can't bring it back to version 1. Nothing was imported"
                .into(),
        ),
        (
            "lifx-serial",
            |src| entry(src, "lifx", |e| e["data"]["serial"] = json!("d073d5000002")),
            "the lifx integration's config entry \"Desk\" is version 2.1 and its serial is not \
             its unique id"
                .into(),
        ),
        (
            "met-flat",
            |src| {
                entry(src, "met", |e| {
                    e["data"] = json!({"elevation": 12, "latitude": 60.39, "longitude": 5.32})
                })
            },
            "the met integration's config entry \"Cabin\" is version 2.1 and its coordinates \
             are not in `location`"
                .into(),
        ),
        (
            "met-location",
            |src| entry(src, "met", |e| e["data"]["location"]["latitude"] = json!("north")),
            "the met integration's config entry \"Cabin\" is version 2.1 and its location is \
             not a latitude and a longitude"
                .into(),
        ),
        // A child device disabled through its parent: 2026.6.1 has no
        // such state for a device.
        (
            "child-disabled",
            |src| device(src, "child_devices", "Socket 1", |d| d["disabled_by"] = json!("device")),
            "is disabled because its parent device is (`disabled_by: device`), which Home \
             Assistant 2026.6.1 has no way to say. Enable the parent device in Home Assistant, \
             then copy the config directory again. Nothing was imported"
                .into(),
        ),
        (
            "child-orphan",
            |src| device(src, "child_devices", "Socket 1", |d| d["parent_device_id"] = json!("x")),
            "has no parent device in the file. Nothing was imported".into(),
        ),
        (
            "device-no-entry",
            |src| device(src, "devices", "Hall", |d| d["config_entry_id"] = json!(null)),
            "has no config entry, which storage version 3 doesn't allow".into(),
        ),
        (
            "device-field",
            |src| device(src, "devices", "Hall", |d| d["labels"] = json!("SYNTHETIC_MARKER")),
            "has a field of a type Home Assistant doesn't write. Nothing was imported".into(),
        ),
        // A minor version of an entry later than any fixture holds.
        (
            "hue-1.3",
            |src| entry(src, "hue", |e| e["minor_version"] = json!(3)),
            "the hue integration's config entry \"Hue Bridge\" is version 1.3".into(),
        ),
        (
            "sun-1.2",
            |src| entry(src, "sun", |e| e["minor_version"] = json!(2)),
            "the sun integration's config entry \"Sun (2026.11.2)\" is version 1.2".into(),
        ),
        // The last states in 2026.11's form, in a file of a later version.
        (
            "restore-1.2",
            |src| edit_store(src, "core.restore_state", |s| s["minor_version"] = json!(2)),
            "core.restore_state is storage version 1.2, in a config directory of Home \
             Assistant 2026.11.2; hars reads version 1.1"
                .into(),
        ),
        // Two devices of one integration with one identifier: two phones'
        // apps that give the same device id, which 2026.8 keeps apart by
        // config entry. hars would give the second phone the first's
        // device.
        (
            "phone-twins",
            |src| {
                edit_store(src, "core.config_entries", |s| {
                    let entries = s["data"]["entries"].as_array_mut().unwrap();
                    let mut twin = entries
                        .iter()
                        .find(|e| e["domain"] == "mobile_app")
                        .unwrap()
                        .clone();
                    twin["entry_id"] = json!("second-phone");
                    twin["unique_id"] = json!("io.example.Other-synthetic-phone");
                    entries.push(twin);
                });
                edit_store(src, "core.device_registry", |s| {
                    let rows = s["data"]["devices"].as_array_mut().unwrap();
                    let mut twin = rows.iter().find(|d| d["name"] == "Test Phone").unwrap().clone();
                    twin["id"] = json!("second-phone-device");
                    twin["config_entry_id"] = json!("second-phone");
                    twin["primary_config_entry"] = json!("second-phone");
                    rows.push(twin);
                });
            },
            "and second-phone-device both have an identifier of the mobile_app integration, \
             and hars, which finds a device by its identifier alone as Home Assistant 2026.6.1 \
             does, can't tell which one the integration means"
                .into(),
        ),
        // The same `lifx` entity twice, its serial written each way.
        (
            "lifx-twins",
            |src| {
                edit_store(src, "core.entity_registry", |s| {
                    let rows = s["data"]["entities"].as_array_mut().unwrap();
                    let mut twin = rows
                        .iter()
                        .find(|e| e["entity_id"] == "light.desk")
                        .unwrap()
                        .clone();
                    twin["entity_id"] = json!("light.desk_2");
                    twin["unique_id"] = json!("d0:73:d5:00:00:01");
                    rows.push(twin);
                })
            },
            "two `lifx` entities of the same kind have the unique id of".into(),
        ),
    ];
    for (name, change, says) in cases {
        let src = temp(&format!("unconverted-{name}"));
        let dest = temp(&format!("unconverted-{name}-dest"));
        release("2026.11.2", &src, &[]);
        change(&src);
        for flags in [&["--dry-run"][..], &["--force"], &[]] {
            let mut args = vec!["import", src.to_str().unwrap()];
            args.extend(flags);
            let out = hars(&dest, &args);
            assert!(!out.status.success(), "{name} {flags:?}");
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(stderr.contains(&says), "{name}: {stderr}");
            assert!(stderr.contains("Nothing was imported"), "{name}: {stderr}");
            assert!(!stderr.contains("SYNTHETIC"), "{name}: {stderr}");
            nothing_imported(&dest);
        }
        // An existing install is left exactly as it was.
        let dest = temp(&format!("unconverted-{name}-dest2"));
        assert!(hars(&dest, &["auth", "add", "bob", "pw"]).status.success());
        let before = std::fs::read(dest.join(".storage/auth")).unwrap();
        let out = hars(&dest, &["import", src.to_str().unwrap(), "--force"]);
        assert!(!out.status.success(), "{name}");
        assert_eq!(std::fs::read(dest.join(".storage/auth")).unwrap(), before);
        assert!(!dest.join(".import-staging").exists() && !dest.join(".import-backup").exists());
        for d in [src, dest] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    // One store converts only with the others: 2026.7.4's directory with
    // 2026.11.2's config entries (`lifx` and `met` at version 2) imports,
    // its entries converted, its registries as they were.
    let src = temp("mixed-src");
    let dest = temp("mixed-dest");
    let fixture = release(
        "2026.7.4",
        &src,
        &[(".storage/core.config_entries", "2026.11.2")],
    );
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    let report = read_json(&dest.join("import-report.json"));
    assert_eq!(report["converted_entries"].as_array().unwrap().len(), 3);
    assert_eq!(report["converted"], json!([]));
    assert_eq!(
        std::fs::read_to_string(dest.join(".storage/core.entity_registry")).unwrap(),
        fixture["files"][".storage/core.entity_registry"]
            .as_str()
            .unwrap()
    );
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// Directories hars could not keep safely are refused whole, each on a
/// 2026.7.4 directory with one thing changed.
#[test]
fn directories_hars_could_not_keep_are_refused() {
    type Change = fn(&Path);
    let cases: [(&str, Change, &str); 11] = [
        // hars only writes the config entries at 1.5: at 1.4 a phone's
        // registration would be gone after a restart. Home Assistant
        // migrates the file when it starts.
        (
            "older-entries",
            |src| {
                edit_store(src, "core.config_entries", |s| {
                    s["minor_version"] = json!(4)
                })
            },
            "core.config_entries is storage version 1.4, older than the 1.5 hars reads and \
             writes: Home Assistant brings it up to date when it starts. Start Home Assistant \
             2026.6 to 2026.11 on this config directory once",
        ),
        (
            "older-entities",
            |src| {
                edit_store(src, "core.entity_registry", |s| {
                    s["minor_version"] = json!(21)
                })
            },
            "core.entity_registry is storage version 1.21, older than the 1.22",
        ),
        // 2026.6.1's own person migration fails on a newer minor version.
        (
            "newer-person",
            |src| edit_store(src, "person", |s| s["minor_version"] = json!(2)),
            "person is storage version 2.2, in a config directory of Home Assistant 2026.7.4; \
             hars reads version 2.1",
        ),
        // Stores the server would start without, and then overwrite.
        (
            "restore-shape",
            |src| edit_store(src, "core.restore_state", |s| s["data"] = json!({})),
            "core.restore_state doesn't load",
        ),
        (
            "deleted-ids-shape",
            |src| edit_store(src, "mobile_app", |s| s["data"]["deleted_ids"] = json!("x")),
            "mobile_app's store doesn't load",
        ),
        (
            "analytics-shape",
            |src| {
                edit_store(src, "core.analytics", |s| {
                    s["data"]["preferences"] = json!(1)
                })
            },
            "core.analytics doesn't load",
        ),
        (
            "labs-shape",
            |src| {
                edit_store(src, "core.labs", |s| {
                    s["data"]["preview_feature_status"] = json!([{"domain": 1}])
                })
            },
            "core.labs doesn't load",
        ),
        // A row the server would drop, and then save without.
        (
            "restore-row",
            |src| {
                edit_store(src, "core.restore_state", |s| {
                    s["data"][0]["state"]
                        .as_object_mut()
                        .unwrap()
                        .remove("entity_id");
                })
            },
            "core.restore_state doesn't load",
        ),
        // A scope the server would list as empty.
        (
            "category-scope",
            |src| {
                edit_store(src, "core.category_registry", |s| {
                    s["data"]["categories"]["automation"] = json!({})
                })
            },
            "core.category_registry doesn't load",
        ),
        // hars only lists the categories, and reads them leniently.
        (
            "categories-torn",
            |src| std::fs::write(src.join(".storage/core.category_registry"), "{").unwrap(),
            "core.category_registry doesn't load",
        ),
        (
            "version-text",
            |src| std::fs::write(src.join(".HA_VERSION"), "2026.7.wrong").unwrap(),
            ".HA_VERSION says \"2026.7.wrong\", which is not a Home Assistant version",
        ),
    ];
    for (name, change, says) in cases {
        let src = temp(&format!("refused-{name}"));
        let dest = temp(&format!("refused-{name}-dest"));
        release("2026.7.4", &src, &[]);
        change(&src);
        for flags in [&["--dry-run"][..], &["--force"], &[]] {
            let mut args = vec!["import", src.to_str().unwrap()];
            args.extend(flags);
            let out = hars(&dest, &args);
            assert!(!out.status.success(), "{name} {flags:?}");
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(stderr.contains(says), "{name}: {stderr}");
            nothing_imported(&dest);
        }
        let _ = std::fs::remove_dir_all(src);
    }

    // A pre-release of a tested line is that line.
    let src = temp("beta-src");
    let dest = temp("beta-dest");
    release("2026.7.4", &src, &[]);
    std::fs::write(src.join(".HA_VERSION"), "2026.7.0b3").unwrap();
    let out = hars(&dest, &["import", src.to_str().unwrap(), "--dry-run"]);
    assert!(out.status.success(), "{out:?}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("not tested"));
    nothing_imported(&dest);
    let _ = std::fs::remove_dir_all(src);
}

/// The import works under the destination (staging, backup): a source in
/// there is refused before anything is touched, a dry run included.
#[test]
fn a_source_inside_the_destination_is_refused() {
    let dest = temp("inside-dest");
    for inside in [".import-staging", "ha-copy"] {
        let src = dest.join(inside);
        std::fs::create_dir_all(&src).unwrap();
        let fixture = release("2026.7.4", &src, &[]);
        for flag in ["--dry-run", "--force"] {
            let out = hars(&dest, &["import", src.to_str().unwrap(), flag]);
            assert!(!out.status.success(), "{inside} {flag}");
            assert!(
                String::from_utf8_lossy(&out.stderr)
                    .contains("the source is inside the destination"),
                "{out:?}"
            );
            for (rel, text) in fixture["files"].as_object().unwrap() {
                assert_eq!(
                    std::fs::read_to_string(src.join(rel)).unwrap(),
                    text.as_str().unwrap(),
                    "{inside} {flag} {rel}"
                );
            }
            assert!(!dest.join(".storage").exists());
        }
        std::fs::remove_dir_all(&src).unwrap();
    }
    let _ = std::fs::remove_dir_all(dest);
}

/// The report is written last: a path it can't be written to stops the
/// import before anything is replaced.
#[test]
fn an_unwritable_report_path_is_refused_before_any_change() {
    let src = temp("report-src");
    let dest = temp("report-dest");
    release("2026.7.4", &src, &[]);
    assert!(hars(&dest, &["auth", "add", "bob", "pw"]).status.success());
    let before = std::fs::read(dest.join(".storage/auth")).unwrap();
    std::fs::create_dir(dest.join("import-report.json")).unwrap();
    let out = hars(&dest, &["import", src.to_str().unwrap(), "--force"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("import-report.json is not a regular file"),
        "{out:?}"
    );
    assert_eq!(std::fs::read(dest.join(".storage/auth")).unwrap(), before);
    assert!(!dest.join(".import-backup").exists() && !dest.join(".import-staging").exists());

    // An old report that can't be written to is replaced, not written
    // through: the import ends with its report.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let report = dest.join("import-report.json");
        std::fs::remove_dir(&report).unwrap();
        std::fs::write(&report, "old").unwrap();
        std::fs::set_permissions(&report, std::fs::Permissions::from_mode(0o444)).unwrap();
        let out = hars(&dest, &["import", src.to_str().unwrap(), "--force"]);
        assert!(out.status.success(), "{out:?}");
        assert_eq!(read_json(&report)["ha_version"], "2026.7.4");
        assert_ne!(std::fs::read(dest.join(".storage/auth")).unwrap(), before);
    }
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// With `.storage` a symlink the two directories can share files, and what
/// the import clears or replaces in the destination would be the source's.
#[cfg(unix)]
#[test]
fn a_symlinked_storage_directory_is_refused() {
    use std::os::unix::fs::symlink;
    // The source's `.storage` is the destination's staging directory.
    let src = temp("alias-src");
    let dest = temp("alias-dest");
    release("2026.7.4", &src, &[]);
    let real = dest.join(".import-staging/.storage");
    std::fs::create_dir_all(dest.join(".import-staging")).unwrap();
    std::fs::rename(src.join(".storage"), &real).unwrap();
    symlink(&real, src.join(".storage")).unwrap();
    let auth = std::fs::read(real.join("auth")).unwrap();
    for flag in ["--dry-run", "--force"] {
        let out = hars(&dest, &["import", src.to_str().unwrap(), flag]);
        assert!(!out.status.success(), "{flag}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("the source's .storage"),
            "{out:?}"
        );
        assert_eq!(std::fs::read(real.join("auth")).unwrap(), auth, "{flag}");
    }
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }

    // The destination's `.storage` is the source's.
    let src = temp("alias2-src");
    let dest = temp("alias2-dest");
    release("2026.7.4", &src, &[]);
    std::fs::create_dir_all(&dest).unwrap();
    symlink(src.join(".storage"), dest.join(".storage")).unwrap();
    let before = std::fs::read(src.join(".storage/http.auth")).unwrap();
    let out = hars(&dest, &["import", src.to_str().unwrap(), "--force"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("the destination's .storage"),
        "{out:?}"
    );
    assert_eq!(
        std::fs::read(src.join(".storage/http.auth")).unwrap(),
        before
    );
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// A directory where the import would put a YAML file is not moved aside
/// and deleted with the backup, with --force either.
#[test]
fn a_directory_in_a_yaml_files_place_is_refused() {
    let src = temp("yaml-dir-src");
    let dest = temp("yaml-dir-dest");
    release("2026.7.4", &src, &[]);
    std::fs::create_dir_all(dest.join("configuration.yaml")).unwrap();
    std::fs::write(dest.join("configuration.yaml/old.yaml"), "mine: 1\n").unwrap();
    for flags in [&["--force"][..], &["--dry-run"], &[]] {
        let mut args = vec!["import", src.to_str().unwrap()];
        args.extend(flags);
        let out = hars(&dest, &args);
        assert!(!out.status.success(), "{flags:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains("configuration.yaml is not a regular file in the destination"),
            "{out:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("configuration.yaml/old.yaml")).unwrap(),
            "mine: 1\n"
        );
        assert!(!dest.join(".storage").exists());
    }
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// A staging directory that is already there is never cleared: it may be
/// another import's, or hold someone's files. The import says what to do.
#[test]
fn an_occupied_staging_directory_is_left_alone() {
    let src = temp("occupied-src");
    let dest = temp("occupied-dest");
    release("2026.7.4", &src, &[]);
    let marker = dest.join(".import-staging/.storage/auth");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, "another import's").unwrap();
    for flags in [&["--dry-run"][..], &["--force"], &[]] {
        let mut args = vec!["import", src.to_str().unwrap()];
        args.extend(flags);
        let out = hars(&dest, &args);
        assert!(!out.status.success(), "{flags:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains(".import-staging is already there: another import is running"),
            "{out:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "another import's"
        );
        assert!(!dest.join(".storage").exists());
    }
    // Once it is gone the import runs, and leaves no staging behind; the
    // report is the only file beside the stores and the YAML.
    std::fs::remove_dir_all(dest.join(".import-staging")).unwrap();
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(!dest.join(".import-staging").exists());
    assert_eq!(
        read_json(&dest.join("import-report.json"))["ha_version"],
        "2026.7.4"
    );
    let mut left: Vec<String> = std::fs::read_dir(&dest)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !n.ends_with(".yaml"))
        .collect();
    left.sort();
    assert_eq!(left, [".storage", "import-report.json"]);
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

// ----- Directories of Home Assistant 2026.8 to 2026.11, converted -----

const CONVERTED: [&str; 4] = ["2026.8.3", "2026.9.4", "2026.10.4", "2026.11.2"];

/// The rows of a store's list, by a field of each.
fn rows<'a>(
    store: &'a Value,
    list: &str,
    key: &str,
) -> std::collections::BTreeMap<String, &'a Value> {
    store["data"][list]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| (row[key].as_str().unwrap().to_owned(), row))
        .collect()
}

fn store_of(fixture: &Value, key: &str) -> Value {
    serde_json::from_str(
        fixture["files"][format!(".storage/{key}")]
            .as_str()
            .unwrap(),
    )
    .unwrap()
}

/// One difference between a row of the 2026.6.1 install and the row it is
/// after another release ran on it and hars converted it back.
type Difference = (String, Value, Value);

/// The fields that differ between two rows. `modified_at` is left out: a
/// row a release changed has that release's time, and that is the only
/// reason it differs.
fn row_differences(
    path: &str,
    before: &Value,
    after: &Value,
    skip: &[&str],
    out: &mut Vec<Difference>,
) {
    let absent = json!("(absent)");
    let (before, after) = (before.as_object().unwrap(), after.as_object().unwrap());
    let keys: std::collections::BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    for key in keys {
        if key == "modified_at" || skip.contains(&key.as_str()) {
            continue;
        }
        let (a, b) = (
            before.get(key).unwrap_or(&absent),
            after.get(key).unwrap_or(&absent),
        );
        if a != b {
            out.push((format!("{path}/{key}"), a.clone(), b.clone()));
        }
    }
}

/// The round trip. Home Assistant has no code that takes a config
/// directory back a version, so the check is made with its own code going
/// forward: `scripts/vectors/import_releases.py` makes one install with
/// 2026.6.1 and starts each later release on it. What hars makes of each
/// result must be that 2026.6.1 install again, field for field, except
/// for the differences listed here, each with its reason. They are of
/// three kinds: what the generator itself changed through the newer
/// release (so that every store is written by it), what the newer release
/// did that 2026.6.1 has a place for and hars keeps, and the two things a
/// way back doesn't exist for.
#[test]
fn a_converted_install_is_the_2026_6_1_install_again() {
    let seed = vector("2026.6.1.json");
    let seed_entries = store_of(&seed, "core.config_entries");
    let seed_entries = rows(&seed_entries, "entries", "entry_id");
    let entry_id = |domain: &str, title: &str| -> String {
        let found = seed_entries
            .iter()
            .find(|(_, e)| e["domain"] == domain && e["title"] == title);
        found.unwrap().0.clone()
    };
    let entry_name = |id: &str| {
        let e = seed_entries[id];
        format!(
            "{} {}",
            e["domain"].as_str().unwrap(),
            e["title"].as_str().unwrap()
        )
    };
    let (lifx, homekit) = (
        entry_id("lifx", "Desk"),
        entry_id("homekit_controller", "Desk"),
    );
    let seed_devices = store_of(&seed, "core.device_registry");
    let seed_entities = store_of(&seed, "core.entity_registry");
    let device_id = |name: &str| -> String {
        let all = rows(&seed_devices, "devices", "id");
        let found = all.iter().find(|(_, d)| d["name"] == name);
        found.unwrap().0.clone()
    };
    let (desk, strip) = (device_id("Desk"), device_id("Power strip"));

    for version in CONVERTED {
        let month: u32 = version.split('.').nth(1).unwrap().parse().unwrap();
        let src = temp(&format!("round-{version}"));
        let dest = temp(&format!("round-{version}-dest"));
        let theirs = release(version, &src, &[]);
        let out = hars(&dest, &["import", src.to_str().unwrap()]);
        assert!(out.status.success(), "{version}: {out:?}");
        let report = read_json(&dest.join("import-report.json"));
        let read = |key: &str| read_json(&dest.join(".storage").join(key));
        let mut found: Vec<Difference> = Vec::new();
        let mut expected: Vec<Difference> = Vec::new();
        let mut expect = |path: &str, before: Value, after: Value| {
            expected.push((path.to_owned(), before, after));
        };
        let absent = || json!("(absent)");

        // ----- core.device_registry -----
        let devices = read("core.device_registry");
        assert_eq!(
            (&devices["version"], &devices["minor_version"]),
            (&json!(1), &json!(12))
        );
        let before = rows(&seed_devices, "devices", "id");
        let after = rows(&devices, "devices", "id");
        // The parts' ids and the time of the split are the release's own:
        // what its file has, not what hars says.
        let their_devices = store_of(&theirs, "core.device_registry");
        let their_parts: std::collections::BTreeMap<String, &Value> =
            rows(&their_devices, "devices", "id")
                .into_iter()
                .filter(|(_, d)| d["composite_device_id"] == desk.as_str())
                .collect();
        let new_ids: Vec<String> = their_parts.keys().cloned().collect();
        assert_eq!(new_ids.len(), 2, "{version}");
        assert_eq!(report["split_devices"][0]["old_id"], desk.as_str());
        assert_eq!(report["split_devices"][0]["new_ids"], json!(new_ids));
        let label = |row: &Value| {
            format!(
                "devices/{} ({})",
                row["name"].as_str().unwrap(),
                entry_name(row["config_entries"][0].as_str().unwrap())
            )
        };
        let mut seen = std::collections::BTreeSet::new();
        for (id, row) in &after {
            // A part of the device two integrations shared is compared
            // with that device. Its id is new (2026.8 gave each part one)
            // and `split_at` says when; both are checked just below.
            let old = row["composite_device_id"].as_str().unwrap_or(id);
            let skip: &[&str] = if old == id { &[] } else { &["id", "split_at"] };
            if old != id {
                assert_eq!(row["split_at"], their_parts[id]["split_at"], "{row}");
                assert!(row["split_at"].is_string(), "{row}");
            }
            seen.insert(old.to_owned());
            row_differences(&label(row), before[old], row, skip, &mut found);
        }
        assert_eq!(
            seen,
            before.keys().cloned().collect(),
            "{version}: no device is lost"
        );
        assert_eq!(after.len(), before.len() + 1, "{version}");
        // 2026.8 (`DeviceRegistryStore._async_migrate_func`, version 3)
        // made the bulb `lifx` and `homekit_controller` shared two
        // devices, one for each. They stay two: the entities are on the
        // new ids, and automations made since use them. Each part lists
        // its one config entry, which is also its primary one, and keeps
        // the fields that say where it came from (hars writes them back).
        let (lifx_part, homekit_part) = (
            format!("devices/Desk ({})", entry_name(&lifx)),
            format!("devices/Desk ({})", entry_name(&homekit)),
        );
        let both = json!([lifx, homekit]);
        let subentries = |ids: &[&String]| -> Value {
            ids.iter()
                .map(|id| ((*id).clone(), json!([null])))
                .collect::<serde_json::Map<_, _>>()
                .into()
        };
        for (part, entry) in [(&lifx_part, &lifx), (&homekit_part, &homekit)] {
            expect(
                &format!("{part}/config_entries"),
                both.clone(),
                json!([entry]),
            );
            expect(
                &format!("{part}/config_entries_subentries"),
                subentries(&[&lifx, &homekit]),
                subentries(&[entry]),
            );
            expect(
                &format!("{part}/composite_device_id"),
                absent(),
                json!(desk),
            );
            expect(
                &format!("{part}/composite_primary_config_entry"),
                absent(),
                json!(lifx),
            );
        }
        expect(
            &format!("{homekit_part}/primary_config_entry"),
            json!(lifx),
            json!(homekit),
        );
        // `lifx` registered its part again (every start does), which
        // leaves it the identifiers `lifx` gives; the other integration
        // did not, so its part still holds a copy of them all and says so.
        expect(
            &format!("{lifx_part}/identifiers"),
            json!([
                ["homekit_controller:accessory-id", "00:00:00:00:00:01:aid:1"],
                ["lifx", "d0:73:d5:00:00:01"]
            ]),
            json!([["lifx", "d0:73:d5:00:00:01"]]),
        );
        expect(
            &format!("{homekit_part}/has_composite_identifiers"),
            absent(),
            json!(true),
        );
        // A device reached through the shared one is reached through the
        // part of its own config entry (version 3.2's `via_device_id`
        // rule).
        let homekit_id = after
            .iter()
            .find(|(_, d)| label(d) == homekit_part)
            .map(|(id, _)| (*id).clone())
            .unwrap();
        expect(
            &format!(
                "devices/Desk sensor ({})/via_device_id",
                entry_name(&homekit)
            ),
            json!(desk),
            json!(homekit_id),
        );
        if month >= 9 {
            // The generator made the socket a child device of the strip,
            // as 2026.9 lets an integration do
            // (`_async_convert_device_to_child`): that drops the fields a
            // child device doesn't have, and they are not in the file to
            // bring back. It is a device again, its parent kept on the
            // row.
            let socket = format!(
                "devices/Socket 1 ({})",
                entry_name(&entry_id("mqtt", "192.0.2.30"))
            );
            expect(
                &format!("{socket}/manufacturer"),
                json!("Example"),
                json!(null),
            );
            expect(&format!("{socket}/model"), json!("Socket"), json!(null));
            expect(
                &format!("{socket}/via_device_id"),
                json!(strip),
                json!(null),
            );
            expect(
                &format!("{socket}/parent_device_id"),
                absent(),
                json!(strip),
            );
        }
        let hue = format!("({})", entry_name(&entry_id("hue", "Hue Bridge")));
        if month >= 10 {
            // 2026.10's `hue` (entry 1.2, `_migrate_zigbee_connections`)
            // takes the Zigbee addresses out of the `mac` connections and
            // its setup adds them as `zigbee` ones. They stay as 2026.10
            // left them: hars reads no connection, and 2026.6.1's `hue`
            // would add its own again.
            for (name, n) in [("Hue Bridge", 1), ("Hall", 2)] {
                expect(
                    &format!("devices/{name} {hue}/connections"),
                    json!([["mac", format!("00:17:88:01:00:00:00:0{n}")]]),
                    json!([["zigbee", format!("00:17:88:01:00:00:00:0{n}")]]),
                );
            }
            // The disabled lamp was not registered again: 2026.10.4 keeps
            // its address as a `zigbee` connection, 2026.11.2 removes it.
            expect(
                &format!("devices/Porch {hue}/connections"),
                json!([["mac", "00:17:88:01:00:00:00:03"]]),
                if month >= 11 {
                    json!([])
                } else {
                    json!([["zigbee", "00:17:88:01:00:00:00:03"]])
                },
            );
        }
        // The generator's own change, through the release.
        expect(
            &format!("devices/Hue Bridge {hue}/name_by_user"),
            json!(null),
            json!(format!("Bridge ({version})")),
        );
        let (before, after) = (
            rows(&seed_devices, "deleted_devices", "id"),
            rows(&devices, "deleted_devices", "id"),
        );
        assert_eq!(before, after, "{version}: the removed devices");

        // ----- core.entity_registry -----
        let entities = read("core.entity_registry");
        assert_eq!(
            (&entities["version"], &entities["minor_version"]),
            (&json!(1), &json!(22))
        );
        assert_eq!(
            entities["data"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["deleted_entities", "entities"],
            "`settings` is 2026.8's"
        );
        let before = rows(&seed_entities, "entities", "id");
        let after = rows(&entities, "entities", "id");
        for (id, row) in &after {
            match before.get(id) {
                Some(old) => {
                    let path = format!("entities/{}", old["entity_id"].as_str().unwrap());
                    row_differences(&path, old, row, &[], &mut found);
                }
                None => found.push((
                    format!("entities/{}", row["entity_id"].as_str().unwrap()),
                    absent(),
                    json!("(new)"),
                )),
            }
        }
        assert!(
            before.keys().all(|id| after.contains_key(id)),
            "{version}: no entity is lost"
        );
        // The entities of the shared device are on the part their
        // integration owns (`EntityRegistry._async_load`).
        let lifx_id = new_ids.iter().find(|id| **id != homekit_id).unwrap();
        for (entity, part) in [
            ("light.desk", lifx_id),
            ("button.desk_restart", lifx_id),
            ("light.desk_homekit", &homekit_id),
        ] {
            expect(
                &format!("entities/{entity}/device_id"),
                json!(desk),
                json!(part),
            );
        }
        if month >= 10 {
            // 2026.10's `lifx` migration rewrote the two unique ids, and
            // the registry notes the one before each change. The unique
            // ids are back; what `previous_unique_id` held before is not
            // in the file.
            for (entity, was) in [
                ("light.desk", "d0:73:d5:00:00:01"),
                ("button.desk_restart", "d0:73:d5:00:00:01_restart"),
            ] {
                expect(
                    &format!("entities/{entity}/previous_unique_id"),
                    json!(null),
                    json!(was),
                );
            }
        }
        if month >= 11 {
            // 1.24's migration made the old name the first alias of an
            // entity exposed to Assist, in place of "the entity's name"
            // (`null`). It stays: an alias the user wrote out looks the
            // same in the file, and must not start to follow the name.
            expect(
                "entities/sensor.hall_temperature/aliases_v2",
                json!([null]),
                json!(["Warmth downstairs"]),
            );
            // Entity registry 1.24 stored "Desk lamp reading light"
            // without the device's name and with a capital
            // (`_async_strip_prefix_from_entity_name`): "Reading light".
            // 2026.11 shows, and gives as the friendly name, the device's
            // name and that; so does hars. The lower-case "r" is gone.
            expect(
                "entities/light.desk/name",
                json!("Desk lamp reading light"),
                json!("Desk lamp Reading light"),
            );
        }
        // The generator's own changes, through the release.
        expect(
            "entities/light.hall/icon",
            json!(null),
            json!("mdi:ceiling-light"),
        );
        expect(
            "entities/switch.towel_rail/entity_id",
            json!("switch.towel_rail"),
            json!("switch.towel_warmer"),
        );
        let tag = version.replace('.', "_");
        for new in [
            "automation.when_the_hall_light_turns_on".to_owned(),
            format!("person.person_{tag}"),
            format!("zone.zone_{tag}"),
        ] {
            expect(&format!("entities/{new}"), absent(), json!("(new)"));
        }
        assert_eq!(
            rows(&seed_entities, "deleted_entities", "id"),
            rows(&entities, "deleted_entities", "id"),
            "{version}: the removed entities"
        );

        // ----- core.config_entries -----
        let entries = read("core.config_entries");
        let after = rows(&entries, "entries", "entry_id");
        for (id, old) in &seed_entries {
            let path = format!("entries/{}", entry_name(id));
            match after.get(id) {
                Some(row) => row_differences(&path, old, row, &[], &mut found),
                None => found.push((path, json!("(entry)"), absent())),
            }
        }
        assert!(after.keys().all(|id| seed_entries.contains_key(id)));
        expect(
            "entries/sun Sun/title",
            json!("Sun"),
            json!(format!("Sun ({version})")),
        );
        if month >= 10 {
            // `hue` 1.2 is left at 1.2: a newer minor version is
            // compatible by Home Assistant's rule, and 2026.6.1 runs it.
            expect("entries/hue Hue Bridge/minor_version", json!(1), json!(2));
            // 2026.10's `lifx` removes the entry from before one entry
            // per bulb (`async_migrate_entry`). It ran nothing, and it
            // is not in the file to bring back.
            expect("entries/lifx LIFX", json!("(entry)"), absent());
        }

        found.sort_by(|a, b| a.0.cmp(&b.0));
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        let show = |all: &[Difference]| -> String {
            all.iter()
                .map(|(path, a, b)| format!("{path}: {a} -> {b}\n"))
                .collect()
        };
        assert_eq!(show(&found), show(&expected), "{version}");

        // ----- core.restore_state -----
        // A last state is found by entity id again. 2026.11 keeps it by
        // the registry row, so there the renamed switch's state follows
        // it; before 2026.11 (and in 2026.6.1) it stays under the old id.
        // Each row is the release's own, whole, but for that.
        let restore = read("core.restore_state");
        let mut expected = store_of(&theirs, "core.restore_state");
        let switch = if month >= 11 {
            "switch.towel_warmer"
        } else {
            "switch.towel_rail"
        };
        let mut ids = Vec::new();
        for row in expected["data"].as_array_mut().unwrap() {
            row.as_object_mut().unwrap().remove("entity_registry_id");
            if row["state"]["entity_id"] == "switch.towel_rail" {
                row["state"]["entity_id"] = json!(switch);
            }
            ids.push(row["state"]["entity_id"].as_str().unwrap().to_owned());
        }
        assert_eq!(restore["data"], expected["data"], "{version}");
        ids.sort_unstable();
        let person = format!("person.person_{tag}");
        let mut want = vec![
            "light.desk",
            person.as_str(),
            "person.test_owner",
            "scene.evening",
            switch,
        ];
        want.sort_unstable();
        assert_eq!(ids, want, "{version}");
        for d in [src, dest] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// A WebSocket connection to hars, logged in: enough of RFC 6455 for one
/// command and its answer at a time.
struct Ws(TcpStream);

impl Ws {
    fn connect(port: u16, access_token: &str) -> Self {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(
            b"GET /api/websocket HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\
              Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
              Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .unwrap();
        let (mut head, mut byte) = (Vec::new(), [0u8; 1]);
        while !head.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        assert!(
            head.starts_with(b"HTTP/1.1 101"),
            "{}",
            String::from_utf8_lossy(&head)
        );
        let mut ws = Self(s);
        assert_eq!(ws.receive()["type"], "auth_required");
        ws.send(&json!({"type": "auth", "access_token": access_token}));
        assert_eq!(ws.receive()["type"], "auth_ok");
        ws
    }

    fn send(&mut self, message: &Value) {
        let payload = message.to_string().into_bytes();
        // A final text frame, masked (with zeros) as a client's must be.
        let mut frame = vec![0x81];
        match payload.len() {
            n if n < 126 => frame.push(0x80 | n as u8),
            n => {
                frame.push(0x80 | 126);
                frame.extend((n as u16).to_be_bytes());
            }
        }
        frame.extend([0u8; 4]);
        frame.extend(payload);
        self.0.write_all(&frame).unwrap();
    }

    fn receive(&mut self) -> Value {
        loop {
            let mut head = [0u8; 2];
            self.0.read_exact(&mut head).unwrap();
            let mut len = u64::from(head[1] & 0x7f);
            if len == 126 {
                let mut b = [0u8; 2];
                self.0.read_exact(&mut b).unwrap();
                len = u64::from(u16::from_be_bytes(b));
            } else if len == 127 {
                let mut b = [0u8; 8];
                self.0.read_exact(&mut b).unwrap();
                len = u64::from_be_bytes(b);
            }
            let mut payload = vec![0u8; len as usize];
            self.0.read_exact(&mut payload).unwrap();
            // Text, whole (hars sends no fragments); pings are skipped.
            if head[0] == 0x81 {
                return serde_json::from_slice(&payload).unwrap();
            }
        }
    }

    /// A command's result.
    fn call(&mut self, id: u64, command: &str) -> Value {
        self.send(&json!({"id": id, "type": command}));
        loop {
            let message = self.receive();
            if message["id"] == id {
                assert_eq!(message["success"], true, "{message}");
                return message["result"].clone();
            }
        }
    }
}

/// A directory from Home Assistant 2026.11.2 runs once converted: the
/// session 2026.11.2 issued logs in, and the states, devices, entities
/// and names served are the 2026.6.1 ones.
#[test]
fn a_2026_11_install_runs() {
    let src = temp("runs11-src");
    let dest = temp("runs11-dest");
    let fixture = release("2026.11.2", &src, &[]);
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    let report = read_json(&dest.join("import-report.json"));
    let parts: Vec<&str> = report["split_devices"][0]["new_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap())
        .collect();

    // No test reaches the network: the integrations that would call a
    // bridge, a bulb or a cloud stay off. Their entities are then shown
    // from the registry alone, as `unavailable`.
    edit_store(&dest, "core.config_entries", |store| {
        for entry in store["data"]["entries"].as_array_mut().unwrap() {
            let domain = entry["domain"].as_str().unwrap();
            if ["met", "hue", "lifx", "switchbot_cloud"].contains(&domain) {
                entry["disabled_by"] = json!("user");
            }
        }
    });
    let devices_before =
        std::fs::read_to_string(dest.join(".storage/core.device_registry")).unwrap();

    let auth = store_of(&fixture, "auth");
    let session = auth["data"]["refresh_tokens"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["client_id"] == "http://127.0.0.1:8123/2026.11.2")
        .unwrap();
    assert_eq!(session["version"], "2026.11.2");

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
        "http%3A%2F%2F127.0.0.1%3A8123%2F2026.11.2",
        session["token"].as_str().unwrap()
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
    let bearer = format!("Bearer {access}");
    let state = |entity: &str| -> (u16, Value) {
        let (status, body) = http(
            port,
            "GET",
            &format!("/api/states/{entity}"),
            &[("Authorization", &bearer)],
            "",
        );
        (status, serde_json::from_str(&body).unwrap_or_default())
    };

    // Names: what Home Assistant 2026.11 shows as each entity's friendly
    // name, stored the 2026.6.1 way.
    for (entity, name) in [
        // The user's name, which 1.24 stored without the device's.
        ("light.desk", "Desk lamp Reading light"),
        // The user's name was the device's, so 1.24 stored "".
        ("switch.towel_warmer", "Towel rail"),
        // Kept apart in `compat_name` by 1.24.
        ("sensor.hall_temperature", "Warmth downstairs"),
        ("binary_sensor.hall_motion", "Hall movement"),
        // Named by the integration.
        ("sensor.hall_illuminance", "Hall Illuminance"),
        ("button.desk_restart", "Restart"),
        // `met`'s entities have the place's name back.
        ("weather.forecast_cabin", "Cabin"),
        ("weather.forecast_home", "Home"),
        ("person.test_owner", "Test Owner"),
        ("zone.zone_2026_11_2", "Zone 2026.11.2"),
        ("automation.desk_off_at_night", "Desk off at night"),
    ] {
        let (status, state) = state(entity);
        assert_eq!(status, 200, "{entity}");
        assert_eq!(state["attributes"]["friendly_name"], name, "{state}");
    }
    assert_eq!(state("light.desk").1["state"], "unavailable");
    assert_eq!(state("automation.desk_off_at_night").1["state"], "on");
    // Disabled entities have no state, and the old entity id is gone.
    for gone in [
        "light.porch",
        "sensor.porch_signal",
        "sensor.sun_next_dawn",
        "switch.towel_rail",
    ] {
        assert_eq!(state(gone).0, 404, "{gone}");
    }

    // The registries, as the 2026.6 app reads them.
    let mut ws = Ws::connect(port, &access);
    let devices = ws.call(1, "config/device_registry/list");
    let devices = devices.as_array().unwrap();
    // The seed's twelve, the shared one now two.
    assert_eq!(devices.len(), 13);
    let named =
        |name: &str| -> Vec<&Value> { devices.iter().filter(|d| d["name"] == name).collect() };
    // In 2026.6.1's shape: no field of storage version 3.
    let fields: Vec<&String> = devices[0].as_object().unwrap().keys().collect();
    assert_eq!(
        fields,
        [
            "area_id",
            "config_entries",
            "config_entries_subentries",
            "configuration_url",
            "connections",
            "created_at",
            "disabled_by",
            "entry_type",
            "hw_version",
            "id",
            "identifiers",
            "labels",
            "manufacturer",
            "model",
            "model_id",
            "modified_at",
            "name",
            "name_by_user",
            "primary_config_entry",
            "serial_number",
            "sw_version",
            "via_device_id"
        ]
    );
    // The two parts of the shared device, one config entry each.
    let desks = named("Desk");
    assert_eq!(desks.len(), 2);
    for desk in &desks {
        assert!(parts.contains(&desk["id"].as_str().unwrap()), "{desk}");
        assert_eq!(desk["config_entries"].as_array().unwrap().len(), 1);
        assert_eq!(desk["primary_config_entry"], desk["config_entries"][0]);
        assert_eq!(
            (&desk["name_by_user"], &desk["area_id"]),
            (&json!("Desk lamp"), &json!("study"))
        );
    }
    assert_eq!(named("Forecast").len(), 2, "`met`'s devices");
    assert_eq!(named("Porch")[0]["disabled_by"], "user");
    assert_eq!(
        named("Hall")[0]["via_device_id"],
        named("Hue Bridge")[0]["id"]
    );
    let socket = named("Socket 1")[0];
    let strip = named("Power strip")[0];
    assert_eq!(socket["config_entries"], strip["config_entries"]);
    assert_eq!(
        socket["config_entries_subentries"],
        strip["config_entries_subentries"]
    );
    assert_eq!(socket["via_device_id"], json!(null));

    let entities = ws.call(2, "config/entity_registry/list");
    let entity = |id: &str| -> &Value {
        let all = entities.as_array().unwrap();
        all.iter().find(|e| e["entity_id"] == id).unwrap()
    };
    let lifx_part = desks
        .iter()
        .find(|d| d["identifiers"] == json!([["lifx", "d0:73:d5:00:00:01"]]))
        .unwrap();
    assert_eq!(entity("light.desk")["device_id"], lifx_part["id"]);
    assert_eq!(entity("light.desk")["name"], "Desk lamp Reading light");
    assert_eq!(entity("light.desk")["unique_id"], "d0:73:d5:00:00:01");
    assert_eq!(
        entity("button.desk_restart")["unique_id"],
        "d0:73:d5:00:00:01_restart"
    );
    assert_eq!(entity("switch.towel_warmer")["name"], "Towel rail");
    assert_eq!(
        entity("switch.power_strip_socket_1")["device_id"],
        socket["id"]
    );
    assert!(entity("light.desk").get("compat_name").is_none());
    assert_eq!(entity("sensor.porch_signal")["disabled_by"], "user");
    assert_eq!(entity("sensor.hall_illuminance")["hidden_by"], "user");
    assert_eq!(
        entity("sensor.hall_temperature")["options"]["sensor"]["display_precision"],
        1
    );
    drop(ws);

    let _ = Command::new("kill")
        .args(["-TERM", &server.id().to_string()])
        .status();
    server.wait().unwrap();
    // The fields hars keeps on a row without using them are still there.
    let devices_after =
        std::fs::read_to_string(dest.join(".storage/core.device_registry")).unwrap();
    assert_eq!(devices_after, devices_before);
    assert!(
        devices_after.contains("\"composite_device_id\"")
            && devices_after.contains("\"parent_device_id\"")
    );
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// What hars's integrations look for at run time is there in a converted
/// 2026.11.2 directory: each finds its device by identifier and its
/// entities by unique id, in the form its 2026.6.1 code makes them. (No
/// bulb, bridge or cloud is contacted: these are the lookups alone.)
#[tokio::test]
async fn integrations_find_their_rows_in_a_converted_install() {
    for swapped in [false, true] {
        let src = temp(&format!("lookups-src-{swapped}"));
        let dest = temp(&format!("lookups-dest-{swapped}"));
        release("2026.11.2", &src, &[]);
        if swapped {
            // The other integration's part of the shared device first. It
            // still holds a copy of `lifx`'s identifier.
            edit_store(&src, "core.device_registry", |store| {
                let rows = store["data"]["devices"].as_array_mut().unwrap();
                let at: Vec<usize> = (0..rows.len())
                    .filter(|&i| rows[i]["name"] == "Desk")
                    .collect();
                rows.swap(at[0], at[1]);
            });
        }
        let out = hars(&dest, &["import", src.to_str().unwrap()]);
        assert!(out.status.success(), "{out:?}");
        let report = read_json(&dest.join("import-report.json"));
        let moved = report["converted"][0]["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c.as_str().unwrap().contains("is listed before another"));
        assert_eq!(moved, swapped, "{report}");

        let registries = ha_registry::Registries::load_strict(&dest).unwrap();
        let entries = read_json(&dest.join(".storage/core.config_entries"));
        let entry = |domain: &str, title: &str| -> Value {
            let all = entries["data"]["entries"].as_array().unwrap();
            let found = all
                .iter()
                .find(|e| e["domain"] == domain && e["title"] == title);
            found.unwrap().clone()
        };
        let hass = ha_core::Hass::new();

        // `lifx` (`crates/ha-integrations/src/lifx`): the entry's unique
        // id is the serial the bulb answers with, written with colons;
        // the device and the entities are found by it.
        let lifx = entry("lifx", "Desk");
        assert_eq!(
            (&lifx["version"], &lifx["minor_version"]),
            (&json!(1), &json!(1))
        );
        assert_eq!(lifx["unique_id"], "d0:73:d5:00:00:01");
        assert_eq!(lifx["data"], json!({"host": "192.0.2.21"}));
        let serial = "d0:73:d5:00:00:01";
        let names = ha_integrations::naming::Names::new(&hass, &registries, "lifx");
        let device = names.registry_device(serial).unwrap();
        assert_eq!(device.config_entries, [lifx["entry_id"].as_str().unwrap()]);
        assert_eq!(names.device_name(serial, "Desk"), "Desk lamp");
        let (id, name) = names.entity("light", serial, serial, "Desk", None).unwrap();
        assert_eq!(
            (id.as_str(), name.as_str()),
            ("light.desk", "Desk lamp Reading light")
        );
        let restart = format!("{serial}_restart");
        let (id, name) = names
            .entity("button", &restart, serial, "Desk", Some("Restart"))
            .unwrap();
        assert_eq!(
            (id.as_str(), name.as_str()),
            ("button.desk_restart", "Desk lamp Restart")
        );

        // `switchbot_cloud`: nothing in it changed.
        let names = ha_integrations::naming::Names::new(&hass, &registries, "switchbot_cloud");
        let (id, name) = names
            .entity("switch", "AABBCC000001", "AABBCC000001", "Towel rail", None)
            .unwrap();
        assert_eq!(
            (id.as_str(), name.as_str()),
            ("switch.towel_warmer", "Towel rail")
        );

        // `met`: the place from the entry as version 1 has it, the entity
        // by the unique id made from it, its device "Forecast" (hars
        // shows "Forecast Cabin", as 2026.6.1 does).
        let config = ha_core::config::CoreConfig::load(&dest).unwrap();
        let cabin = entry("met", "Cabin");
        assert_eq!(
            (&cabin["version"], &cabin["minor_version"]),
            (&json!(1), &json!(1))
        );
        assert_eq!(
            cabin["data"],
            json!({"elevation": 12, "latitude": 60.39, "longitude": 5.32, "name": "Cabin"})
        );
        let place =
            ha_integrations::met::Location::new(cabin["data"].as_object().unwrap(), &config)
                .unwrap();
        assert_eq!(
            (place.unique_id.as_str(), place.name.as_str()),
            ("60.39-5.32", "Cabin")
        );
        assert_eq!(
            (place.latitude, place.longitude, place.elevation),
            (60.39, 5.32, 12.0)
        );
        let home = entry("met", "Home");
        assert_eq!(home["data"], json!({"track_home": true}));
        let place = ha_integrations::met::Location::new(home["data"].as_object().unwrap(), &config)
            .unwrap();
        assert_eq!(
            (place.unique_id.as_str(), place.name.as_str()),
            ("home", "Home")
        );
        for (unique_id, entity_id, name) in [
            ("60.39-5.32", "weather.forecast_cabin", "Cabin"),
            ("home", "weather.forecast_home", "Home"),
        ] {
            let row = registries
                .entities
                .iter()
                .find(|e| e.platform == "met" && e.unique_id == Some(json!(unique_id)))
                .unwrap();
            assert_eq!(row.entity_id, entity_id);
            assert_eq!(row.original_name.as_deref(), Some(name));
            assert_eq!(registries.device_name_of(row), Some("Forecast"));
        }

        // `hue`: each identifier names one device, the `hue` entry's.
        let hue = entry("hue", "Hue Bridge");
        for id in [
            "00000000-0000-4000-8000-000000000100",
            "00000000-0000-4000-8000-000000000101",
        ] {
            let holders: Vec<_> = registries
                .devices
                .iter()
                .filter(|d| d.identifiers.contains(&json!(["hue", id])))
                .collect();
            assert_eq!(holders.len(), 1, "{id}");
            assert_eq!(
                holders[0].config_entries,
                [hue["entry_id"].as_str().unwrap()]
            );
        }

        // `mobile_app`: a phone that registers again gets its device, not
        // a new one.
        let phone = entry("mobile_app", "Test Phone");
        let before = registries.devices.len();
        let registry = ha_registry::Registry::in_memory(&hass, registries.clone());
        let device = registry.get_or_create_device(ha_registry::DeviceInfo {
            config_entry_id: phone["entry_id"].as_str().unwrap().to_owned(),
            identifiers: vec![json!(["mobile_app", "synthetic-phone"])],
            ..Default::default()
        });
        assert_eq!(device.name.as_deref(), Some("Test Phone"));
        assert_eq!(registry.snapshot().devices.len(), before);

        // And hars's own writers give back the files as imported: nothing
        // on a row is lost when hars next saves a registry.
        let copy = temp(&format!("lookups-copy-{swapped}"));
        registries.save_devices(&copy).unwrap();
        registries.save_entities(&copy).unwrap();
        for key in ["core.device_registry", "core.entity_registry"] {
            assert_eq!(
                std::fs::read(copy.join(".storage").join(key)).unwrap(),
                std::fs::read(dest.join(".storage").join(key)).unwrap(),
                "{key}"
            );
        }
        for d in [src, dest, copy] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// What the fixtures don't hold, on Home Assistant 2026.11.2's directory
/// with rows added: the rules of 2026.11's own code for a last state, an
/// entity still on a split device's old id, a name given after the update,
/// a child device in its parent's area, a removed child device, a setting
/// 2026.6.1 has no place for, and an action on a parent device.
#[test]
fn rows_the_fixtures_do_not_hold_are_converted_by_home_assistants_rules() {
    let src = temp("corners-src");
    let dest = temp("corners-dest");
    let fixture = release("2026.11.2", &src, &[]);
    let devices = store_of(&fixture, "core.device_registry");
    let device = |list: &str, name: &str| -> Value {
        let all = devices["data"][list].as_array().unwrap();
        all.iter().find(|d| d["name"] == name).unwrap().clone()
    };
    let old_device = device("devices", "Desk")["composite_device_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let strip = device("devices", "Power strip");
    let (strip_id, socket_id) = (
        strip["id"].as_str().unwrap().to_owned(),
        device("child_devices", "Socket 1")["id"]
            .as_str()
            .unwrap()
            .to_owned(),
    );

    let row = |entity_id: &str, state: &str, registry_id: Option<Value>| {
        let mut row = json!({"state": {"entity_id": entity_id, "state": state, "attributes": {}},
                             "extra_data": null, "last_seen": "2026-11-15T12:00:00+00:00"});
        if let Some(id) = registry_id {
            row["entity_registry_id"] = id;
        }
        row
    };
    edit_store(&src, "core.restore_state", |store| {
        let rows = store["data"].as_array_mut().unwrap();
        // Stored for an entity with no registry row, under an id a
        // registered entity has now: 2026.11 never gives it to that one.
        rows.push(row("light.hall", "a", Some(json!(null))));
        // Stored for a registry row that is gone: 2026.11 gives it to
        // nobody, whether a registered entity has the id now ...
        rows.push(row("light.hall", "b", Some(json!("gone"))));
        // ... or nothing has (2026.6.1 would give it to whatever takes it).
        rows.push(row("light.attic", "c", Some(json!("gone too"))));
        // Two under one free id: an unregistered entity of that id gets
        // the one stored for an unregistered entity.
        rows.push(row("scene.reused", "d", Some(json!("gone three"))));
        rows.push(row("scene.reused", "e", Some(json!(null))));
        // A second one for an id: the later one, as in Home Assistant.
        rows.push(row("scene.evening", "f", Some(json!(null))));
        // Stored before 2026.11, for an entity that is registered.
        rows.push(row("sensor.hall_temperature", "g", None));
        // Not an entity id: Home Assistant skips the row, whatever
        // registry id it carries, and so it replaces nothing here.
        let hall = rows
            .iter()
            .find(|r| r["state"]["entity_id"] == "light.desk")
            .map(|r| r["entity_registry_id"].clone());
        rows.push(row("Invalid", "h", hall));
    });
    let old = old_device.clone();
    edit_store(&src, "core.entity_registry", move |store| {
        store["data"]["settings"]["entity_id_parts"] = json!(["area", "device", "entity"]);
        // A removed entity with a name given in 2026.11: stored without
        // its device's name, and the row doesn't say which device.
        store["data"]["deleted_entities"][0]["name"] = json!("Temperature");
        for entity in store["data"]["entities"].as_array_mut().unwrap() {
            match entity["entity_id"].as_str().unwrap() {
                // Not moved to its part yet (Home Assistant moves it when
                // it next loads the file).
                "light.desk" => entity["device_id"] = json!(old),
                // On the old id with no part of its own integration.
                "sensor.towel_rail_battery" => entity["device_id"] = json!(old),
                // Named by the user in 2026.11: stored without the
                // device's name, shown with it.
                "sensor.test_phone_battery_level" => entity["name"] = json!("Charge"),
                // A name, and no device to put in front of it.
                "automation.wait_then_off" => entity["name"] = json!("Later"),
                _ => {}
            }
        }
    });
    let entry = strip["config_entry_id"].clone();
    edit_store(&src, "core.device_registry", move |store| {
        // The strip is in an area; its socket has none of its own.
        for d in store["data"]["devices"].as_array_mut().unwrap() {
            if d["name"] == "Power strip" {
                d["area_id"] = json!("hall");
                d["labels"] = json!(["lights"]);
            }
        }
        // A removed child device, disabled through its parent when it
        // was removed; and an orphan with its integration recorded.
        let removed = store["data"]["deleted_devices"].as_array_mut().unwrap();
        let mut child = removed[0].clone();
        child["id"] = json!("removed-socket");
        child["config_entry_id"] = entry.clone();
        child["identifiers"] = json!([["mqtt", "synthetic-strip-socket-2"]]);
        child["disabled_by"] = json!("device");
        removed.push(child.clone());
        child["id"] = json!("orphan");
        child["config_entry_id"] = json!(null);
        child["disabled_by"] = json!("user");
        child["domain"] = json!("mqtt");
        child["orphaned_timestamp"] = json!(1781524800.0);
        removed.push(child);
    });
    // An action on the strip, its id written with an escape.
    let mut yaml = std::fs::read_to_string(src.join("automations.yaml")).unwrap();
    yaml.push_str(&format!(
        "- id: strip\n  alias: Strip off\n  triggers: [{{trigger: time, at: '01:00:00'}}]\n  \
         actions:\n  - action: switch.turn_off\n    target:\n      device_id: \"{}\\u003{}\"\n",
        &strip_id[..31],
        &strip_id[31..]
    ));
    assert!(!yaml.contains(&strip_id));
    // A target with 2026.11's `exclude`.
    yaml.push_str(
        "- id: all-but\n  alias: All but the hall\n  triggers: [{trigger: time, at: '02:00:00'}]\n  \
         actions:\n  - action: light.turn_off\n    target:\n      entity_id: all\n      \
         exclude:\n        entity_id: light.hall\n",
    );
    std::fs::write(src.join("automations.yaml"), yaml).unwrap();

    // A release before 2026.10 keeps a label to the devices that have it,
    // so there is nothing to say of the strip's label there.
    std::fs::write(src.join(".HA_VERSION"), "2026.9.4").unwrap();
    let out = hars(&dest, &["import", src.to_str().unwrap(), "--dry-run"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("the label(s)"),
        "{out:?}"
    );
    std::fs::write(src.join(".HA_VERSION"), "2026.11.2").unwrap();

    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    let report = read_json(&dest.join("import-report.json"));
    let all_but = report["automations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["alias"] == "All but the hall")
        .unwrap();
    assert!(
        all_but["warning"]
            .as_str()
            .unwrap()
            .contains("a target in it has `exclude`"),
        "{all_but}"
    );
    let dropped: Vec<(&str, &str)> = report["dropped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| (d["store"].as_str().unwrap(), d["what"].as_str().unwrap()))
        .collect();
    let link = format!("sensor.towel_rail_battery's link to the device {old_device}");
    assert_eq!(
        dropped,
        [
            (
                "core.entity_registry",
                "the setting for what a new entity's id is made of (`entity_id_parts`)"
            ),
            ("core.entity_registry", link.as_str()),
            ("core.restore_state", "a last state stored for light.hall"),
            ("core.restore_state", "a last state stored for light.hall"),
            ("core.restore_state", "a last state stored for light.attic"),
            ("core.restore_state", "a last state stored for scene.reused"),
            (
                "core.restore_state",
                "a last state stored for scene.evening"
            ),
        ]
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("left out of core.restore_state: a last state stored for light.hall"),
        "{stdout}"
    );
    // The removed entity's name is not guessed at: the row is as it was,
    // and the report says so.
    assert_eq!(report["not_converted"].as_array().unwrap().len(), 1);
    assert_eq!(
        (
            &report["not_converted"][0]["store"],
            &report["not_converted"][0]["what"]
        ),
        (
            &json!("core.entity_registry"),
            &json!("the name \"Temperature\" of the removed entity sensor.hall_old_sensor")
        )
    );
    assert!(
        stdout.contains("not converted in core.entity_registry: the name \"Temperature\""),
        "{stdout}"
    );
    let removed_entities = read_json(&dest.join(".storage/core.entity_registry"));
    assert_eq!(
        removed_entities["data"]["deleted_entities"][0]["name"],
        "Temperature"
    );

    let restore = read_json(&dest.join(".storage/core.restore_state"));
    let mut states: Vec<(&str, &str)> = restore["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            assert!(row.get("entity_registry_id").is_none(), "{row}");
            let state = &row["state"];
            (
                state["entity_id"].as_str().unwrap(),
                state["state"].as_str().unwrap(),
            )
        })
        .collect();
    states.sort_unstable();
    assert_eq!(
        states,
        [
            ("Invalid", "h"),
            ("light.desk", "off"),
            ("person.person_2026_11_2", "unknown"),
            ("person.test_owner", "unknown"),
            ("scene.evening", "f"),
            ("scene.reused", "e"),
            ("sensor.hall_temperature", "g"),
            ("switch.towel_warmer", "on"),
        ]
    );

    let entities = read_json(&dest.join(".storage/core.entity_registry"));
    let entity = |id: &str| -> &Value {
        let all = entities["data"]["entities"].as_array().unwrap();
        all.iter().find(|e| e["entity_id"] == id).unwrap()
    };
    let lifx_part = entity("button.desk_restart")["device_id"].clone();
    assert!(lifx_part.is_string() && lifx_part != json!(old_device));
    assert_eq!(entity("light.desk")["device_id"], lifx_part);
    assert_eq!(entity("light.desk")["name"], "Desk lamp Reading light");
    assert_eq!(
        entity("sensor.towel_rail_battery")["device_id"],
        json!(null)
    );
    assert_eq!(
        entity("sensor.test_phone_battery_level")["name"],
        "Test Phone Charge"
    );
    assert_eq!(entity("automation.wait_then_off")["name"], "Later");

    // The socket is in the strip's area, as 2026.9 has it
    // (`async_get_effective_area_id`), so an action on the area reaches it.
    let converted = read_json(&dest.join(".storage/core.device_registry"));
    let socket = converted["data"]["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == socket_id.as_str())
        .unwrap();
    assert_eq!(socket["area_id"], "hall");
    let registries = ha_registry::Registries::load_strict(&dest).unwrap();
    let target = json!({"area_id": "hall"});
    let reached = registries.target_entities(target.as_object().unwrap());
    assert!(
        reached.contains(&"switch.power_strip_socket_1".to_owned()),
        "{reached:?}"
    );
    // The removed child device comes back enabled, as Home Assistant
    // brings it back (`_calculate_disable_by`); the orphan keeps its
    // integration's name on the row.
    let removed = |id: &str| -> &Value {
        let all = converted["data"]["deleted_devices"].as_array().unwrap();
        all.iter().find(|d| d["id"] == id).unwrap()
    };
    assert_eq!(removed("removed-socket")["disabled_by"], json!(null));
    assert_eq!(
        removed("removed-socket")["config_entries"],
        json!([strip["config_entry_id"]])
    );
    assert!(removed("removed-socket").get("domain").is_none());
    let orphan = removed("orphan");
    assert_eq!(
        (
            &orphan["config_entries"],
            &orphan["config_entries_subentries"],
            &orphan["domain"]
        ),
        (&json!([]), &json!({}), &json!("mqtt"))
    );
    assert_eq!(
        (&orphan["disabled_by"], &orphan["orphaned_timestamp"]),
        (&json!("user"), &json!(1781524800.0))
    );

    // An action on the strip reached the socket in 2026.9 and won't in
    // hars: said of the automation and of the device, though the file
    // spells the id another way.
    assert_eq!(
        report["parent_devices"],
        json!([{"id": strip_id, "name": "Power strip", "children": [socket_id],
                "used_in": ["configuration.yaml or a file it includes"], "labels": ["lights"]}])
    );
    let strip_off = report["automations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["alias"] == "Strip off")
        .unwrap();
    assert_eq!(strip_off["status"], "ok");
    assert!(
        strip_off["warning"]
            .as_str()
            .unwrap()
            .contains("which has child devices"),
        "{strip_off}"
    );
    assert!(
        stdout.contains("automation Strip off loads, but it names the device")
            && stdout.contains("name the child devices too")
            && stdout.contains("has child devices and the label(s) lights"),
        "{stdout}"
    );
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// The import only reads its source: a destination inside it is refused
/// before anything is made there, a dry run included.
#[test]
fn a_destination_inside_the_source_is_refused() {
    let src = temp("outside-src");
    release("2026.7.4", &src, &[]);
    for flag in ["--dry-run", "--force"] {
        // Plainly inside, and through a `..` over a directory not there yet.
        for dest in [
            src.join("hars").join("config"),
            src.join("new").join("..").join("hars"),
        ] {
            let out = hars(&dest, &["import", src.to_str().unwrap(), flag]);
            assert!(!out.status.success(), "{flag} {dest:?}");
            assert!(
                String::from_utf8_lossy(&out.stderr)
                    .contains("the destination is inside the source"),
                "{out:?}"
            );
            assert!(
                !src.join("hars").exists() && !src.join("new").exists(),
                "{flag}"
            );
        }
    }
    // A destination outside, reached through the source: it is made where
    // it ends up, and nothing on the way to it.
    let outside = temp("outside-dest");
    let through = src
        .join("new")
        .join("..")
        .join("..")
        .join(outside.file_name().unwrap());
    let out = hars(&through, &["import", src.to_str().unwrap(), "--dry-run"]);
    assert!(out.status.success(), "{out:?}");
    assert!(!src.join("new").exists());
    let out = hars(&through, &["import", src.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(!src.join("new").exists() && outside.join("import-report.json").is_file());
    for d in [src, outside] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// The names the importer keeps for itself are kept in any case: on a
/// file system that doesn't tell `IMPORT-REPORT.JSON` from
/// `import-report.json`, the report would replace the included file.
#[test]
fn a_reserved_name_in_another_case_is_refused() {
    let src = temp("case-src");
    let dest = temp("case-dest");
    ha_config(&src);
    std::fs::write(src.join("IMPORT-REPORT.JSON"), "{\"mine\": 1}\n").unwrap();
    std::fs::write(
        src.join("configuration.yaml"),
        "x: !include IMPORT-REPORT.JSON\n",
    )
    .unwrap();
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("is a path hars keeps for itself"),
        "{out:?}"
    );
    assert!(!dest.join(".storage").exists());
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// A secrets file that only needs its mode put right is replaced in the
/// commit: where that can't be done, nothing else is changed either.
#[cfg(unix)]
#[test]
fn a_secrets_file_that_cannot_be_replaced_rolls_the_import_back() {
    use std::os::unix::fs::PermissionsExt;
    let src = temp("locked-src");
    let dest = temp("locked-dest");
    ha_config(&src);
    std::fs::create_dir_all(src.join("nested")).unwrap();
    std::fs::write(src.join("nested/a.yaml"), "pw: !secret pw\n").unwrap();
    std::fs::write(src.join("nested/secrets.yaml"), "pw: secret-value\n").unwrap();
    std::fs::write(
        src.join("configuration.yaml"),
        "x: !include nested/a.yaml\n",
    )
    .unwrap();
    // The destination has the same files, the secrets readable by all,
    // in a directory nothing can be put into.
    std::fs::create_dir_all(dest.join("nested")).unwrap();
    for file in ["a.yaml", "secrets.yaml"] {
        std::fs::copy(
            src.join("nested").join(file),
            dest.join("nested").join(file),
        )
        .unwrap();
    }
    let secrets = dest.join("nested/secrets.yaml");
    let mode = |m| std::fs::Permissions::from_mode(m);
    std::fs::set_permissions(&secrets, mode(0o644)).unwrap();
    std::fs::set_permissions(dest.join("nested"), mode(0o555)).unwrap();
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    std::fs::set_permissions(dest.join("nested"), mode(0o755)).unwrap();
    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("rolled back; nothing changed"),
        "{out:?}"
    );
    assert!(!dest.join(".storage/auth").exists() && !dest.join("configuration.yaml").exists());
    assert!(!dest.join("import-report.json").exists());
    assert_eq!(
        std::fs::metadata(&secrets).unwrap().permissions().mode() & 0o777,
        0o644
    );
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// A destination file that is a hard link to the source's is not changed
/// in place: making `secrets.yaml` owner-only leaves the source's mode.
#[cfg(unix)]
#[test]
fn a_hard_linked_secrets_file_is_replaced_not_changed() {
    use std::os::unix::fs::PermissionsExt;
    let src = temp("linked-src");
    let dest = temp("linked-dest");
    ha_config(&src);
    std::fs::write(src.join("configuration.yaml"), "http:\n  key: !secret pw\n").unwrap();
    let theirs = src.join("secrets.yaml");
    std::fs::set_permissions(&theirs, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::hard_link(&theirs, dest.join("secrets.yaml")).unwrap();
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&theirs), 0o644, "the source's");
    assert_eq!(mode(&dest.join("secrets.yaml")), 0o600);
    assert_eq!(
        std::fs::read(dest.join("secrets.yaml")).unwrap(),
        std::fs::read(&theirs).unwrap()
    );
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// `met` entries of 2026.11 that the fixture doesn't hold: a location with
/// the radius the selector can store, which `met` doesn't use, and an
/// entry with no place at all (2026.11's migration leaves one so when the
/// old entry had none).
#[test]
fn met_entries_of_2026_11_convert() {
    for (name, edit, says) in [
        (
            "radius",
            (|data: &mut Value| data["location"]["radius"] = json!(150)) as fn(&mut Value),
            "the location's `radius`, which `met` doesn't use, left out",
        ),
        (
            "no-place",
            |data: &mut Value| {
                data.as_object_mut().unwrap().remove("location");
            },
            "it has no place",
        ),
    ] {
        let src = temp(&format!("met-{name}-src"));
        let dest = temp(&format!("met-{name}-dest"));
        release("2026.11.2", &src, &[]);
        edit_store(&src, "core.config_entries", |s| {
            let entries = s["data"]["entries"].as_array_mut().unwrap();
            edit(&mut entries.iter_mut().find(|e| e["title"] == "Cabin").unwrap()["data"]);
        });
        let out = hars(&dest, &["import", src.to_str().unwrap()]);
        assert!(out.status.success(), "{name}: {out:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains(says), "{name}: {stdout}");
        let entries = read_json(&dest.join(".storage/core.config_entries"));
        let cabin = entries["data"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["title"] == "Cabin")
            .unwrap();
        let expected = if name == "radius" {
            json!({"elevation": 12, "latitude": 60.39, "longitude": 5.32, "name": "Cabin"})
        } else {
            json!({"elevation": 12, "name": "Cabin"})
        };
        assert_eq!(
            (&cabin["version"], &cabin["data"]),
            (&json!(1), &expected),
            "{name}"
        );
        for d in [src, dest] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// A part of a split device that still holds a copy of `lifx`'s
/// identifier, with `lifx`'s own part gone: imported, and said.
#[test]
fn a_copy_of_an_identifier_without_its_original_is_said() {
    let src = temp("copy-src");
    let dest = temp("copy-dest");
    release("2026.11.2", &src, &[]);
    edit_store(&src, "core.device_registry", |s| {
        let rows = s["data"]["devices"].as_array_mut().unwrap();
        rows.retain(|d| !(d["name"] == "Desk" && d["has_composite_identifiers"] == false));
    });
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(
            "1 identifier is of an integration hars runs on a device that Home Assistant 2026.8 \
             split off for another integration, and that integration's own part is gone"
        ),
        "{stdout}"
    );
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// Whether the file system `dir` is on takes two spellings of a name for
/// one file (macOS's does by default; CI's Linux doesn't, and there the
/// decision is covered by `import::tests` on its own).
fn folds(dir: &Path, name: &str, other: &str) -> bool {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(name), "").unwrap();
    let folded = dir.join(other).exists();
    std::fs::remove_file(dir.join(name)).unwrap();
    folded
}

/// One file the configuration includes under two names, where the file
/// system takes them for one: staged and committed as two files, the
/// second would go over the first and over the copy of what was there
/// before. It is refused before anything is replaced, `--force` or not.
#[test]
fn one_file_under_two_names_is_refused() {
    let src = temp("two-names-src");
    let dest = temp("two-names-dest");
    if !folds(&src, "parts.yaml", "Parts.yaml") {
        return;
    }
    ha_config(&src);
    std::fs::write(src.join("parts.yaml"), "new: 1\n").unwrap();
    std::fs::write(
        src.join("configuration.yaml"),
        "a: !include parts.yaml\nb: !include Parts.yaml\n",
    )
    .unwrap();
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("parts.yaml"), "old: 1\n").unwrap();
    for flags in [&["--force"][..], &["--dry-run"], &[]] {
        let mut args = vec!["import", src.to_str().unwrap()];
        args.extend(flags);
        let out = hars(&dest, &args);
        assert!(!out.status.success(), "{flags:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("are one file in the destination") && stderr.contains("arts.yaml"),
            "{stderr}"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("parts.yaml")).unwrap(),
            "old: 1\n"
        );
        let mut left: Vec<_> = std::fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        left.sort();
        assert_eq!(left, ["parts.yaml"], "{flags:?}");
    }
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// A name the file system takes for one the importer keeps for itself,
/// though no rule about letters says so: APFS reads the long s in
/// `import-report.jſon` as an `s`, and the report would replace the file.
#[test]
fn a_reserved_name_in_a_spelling_the_file_system_folds_is_refused() {
    let src = temp("fold-src");
    let dest = temp("fold-dest");
    let long_s = "import-report.j\u{17f}on";
    if !folds(&src, "import-report.json", long_s) {
        return;
    }
    ha_config(&src);
    std::fs::write(src.join(long_s), "mine: 1\n").unwrap();
    std::fs::write(
        src.join("configuration.yaml"),
        format!("x: !include {long_s}\n"),
    )
    .unwrap();
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("is a path hars keeps for itself"),
        "{out:?}"
    );
    nothing_imported(&dest);
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// A store the destination has under another spelling of its name, where
/// the file system takes the two for one file: by names alone the import
/// would not see it as there (and replace it without `--force`), or would
/// put the new one and remove "the other" in one go. It asks the file
/// system, and stops.
#[test]
fn a_destination_file_under_another_spelling_is_refused() {
    let probe = temp("spelling-probe");
    if !folds(&probe, "auth", "AUTH") {
        return;
    }
    let _ = std::fs::remove_dir_all(probe);
    for (theirs, ours) in [
        ("frontend.system_data", "FRONTEND.SYSTEM_DATA"),
        ("frontend.user_data_", "frontend.user_data_"),
        ("auth", "AUTH"),
    ] {
        let src = temp("spelling-src");
        let dest = temp("spelling-dest");
        let owner = ha_config(&src);
        write_store(&src, "frontend.system_data", 1, 1, json!({"theirs": 1}));
        // The owner's settings, under the same id in capitals.
        let (theirs, ours) = if theirs.ends_with('_') {
            (
                format!("{theirs}{owner}"),
                format!("{ours}{}", owner.to_uppercase()),
            )
        } else {
            (theirs.to_owned(), ours.to_owned())
        };
        std::fs::create_dir_all(dest.join(".storage")).unwrap();
        std::fs::write(dest.join(".storage").join(&ours), "mine").unwrap();
        for flags in [&[][..], &["--force"], &["--dry-run"]] {
            let mut args = vec!["import", src.to_str().unwrap()];
            args.extend(flags);
            let out = hars(&dest, &args);
            assert!(!out.status.success(), "{ours} {flags:?}");
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stderr.contains(&format!("{ours} in ")) && stderr.contains("are one file there"),
                "{stderr}"
            );
            let left: Vec<_> = std::fs::read_dir(dest.join(".storage"))
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(left, std::slice::from_ref(&ours), "{theirs} {flags:?}");
            assert_eq!(
                std::fs::read_to_string(dest.join(".storage").join(&ours)).unwrap(),
                "mine"
            );
        }
        for d in [src, dest] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// A directory the configuration includes whole (`!include_dir_*`) is
/// loaded whole. A YAML file the destination has in one, which the import
/// doesn't bring, would be loaded beside the imported ones: an old
/// automation running again. It stops a plain import, and `--force`
/// removes it with what else it replaces. All four tags, and directories
/// inside the included one.
#[test]
fn yaml_left_in_an_included_directory_is_not_left_active() {
    let src = temp("stale-src");
    let dest = temp("stale-dest");
    ha_config(&src);
    let write = |root: &Path, rel: &str, text: &str| {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write(
        &src,
        "configuration.yaml",
        "automation: !include_dir_merge_list parts\na: !include_dir_list lists\n\
         b: !include_dir_named named\nc: !include_dir_merge_named merged\n",
    );
    write(&src, "parts/new.yaml", "- id: new\n");
    write(&src, "lists/one.yaml", "1\n");
    write(&src, "named/one.yaml", "x: 1\n");
    write(&src, "merged/one.yaml", "x: 1\n");
    write(&src, "merged/deep/two.yaml", "y: 2\n");
    // In the destination only: one per tag, two of them further in.
    let stale = [
        "lists/sub/old.yaml",
        "merged/deep/old.yaml",
        "named/old.yaml",
        "parts/old.yaml",
    ];
    for rel in stale {
        write(&dest, rel, "- id: old\n");
    }
    // What no include would load stays: not YAML, hidden, a secrets
    // file, and a directory the configuration doesn't include.
    let kept = [
        "parts/notes.txt",
        "parts/.hidden.yaml",
        "named/secrets.yaml",
        "other/keep.yaml",
    ];
    for rel in kept {
        write(&dest, rel, "mine\n");
    }

    // A plain import stops, and says which files.
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    for rel in stale {
        assert!(
            stderr.contains(&format!("{rel} (removed)")),
            "{rel}: {stderr}"
        );
    }
    assert!(stderr.contains("--force"), "{stderr}");
    for rel in stale.iter().chain(&kept) {
        assert!(dest.join(rel).is_file(), "{rel}");
    }
    assert!(!dest.join(".storage").exists() && !dest.join("configuration.yaml").exists());

    // A dry run says what `--force` would remove.
    let out = hars(&dest, &["import", src.to_str().unwrap(), "--dry-run"]);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("removed: {}", stale.join(", "))),
        "{stdout}"
    );
    assert!(dest.join("parts/old.yaml").is_file());

    let out = hars(&dest, &["import", src.to_str().unwrap(), "--force"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        read_json(&dest.join("import-report.json"))["removed"],
        json!(stale)
    );
    for rel in stale {
        assert!(!dest.join(rel).exists(), "{rel}");
    }
    for rel in kept {
        assert_eq!(
            std::fs::read_to_string(dest.join(rel)).unwrap(),
            "mine\n",
            "{rel}"
        );
    }
    for rel in ["parts/new.yaml", "merged/deep/two.yaml", "lists/one.yaml"] {
        assert!(dest.join(rel).is_file(), "{rel}");
    }
    assert!(!dest.join(".import-backup").exists());
    // What hars loads now is what the source's configuration says.
    let loaded = ha_core::yaml::load_file(&dest.join("configuration.yaml"), None).unwrap();
    assert_eq!(
        loaded.to_json(),
        json!({"automation": [{"id": "new"}], "a": [1], "b": {"one": {"x": 1}},
               "c": {"x": 1, "y": 2}})
    );
    // And the same import again has nothing to remove.
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// A copy of an integration's identifier on another integration's live
/// device, with the integration's own device removed (not lost): Home
/// Assistant 2026.8 brings the removed one back when the integration
/// registers again, and hars would find the other integration's device by
/// the identifier and attach the phone to it. The import stops, and names
/// the two.
#[test]
fn a_copy_beside_a_removed_original_is_refused() {
    let src = temp("removed-original-src");
    let dest = temp("removed-original-dest");
    let fixture = release("2026.11.2", &src, &[]);
    let devices = store_of(&fixture, "core.device_registry");
    let id_of = |name: &str, copy: bool| -> String {
        let all = devices["data"]["devices"].as_array().unwrap();
        let found = all
            .iter()
            .find(|d| d["name"] == name && d["has_composite_identifiers"] == copy);
        found.unwrap()["id"].as_str().unwrap().to_owned()
    };
    let (phone, other) = (id_of("Test Phone", false), id_of("Desk", true));
    edit_store(&src, "core.device_registry", |store| {
        let rows = store["data"]["devices"].as_array_mut().unwrap();
        let at = rows.iter().position(|d| d["name"] == "Test Phone").unwrap();
        let row = rows.remove(at);
        // The part of the split device that still holds copies gets one
        // of the phone's identifier.
        let copy = rows
            .iter_mut()
            .find(|d| d["has_composite_identifiers"] == true)
            .unwrap();
        copy["identifiers"]
            .as_array_mut()
            .unwrap()
            .push(json!(["mobile_app", "synthetic-phone"]));
        // The phone's own device, removed (`DeletedDeviceEntry`'s row).
        let mut removed = json!({"disabled_by_undefined": false, "orphaned_timestamp": null,
                                 "domain": null});
        for key in [
            "area_id",
            "config_entry_id",
            "config_subentry_id",
            "connections",
            "created_at",
            "disabled_by",
            "identifiers",
            "id",
            "labels",
            "modified_at",
            "name_by_user",
        ] {
            removed[key] = row[key].clone();
        }
        store["data"]["deleted_devices"]
            .as_array_mut()
            .unwrap()
            .push(removed);
    });
    for flags in [&["--dry-run"][..], &["--force"], &[]] {
        let mut args = vec!["import", src.to_str().unwrap()];
        args.extend(flags);
        let out = hars(&dest, &args);
        assert!(!out.status.success(), "{flags:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(&format!(
                "core.device_registry: the device {other} (Desk lamp) belongs to another \
                 integration and still holds an identifier of the mobile_app integration's \
                 removed device {phone}"
            )),
            "{stderr}"
        );
        assert!(stderr.contains("Nothing was imported"), "{stderr}");
        nothing_imported(&dest);
    }
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// A device registry of version 3 beside an entity registry already at
/// 1.22, with nothing to change in it but two device links: an entity on
/// a split device's old id that has no part of its own, and one on an id
/// no device has. Both are detached, as Home Assistant 2026.8 detaches
/// them when it loads the file, and the store is written with that: what
/// the report says was done is what is on disk.
#[test]
fn a_detached_entity_is_written_detached() {
    let src = temp("detached-src");
    let dest = temp("detached-dest");
    let fixture = release("2026.8.3", &src, &[]);
    let devices = store_of(&fixture, "core.device_registry");
    let old = devices["data"]["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|d| d["composite_device_id"].as_str())
        .unwrap()
        .to_owned();
    let old_id = old.clone();
    edit_store(&src, "core.entity_registry", move |store| {
        // As 2026.6.1 writes the file: 1.22, no `settings`.
        store["minor_version"] = json!(22);
        store["data"].as_object_mut().unwrap().remove("settings");
        for entity in store["data"]["entities"].as_array_mut().unwrap() {
            match entity["entity_id"].as_str().unwrap() {
                "sensor.towel_rail_battery" => entity["device_id"] = json!(old_id),
                "sensor.hall_illuminance" => entity["device_id"] = json!("no-such-device"),
                _ => {}
            }
        }
    });
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    let report = read_json(&dest.join("import-report.json"));
    let dropped: Vec<&str> = report["dropped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["what"].as_str().unwrap())
        .collect();
    assert_eq!(
        dropped,
        [
            "sensor.hall_illuminance's link to the device no-such-device".to_owned(),
            format!("sensor.towel_rail_battery's link to the device {old}"),
        ]
    );
    let converted: Vec<(&str, &str)> = report["converted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["key"].as_str().unwrap(), c["from"].as_str().unwrap()))
        .collect();
    assert_eq!(
        converted,
        [
            ("core.device_registry", "3.2"),
            ("core.entity_registry", "1.22")
        ]
    );
    let entities = read_json(&dest.join(".storage/core.entity_registry"));
    for entity in entities["data"]["entities"].as_array().unwrap() {
        let id = entity["entity_id"].as_str().unwrap();
        if id == "sensor.towel_rail_battery" || id == "sensor.hall_illuminance" {
            assert_eq!(entity["device_id"], json!(null), "{id}");
        }
    }

    // Beside a device registry 2026.6.1 wrote, an id no device has is left
    // as it is, as 2026.6.1 leaves it.
    let src = temp("dangling-src");
    let dest = temp("dangling-dest");
    release("2026.7.4", &src, &[]);
    edit_store(&src, "core.entity_registry", |store| {
        store["data"]["entities"][0]["device_id"] = json!("no-such-device");
    });
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        std::fs::read(dest.join(".storage/core.entity_registry")).unwrap(),
        std::fs::read(src.join(".storage/core.entity_registry")).unwrap()
    );
    for d in [temp("detached-src"), temp("detached-dest"), src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}
