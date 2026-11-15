//! Users, credentials and tokens, compatible with Home Assistant's
//! `homeassistant/auth` package and its `.storage` files.
//!
//! One auth provider is supported: HA's default `homeassistant` username and
//! password provider. [`Auth`] owns everything in memory; [`Auth::save`]
//! writes what changed. The API is synchronous: password hashing and file
//! writes block, so call those from `spawn_blocking`.

pub mod indieauth;
mod jwt;
mod models;

pub use models::{
    Credential, GROUP_ID_ADMIN, GROUP_ID_READ_ONLY, GROUP_ID_USER, RefreshToken, User,
};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ha_core::{Clock, storage::Store};
use jiff::{SignedDuration, Timestamp};
use models::{AuthData, ProviderData, ProviderUser, TOKEN_TYPE_NORMAL};
use parking_lot::Mutex;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

/// The HA release whose API `hars` speaks; stamped on new refresh tokens.
pub const HA_VERSION: &str = "2026.6.1";
/// `ACCESS_TOKEN_EXPIRATION` for normal refresh tokens.
pub const ACCESS_TOKEN_EXPIRATION_SECS: i64 = 30 * 60;
/// `REFRESH_TOKEN_EXPIRATION`: re-slid each time an access token is issued.
const REFRESH_TOKEN_EXPIRATION_SECS: f64 = 90.0 * 86400.0;
/// Authorization codes are valid for less than this, and used once.
const AUTH_CODE_TTL: SignedDuration = SignedDuration::from_mins(10);
/// HA keeps login flows until they finish; `hars` caps and expires them.
const LOGIN_FLOW_TTL: SignedDuration = SignedDuration::from_mins(10);
const MAX_PENDING: usize = 64;

pub const PROVIDER_TYPE: &str = "homeassistant";
pub const PROVIDER_NAME: &str = "Home Assistant Local";
const BCRYPT_COST: u32 = 12;
/// HA checks this when the username is unknown, so both cases take as long.
const DUMMY_HASH: &str = "$2b$12$CiuFGszHx9eNHxPuQcwBWez4CwDTOcLTX5CbOpV6gef2nYuXkY7BO";

/// `components/onboarding/const.py`.
pub const ONBOARDING_STEPS: [&str; 4] = ["user", "core_config", "analytics", "integration"];

/// The authenticated caller of a request.
#[derive(Debug, Clone)]
pub struct Principal {
    pub user_id: String,
    pub refresh_token_id: String,
    pub permission: Permission,
}

/// What a user may do, from HA's built-in group policies
/// (`auth/permissions/const.py`). Custom group policies are not evaluated
/// yet, so a user with only custom groups gets [`Permission::None`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Permission {
    None,
    /// `system-read-only`: read every entity, control none.
    Read,
    /// `system-users`: read and control every entity; no admin-only views.
    Control,
    /// Owner or `system-admin`.
    Admin,
}

impl User {
    /// The strongest built-in policy among the user's groups.
    pub fn permission(&self) -> Permission {
        if self.is_admin() {
            return Permission::Admin;
        }
        self.group_ids
            .iter()
            .map(|g| match g.as_str() {
                GROUP_ID_USER => Permission::Control,
                GROUP_ID_READ_ONLY => Permission::Read,
                _ => Permission::None,
            })
            .max()
            .unwrap_or(Permission::None)
    }
}

/// What `/auth/token` returns on success.
#[derive(Debug)]
pub struct Tokens {
    pub access_token: String,
    /// Only for `authorization_code`: refresh tokens are not rotated.
    pub refresh_token: Option<String>,
    pub expires_in: i64,
}

