//! What a discovery message says, as `discovery.py` reads it: the topic
//! (`<prefix>/<component>/[<node_id>/]<object_id>/config`), the payload with
//! its abbreviations expanded (`abbreviations.py`) and its `~` topic base
//! put in, and a device's message (`<prefix>/device/<id>/config`, its
//! `components` each a message of their own, with the device's shared
//! options). What the client does with each is in [`super::client`].
//!
//! Not yet: discovery migration (`migrate_discovery`).

use super::tables;
use serde_json::{Map, Value};

/// A component's discovery: `(component, discovery_id)` is HA's
/// discovery hash.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub component: String,
    /// `"<node_id> <object_id>"`, or the object id alone.
    pub discovery_id: String,
    /// Empty: the component is removed.
    pub payload: Map<String, Value>,
    pub device_discovery: bool,
}

/// `TOPIC_MATCHER` on the topic with `<prefix>/` taken out (its first
/// occurrence, as `str.replace(…, 1)`): `(component, node_id, object_id)`.
/// `None` for a topic that isn't one; `Err` for one that ends in `config`
/// but has other characters (HA warns).
pub fn parse_topic(prefix: &str, topic: &str) -> Result<Option<Topic>, IllegalTopic> {
    let trimmed = topic.replacen(&format!("{prefix}/"), "", 1);
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    };
    // `\w+`: Unicode word characters.
    let word = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_');
    let parts: Vec<&str> = trimmed.split('/').collect();
    // `re.match` isn't anchored at the end, but HA's subscriptions only
    // bring topics of three or four levels ending in `config`.
    let found = match parts.as_slice() {
        [c, n, o, "config", ..] if word(c) && ok(n) && ok(o) => Some((*c, Some(*n), *o)),
        [c, o, "config", ..] if word(c) && ok(o) => Some((*c, None, *o)),
        _ => None,
    };
    match found {
        Some((c, n, o)) => Ok(Some(Topic {
            component: c.to_owned(),
            node_id: n.map(str::to_owned),
            object_id: o.to_owned(),
        })),
        None if trimmed.ends_with("config") => Err(IllegalTopic),
        None => Ok(None),
    }
}

/// A topic that ends in `config` but has characters discovery doesn't
/// take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IllegalTopic;

/// A discovery topic's parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topic {
    pub component: String,
    pub node_id: Option<String>,
    pub object_id: String,
}

fn expand(payload: &mut Map<String, Value>, table: &[(&str, &str)]) {
    // Each abbreviation present is replaced by its full key (overwriting
    // the full key if both are there, as HA's `pop` and set).
    let present: Vec<&str> = payload
        .keys()
        .filter_map(|k| {
            table
                .binary_search_by(|(a, _)| a.cmp(&k.as_str()))
                .ok()
                .map(|i| table[i].0)
        })
        .collect();
    for short in present {
        let full = table[table
            .binary_search_by(|(a, _)| a.cmp(&short))
            .unwrap_or_default()]
        .1;
        if let Some(v) = payload.remove(short) {
            payload.insert(full.to_owned(), v);
        }
    }
}

/// `_replace_all_abbreviations`.
pub fn expand_all(payload: &mut Map<String, Value>, component_only: bool) {
    expand(payload, tables::ABBREVIATIONS);
    if let Some(a) = payload.get_mut("availability") {
        match a {
            Value::Array(items) => {
                for item in items {
                    if let Value::Object(m) = item {
                        expand(m, tables::ABBREVIATIONS);
                    }
                }
            }
            Value::Object(m) => expand(m, tables::ABBREVIATIONS),
            _ => {}
        }
    }
    if component_only {
        return;
    }
    if let Some(Value::Object(m)) = payload.get_mut("origin") {
        expand(m, tables::ORIGIN_ABBREVIATIONS);
    }
    if let Some(Value::Object(m)) = payload.get_mut("device") {
        expand(m, tables::DEVICE_ABBREVIATIONS);
    }
    if let Some(Value::Object(components)) = payload.get_mut("components") {
        for c in components.values_mut() {
            if let Value::Object(m) = c {
                expand_all(m, true);
            }
        }
    }
}

/// `_replace_topic_base`: `~` at the start or end of a `…topic` value (and
/// an availability entry's `topic`) is the payload's `~`.
pub fn replace_base(payload: &mut Map<String, Value>) {
    let Some(base) = payload.remove("~") else {
        return;
    };
    // `f"{base}…"`: Python's `str()` of whatever the base is.
    let base = match &base {
        Value::String(s) => s.clone(),
        other => super::entity::cv_string(other).unwrap_or_else(|_| other.to_string()),
    };
    let put = |v: &mut String| {
        if v.starts_with('~') {
            *v = format!("{base}{}", &v[1..]);
        }
        if v.ends_with('~') {
            *v = format!("{}{base}", &v[..v.len() - 1]);
        }
    };
    for (k, v) in payload.iter_mut() {
        if k.ends_with("topic")
            && let Value::String(s) = v
            && !s.is_empty()
        {
            put(s);
        }
    }
    if let Some(a) = payload.get_mut("availability") {
        let items: Vec<&mut Value> = match a {
            Value::Array(items) => items.iter_mut().collect(),
            other => vec![other],
        };
        for item in items {
            if let Value::Object(m) = item
                && let Some(Value::String(t)) = m.get_mut("topic")
                && !t.is_empty()
            {
                put(t);
            }
        }
    }
}

