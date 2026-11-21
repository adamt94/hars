//! Hue's config flow (`components/hue/config_flow.py` with aiohue 4.8.1),
//! as a user starts it: the bridges the discovery service lists, or a host
//! typed in, then the link button.
//!
//! The requests are aiohue's (`discover_nupnp`, `discover_bridge`,
//! `create_app_key`), in its order: the discovery service; for each bridge
//! it lists `GET http://<host>/api/config` (its id) and
//! `GET https://<host>/clip/v2/resource` (403 means it has the v2 API);
//! for the key `POST https://<host>/api`, then `http://` if that fails.
//! As in aiohue, no certificate is checked (a bridge signs its own) and
//! the first request is plain HTTP: nothing proves the bridge is the
//! bridge before the user presses its button.
//!
//! Where `hars` differs from HA, on purpose:
//! - A bridge without the v2 API ends the flow (`not_v2_bridge`, a reason
//!   HA doesn't have) where HA goes on to make a v1 entry: `hars` has no
//!   v1 client, so the entry could never be set up.
//! - Flows HA starts itself (zeroconf, HomeKit, an import after a refused
//!   key) don't exist: `hars` listens for no announcements.
//! - The application's name on the bridge is `home-assistant#` and the
//!   location's name cut to 19 characters, as HA's, but slugified by
//!   `hars`'s rule rather than python-slugify's (they differ for names
//!   outside ASCII).
//!
//! Checked by `ha-api`'s `config_entries/vectors/hue_flow.json`, made by
//! `scripts/vectors/hue_flow.py`.

use crate::flow::{Cx, Field, Flow, Handler, Kind, Step, StepFuture};
use serde_json::{Map, Value, json};

/// aiohue's `URL_NUPNP`.
pub const DISCOVERY: &str = "https://discovery.meethue.com/";
/// `HUE_MANUAL_BRIDGE_ID`.
const MANUAL: &str = "manual";

pub const HANDLER: Handler = Handler {
    domain: super::DOMAIN,
    class: "HueFlowHandler",
    version: 1,
    minor_version: 1,
    steps: &["user", "init", "manual", "link"],
    new: || Box::new(HueFlow::default()),
};

/// aiohue's `DiscoveredHueBridge`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Found {
    host: String,
    id: String,
    supports_v2: bool,
}

#[derive(Default)]
struct HueFlow {
    bridge: Option<Found>,
    /// `discovered_bridges`: `None` until a discovery found one.
    discovered: Option<Vec<Found>>,
}

/// aiohue's `normalize_bridge_id`.
pub(super) fn normalize_bridge_id(id: &str) -> String {
    let id = id.to_lowercase();
    if id.len() == 17 && id.matches(':').count() == 5 {
        return id.replace(':', "");
    }
    if id.len() == 16 && id.get(6..10) == Some("fffe") {
        return format!("{}{}", &id[..6], &id[10..]);
    }
    id
}

/// The bridge's base URL for a scheme: at its host, or under the stand-in
/// (which is told the scheme and host it stands for).
fn base(cx: &Cx<'_>, scheme: &str, host: &str) -> String {
    crate::hue::base(cx.endpoints.hue_bridge.as_deref(), scheme, host)
}

/// The client aiohue's calls share: no certificate check (`ssl=False`).
fn client() -> Option<ha_http::Client> {
    ha_http::Client::builder(crate::USER_AGENT)
        .danger_accept_any_certificate()
        .build()
        .ok()
}

/// Whether a reply says it is JSON (aiohttp's `res.json()` raises a
/// `ContentTypeError` for anything but `application/json` or
/// `application/…+json`).
pub(super) fn is_json(res: &ha_http::Response) -> bool {
    let kind = res
        .headers
        .get(ha_http::header::CONTENT_TYPE)
        .and_then(|kind| kind.to_str().ok())
        .unwrap_or_default();
    let kind = kind.split(';').next().unwrap_or_default().trim();
    let kind = kind.to_ascii_lowercase();
    kind.strip_prefix("application/")
        .is_some_and(|sub| sub == "json" || sub.ends_with("+json"))
}

/// aiohttp's `res.json()`: the body as JSON, if the reply says it is JSON
/// and it is.
fn json_of(res: &ha_http::Response) -> Option<Value> {
    is_json(res)
        .then(|| serde_json::from_slice(&res.body).ok())
        .flatten()
}

