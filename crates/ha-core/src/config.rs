//! Location, units and locale: HA's `hass.config`, stored in
//! `.storage/core.config` (version 1.4, `core_config.py`).

use crate::storage::{Store, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::path::Path;

/// `.storage/core.config` data, with HA's defaults for a fresh install.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoreConfig {
    pub latitude: f64,
    pub longitude: f64,
    /// Meters, whatever the unit system.
    pub elevation: i64,
    /// `"metric"` or `"us_customary"`.
    pub unit_system_v2: String,
    pub location_name: String,
    pub time_zone: String,
    pub external_url: Option<String>,
    pub internal_url: Option<String>,
    pub currency: String,
    pub country: Option<String>,
    pub language: String,
    /// Home zone radius in meters.
    pub radius: i64,
    /// Fields `hars` doesn't use, written back unchanged.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            latitude: 0.0,
            longitude: 0.0,
            elevation: 0,
            unit_system_v2: "metric".into(),
            location_name: "Home".into(),
            time_zone: "UTC".into(),
            external_url: None,
            internal_url: None,
            currency: "EUR".into(),
            country: None,
            language: "en".into(),
            radius: 100,
            extra: Map::new(),
        }
    }
}

/// Where the config came from (`ConfigSource`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfigSource {
    #[default]
    Default,
    Storage,
}

impl ConfigSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Storage => "storage",
        }
    }
}

pub fn store(config_dir: &Path) -> Store {
    Store::new(config_dir, "core.config", 1, 4, true)
}

impl CoreConfig {
    /// Load `.storage/core.config`; HA's defaults if there is none.
    pub fn load(config_dir: &Path) -> Result<(Self, ConfigSource), StoreError> {
        Ok(match store(config_dir).load::<Self>()? {
            Some(c) => (c, ConfigSource::Storage),
            None => (Self::default(), ConfigSource::Default),
        })
    }

    /// `UnitSystem.as_dict()` for the configured system.
    pub fn unit_system(&self) -> Value {
        if self.unit_system_v2 == "us_customary" {
            json!({
                "length": "mi", "accumulated_precipitation": "in", "area": "ft²", "mass": "lb",
                "pressure": "psi", "temperature": "°F", "volume": "gal", "wind_speed": "mph",
            })
        } else {
            json!({
                "length": "km", "accumulated_precipitation": "mm", "area": "m²", "mass": "g",
                "pressure": "Pa", "temperature": "°C", "volume": "L", "wind_speed": "m/s",
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_has_file_and_defaults() {
        let dir = std::env::temp_dir().join(format!("hars-core-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (c, source) = CoreConfig::load(&dir).unwrap();
        assert_eq!(
            (c.clone(), source),
            (CoreConfig::default(), ConfigSource::Default)
        );
        assert_eq!(c.unit_system()["temperature"], "°C");

        std::fs::create_dir_all(dir.join(".storage")).unwrap();
        std::fs::write(
            dir.join(".storage/core.config"),
            json!({"version": 1, "minor_version": 4, "key": "core.config", "data": {
                "latitude": 52.37, "longitude": 4.89, "elevation": 3, "unit_system_v2": "us_customary",
                "location_name": "Home", "time_zone": "Europe/Amsterdam", "external_url": null,
                "internal_url": null, "currency": "EUR", "country": "NL", "language": "en", "radius": 100,
                "future": true}})
            .to_string(),
        )
        .unwrap();
        let (c, source) = CoreConfig::load(&dir).unwrap();
        assert_eq!(source, ConfigSource::Storage);
        assert_eq!(
            (c.latitude, c.time_zone.as_str()),
            (52.37, "Europe/Amsterdam")
        );
        assert_eq!(c.unit_system()["temperature"], "°F");
        // Unknown fields survive a save.
        store(&dir).save(&c).unwrap();
        let (again, _) = CoreConfig::load(&dir).unwrap();
        assert_eq!(again.extra["future"], true);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
