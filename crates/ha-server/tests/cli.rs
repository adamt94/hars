//! `hars auth add`: the offline command that creates the first login.

use std::process::Command;

fn hars(config: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_hars"))
        .env("HARS_CONFIG", config)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn auth_add_creates_the_owner_in_has_files() {
    let dir = std::env::temp_dir().join(format!("hars-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let out = hars(&dir, &["auth", "add", "adam", "secret"]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("created owner"));

    let read = |key: &str| -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(dir.join(".storage").join(key)).unwrap()).unwrap()
    };
    let onboarding = read("onboarding");
    assert_eq!(onboarding["version"], 4);
    assert_eq!(
        onboarding["data"]["done"],
        serde_json::json!(["user", "core_config", "analytics", "integration"])
    );
    let auth = read("auth");
    let users = auth["data"]["users"].as_array().unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["is_owner"], true);
    assert_eq!(users[0]["group_ids"], serde_json::json!(["system-admin"]));
    let provider = read("auth_provider.homeassistant");
    assert_eq!(provider["data"]["users"][0]["username"], "adam");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join(".storage/auth"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // A second login is just a credential; duplicates and bad usage fail.
    let out = hars(&dir, &["auth", "add", "bob", "pw"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("added login"),
        "{out:?}"
    );
    assert!(!hars(&dir, &["auth", "add", "adam", "x"]).status.success());
    assert!(
        !hars(&dir, &["auth", "add", "Bad Name", "x"])
            .status
            .success()
    );
    assert_eq!(hars(&dir, &["nonsense"]).status.code(), Some(2));
    std::fs::remove_dir_all(dir).unwrap();
}