/// aiohue's `discover_bridge`: the bridge's id (`is_hue_bridge`) and
/// whether it has the v2 API (`is_v2_bridge`). `None` for whatever HA
/// treats as "no bridge there" (aiohttp's `ClientError`).
async fn discover_bridge(cx: &Cx<'_>, client: &ha_http::Client, host: &str) -> Option<Found> {
    let url = format!("{}/api/config", base(cx, "http", host));
    let res = client.get(&url, &[]).await.ok()?;
    if res.status.as_u16() >= 400 {
        return None;
    }
    let config = json_of(&res)?;
    // "There are some emulator projects out there that emulate a Hue
    // bridge in a sloppy way, ignore them."
    let id = normalize_bridge_id(config.get("bridgeid")?.as_str()?);
    // The v2 API is https only, and answers 403 without a key.
    let url = format!("{}/clip/v2/resource", base(cx, "https", host));
    let supports_v2 = matches!(client.get(&url, &[]).await, Ok(res) if res.status.as_u16() == 403);
    Some(Found {
        host: host.to_owned(),
        id,
        supports_v2,
    })
}

/// aiohue's `discover_nupnp`, within HA's time-out: every bridge the
/// service lists that answers. None if the service can't be reached, or
/// is too slow.
async fn discover_nupnp(cx: &Cx<'_>, client: &ha_http::Client) -> Vec<Found> {
    let find = async {
        let mut found = Vec::new();
        let Ok(res) = client.get(&cx.endpoints.hue_discovery, &[]).await else {
            return found;
        };
        let listed = json_of(&res).unwrap_or_default();
        for item in listed.as_array().into_iter().flatten() {
            // "The nupnp discovery might return items that are not in
            // local network": each is asked whether it is there.
            if let Some(host) = item["internalipaddress"].as_str()
                && let Some(bridge) = discover_bridge(cx, client, host).await
            {
                found.push(bridge);
            }
        }
        found
    };
    tokio::time::timeout(cx.endpoints.hue_discovery_timeout, find)
        .await
        .unwrap_or_default()
}

/// Why a key wasn't made.
enum LinkError {
    /// The bridge's error 101: `LinkButtonNotPressed`.
    NotPressed,
    Other,
}

/// aiohue's `create_app_key`: over https, then over http.
async fn create_app_key(
    cx: &Cx<'_>,
    client: &ha_http::Client,
    host: &str,
    device_type: &str,
) -> Result<String, LinkError> {
    let body = json!({"devicetype": device_type});
    let mut last = LinkError::Other;
    for scheme in ["https", "http"] {
        let url = format!("{}/api", base(cx, scheme, host));
        let sent = serde_json::to_vec(&body).unwrap_or_default();
        let post = client.request(ha_http::Method::POST, &url, &[], Some(sent.into()));
        let Ok(res) = post.await else {
            last = LinkError::Other;
            continue;
        };
        if res.status.as_u16() >= 400 {
            last = LinkError::Other;
            continue;
        }
        // The answer is a list, of one result; an error in it comes
        // before anything else it says.
        let answer = json_of(&res).unwrap_or_default();
        let first = &answer[0];
        if let Some(error) = first.get("error") {
            last = match error["type"].as_i64() {
                Some(101) => LinkError::NotPressed,
                _ => LinkError::Other,
            };
            continue;
        }
        match first["success"]["username"].as_str() {
            Some(key) => return Ok(key.to_owned()),
            None => last = LinkError::Other,
        }
    }
    Err(last)
}

/// `slugify(location_name, max_length=19)`, for the name the bridge lists
/// the application under.
fn device_name(location: &str) -> String {
    let slug = ha_core::slugify(location).replace('_', "-");
    let cut: String = slug.chars().take(19).collect();
    cut.trim_end_matches('-').to_owned()
}

impl HueFlow {
    /// `async_step_init`.
    async fn init(&mut self, cx: &mut Cx<'_>, input: Option<Map<String, Value>>) -> Step {
        let picked = input.as_ref().and_then(|i| i["id"].as_str());
        if picked == Some(MANUAL) {
            return self.manual(cx, None).await;
        }
        if let Some(id) = picked
            && let Some(bridge) = self
                .discovered
                .iter()
                .flatten()
                .find(|b| b.id == id)
                .cloned()
        {
            cx.set_unique_id(&bridge.id);
            return self.link_to(bridge);
        }
        let Some(client) = client() else {
            return Step::abort("cannot_connect");
        };
        let mut bridges = discover_nupnp(cx, &client).await;
        if !bridges.is_empty() {
            // Those that aren't set up yet.
            let configured = cx.current_unique_ids();
            bridges.retain(|b| !configured.contains(&b.id));
            // One per id, as HA's dict keeps them: where the first was
            // listed, with what the last one said.
            let mut by_id: Vec<Found> = Vec::with_capacity(bridges.len());
            for bridge in bridges {
                match by_id.iter_mut().find(|b| b.id == bridge.id) {
                    Some(same) => *same = bridge,
                    None => by_id.push(bridge),
                }
            }
            bridges = by_id;
            self.discovered = Some(bridges.clone());
        }
        if self.discovered.as_ref().is_none_or(Vec::is_empty) {
            return self.manual(cx, None).await;
        }
        let mut options: Vec<(String, String)> = bridges
            .iter()
            .map(|b| (b.id.clone(), b.host.clone()))
            .collect();
        options.push((MANUAL.into(), "Manually add a Hue Bridge".into()));
        Step::Form {
            step_id: "init".into(),
            schema: Some(vec![Field::required("id", Kind::Select(options))]),
            errors: None,
            description_placeholders: None,
        }
    }