/// `SHARED_OPTIONS`.
const SHARED: [&str; 11] = [
    "availability",
    "availability_mode",
    "availability_template",
    "availability_topic",
    "command_topic",
    "encoding",
    "message_expiry_interval",
    "payload_available",
    "payload_not_available",
    "state_topic",
    "qos",
];

/// Why a message is dropped, worded as HA logs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dropped {
    /// A warning.
    Warn(String),
    /// Nothing to say.
    Quiet,
}

/// `json_loads_object`.
fn object(payload: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str(payload) {
        Ok(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// `MQTT_ORIGIN_INFO_SCHEMA`: a `name`, and strings.
fn valid_origin(v: &Value) -> Result<(), String> {
    let Value::Object(m) = v else {
        return Err("expected a dictionary".into());
    };
    if !m.contains_key("name") {
        return Err("required key not provided @ data['name']".into());
    }
    for (k, v) in m {
        if matches!(k.as_str(), "name" | "sw_version" | "support_url")
            && super::entity::cv_string(v).is_err()
        {
            return Err(format!("expected str for dictionary value @ data['{k}']"));
        }
    }
    Ok(())
}

/// A component's message (`<prefix>/<component>/…/config`).
pub fn component(
    component: &str,
    node_id: Option<&str>,
    object_id: &str,
    payload: &str,
) -> Result<Found, Dropped> {
    let mut map = match payload.is_empty() {
        true => Map::new(),
        false => object(payload).ok_or_else(|| {
            Dropped::Warn(format!("Unable to parse JSON {object_id}: '{payload}'"))
        })?,
    };
    expand_all(&mut map, false);
    if let Some(origin) = map.get("origin")
        && let Err(e) = valid_origin(origin)
    {
        return Err(Dropped::Warn(format!(
            "Unable to parse origin information from discovery message: {e}, got {origin}"
        )));
    }
    if map.contains_key("~") {
        replace_base(&mut map);
    }
    Ok(Found {
        component: component.to_owned(),
        discovery_id: match node_id {
            Some(n) => format!("{n} {object_id}"),
            None => object_id.to_owned(),
        },
        payload: map,
        device_discovery: false,
    })
}

/// A device's message (`<prefix>/device/[<node_id>/]<object_id>/config`):
/// each of its components. `already` is what is discovered now (the
/// hashes), for an empty payload, which removes every component of the
/// device.
pub fn device<'a>(
    node_id: Option<&str>,
    object_id: &str,
    payload: &str,
    already: impl Iterator<Item = (&'a str, &'a str)>,
) -> Result<Vec<Found>, Dropped> {
    let device_node_id = match node_id {
        Some(n) => format!("{n} {object_id}"),
        None => object_id.to_owned(),
    };
    if payload.is_empty() {
        // `_generate_device_config`.
        let mut out = Vec::new();
        for (platform, discovery_id) in already {
            let mut ids: Vec<&str> = discovery_id.split(' ').collect();
            let component_node_id = if ids.len() > 2 {
                let b = ids.remove(1);
                let a = ids.remove(0);
                format!("{b} {a}")
            } else {
                ids.remove(0).to_owned()
            };
            if ids.is_empty() || component_node_id != device_node_id {
                continue;
            }
            out.push(Found {
                component: platform.to_owned(),
                discovery_id: discovery_id.to_owned(),
                payload: Map::new(),
                device_discovery: true,
            });
        }
        if out.is_empty() {
            return Err(Dropped::Warn(format!(
                "No device components to cleanup for {object_id}, node_id '{}'",
                node_id.unwrap_or("None")
            )));
        }
        return Ok(out);
    }
    let mut map = object(payload)
        .ok_or_else(|| Dropped::Warn(format!("Unable to parse JSON {object_id}: '{payload}'")))?;
    expand_all(&mut map, false);
    let invalid = |why: String| {
        Dropped::Warn(format!(
            "Invalid MQTT device discovery payload for {object_id}, {why}: '{payload}'"
        ))
    };
    // `DEVICE_DISCOVERY_SCHEMA`.
    for key in ["device", "components", "origin"] {
        if !map.contains_key(key) {
            return Err(invalid(format!(
                "required key not provided @ data['{key}']"
            )));
        }
    }
    if let Err(e) = map.get("device").map(device_ok).unwrap_or(Ok(())) {
        return Err(invalid(format!(
            "{e} for dictionary value @ data['device']"
        )));
    }
    if let Some(Err(e)) = map.get("origin").map(valid_origin) {
        return Err(invalid(format!(
            "{e} for dictionary value @ data['origin']"
        )));
    }
    let Some(Value::Object(components)) = map.get("components") else {
        return Err(invalid(
            "expected dict for dictionary value @ data['components']".into(),
        ));
    };
    for (id, c) in components {
        let Value::Object(c) = c else {
            return Err(invalid(format!(
                "expected a dictionary for dictionary value @ data['components']['{id}']"
            )));
        };
        let platform = c.get("platform").and_then(Value::as_str);
        let Some(platform) = platform.filter(|p| tables::SUPPORTED_COMPONENTS.contains(p)) else {
            return Err(invalid(format!(
                "value must be one of {:?} for dictionary value @ data['components']['{id}']['platform']",
                tables::SUPPORTED_COMPONENTS
            )));
        };
        // `check_unique_id`: an entity platform with more than its
        // platform needs a unique id.
        if platform != "device_automation"
            && platform != "tag"
            && c.len() > 1
            && !c.contains_key("unique_id")
        {
            return Err(invalid(format!(
                "required key not provided @ data['components']['{id}']['unique_id']"
            )));
        }
    }
    let device = map.get("device").cloned().unwrap_or(Value::Null);
    let origin = map.get("origin").cloned().unwrap_or(Value::Null);
    let mut out = Vec::new();
    for (id, c) in components {
        let mut c = c.as_object().cloned().unwrap_or_default();
        let platform = c
            .remove("platform")
            .and_then(|p| p.as_str().map(str::to_owned))
            .unwrap_or_default();
        if !c.is_empty() {
            c.insert("device".into(), device.clone());
            c.insert("origin".into(), origin.clone());
            for option in SHARED {
                if let Some(v) = map.get(option)
                    && !c.contains_key(option)
                {
                    c.insert(option.to_owned(), v.clone());
                }
            }
        }
        if c.contains_key("~") {
            replace_base(&mut c);
        }
        out.push(Found {
            component: platform,
            discovery_id: match node_id {
                Some(n) => format!("{object_id} {n} {id}"),
                None => format!("{object_id} {id}"),
            },
            payload: c,
            device_discovery: true,
        });
    }
    Ok(out)
}

/// `MQTT_ENTITY_DEVICE_INFO_SCHEMA`'s one rule a device message checks
/// before its components: an identifier or a connection.
fn device_ok(v: &Value) -> Result<(), String> {
    let Value::Object(m) = v else {
        return Err("expected a dictionary".into());
    };
    let has = |k: &str| match m.get(k) {
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Null) | None => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    };
    match has("identifiers") || has("connections") {
        true => Ok(()),
        false => Err(
            "Device must have at least one identifying value in 'identifiers' \
                      and/or 'connections'"
                .into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn topics_have_hass_shape() {
        let p = "homeassistant";
        let t = |c: &str, n: Option<&str>, o: &str| {
            Ok(Some(Topic {
                component: c.into(),
                node_id: n.map(Into::into),
                object_id: o.into(),
            }))
        };
        assert_eq!(
            parse_topic(
                p,
                "homeassistant/sensor/0x00158d00a1b2c3e5/temperature/config"
            ),
            t("sensor", Some("0x00158d00a1b2c3e5"), "temperature")
        );
        assert_eq!(
            parse_topic(p, "homeassistant/sensor/A1B2C3_status/config"),
            t("sensor", None, "A1B2C3_status")
        );
        assert_eq!(
            parse_topic(p, "homeassistant/sensor/a b/config"),
            Err(IllegalTopic)
        );
        assert_eq!(parse_topic(p, "homeassistant/sensor/a/state"), Ok(None));
    }

    #[test]
    fn abbreviations_and_base_are_expanded() {
        let mut m = json!({"~": "tele/x", "stat_t": "~/SENSOR", "avty": [{"t": "~/LWT"}],
                           "dev": {"ids": ["a"], "mf": "M"}, "o": {"name": "n", "sw": "1"},
                           "uniq_id": "u"})
        .as_object()
        .unwrap()
        .clone();
        expand_all(&mut m, false);
        replace_base(&mut m);
        assert_eq!(
            Value::Object(m),
            json!({"state_topic": "tele/x/SENSOR", "availability": [{"topic": "tele/x/LWT"}],
                   "device": {"identifiers": ["a"], "manufacturer": "M"},
                   "origin": {"name": "n", "sw_version": "1"}, "unique_id": "u"})
        );
    }

    #[test]
    fn a_device_message_shares_its_options() {
        let payload = json!({"dev": {"ids": "d"}, "o": {"name": "x"}, "stat_t": "s", "qos": 1,
                             "cmps": {"a": {"p": "sensor", "uniq_id": "a", "qos": 0},
                                      "b": {"p": "binary_sensor"}}})
        .to_string();
        let found = device(None, "d", &payload, std::iter::empty()).unwrap();
        assert_eq!(found[0].discovery_id, "d a");
        assert_eq!(found[0].payload["state_topic"], "s");
        assert_eq!(found[0].payload["qos"], 0);
        assert!(found[1].payload.is_empty());
        let gone = device(
            None,
            "d",
            "",
            [("sensor", "d a"), ("sensor", "e a")].into_iter(),
        )
        .unwrap();
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].discovery_id, "d a");
    }
}
