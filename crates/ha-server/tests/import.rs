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
    assert_eq!(report["importer_version"], 3);
    assert_eq!(report["ha_version"], "2026.6.1");
    assert_eq!(report["ha_version_tested"], true);
    assert_eq!(report["behaves_like"], "2026.6.1");
    assert_eq!(report["newer_stores"], json!([]));
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
            "this is Home Assistant 2025.12.4; hars imports 2026.6 and 2026.7. \
             Update Home Assistant to 2026.7 first (not to a later release)"
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

/// hars imports a release's directory exactly when Home Assistant 2026.6.1
/// itself loads it (`downgrade.json`: its
/// `bootstrap.async_load_base_functionality` on each directory), and names
/// the store 2026.6.1 names when it doesn't.
#[test]
fn imports_the_directories_home_assistant_2026_6_1_loads() {
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
            "2026.11.2"
        ]
    );
    for case in cases {
        let version = case["from"].as_str().unwrap();
        let src = temp(&format!("release-{version}"));
        let dest = temp(&format!("release-{version}-dest"));
        let fixture = release(version, &src, &[]);
        let out = hars(&dest, &["import", src.to_str().unwrap()]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.success(),
            case["loads"].as_bool().unwrap(),
            "{version}: {stderr}"
        );
        if !out.status.success() {
            // "Storage file core.device_registry was created by a newer
            // version of Home Assistant (storage version 3 > 1)".
            assert!(
                case["errors"][0]
                    .as_str()
                    .unwrap()
                    .contains("core.device_registry was created by a newer version"),
                "{case}"
            );
            let minor = if version == "2026.8.3" { 2 } else { 4 };
            assert!(
                stderr.contains(&format!(
                    "core.device_registry is storage version 3.{minor}, in a config directory \
                     of Home Assistant {version}; hars reads version 1.12, as Home Assistant \
                     2026.6.1 does. Home Assistant 2026.8 and later keep each device under one \
                     config entry (storage version 3), which hars can't read yet. hars imports \
                     Home Assistant 2026.6 and 2026.7: import a copy or backup of this config \
                     directory made before it was updated past 2026.7. Nothing was imported"
                )),
                "{stderr}"
            );
            nothing_imported(&dest);
            // Nor does a dry run or --force get past it.
            for flag in ["--dry-run", "--force"] {
                let out = hars(&dest, &["import", src.to_str().unwrap(), flag]);
                assert!(!out.status.success(), "{version} {flag}");
                nothing_imported(&dest);
            }
            continue;
        }

        let report = read_json(&dest.join("import-report.json"));
        assert_eq!(report["ha_version"], version);
        assert_eq!(report["ha_version_tested"], true, "{version}");
        assert_eq!(report["behaves_like"], "2026.6.1");
        // Every store but `http.auth` (the user media is served as) is one
        // hars reads: copied byte for byte.
        let mut imported = stores(&fixture);
        imported.retain(|k| k != "http.auth");
        assert_eq!(report["imported"], json!(imported), "{version}");
        assert_eq!(report["skipped"][0]["key"], "http.auth");
        assert_eq!(report["skipped"].as_array().unwrap().len(), 1, "{report}");
        for key in &imported {
            assert_eq!(
                std::fs::read_to_string(dest.join(".storage").join(key)).unwrap(),
                fixture["files"][format!(".storage/{key}")]
                    .as_str()
                    .unwrap(),
                "{version} {key}"
            );
        }
        // 2026.7 writes `mobile_app`'s store at 1.2.
        let newer = if version.starts_with("2026.7.") {
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
        assert_eq!(unsupported, ["radio_browser", "homekit_controller"]);
        assert_eq!(report["config_entries"].as_array().unwrap().len(), 9);

        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            stdout.contains(
                "hars behaves like Home Assistant 2026.6.1 and serves its frontend: \
                 what Home Assistant added after it is not there"
            ),
            version.starts_with("2026.7."),
            "{stdout}"
        );
        assert!(!stdout.contains("not tested"), "{stdout}");
        // An automation the 2026.7 editor can save and 2026.6 can't run.
        let automations = report["automations"].as_array().unwrap();
        if version.starts_with("2026.7.") {
            assert_eq!(automations.len(), 3);
            assert_eq!(automations[2]["alias"], "When the hall light turns on");
            assert_eq!(automations[2]["status"], "unavailable");
            assert!(
                stdout.contains("automation When the hall light turns on will be unavailable"),
                "{stdout}"
            );
        } else {
            assert_eq!(automations.len(), 2);
        }
        assert_eq!(automations[0]["status"], "ok");
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
            "hars is not tested with Home Assistant 2026.12.0 (it is with 2026.6 and 2026.7)"
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

/// What later releases changed, one at a time, each on a real file of
/// Home Assistant 2026.11.2: every one refuses the whole import.
#[test]
fn stores_and_entries_from_newer_releases_are_refused() {
    let device = ".storage/core.device_registry";
    let entity = ".storage/core.entity_registry";
    let entries = ".storage/core.config_entries";
    let cases: [(&[(&str, &str)], &str); 3] = [
        // The entity registry at 1.24 (2026.11 stores names another way):
        // a newer minor version of a store hars would never write back.
        (
            &[(device, "2026.7.4")],
            "core.entity_registry is storage version 1.24, in a config directory of Home \
             Assistant 2026.11.2; hars reads version 1.22, as Home Assistant 2026.6.1 does. \
             hars imports",
        ),
        // `met` at config entry version 2 (2026.11 nests its location).
        (
            &[(device, "2026.7.4"), (entity, "2026.7.4")],
            "the met integration's config entry \"Home\" is version 2, in a config directory \
             of Home Assistant 2026.11.2; hars runs version 1 of it, as Home Assistant 2026.6.1 \
             does. hars imports Home Assistant 2026.6 and 2026.7",
        ),
        // Everything from 2026.11.2 at once: the device registry is named.
        (&[], "core.device_registry is storage version 3.4"),
    ];
    for (swap, says) in cases {
        let src = temp("newer-src");
        let dest = temp("newer-dest");
        release("2026.11.2", &src, swap);
        let out = hars(&dest, &["import", src.to_str().unwrap(), "--force"]);
        assert!(!out.status.success(), "{says}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(says), "{stderr}");
        assert!(stderr.contains("Nothing was imported"), "{stderr}");
        nothing_imported(&dest);
        let _ = std::fs::remove_dir_all(src);
    }

    // The other way round: a 2026.7.4 directory whose config entries are
    // 2026.11.2's (`lifx` and `met` at version 2, `hue` at 1.2).
    let src = temp("newer-entries-src");
    let dest = temp("newer-entries-dest");
    release("2026.7.4", &src, &[(entries, "2026.11.2")]);
    let out = hars(&dest, &["import", src.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("the met integration's config entry"),
        "{out:?}"
    );
    nothing_imported(&dest);
    // An existing install is left exactly as it was.
    let dest = temp("newer-entries-dest2");
    assert!(hars(&dest, &["auth", "add", "bob", "pw"]).status.success());
    let before = std::fs::read(dest.join(".storage/auth")).unwrap();
    let out = hars(&dest, &["import", src.to_str().unwrap(), "--force"]);
    assert!(!out.status.success());
    assert_eq!(std::fs::read(dest.join(".storage/auth")).unwrap(), before);
    assert!(!dest.join(".import-staging").exists());
    for d in [src, dest] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// Directories hars could not keep safely are refused whole, each on a
/// 2026.7.4 directory with one thing changed.
#[test]
fn directories_hars_could_not_keep_are_refused() {
    type Change = fn(&Path);
    let cases: [(&str, Change, &str); 9] = [
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
             2026.6 or 2026.7 on this config directory once",
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
