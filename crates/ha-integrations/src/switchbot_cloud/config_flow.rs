//! The SwitchBot Cloud config flow (`components/switchbot_cloud/config_flow.py`):
//! one step asking for the account's API token and secret, checked by
//! listing its devices. The token is the entry's unique id.

use super::DOMAIN;
use super::api::{Api, ApiError};
use crate::flow::{Cx, Field, Flow, Handler, SOURCE_USER, Step, StepFuture};
use serde_json::{Map, Value};

/// `ENTRY_TITLE`.
const TITLE: &str = "SwitchBot Cloud";
const API_TOKEN: &str = "api_token";
const API_KEY: &str = "api_key";

pub const HANDLER: Handler = Handler {
    domain: DOMAIN,
    class: "SwitchBotCloudConfigFlow",
    version: 1,
    minor_version: 1,
    steps: &[SOURCE_USER],
    new: || Box::new(SwitchBotFlow),
};

/// `STEP_USER_DATA_SCHEMA`.
fn schema() -> Vec<Field> {
    vec![Field::required_str(API_TOKEN), Field::required_str(API_KEY)]
}

/// The flow keeps nothing between steps.
struct SwitchBotFlow;

impl Flow for SwitchBotFlow {
    /// `async_step_user`.
    fn step<'a>(
        &'a mut self,
        cx: &'a mut Cx<'_>,
        _step_id: &'a str,
        input: Option<Map<String, Value>>,
    ) -> StepFuture<'a> {
        Box::pin(async move {
            let Some(input) = input else {
                return Step::form(SOURCE_USER, schema(), None);
            };
            let field = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or_default();
            let (token, secret) = (field(API_TOKEN), field(API_KEY));
            let listed = match ha_http::Client::builder(crate::USER_AGENT).build() {
                Ok(client) => Api::new(
                    client,
                    cx.endpoints.switchbot.clone(),
                    token.to_owned(),
                    secret.to_owned(),
                )
                .list_devices()
                .await
                .map(drop),
                Err(e) => Err(ApiError::Other(e.to_string())),
            };
            let error = match listed {
                Ok(()) => {
                    cx.set_unique_id(token);
                    if let Some(abort) = cx.abort_if_unique_id_configured() {
                        return abort;
                    }
                    return Step::CreateEntry {
                        title: TITLE.into(),
                        data: input,
                    };
                }
                // `SwitchBotConnectionError`.
                Err(ApiError::Connection(_)) => "cannot_connect",
                // `SwitchBotAuthenticationError`.
                Err(ApiError::Auth) => "invalid_auth",
                // Anything else the library raises.
                Err(e) => {
                    tracing::error!("Unexpected exception: {e:?}");
                    "unknown"
                }
            };
            Step::form(SOURCE_USER, schema(), Some(error))
        })
    }
}