/// `/auth/token` failures, mapped to HA's OAuth error bodies by the API.
#[derive(Debug, PartialEq, Eq)]
pub enum TokenError {
    InvalidClient,
    InvalidCode,
    /// `{"error":"invalid_request"}` with no description.
    InvalidRequest,
    InvalidGrant,
    AccessDenied(&'static str),
}

/// Why a finished login flow didn't produce a code.
#[derive(Debug, PartialEq, Eq)]
pub enum LoginError {
    UnknownFlow,
    InvalidRedirectUri,
    Blocked(&'static str),
}

#[derive(Debug, thiserror::Error)]
pub enum AddUserError {
    #[error("username already exists")]
    Exists,
    #[error("username must be lowercase with no surrounding spaces, e.g. {0:?}")]
    NotNormalized(String),
    #[error("password hashing failed: {0}")]
    Hash(#[from] bcrypt::BcryptError),
}

/// What [`Auth::create_owner`] made: the user, and the username its
/// credential logs in with (for [`Auth::create_auth_code`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Onboarded {
    pub user_id: String,
    pub username: String,
}

/// What [`Auth::add_user`] created.
#[derive(Debug, PartialEq, Eq)]
pub enum Added {
    /// The first user: owner and admin, with onboarding marked complete.
    Owner,
    /// A login for the provider; HA creates its admin user at first login.
    Credential,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct OnboardingData {
    #[serde(default)]
    done: Vec<String>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

struct LoginFlow {
    redirect_uri: String,
    ip: Option<IpAddr>,
    created: Timestamp,
}

struct AuthCode {
    client_id: String,
    username: String,
    created: Timestamp,
}

const DIRTY_AUTH: u8 = 1;
const DIRTY_PROVIDER: u8 = 2;
const DIRTY_ONBOARDING: u8 = 4;

struct State {
    data: AuthData,
    provider: ProviderData,
    /// HA compares usernames verbatim when any stored one is not normalized.
    legacy_usernames: bool,
    onboarding: OnboardingData,
    flows: HashMap<String, LoginFlow>,
    codes: HashMap<String, AuthCode>,
    dirty: u8,
}

struct Inner {
    clock: Clock,
    auth_store: Store,
    provider_store: Store,
    onboarding_store: Store,
    state: Mutex<State>,
    /// Serializes [`Auth::save`] so an older snapshot never lands last.
    save_lock: Mutex<()>,
    bcrypt_cost: u32,
    /// Ids of revoked refresh tokens, so open WebSockets using them close.
    revoked: tokio::sync::broadcast::Sender<String>,
}

/// Shared handle; cheap to clone.
#[derive(Clone)]
pub struct Auth(Arc<Inner>);

impl Auth {
    /// Load `<config_dir>/.storage/{auth, auth_provider.homeassistant,
    /// onboarding}`. Missing files mean a fresh install.
    pub fn load(config_dir: &Path, clock: Clock) -> Result<Self, ha_core::storage::StoreError> {
        Self::load_with_cost(config_dir, clock, BCRYPT_COST)
    }

    /// [`Auth::load`] with a different bcrypt cost for new passwords. HA uses
    /// 12; tests use a low cost to stay fast.
    pub fn load_with_cost(
        config_dir: &Path,
        clock: Clock,
        bcrypt_cost: u32,
    ) -> Result<Self, ha_core::storage::StoreError> {
        let auth_store = Store::new(config_dir, "auth", 1, 1, true);
        let provider_store = Store::new(config_dir, "auth_provider.homeassistant", 1, 1, true);
        let onboarding_store = Store::new(config_dir, "onboarding", 4, 1, true);

        let mut data: AuthData = auth_store.load()?.unwrap_or_default();
        data.normalize();
        let provider: ProviderData = provider_store.load()?.unwrap_or_default();
        let legacy_usernames = provider
            .users
            .iter()
            .any(|u| normalize_username(&u.username) != u.username);
        if legacy_usernames {
            tracing::warn!(
                "a stored username is not normalized; usernames are compared verbatim, as HA does"
            );
        }
        let mut dirty = 0;
        let onboarding = match onboarding_store.load::<OnboardingData>()? {
            Some(o) => o,
            // HA: no onboarding file but an owner exists means the user step is done.
            None if data.users.iter().any(|u| u.is_owner) => {
                dirty |= DIRTY_ONBOARDING;
                OnboardingData {
                    done: vec!["user".into()],
                    ..Default::default()
                }
            }
            None => OnboardingData::default(),
        };
        Ok(Self(Arc::new(Inner {
            clock,
            auth_store,
            provider_store,
            onboarding_store,
            state: Mutex::new(State {
                data,
                provider,
                legacy_usernames,
                onboarding,
                flows: HashMap::new(),
                codes: HashMap::new(),
                dirty,
            }),
            save_lock: Mutex::new(()),
            bcrypt_cost,
            revoked: tokio::sync::broadcast::channel(16).0,
        })))
    }

    /// Whether anything changed since the last [`Auth::save`].
    pub fn needs_save(&self) -> bool {
        self.0.state.lock().dirty != 0
    }

    /// Write the stores that changed. Expired refresh tokens are dropped
    /// here, where HA uses an hourly timer: no wakeups while idle.
    pub fn save(&self) -> std::io::Result<()> {
        let _order = self.0.save_lock.lock();
        let now = self.now_f64();
        let (dirty, auth, provider, onboarding) = {
            let mut s = self.0.state.lock();
            let before = s.data.refresh_tokens.len();
            s.data.refresh_tokens.retain(|t| !expired(t, now));
            if s.data.refresh_tokens.len() != before {
                s.dirty |= DIRTY_AUTH;
            }
            let dirty = std::mem::take(&mut s.dirty);
            (
                dirty,
                (dirty & DIRTY_AUTH != 0).then(|| serde_json::to_value(&s.data)),
                (dirty & DIRTY_PROVIDER != 0).then(|| serde_json::to_value(&s.provider)),
                (dirty & DIRTY_ONBOARDING != 0).then(|| serde_json::to_value(&s.onboarding)),
            )
        };
        // The passwords before the users: if the second write fails, disk
        // never has a user (an owner, after onboarding) without its
        // password, only a login no user has yet.
        let result = (|| {
            if let Some(v) = provider {
                self.0.provider_store.save(&v?)?;
            }
            if let Some(v) = auth {
                self.0.auth_store.save(&v?)?;
            }
            if let Some(v) = onboarding {
                self.0.onboarding_store.save(&v?)?;
            }
            Ok(())
        })();
        if result.is_err() {
            self.0.state.lock().dirty |= dirty; // retry next time
        }
        result
    }

    // ----- onboarding -----

    /// Every onboarding step is done.
    pub fn is_onboarded(&self) -> bool {
        let s = self.0.state.lock();
        ONBOARDING_STEPS
            .iter()
            .all(|step| s.onboarding.done.iter().any(|d| d == step))
    }

    /// The `user` step is done (an owner exists).
    pub fn is_user_onboarded(&self) -> bool {
        self.0
            .state
            .lock()
            .onboarding
            .done
            .iter()
            .any(|d| d == "user")
    }

    /// `GET /api/onboarding`: each step and whether it's done.
    pub fn onboarding_steps(&self) -> Vec<(&'static str, bool)> {
        let s = self.0.state.lock();
        ONBOARDING_STEPS
            .iter()
            .map(|step| (*step, s.onboarding.done.iter().any(|d| d == step)))
            .collect()
    }

    /// Whether `step` is done.
    pub fn is_step_done(&self, step: &str) -> bool {
        self.0
            .state
            .lock()
            .onboarding
            .done
            .iter()
            .any(|d| d == step)
    }

    /// `_async_mark_done`: append `step` to the store's `done` list (in the
    /// order steps finish, as HA keeps it). [`Auth::save`] writes it.
    pub fn mark_step_done(&self, step: &str) {
        let mut s = self.0.state.lock();
        s.onboarding.done.push(step.to_owned());
        s.dirty |= DIRTY_ONBOARDING;
    }

    /// `UserOnboardingView`'s accounts (blocking: bcrypt): a user named
    /// `name` in `system-admin`, the owner as the first user that isn't
    /// system-generated (`_user_should_be_owner`), with a `homeassistant`
    /// login for `username` linked to it. HA checks the username in
    /// `async_add_auth` after creating the user, and its error becomes a
    /// 500 with the user left behind; `hars` checks first and creates
    /// nothing.
    pub fn create_owner(
        &self,
        name: &str,
        username: &str,
        password: &str,
    ) -> Result<Onboarded, AddUserError> {
        // `_validate_new_username`: normalized whatever the legacy mode.
        let normalized = normalize_username(username);
        if normalized != username {
            return Err(AddUserError::NotNormalized(normalized));
        }
        if self.username_taken(username) {
            return Err(AddUserError::Exists);
        }
        let hash = bcrypt::hash_with_result(truncate72(password), self.0.bcrypt_cost)?
            .format_for_version(bcrypt::Version::TwoB);
        let mut s = self.0.state.lock();
        // Checked again: another login may have been added while hashing.
        if taken(&s, username) {
            return Err(AddUserError::Exists);
        }
        // A login no user has, left by an owner whose save failed half way
        // (the passwords were written, the users weren't): replaced.
        let legacy = s.legacy_usernames;
        s.provider.users.retain(|u| {
            normalize_username(&u.username) != normalized && !(legacy && u.username == username)
        });
        s.provider.users.push(ProviderUser {
            username: username.into(),
            password: B64.encode(hash),
            extra: Map::new(),
        });
        let is_owner = s.data.users.iter().all(|u| u.system_generated);
        let user_id = uuid_hex();
        s.data.users.push(User {
            id: user_id.clone(),
            group_ids: vec![GROUP_ID_ADMIN.into()],
            is_owner,
            is_active: true,
            name: Some(name.into()),
            system_generated: false,
            local_only: false,
            extra: Map::new(),
        });
        // `async_get_or_create_credentials` then `async_link_user`: a stale
        // credential for the same username is replaced.
        s.data
            .credentials
            .retain(|c| !(c.auth_provider_type == PROVIDER_TYPE && c.username() == Some(username)));
        s.data.credentials.push(Credential {
            id: uuid_hex(),
            user_id: user_id.clone(),
            auth_provider_type: PROVIDER_TYPE.into(),
            auth_provider_id: None,
            data: json!({ "username": username }),
            extra: Map::new(),
        });
        s.dirty |= DIRTY_AUTH | DIRTY_PROVIDER;
        Ok(Onboarded {
            user_id,
            username: username.into(),
        })
    }

    fn username_taken(&self, username: &str) -> bool {
        taken(&self.0.state.lock(), username)
    }

    /// Take back a [`Auth::create_owner`] whose save failed: the user, its
    /// credential and its login go, so the user step can run again and
    /// make the owner afresh. [`Auth::save`] writes the result.
    pub fn remove_owner(&self, owner: &Onboarded) {
        let mut s = self.0.state.lock();
        s.data.users.retain(|u| u.id != owner.user_id);
        s.data.credentials.retain(|c| c.user_id != owner.user_id);
        s.provider.users.retain(|u| u.username != owner.username);
        s.dirty |= DIRTY_AUTH | DIRTY_PROVIDER;
    }

    /// The `homeassistant` username of the credential a refresh token was
    /// issued for (`refresh_token.credential`), if it has one.
    pub fn refresh_token_username(&self, refresh_token_id: &str) -> Option<String> {
        let s = self.0.state.lock();
        let token = s
            .data
            .refresh_tokens
            .iter()
            .find(|t| t.id == refresh_token_id)?;
        let credential_id = token.credential_id.as_deref()?;
        s.data
            .credentials
            .iter()
            .find(|c| c.id == credential_id && c.auth_provider_type == PROVIDER_TYPE)
            .and_then(|c| c.username())
            .map(str::to_owned)
    }

    // ----- the homeassistant provider -----

    /// `hars auth add`: add a username and password to the provider
    /// (blocking: bcrypt). The first one also becomes the owner and
    /// completes every onboarding step, so an install can be set up without
    /// a browser (the browser's onboarding is [`Auth::create_owner`] and
    /// the onboarding views).
    pub fn add_user(&self, username: &str, password: &str) -> Result<Added, AddUserError> {
        let normalized = normalize_username(username);
        if normalized != username {
            return Err(AddUserError::NotNormalized(normalized));
        }
        let hash = bcrypt::hash_with_result(truncate72(password), self.0.bcrypt_cost)?
            .format_for_version(bcrypt::Version::TwoB);

        let mut s = self.0.state.lock();
        let legacy = s.legacy_usernames;
        if s.provider.users.iter().any(|u| {
            if legacy {
                u.username == username
            } else {
                normalize_username(&u.username) == normalized
            }
        }) {
            return Err(AddUserError::Exists);
        }
        s.provider.users.push(ProviderUser {
            username: username.into(),
            password: B64.encode(hash),
            extra: Map::new(),
        });
        s.dirty |= DIRTY_PROVIDER;

        if s.data.users.iter().any(|u| u.is_owner) {
            return Ok(Added::Credential);
        }
        // `onboarding/views.py` UserOnboardingView, minus the person and areas.
        let user_id = uuid_hex();
        s.data.users.push(User {
            id: user_id.clone(),
            group_ids: vec![GROUP_ID_ADMIN.into()],
            is_owner: true,
            is_active: true,
            name: Some(username.into()),
            system_generated: false,
            local_only: false,
            extra: Map::new(),
        });
        s.data.credentials.push(Credential {
            id: uuid_hex(),
            user_id,
            auth_provider_type: PROVIDER_TYPE.into(),
            auth_provider_id: None,
            data: json!({ "username": username }),
            extra: Map::new(),
        });
        for step in ONBOARDING_STEPS {
            if !s.onboarding.done.iter().any(|d| d == step) {
                s.onboarding.done.push(step.into());
            }
        }
        s.dirty |= DIRTY_AUTH | DIRTY_ONBOARDING;
        Ok(Added::Owner)
    }

    /// Check a username and password (blocking: bcrypt). Returns the stored
    /// username on success.
    pub fn check_password(&self, username: &str, password: &str) -> Option<String> {
        let stored = {
            let s = self.0.state.lock();
            let wanted = normalize_username(username);
            let found = s.provider.users.iter().find(|u| {
                if s.legacy_usernames {
                    u.username == username
                } else {
                    normalize_username(&u.username) == wanted
                }
            });
            found.map(|u| (u.username.clone(), u.password.clone()))
        };
        let password = truncate72(password);
        match stored {
            Some((name, hash)) => {
                let hash = B64
                    .decode(hash)
                    .ok()
                    .and_then(|h| String::from_utf8(h).ok())?;
                bcrypt::verify(password, &hash)
                    .unwrap_or(false)
                    .then_some(name)
            }
            None => {
                let _ = bcrypt::verify(password, DUMMY_HASH);
                None
            }
        }
    }

    // ----- login flows and authorization codes -----

    /// `POST /auth/login_flow`: returns the new flow id.
    pub fn start_login_flow(&self, redirect_uri: &str, ip: Option<IpAddr>) -> String {
        let now = self.0.clock.now();
        let id = uuid_hex();
        let mut s = self.0.state.lock();
        s.flows
            .retain(|_, f| now.duration_since(f.created) < LOGIN_FLOW_TTL);
        if s.flows.len() >= MAX_PENDING {
            let oldest = s
                .flows
                .iter()
                .min_by_key(|(_, f)| f.created)
                .map(|(k, _)| k.clone());
            s.flows.remove(&oldest.expect("not empty"));
        }
        s.flows.insert(
            id.clone(),
            LoginFlow {
                redirect_uri: redirect_uri.into(),
                ip,
                created: now,
            },
        );
        id
    }

    /// The IP a pending flow was started from; `None` if there is no such flow.
    pub fn login_flow_ip(&self, flow_id: &str) -> Option<Option<IpAddr>> {
        let now = self.0.clock.now();
        let s = self.0.state.lock();
        s.flows
            .get(flow_id)
            .filter(|f| now.duration_since(f.created) < LOGIN_FLOW_TTL)
            .map(|f| f.ip)
    }

    pub fn abort_login_flow(&self, flow_id: &str) -> bool {
        self.0.state.lock().flows.remove(flow_id).is_some()
    }

    /// The flow's credentials checked out: end it and issue an authorization
    /// code for `client_id`, as `LoginFlowBaseView` does.
    pub fn finish_login_flow(
        &self,
        flow_id: &str,
        client_id: &str,
        username: &str,
        ip: Option<IpAddr>,
    ) -> Result<String, LoginError> {
        let now = self.0.clock.now();
        let mut s = self.0.state.lock();
        let flow = s.flows.remove(flow_id).ok_or(LoginError::UnknownFlow)?;
        if !indieauth::verify_redirect_uri(client_id, &flow.redirect_uri) {
            return Err(LoginError::InvalidRedirectUri);
        }
        if let Some(reason) =
            user_for_username(&s.data, username).and_then(|u| user_not_allowed(u, ip))
        {
            return Err(LoginError::Blocked(reason));
        }
        Ok(store_code(&mut s, client_id, username, now))
    }

    /// `create_auth_code`: a single-use code `/auth/token` exchanges for
    /// tokens of the user `username` logs in as (onboarding's user and
    /// integration steps).
    pub fn create_auth_code(&self, client_id: &str, username: &str) -> String {
        let now = self.0.clock.now();
        store_code(&mut self.0.state.lock(), client_id, username, now)
    }

    // ----- tokens -----

    /// `grant_type=authorization_code`.
    pub fn exchange_code(
        &self,
        client_id: Option<&str>,
        code: Option<&str>,
        ip: Option<IpAddr>,
    ) -> Result<Tokens, TokenError> {
        let client_id = client_id
            .filter(|c| indieauth::verify_client_id(c))
            .ok_or(TokenError::InvalidClient)?;
        let now = self.0.clock.now();
        let mut s = self.0.state.lock();
        // Single use: taken out even if it turns out to be invalid.
        let code = code
            .and_then(|c| s.codes.remove(c))
            .filter(|c| c.client_id == client_id && now.duration_since(c.created) < AUTH_CODE_TTL)
            .ok_or(TokenError::InvalidCode)?;

        let (user_id, credential_id) = get_or_create_user(&mut s.data, &code.username);
        let user = s
            .data
            .users
            .iter()
            .find(|u| u.id == user_id)
            .expect("just found or made");
        if let Some(reason) = user_not_allowed(user, ip) {
            s.dirty |= DIRTY_AUTH;
            return Err(TokenError::AccessDenied(reason));
        }
        let now_f = now.as_microsecond() as f64 / 1e6;
        let token = RefreshToken {
            id: uuid_hex(),
            user_id,
            client_id: Some(client_id.into()),
            client_name: None,
            client_icon: None,
            token_type: Some(TOKEN_TYPE_NORMAL.into()),
            created_at: iso(now),
            access_token_expiration: ACCESS_TOKEN_EXPIRATION_SECS as f64,
            token: token_hex(),
            jwt_key: Some(token_hex()),
            last_used_at: None,
            last_used_ip: None,
            expire_at: Some(now_f + REFRESH_TOKEN_EXPIRATION_SECS),
            credential_id: Some(credential_id),
            version: Some(HA_VERSION.into()),
            extra: Map::new(),
        };
        let refresh = token.token.clone();
        s.data.refresh_tokens.push(token);
        let idx = s.data.refresh_tokens.len() - 1;
        let (access_token, expires_in) = issue_access_token(&mut s, idx, ip, now);
        Ok(Tokens {
            access_token,
            refresh_token: Some(refresh),
            expires_in,
        })
    }

    /// `grant_type=refresh_token`. No new refresh token is issued.
    pub fn refresh(
        &self,
        client_id: Option<&str>,
        refresh_token: Option<&str>,
        ip: Option<IpAddr>,
    ) -> Result<Tokens, TokenError> {
        if client_id.is_some_and(|c| !indieauth::verify_client_id(c)) {
            return Err(TokenError::InvalidClient);
        }
        let refresh_token = refresh_token.ok_or(TokenError::InvalidRequest)?;
        let now = self.0.clock.now();
        let now_f = now.as_microsecond() as f64 / 1e6;
        let mut s = self.0.state.lock();
        let idx = find_by_token(&s.data.refresh_tokens, refresh_token)
            .filter(|&i| !expired(&s.data.refresh_tokens[i], now_f))
            .ok_or(TokenError::InvalidGrant)?;
        let stored = &s.data.refresh_tokens[idx];
        if stored.client_id.as_deref() != client_id {
            return Err(TokenError::InvalidRequest);
        }
        let user = s
            .data
            .users
            .iter()
            .find(|u| u.id == stored.user_id)
            .ok_or(TokenError::InvalidGrant)?;
        if let Some(reason) = user_not_allowed(user, ip) {
            return Err(TokenError::AccessDenied(reason));
        }
        let (access_token, expires_in) = issue_access_token(&mut s, idx, ip, now);
        Ok(Tokens {
            access_token,
            refresh_token: None,
            expires_in,
        })
    }

    /// `POST /auth/revoke`: drop a refresh token and with it every access
    /// token it issued. Unknown tokens are ignored.
    pub fn revoke(&self, refresh_token: &str) -> bool {
        let mut s = self.0.state.lock();
        match find_by_token(&s.data.refresh_tokens, refresh_token) {
            Some(i) => {
                let token = s.data.refresh_tokens.remove(i);
                s.dirty |= DIRTY_AUTH;
                let _ = self.0.revoked.send(token.id);
                true
            }
            None => false,
        }
    }

    /// Refresh token ids as they are revoked. A receiver that lags has
    /// missed some; it should re-check its token with [`Auth::is_token_live`].
    pub fn subscribe_revocations(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.0.revoked.subscribe()
    }

    /// Whether a refresh token still exists and hasn't expired.
    pub fn is_token_live(&self, refresh_token_id: &str) -> bool {
        let now = self.now_f64();
        self.0
            .state
            .lock()
            .data
            .refresh_tokens
            .iter()
            .any(|t| t.id == refresh_token_id && !expired(t, now))
    }

    /// The credentials linked to a user, as `auth/current_user` lists them.
    pub fn credentials_of(&self, user_id: &str) -> Vec<Credential> {
        self.0
            .state
            .lock()
            .data
            .credentials
            .iter()
            .filter(|c| c.user_id == user_id)
            .cloned()
            .collect()
    }

    /// Validate a `Bearer` access token (`async_validate_access_token` plus
    /// `async_user_not_allowed_do_auth`).
    pub fn validate_access_token(&self, token: &str, ip: Option<IpAddr>) -> Option<Principal> {
        let iss = jwt::unverified_issuer(token)?;
        let now = self.0.clock.now();
        let s = self.0.state.lock();
        let rt = s.data.refresh_tokens.iter().find(|t| t.id == iss)?;
        if expired(rt, now.as_microsecond() as f64 / 1e6) {
            return None;
        }
        jwt::verify(token, rt.jwt_key.as_deref()?, now.as_second())?;
        let user = s.data.users.iter().find(|u| u.id == rt.user_id)?;
        if user_not_allowed(user, ip).is_some() {
            return None;
        }
        Some(Principal {
            user_id: user.id.clone(),
            refresh_token_id: rt.id.clone(),
            permission: user.permission(),
        })
    }

    /// `async_update_user(group_ids=...)`. Takes effect on the next request:
    /// permissions are read when each access token is validated.
    pub fn set_user_groups(&self, user_id: &str, group_ids: &[&str]) -> bool {
        let mut s = self.0.state.lock();
        let Some(user) = s.data.users.iter_mut().find(|u| u.id == user_id) else {
            return false;
        };
        user.group_ids = group_ids.iter().map(|g| g.to_string()).collect();
        s.dirty |= DIRTY_AUTH;
        true
    }

    pub fn user(&self, user_id: &str) -> Option<User> {
        self.0
            .state
            .lock()
            .data
            .users
            .iter()
            .find(|u| u.id == user_id)
            .cloned()
    }

    fn now_f64(&self) -> f64 {
        self.0.clock.now().as_microsecond() as f64 / 1e6
    }
}

/// Whether a user has a login for `username` (during onboarding, only an
/// owner can). A login without a user is left over from a failed save.
fn taken(s: &State, username: &str) -> bool {
    let normalized = normalize_username(username);
    s.provider.users.iter().any(|u| {
        let same = normalize_username(&u.username) == normalized
            || (s.legacy_usernames && u.username == username);
        same && user_for_username(&s.data, &u.username).is_some()
    })
}

/// `_create_auth_code_store`'s `store_result`: codes expire after 10
/// minutes; `hars` also keeps at most [`MAX_PENDING`].
fn store_code(s: &mut State, client_id: &str, username: &str, now: Timestamp) -> String {
    s.codes
        .retain(|_, c| now.duration_since(c.created) < AUTH_CODE_TTL);
    if s.codes.len() >= MAX_PENDING {
        let oldest = s
            .codes
            .iter()
            .min_by_key(|(_, c)| c.created)
            .map(|(k, _)| k.clone());
        s.codes.remove(&oldest.expect("not empty"));
    }
    let code = uuid_hex();
    s.codes.insert(
        code.clone(),
        AuthCode {
            client_id: client_id.into(),
            username: username.into(),
            created: now,
        },
    );
    code
}

/// `normalize_username`: `str.strip().casefold()`.
fn normalize_username(username: &str) -> String {
    ha_core::casefold::casefold(username.trim())
}

/// bcrypt only uses the first 72 bytes; HA truncates explicitly.
fn truncate72(password: &str) -> &[u8] {
    &password.as_bytes()[..password.len().min(72)]
}

fn expired(t: &RefreshToken, now: f64) -> bool {
    t.expire_at.is_some_and(|at| at < now)
}

fn user_for_username<'a>(data: &'a AuthData, username: &str) -> Option<&'a User> {
    let cred = data
        .credentials
        .iter()
        .find(|c| c.auth_provider_type == PROVIDER_TYPE && c.username() == Some(username))?;
    data.users.iter().find(|u| u.id == cred.user_id)
}

/// `async_get_or_create_user`: a provider login without a linked user gets
/// a new admin user named after the username (not the owner).
fn get_or_create_user(data: &mut AuthData, username: &str) -> (String, String) {
    let linked = data
        .credentials
        .iter()
        .find(|c| c.auth_provider_type == PROVIDER_TYPE && c.username() == Some(username))
        .filter(|c| data.users.iter().any(|u| u.id == c.user_id));
    if let Some(cred) = linked {
        return (cred.user_id.clone(), cred.id.clone());
    }
    let user_id = uuid_hex();
    data.users.push(User {
        id: user_id.clone(),
        group_ids: vec![GROUP_ID_ADMIN.into()],
        is_owner: false,
        is_active: true,
        name: Some(username.into()),
        system_generated: false,
        local_only: false,
        extra: Map::new(),
    });
    let cred_id = uuid_hex();
    data.credentials
        .retain(|c| !(c.auth_provider_type == PROVIDER_TYPE && c.username() == Some(username)));
    data.credentials.push(Credential {
        id: cred_id.clone(),
        user_id: user_id.clone(),
        auth_provider_type: PROVIDER_TYPE.into(),
        auth_provider_id: None,
        data: json!({ "username": username }),
        extra: Map::new(),
    });
    (user_id, cred_id)
}

/// `async_user_not_allowed_do_auth`.
fn user_not_allowed(user: &User, ip: Option<IpAddr>) -> Option<&'static str> {
    if !user.is_active {
        return Some("User is not active");
    }
    if !user.local_only {
        return None;
    }
    match ip {
        None => Some("Invalid remote IP"),
        Some(ip) if !indieauth::is_local(ip) => Some("User cannot authenticate remotely"),
        Some(_) => None,
    }
}

/// `async_create_access_token`: record the use, slide the refresh token's
/// expiry, and sign a JWT with its key.
fn issue_access_token(
    s: &mut State,
    idx: usize,
    ip: Option<IpAddr>,
    now: Timestamp,
) -> (String, i64) {
    let rt = &mut s.data.refresh_tokens[idx];
    rt.last_used_at = Some(iso(now));
    rt.last_used_ip = ip.map(|ip| ip.to_string());
    if rt.expire_at.is_some() {
        rt.expire_at = Some(now.as_microsecond() as f64 / 1e6 + REFRESH_TOKEN_EXPIRATION_SECS);
    }
    let lifetime = rt.access_token_expiration as i64;
    let iat = now.as_second();
    let token = jwt::encode(
        &jwt::Claims {
            iss: rt.id.clone(),
            iat,
            exp: iat + lifetime,
        },
        rt.jwt_key
            .as_deref()
            .expect("tokens without a jwt_key are dropped on load"),
    );
    s.dirty |= DIRTY_AUTH;
    (token, lifetime)
}

/// Constant-time over every token, like HA's `hmac.compare_digest` loop.
fn find_by_token(tokens: &[RefreshToken], token: &str) -> Option<usize> {
    let mut found = None;
    for (i, t) in tokens.iter().enumerate() {
        if ct_eq(t.token.as_bytes(), token.as_bytes()) {
            found = Some(i);
        }
    }
    found
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Python's `datetime.isoformat()` of an aware UTC time.
fn iso(t: Timestamp) -> String {
    t.strftime("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// `uuid.uuid4().hex`.
fn uuid_hex() -> String {
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    hex(&b)
}

/// `secrets.token_hex(64)`.
fn token_hex() -> String {
    let mut b = [0u8; 64];
    rand::rng().fill_bytes(&mut b);
    hex(&b)
}

#[cfg(test)]
mod tests;
