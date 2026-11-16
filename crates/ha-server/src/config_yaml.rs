//! The top-level `configuration.yaml` keys hars doesn't act on, so start-up
//! can say so rather than drop them silently.

use ha_core::yaml::Yaml;

/// Whether hars reads a key's contents: `recorder:` exactly, and every
/// `automation` key (HA collects `automation <label>:` too).
fn is_read(key: &str, domain: &str) -> bool {
    key == "recorder" || domain == "automation"
}

/// Domains where a bare key (`default_config:`, `frontend:`) changes nothing
/// in hars: hars runs them without being asked, or (`script`, `scene`) an
/// empty one holds nothing to run.
const BARE_OK: &[&str] = &[
    "api",
    "auth",
    "config",
    "default_config",
    "frontend",
    "history",
    "homeassistant",
    "http",
    "logbook",
    "lovelace",
    "mobile_app",
    "onboarding",
    "person",
    "recorder",
    "repairs",
    "scene",
    "script",
    "sun",
    "system_log",
    "websocket_api",
    "zone",
];

/// Why a key is ignored.
#[derive(Debug, PartialEq, Eq)]
pub enum Ignored {
    /// hars has no such integration.
    Unsupported,
    /// hars runs the integration but doesn't read these options.
    Options,
}

/// The keys of a loaded `configuration.yaml` that hars ignores, in file
/// order. A key's domain is the part before any space, as in HA
/// (`automation old:` is `automation`).
pub fn ignored(config: &Yaml) -> Vec<(&str, Ignored)> {
    let Yaml::Map(pairs) = config else {
        return Vec::new();
    };
    pairs
        .iter()
        .filter_map(|(k, v)| {
            let key = k.as_str()?;
            let domain = key.split(' ').next().unwrap_or(key);
            if is_read(key, domain) {
                None
            } else if !BARE_OK.contains(&domain) {
                Some((key, Ignored::Unsupported))
            } else if holds_something(domain, v) {
                Some((key, Ignored::Options))
            } else {
                None
            }
        })
        .collect()
}

/// Whether a value sets anything. Any option does, even `false` or `[]`;
/// only `frontend`'s empty `themes` map doesn't (HA's default `frontend:
/// themes: !include_dir_merge_named themes`, with no themes, is
/// `{themes: {}}`).
fn holds_something(domain: &str, v: &Yaml) -> bool {
    let empty_themes = |k: &Yaml, v: &Yaml| {
        domain == "frontend"
            && k.as_str() == Some("themes")
            && matches!(v, Yaml::Map(m) if m.is_empty())
    };
    match v {
        Yaml::Map(pairs) => pairs.iter().any(|(k, v)| !empty_themes(k, v)),
        v => v.is_truthy(),
    }
}

/// One warning per ignored key.
pub fn warn_ignored(config: &Yaml) {
    for (key, why) in ignored(config) {
        match why {
            Ignored::Unsupported => tracing::warn!(
                "configuration.yaml: '{key}' is ignored: hars doesn't have this integration"
            ),
            Ignored::Options => tracing::warn!(
                "configuration.yaml: the '{key}' options are ignored: hars doesn't read them"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(text: &str) -> Yaml {
        let dir = std::env::temp_dir().join(format!(
            "hars-config-yaml-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("configuration.yaml");
        std::fs::write(&file, text).unwrap();
        let config = ha_core::yaml::load_file(&file, None).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        config
    }

    #[test]
    fn a_default_install_has_nothing_ignored() {
        // HA's own default configuration.yaml, with empty include files.
        let config =
            load("default_config:\nfrontend:\n  themes: {}\nautomation: []\nscript:\nscene:\n");
        assert_eq!(ignored(&config), vec![]);
    }

    #[test]
    fn read_keys_are_never_ignored() {
        let config = load(
            "recorder:\n  purge_keep_days: 3\nautomation:\n  - alias: a\nautomation old:\n  - alias: b\n",
        );
        assert_eq!(ignored(&config), vec![]);
    }

    #[test]
    fn unknown_domains_are_ignored_even_when_bare() {
        let config = load(
            "mqtt:\nsensor:\n  - platform: template\nlogger:\n  default: info\npersistent_notification:\n",
        );
        assert_eq!(
            ignored(&config),
            vec![
                ("mqtt", Ignored::Unsupported),
                ("sensor", Ignored::Unsupported),
                ("logger", Ignored::Unsupported),
                ("persistent_notification", Ignored::Unsupported),
            ]
        );
    }

    #[test]
    fn options_of_a_known_domain_are_ignored() {
        let config = load(
            "homeassistant:\n  name: Home\nfrontend:\n  themes:\n    dark: {}\nhttp:\n  cors_allowed_origins: [x]\nscript:\n  hi:\n    sequence: []\nlight 2:\n  - platform: group\nzone: []\n",
        );
        assert_eq!(
            ignored(&config),
            vec![
                ("homeassistant", Ignored::Options),
                ("frontend", Ignored::Options),
                ("http", Ignored::Options),
                ("script", Ignored::Options),
                ("light 2", Ignored::Unsupported),
            ]
        );
    }

    #[test]
    fn falsy_options_and_labelled_recorders_are_ignored() {
        let config = load(
            "http:\n  ip_ban_enabled: false\nhomeassistant:\n  allowlist_external_dirs: []\nrecorder archive:\n  purge_keep_days: 3\nrecorder other:\nfrontend:\n  themes: false\nzone:\n  themes: {}\n",
        );
        assert_eq!(
            ignored(&config),
            vec![
                ("http", Ignored::Options),
                ("homeassistant", Ignored::Options),
                ("recorder archive", Ignored::Options),
                ("frontend", Ignored::Options),
                ("zone", Ignored::Options),
            ]
        );
    }
}
