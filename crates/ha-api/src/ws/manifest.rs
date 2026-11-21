//! `manifest/list`, `manifest/get` and `integration/descriptions`
//! (`websocket_api/commands.py`): the embedded manifests and descriptions
//! (`manifests_data.rs`, generated from HA's wheel).
//!
//! Where `hars` differs from HA: `integration/descriptions` lists the
//! integrations `hars` has a config flow for, each on its own, so the Add
//! integration dialog offers what can be added. HA lists every
//! integration it ships, some grouped under a brand (`switchbot` holds
//! SwitchBot Bluetooth, SwitchBot Cloud and Matter).

use super::{Conn, Invalid};
use crate::ApiConfig;
use crate::manifests_data::{DESCRIPTIONS, MANIFESTS};
use ha_auth::Permission;
use ha_integrations::plugin::{STORE_DOMAIN, store};
use serde_json::{Map, Value, json};
use std::borrow::Cow;

fn manifest(domain: &str) -> Option<&'static str> {
    MANIFESTS
        .binary_search_by(|(d, _)| (*d).cmp(domain))
        .ok()
        .map(|i| MANIFESTS[i].1)
}

fn manifest_for(config: &ApiConfig, domain: &str) -> Option<Cow<'static, str>> {
    if let Some(m) = manifest(domain) {
        return Some(Cow::Borrowed(m));
    }
    if domain == STORE_DOMAIN && config.plugins.is_some() {
        return Some(Cow::Owned(store::store_manifest_json()));
    }
    config
        .plugins
        .as_ref()
        .and_then(|p| p.get(domain))
        .map(|p| Cow::Owned(p.manifest_json()))
}

/// `Integration.integration_type` (`hub` when the manifest doesn't say),
/// for integrations with an embedded manifest or an installed plugin.
pub(crate) fn integration_type_for(config: &ApiConfig, domain: &str) -> Option<String> {
    let raw = manifest_for(config, domain)?;
    let manifest: Value = serde_json::from_str(&raw).ok()?;
    Some(
        manifest
            .get("integration_type")
            .and_then(Value::as_str)
            .unwrap_or("hub")
            .to_owned(),
    )
}

/// The command's schema (`integrations: [str]` for the list,
/// `integration: str` for one), every error reported as voluptuous does.
fn check(msg: &Map<String, Value>, allowed: &str) -> Result<(), Invalid> {
    let mut errors = Vec::new();
    for (k, v) in msg {
        match k.as_str() {
            "id" | "type" => {}
            "integrations" if allowed == "integrations" => match v {
                Value::Array(items) => {
                    for (i, item) in items.iter().enumerate() {
                        if !item.is_string() {
                            errors.push(format!("expected str @ data['integrations'][{i}]"));
                        }
                    }
                }
                _ => errors
                    .push("expected a list for dictionary value @ data['integrations']".into()),
            },
            "integration" if allowed == "integration" => {
                if !v.is_string() {
                    errors.push("expected str for dictionary value @ data['integration']".into());
                }
            }
            k => errors.push(format!("extra keys not allowed @ data['{k}']")),
        }
    }
    if allowed == "integration" && !msg.contains_key("integration") {
        errors.push("required key not provided @ data['integration']".into());
    }
    match errors.is_empty() {
        true => Ok(()),
        false => Err(errors.join("\n")),
    }
}