    /// `async_step_manual`.
    async fn manual(&mut self, cx: &mut Cx<'_>, input: Option<Map<String, Value>>) -> Step {
        let Some(host) = input.as_ref().and_then(|i| i["host"].as_str()) else {
            return Step::Form {
                step_id: "manual".into(),
                schema: Some(vec![Field::required_str("host")]),
                errors: None,
                description_placeholders: None,
            };
        };
        // `_async_abort_entries_match({"host": host})`.
        if cx.entry_with_data("host", host) {
            return Step::abort("already_configured");
        }
        let found = match client() {
            Some(client) => discover_bridge(cx, &client, host).await,
            None => None,
        };
        match found {
            Some(bridge) => self.link_to(bridge),
            None => Step::abort("cannot_connect"),
        }
    }

    /// On to the link step with `bridge`, unless `hars` can't run it.
    fn link_to(&mut self, bridge: Found) -> Step {
        if !bridge.supports_v2 {
            return Step::abort("not_v2_bridge");
        }
        self.bridge = Some(bridge);
        Self::link_form(None)
    }

    fn link_form(error: Option<&'static str>) -> Step {
        Step::Form {
            step_id: "link".into(),
            schema: None,
            errors: error.map(|e| vec![("base".into(), e.into())]),
            description_placeholders: None,
        }
    }

    /// `async_step_link`.
    async fn link(&mut self, cx: &mut Cx<'_>, input: Option<Map<String, Value>>) -> Step {
        let Some(bridge) = self.bridge.clone() else {
            return Step::abort("cannot_connect");
        };
        if input.is_none() {
            return Self::link_form(None);
        }
        let name = device_name(&cx.hass.config().location_name);
        let made = match client() {
            Some(client) => {
                let device_type = format!("home-assistant#{name}");
                create_app_key(cx, &client, &bridge.host, &device_type).await
            }
            None => Err(LinkError::Other),
        };
        let key = match made {
            Ok(key) => key,
            Err(LinkError::NotPressed) => return Self::link_form(Some("register_failed")),
            Err(LinkError::Other) => {
                tracing::error!(
                    "Unknown error connecting with Hue bridge at {}",
                    bridge.host
                );
                return Self::link_form(Some("linking"));
            }
        };
        // "Can happen if we come from import or manual entry".
        if !cx.has_unique_id() {
            cx.set_unique_id(&normalize_bridge_id(&bridge.id));
        }
        let data = json!({"host": bridge.host, "api_key": key, "api_version": 2});
        Step::CreateEntry {
            title: format!("Hue Bridge {}", bridge.id),
            data: data.as_object().cloned().unwrap_or_default(),
        }
    }
}

impl Flow for HueFlow {
    fn step<'a>(
        &'a mut self,
        cx: &'a mut Cx<'_>,
        step_id: &'a str,
        input: Option<Map<String, Value>>,
    ) -> StepFuture<'a> {
        Box::pin(async move {
            match step_id {
                "manual" => self.manual(cx, input).await,
                "link" => self.link(cx, input).await,
                // `user` is `init` ("for backwards compatibility").
                _ => self.init(cx, input).await,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// aiohue's `normalize_bridge_id`, and the application name's cut.
    #[test]
    fn ids_and_names_as_aiohue() {
        assert_eq!(normalize_bridge_id("C42996FFFE000001"), "c42996000001");
        assert_eq!(normalize_bridge_id("c4:29:96:00:00:01"), "c42996000001");
        assert_eq!(normalize_bridge_id("C42996000001"), "c42996000001");
        assert_eq!(device_name("Home"), "home");
        assert_eq!(
            device_name("My Holiday Home By The Sea"),
            "my-holiday-home-by"
        );
    }
}
