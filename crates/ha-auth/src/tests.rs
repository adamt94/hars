use super::*;
use ha_core::SimulatedClock;
use std::path::PathBuf;

const CLIENT: &str = "http://192.168.1.5:8123/";
const REDIRECT: &str = "http://192.168.1.5:8123/?auth_callback=1";

fn lan() -> Option<IpAddr> {
    Some("192.168.1.20".parse().unwrap())
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hars-auth-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn start() -> Timestamp {
    "2026-11-14T08:00:00Z".parse().unwrap()
}

/// A fresh install with an owner `adam` / `secret`; bcrypt cost 4 keeps tests fast.
fn setup(name: &str) -> (Auth, SimulatedClock, PathBuf) {
    let dir = temp_dir(name);
    let sim = SimulatedClock::new(start());
    let auth = Auth::load_with_cost(&dir, sim.clock(), 4).unwrap();
    assert_eq!(auth.add_user("adam", "secret").unwrap(), Added::Owner);
    (auth, sim, dir)
}

fn login(auth: &Auth) -> Tokens {
    let flow = auth.start_login_flow(REDIRECT, lan());
    let user = auth.check_password("adam", "secret").unwrap();
    let code = auth.finish_login_flow(&flow, CLIENT, &user, lan()).unwrap();
    auth.exchange_code(Some(CLIENT), Some(&code), lan())
        .unwrap()
}

#[test]
fn fresh_install_owner_completes_onboarding() {
    let dir = temp_dir("fresh");
    let auth = Auth::load_with_cost(&dir, Clock::system(), 4).unwrap();
    assert!(!auth.is_onboarded() && !auth.is_user_onboarded());
    assert_eq!(
        auth.onboarding_steps(),
        ONBOARDING_STEPS.map(|s| (s, false)).to_vec()
    );
    assert_eq!(auth.add_user("adam", "secret").unwrap(), Added::Owner);
    assert!(auth.is_onboarded());
    assert_eq!(auth.add_user("bob", "pw").unwrap(), Added::Credential);
    assert!(
        matches!(auth.add_user("ADAM", "x"), Err(AddUserError::NotNormalized(n)) if n == "adam")
    );
    assert!(matches!(
        auth.add_user("adam", "x"),
        Err(AddUserError::Exists)
    ));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn passwords_are_checked_with_normalized_usernames() {
    let (auth, _, dir) = setup("passwords");
    assert_eq!(
        auth.check_password("adam", "secret").as_deref(),
        Some("adam")
    );
    assert_eq!(
        auth.check_password("  Adam ", "secret").as_deref(),
        Some("adam")
    );
    assert_eq!(auth.check_password("adam", "wrong"), None);
    assert_eq!(auth.check_password("nobody", "secret"), None);
    // Only the first 72 bytes count, as with HA's explicit truncation.
    auth.add_user("long", &"p".repeat(80)).unwrap();
    assert!(
        auth.check_password("long", &format!("{}x", "p".repeat(72)))
            .is_some()
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn full_login_issues_working_tokens() {
    let (auth, _, dir) = setup("login");
    let tokens = login(&auth);
    assert_eq!(tokens.expires_in, 1800);
    let refresh = tokens.refresh_token.clone().unwrap();
    assert_eq!(refresh.len(), 128);

    let p = auth
        .validate_access_token(&tokens.access_token, lan())
        .unwrap();
    assert_eq!(p.permission, Permission::Admin);
    assert_eq!(auth.user(&p.user_id).unwrap().name.as_deref(), Some("adam"));
    assert!(auth.validate_access_token("garbage", lan()).is_none());

    // Refresh: same client, no new refresh token.
    let again = auth.refresh(Some(CLIENT), Some(&refresh), lan()).unwrap();
    assert!(again.refresh_token.is_none());
    assert!(
        auth.validate_access_token(&again.access_token, lan())
            .is_some()
    );
    // Wrong or missing client id, unknown token.
    assert_eq!(
        auth.refresh(Some("http://other:8123/"), Some(&refresh), lan())
            .unwrap_err(),
        TokenError::InvalidRequest
    );
    assert_eq!(
        auth.refresh(None, Some(&refresh), lan()).unwrap_err(),
        TokenError::InvalidRequest
    );
    assert_eq!(
        auth.refresh(Some(CLIENT), Some("nope"), lan()).unwrap_err(),
        TokenError::InvalidGrant
    );
    assert_eq!(
        auth.refresh(Some(CLIENT), None, lan()).unwrap_err(),
        TokenError::InvalidRequest
    );
    assert_eq!(
        auth.refresh(Some("ftp://x/"), Some(&refresh), lan())
            .unwrap_err(),
        TokenError::InvalidClient
    );

    // Revoking the refresh token kills its access tokens at once.
    assert!(auth.revoke(&refresh));
    assert!(
        auth.validate_access_token(&again.access_token, lan())
            .is_none()
    );
    assert!(!auth.revoke(&refresh));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn codes_are_single_use_bound_to_the_client_and_short_lived() {
    let (auth, sim, dir) = setup("codes");
    let code = |auth: &Auth| {
        let flow = auth.start_login_flow(REDIRECT, lan());
        auth.finish_login_flow(&flow, CLIENT, "adam", lan())
            .unwrap()
    };

    let c = code(&auth);
    assert!(auth.exchange_code(Some(CLIENT), Some(&c), lan()).is_ok());
    assert_eq!(
        auth.exchange_code(Some(CLIENT), Some(&c), lan())
            .unwrap_err(),
        TokenError::InvalidCode
    );

    // Another client can't redeem it, and the attempt burns it.
    let c = code(&auth);
    assert_eq!(
        auth.exchange_code(Some("http://192.168.1.5:8124/"), Some(&c), lan())
            .unwrap_err(),
        TokenError::InvalidCode
    );
    assert_eq!(
        auth.exchange_code(Some(CLIENT), Some(&c), lan())
            .unwrap_err(),
        TokenError::InvalidCode
    );

    let c = code(&auth);
    sim.jump_to(start() + SignedDuration::from_secs(10 * 60 + 1));
    assert_eq!(
        auth.exchange_code(Some(CLIENT), Some(&c), lan())
            .unwrap_err(),
        TokenError::InvalidCode
    );

    assert_eq!(
        auth.exchange_code(Some("bad"), Some("x"), lan())
            .unwrap_err(),
        TokenError::InvalidClient
    );
    assert_eq!(
        auth.exchange_code(None, Some("x"), lan()).unwrap_err(),
        TokenError::InvalidClient
    );
    assert_eq!(
        auth.exchange_code(Some(CLIENT), None, lan()).unwrap_err(),
        TokenError::InvalidCode
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn login_flows_check_redirect_and_expire() {
    let (auth, sim, dir) = setup("flows");
    let flow = auth.start_login_flow("http://evil.example/", lan());
    assert_eq!(auth.login_flow_ip(&flow), Some(lan()));
    assert_eq!(
        auth.finish_login_flow(&flow, CLIENT, "adam", lan())
            .unwrap_err(),
        LoginError::InvalidRedirectUri
    );
    // The flow ended either way.
    assert_eq!(auth.login_flow_ip(&flow), None);

    let flow = auth.start_login_flow(REDIRECT, lan());
    assert!(auth.abort_login_flow(&flow));
    assert!(!auth.abort_login_flow(&flow));

    let flow = auth.start_login_flow(REDIRECT, lan());
    sim.jump_to(start() + SignedDuration::from_mins(11));
    assert_eq!(auth.login_flow_ip(&flow), None);

    // Pending flows are capped.
    for _ in 0..200 {
        auth.start_login_flow(REDIRECT, lan());
    }
    assert!(auth.0.state.lock().flows.len() <= MAX_PENDING);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn access_tokens_expire_and_refresh_tokens_slide() {
    let (auth, sim, dir) = setup("expiry");
    let tokens = login(&auth);
    let refresh = tokens.refresh_token.unwrap();

    sim.jump_to(start() + SignedDuration::from_secs(1800 + 10));
    assert!(
        auth.validate_access_token(&tokens.access_token, lan())
            .is_some()
    );
    sim.jump_to(start() + SignedDuration::from_secs(1800 + 11));
    assert!(
        auth.validate_access_token(&tokens.access_token, lan())
            .is_none()
    );

    // Used at day 80, the refresh token lives until day 170.
    sim.jump_to(start() + SignedDuration::from_hours(80 * 24));
    auth.refresh(Some(CLIENT), Some(&refresh), lan()).unwrap();
    sim.jump_to(start() + SignedDuration::from_hours(169 * 24));
    let late = auth.refresh(Some(CLIENT), Some(&refresh), lan()).unwrap();
    // Unused for 90 days: gone, and its access tokens with it.
    sim.jump_to(start() + SignedDuration::from_hours(260 * 24));
    assert_eq!(
        auth.refresh(Some(CLIENT), Some(&refresh), lan())
            .unwrap_err(),
        TokenError::InvalidGrant
    );
    assert!(
        auth.validate_access_token(&late.access_token, lan())
            .is_none()
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn inactive_and_local_only_users_are_blocked() {
    let (auth, _, dir) = setup("blocked");
    let tokens = login(&auth);
    let set = |f: &dyn Fn(&mut User)| {
        let mut s = auth.0.state.lock();
        let u = s
            .data
            .users
            .iter_mut()
            .find(|u| u.name.as_deref() == Some("adam"))
            .unwrap();
        f(u);
    };
    let internet = Some("8.8.8.8".parse().unwrap());

    set(&|u| u.local_only = true);
    assert!(
        auth.validate_access_token(&tokens.access_token, lan())
            .is_some()
    );
    assert!(
        auth.validate_access_token(&tokens.access_token, internet)
            .is_none()
    );
    let flow = auth.start_login_flow(REDIRECT, internet);
    assert_eq!(
        auth.finish_login_flow(&flow, CLIENT, "adam", internet)
            .unwrap_err(),
        LoginError::Blocked("User cannot authenticate remotely")
    );

    set(&|u| {
        u.local_only = false;
        u.is_active = false;
    });
    assert!(
        auth.validate_access_token(&tokens.access_token, lan())
            .is_none()
    );
    assert_eq!(
        auth.refresh(Some(CLIENT), tokens.refresh_token.as_deref(), lan())
            .unwrap_err(),
        TokenError::AccessDenied("User is not active")
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn state_survives_a_restart() {
    let (auth, sim, dir) = setup("restart");
    let tokens = login(&auth);
    assert!(auth.needs_save());
    auth.save().unwrap();
    assert!(!auth.needs_save());

    let again = Auth::load(&dir, sim.clock()).unwrap();
    assert!(again.is_onboarded());
    // Logged-in clients stay logged in.
    assert!(
        again
            .validate_access_token(&tokens.access_token, lan())
            .is_some()
    );
    assert!(again.check_password("adam", "secret").is_some());
    let _ = std::fs::remove_dir_all(dir);
}

/// A `.storage` directory written by HA 2026.6.1 loads, and saving it again
/// keeps what `hars` doesn't model.
#[test]
fn reads_and_preserves_has_files() {
    let dir = temp_dir("ha-files");
    std::fs::create_dir_all(dir.join(".storage")).unwrap();
    let hash = bcrypt::hash_with_result("secret", 4)
        .unwrap()
        .format_for_version(bcrypt::Version::TwoB);
    let write = |key: &str, v: Value| {
        std::fs::write(dir.join(".storage").join(key), v.to_string()).unwrap()
    };
    write(
        "auth_provider.homeassistant",
        json!({"version": 1, "minor_version": 1, "key": "auth_provider.homeassistant",
        "data": {"users": [{"username": "adam", "password": B64.encode(&hash)}]}}),
    );
    write(
        "onboarding",
        json!({"version": 4, "minor_version": 1, "key": "onboarding",
        "data": {"done": ["user", "core_config", "analytics", "integration"]}}),
    );
    write(
        "auth",
        json!({"version": 1, "minor_version": 1, "key": "auth", "data": {
        "users": [
            {"id": "u1", "group_ids": ["system-admin"], "is_owner": true, "is_active": true, "name": "Adam", "system_generated": false, "local_only": false},
            {"id": "u2", "group_ids": ["system-read-only"], "is_owner": false, "is_active": true, "name": "Home Assistant Content", "system_generated": true, "local_only": false}
        ],
        "groups": [
            {"id": "system-admin", "name": "Renamed"},
            {"id": "custom", "name": "Custom", "policy": {"entities": true}}
        ],
        "credentials": [{"id": "c1", "user_id": "u1", "auth_provider_type": "homeassistant", "auth_provider_id": null, "data": {"username": "adam"}}],
        "refresh_tokens": [
            {"id": "t1", "user_id": "u1", "client_id": CLIENT, "client_name": null, "client_icon": null, "token_type": "normal",
             "created_at": "2026-06-10T12:34:56.789012+00:00", "access_token_expiration": 1800.0, "token": "a".repeat(128),
             "jwt_key": "b".repeat(128), "last_used_at": null, "last_used_ip": null, "expire_at": 4102444800.0,
             "credential_id": "c1", "version": "2026.6.1", "future_field": 1},
            {"id": "t2", "user_id": "u2", "client_id": null, "created_at": "2026-06-10T12:34:56.789012+00:00",
             "access_token_expiration": 1800.0, "token": "c".repeat(128), "jwt_key": "d".repeat(128)},
            {"id": "old", "user_id": "u1", "client_id": CLIENT, "created_at": "2018-01-01T00:00:00+00:00",
             "access_token_expiration": 1800.0, "token": "e".repeat(128)}
        ]}}),
    );

    let auth = Auth::load_with_cost(&dir, Clock::system(), 4).unwrap();
    assert!(auth.is_onboarded());
    let r = auth
        .refresh(Some(CLIENT), Some(&"a".repeat(128)), lan())
        .unwrap();
    let p = auth.validate_access_token(&r.access_token, lan()).unwrap();
    assert_eq!(
        (
            p.user_id.as_str(),
            p.refresh_token_id.as_str(),
            p.permission
        ),
        ("u1", "t1", Permission::Admin)
    );
    // An existing user logs in through their linked credential.
    let flow = auth.start_login_flow(REDIRECT, lan());
    let code = auth
        .finish_login_flow(&flow, CLIENT, "adam", lan())
        .unwrap();
    let t = auth
        .exchange_code(Some(CLIENT), Some(&code), lan())
        .unwrap();
    assert_eq!(
        auth.validate_access_token(&t.access_token, lan())
            .unwrap()
            .user_id,
        "u1"
    );
    auth.save().unwrap();

    let saved: Value =
        serde_json::from_slice(&std::fs::read(dir.join(".storage/auth")).unwrap()).unwrap();
    let data = &saved["data"];
    let tokens = data["refresh_tokens"].as_array().unwrap();
    let t1 = tokens.iter().find(|t| t["id"] == "t1").unwrap();
    assert_eq!(t1["future_field"], 1);
    assert_eq!(t1["last_used_ip"], "192.168.1.20");
    // Inferred, as HA does; the token without a jwt_key is dropped.
    assert_eq!(
        tokens.iter().find(|t| t["id"] == "t2").unwrap()["token_type"],
        "system"
    );
    assert!(tokens.iter().all(|t| t["id"] != "old"));
    let groups = data["groups"].as_array().unwrap();
    assert_eq!(
        groups.iter().find(|g| g["id"] == "system-admin").unwrap()["name"],
        "Administrators"
    );
    assert_eq!(
        groups.iter().find(|g| g["id"] == "custom").unwrap()["policy"],
        json!({"entities": true})
    );
    assert!(groups.iter().any(|g| g["id"] == "system-read-only"));
    assert_eq!(data["users"].as_array().unwrap().len(), 2);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn owner_without_onboarding_file_has_only_the_user_step() {
    let dir = temp_dir("onboarding-migrate");
    std::fs::create_dir_all(dir.join(".storage")).unwrap();
    std::fs::write(
        dir.join(".storage/auth"),
        json!({"version": 1, "key": "auth", "data": {"users": [
            {"id": "u1", "group_ids": ["system-admin"], "is_owner": true, "is_active": true, "name": "A", "system_generated": false}
        ]}}).to_string(),
    )
    .unwrap();
    let auth = Auth::load(&dir, Clock::system()).unwrap();
    assert!(auth.is_user_onboarded() && !auth.is_onboarded());
    assert!(auth.needs_save());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn ids_and_times_have_has_formats() {
    let id = uuid_hex();
    assert_eq!(id.len(), 32);
    assert!(
        id.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    assert_eq!(&id[12..13], "4");
    assert_eq!(token_hex().len(), 128);
    let t: Timestamp = "2026-06-10T12:34:56.789012Z".parse().unwrap();
    assert_eq!(iso(t), "2026-06-10T12:34:56.789012+00:00");
}

#[test]
fn usernames_are_case_folded_like_python() {
    let (auth, _, dir) = setup("casefold");
    auth.add_user("strasse", "pw").unwrap();
    // HA: "straße".casefold() == "strasse".
    assert_eq!(
        auth.check_password("Straße", "pw").as_deref(),
        Some("strasse")
    );
    assert!(
        matches!(auth.add_user("straße", "x"), Err(AddUserError::NotNormalized(n)) if n == "strasse")
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn permissions_follow_the_built_in_groups() {
    let user = |groups: &[&str], owner: bool| User {
        id: "u".into(),
        group_ids: groups.iter().map(|g| g.to_string()).collect(),
        is_owner: owner,
        is_active: true,
        name: None,
        system_generated: false,
        local_only: false,
        extra: Map::new(),
    };
    assert_eq!(user(&[], true).permission(), Permission::Admin);
    assert_eq!(
        user(&[GROUP_ID_ADMIN], false).permission(),
        Permission::Admin
    );
    assert_eq!(
        user(&[GROUP_ID_USER], false).permission(),
        Permission::Control
    );
    assert_eq!(
        user(&[GROUP_ID_READ_ONLY], false).permission(),
        Permission::Read
    );
    assert_eq!(
        user(&[GROUP_ID_READ_ONLY, GROUP_ID_USER], false).permission(),
        Permission::Control
    );
    assert_eq!(user(&["custom"], false).permission(), Permission::None);
    assert_eq!(user(&[], false).permission(), Permission::None);
}