pub(super) fn dispatch(
    conn: &mut Conn,
    id: u64,
    ty: &str,
    msg: &Map<String, Value>,
) -> Option<Result<(), Invalid>> {
    Some(match ty {
        // The asked-for integrations (deduplicated, in order), or every
        // loaded one; ones without a manifest are logged and left out.
        "manifest/list" => check(msg, "integrations").map(|()| {
            let loaded = crate::components(&conn.shared.config);
            let asked: Vec<String> = msg
                .get("integrations")
                .and_then(Value::as_array)
                .filter(|a| !a.is_empty())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_else(|| loaded.iter().map(|c| c.as_ref().to_owned()).collect());
            // `async_get_integrations`: the ones already loaded (cached)
            // first, then the rest, each in the order asked.
            let (mut asked, rest): (Vec<String>, Vec<String>) = asked
                .into_iter()
                .partition(|d| loaded.iter().any(|c| c.as_ref() == d));
            asked.extend(rest);
            let mut seen = Vec::new();
            let mut parts: Vec<Cow<'static, str>> = Vec::new();
            for d in asked {
                if seen.contains(&d) {
                    continue;
                }
                match manifest_for(&conn.shared.config, &d) {
                    Some(m) => parts.push(m),
                    None => tracing::error!(
                        "Unable to get manifest for integration {d}: Integration '{d}' not found."
                    ),
                }
                seen.push(d);
            }
            conn.out.result_raw(id, &format!("[{}]", parts.join(",")));
        }),
        // `require_admin`. `loader.async_get_integration_descriptions`,
        // including installed custom plugins under `custom.integration`.
        "integration/descriptions" => super::only_keys(msg, &[]).map(|()| {
            if conn.principal.permission < Permission::Admin {
                conn.out.error(json!(id), "unauthorized", "Unauthorized");
                return;
            }
            let handlers = crate::config_entries::handlers_of(&conn.shared.config);
            let mut integrations: Vec<String> = handlers
                .iter()
                .filter_map(|h| {
                    DESCRIPTIONS
                        .iter()
                        .find(|(domain, ..)| *domain == h.as_ref())
                        .map(|(domain, description, _)| format!("\"{domain}\":{description}"))
                })
                .collect();
            let mut translated: Vec<String> = handlers
                .iter()
                .filter_map(|h| {
                    DESCRIPTIONS
                        .iter()
                        .find(|(domain, ..)| *domain == h.as_ref())
                        .filter(|(.., is_translated)| *is_translated)
                        .map(|(domain, ..)| format!("\"{domain}\""))
                })
                .collect();
            let mut custom_integrations: Vec<String> = Vec::new();
            if let Some(plugins) = &conn.shared.config.plugins {
                if handlers.iter().any(|h| h.as_ref() == STORE_DOMAIN) {
                    integrations.push(format!(
                        "\"{STORE_DOMAIN}\":{}",
                        store::store_description_json()
                    ));
                    translated.push(format!("\"{STORE_DOMAIN}\""));
                }
                for plugin in plugins.installed() {
                    if plugin.manifest.config_flow {
                        custom_integrations.push(format!(
                            "\"{}\":{}",
                            plugin.domain(),
                            plugin.description_json()
                        ));
                    }
                }
            }
            conn.out.result_raw(
                id,
                &format!(
                    r#"{{"core":{{"integration":{{{}}},"helper":{{}},"translated_name":[{}]}},"custom":{{"integration":{{{}}},"helper":{{}}}}}}"#,
                    integrations.join(","),
                    translated.join(","),
                    custom_integrations.join(",")
                ),
            );
        }),
        "manifest/get" => check(msg, "integration").map(|()| {
            let d = msg["integration"].as_str().unwrap_or_default();
            match manifest_for(&conn.shared.config, d) {
                Some(m) => {
                    conn.out.result_raw(id, &m);
                }
                None => {
                    conn.out
                        .error(json!(id), "not_found", "Integration not found");
                }
            }
        }),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifests_are_sorted_and_valid() {
        assert!(MANIFESTS.windows(2).all(|w| w[0].0 < w[1].0));
        for (d, m) in MANIFESTS {
            let v: Value = serde_json::from_str(m).unwrap();
            assert_eq!(v["domain"], *d);
            assert_eq!(v["is_built_in"], true);
        }
        assert!(
            manifest("hue")
                .unwrap()
                .starts_with(r#"{"domain":"hue","name":"Philips Hue","#)
        );
        assert!(manifest("nope").is_none());
    }
}
