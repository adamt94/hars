//! `.storage/auth` and `.storage/auth_provider.homeassistant`, field for
//! field (`auth/auth_store.py`, `auth/models.py`). Fields `hars` doesn't use
//! are kept in `extra` and written back unchanged, so a config directory can
//! move between HA and `hars` without losing anything.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const GROUP_ID_ADMIN: &str = "system-admin";
pub const GROUP_ID_USER: &str = "system-users";
pub const GROUP_ID_READ_ONLY: &str = "system-read-only";
/// System groups take their name from code, not the file.
pub(crate) const SYSTEM_GROUPS: [(&str, &str); 3] = [
    (GROUP_ID_ADMIN, "Administrators"),
    (GROUP_ID_USER, "Users"),
    (GROUP_ID_READ_ONLY, "Read Only"),
];

pub const TOKEN_TYPE_NORMAL: &str = "normal";
pub const TOKEN_TYPE_SYSTEM: &str = "system";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthData {
    #[serde(default)]
    pub users: Vec<User>,
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub credentials: Vec<Credential>,
    #[serde(default)]
    pub refresh_tokens: Vec<RefreshToken>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    #[serde(default)]
    pub group_ids: Vec<String>,
    #[serde(default)]
    pub is_owner: bool,
    #[serde(default = "yes")]
    pub is_active: bool,
    pub name: Option<String>,
    #[serde(default)]
    pub system_generated: bool,
    #[serde(default)]
    pub local_only: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn yes() -> bool {
    true
}

impl User {
    /// `models.User.is_admin`.
    pub fn is_admin(&self) -> bool {
        self.is_owner || (self.is_active && self.group_ids.iter().any(|g| g == GROUP_ID_ADMIN))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub name: Option<String>,
    /// `policy` and anything else, written back for custom groups.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    pub id: String,
    pub user_id: String,
    pub auth_provider_type: String,
    pub auth_provider_id: Option<String>,
    pub data: Value,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Credential {
    pub(crate) fn username(&self) -> Option<&str> {
        self.data.get("username").and_then(Value::as_str)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshToken {
    pub id: String,
    pub user_id: String,
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default)]
    pub client_icon: Option<String>,
    /// Inferred on load when missing (`auth_store.py`).
    #[serde(default)]
    pub token_type: Option<String>,
    pub created_at: String,
    pub access_token_expiration: f64,
    pub token: String,
    /// Tokens from before HA 0.76 have none and are dropped on load.
    #[serde(default)]
    pub jwt_key: Option<String>,
    #[serde(default)]
    pub last_used_at: Option<String>,
    #[serde(default)]
    pub last_used_ip: Option<String>,
    #[serde(default)]
    pub expire_at: Option<f64>,
    #[serde(default)]
    pub credential_id: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderData {
    #[serde(default)]
    pub users: Vec<ProviderUser>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderUser {
    pub username: String,
    /// base64 of the ASCII bcrypt string.
    pub password: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl AuthData {
    /// The fix-ups HA's `async_load` applies (`auth_store.py`): drop tokens
    /// without a `jwt_key`, infer a missing `token_type`, put every user in
    /// `system-admin` when there are no groups at all, and re-create missing
    /// system groups with their names from code.
    pub(crate) fn normalize(&mut self) {
        self.refresh_tokens.retain(|t| t.jwt_key.is_some());
        for t in &mut self.refresh_tokens {
            if t.token_type.is_none() {
                t.token_type = Some(
                    if t.client_id.is_none() {
                        TOKEN_TYPE_SYSTEM
                    } else {
                        TOKEN_TYPE_NORMAL
                    }
                    .into(),
                );
            }
        }
        if self.groups.is_empty() {
            for u in self.users.iter_mut().filter(|u| !u.system_generated) {
                u.group_ids = vec![GROUP_ID_ADMIN.into()];
            }
        }
        for (id, name) in SYSTEM_GROUPS {
            match self.groups.iter_mut().find(|g| g.id == id) {
                Some(g) => {
                    g.name = Some(name.into());
                    g.extra.remove("policy");
                }
                None => self.groups.push(Group {
                    id: id.into(),
                    name: Some(name.into()),
                    extra: Map::new(),
                }),
            }
        }
    }
}
