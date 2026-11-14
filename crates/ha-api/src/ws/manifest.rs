//! `manifest/list` and `manifest/get` (`websocket_api/commands.py`): the
//! embedded manifests (`manifests_data.rs`, generated from HA's wheel).

use super::{Conn, Invalid};
use crate::manifests_data::MANIFESTS;
use serde_json::{Map, Value, json};

fn manifest(domain: &str) -> Option<&'static str> {
    MANIFESTS
        .binary_search_by(|(d, _)| (*d).cmp(domain))
        .ok()
        .map(|i| MANIFESTS[i].1)
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
            let asked: Vec<String> = msg
                .get("integrations")
                .and_then(Value::as_array)
                .filter(|a| !a.is_empty())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_else(|| {
                    crate::components(&conn.shared.config)
                        .into_iter()
                        .map(str::to_owned)
                        .collect()
                });
            // `async_get_integrations`: the ones already loaded (cached)
            // first, then the rest, each in the order asked.
            let loaded = crate::components(&conn.shared.config);
            let (mut asked, rest): (Vec<String>, Vec<String>) = asked
                .into_iter()
                .partition(|d| loaded.contains(&d.as_str()));
            asked.extend(rest);
            let mut seen = Vec::new();
            let mut parts = Vec::new();
            for d in asked {
                if seen.contains(&d) {
                    continue;
                }
                match manifest(&d) {
                    Some(m) => parts.push(m),
                    None => tracing::error!(
                        "Unable to get manifest for integration {d}: Integration '{d}' not found."
                    ),
                }
                seen.push(d);
            }
            conn.out.result_raw(id, &format!("[{}]", parts.join(",")));
        }),
        "manifest/get" => check(msg, "integration").map(|()| {
            let d = msg["integration"].as_str().unwrap_or_default();
            match manifest(d) {
                Some(m) => {
                    conn.out.result_raw(id, m);
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
