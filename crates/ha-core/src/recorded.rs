//! Which state attributes the recorder keeps.
//!
//! HA takes these from the entity class (`_entity_component_unrecorded_attributes`
//! and `_unrecorded_attributes`, `recorder/db_schema.py`'s
//! `shared_attrs_bytes_from_event`). hars has no entity classes, so the
//! component sets are a table by domain, and an integration declares an
//! entity's own set with [`crate::Hass::set_unrecorded`] (`sun.sun`).

use crate::EntityId;

/// Never recorded, in any domain (`recorder/const.py`'s
/// `ALL_DOMAIN_EXCLUDE_ATTRS`).
pub const ALWAYS_UNRECORDED: [&str; 3] = ["attribution", "restored", "supported_features"];

/// A domain's `_entity_component_unrecorded_attributes`, for the domains
/// hars has (HA 2026.6.1).
pub fn domain_unrecorded(domain: &str) -> &'static [&'static str] {
    match domain {
        // `light/__init__.py` LightEntity.
        "light" => &[
            "supported_color_modes",
            "effect_list",
            "min_color_temp_kelvin",
            "max_color_temp_kelvin",
            "brightness",
            "color_mode",
            "color_temp_kelvin",
            "effect",
            "hs_color",
            "rgb_color",
            "rgbw_color",
            "rgbww_color",
            "xy_color",
        ],
        // `climate/__init__.py` ClimateEntity.
        "climate" => &[
            "hvac_modes",
            "fan_modes",
            "swing_modes",
            "min_temp",
            "max_temp",
            "min_humidity",
            "max_humidity",
            "target_humidity_step",
            "target_temp_step",
            "preset_modes",
        ],
        // `sensor/__init__.py` SensorEntity.
        "sensor" => &["options"],
        // `vacuum/__init__.py` StateVacuumEntity.
        "vacuum" => &["fan_speed_list"],
        // `automation/__init__.py` BaseAutomationEntity.
        "automation" => &["last_triggered", "mode", "current", "max", "id"],
        // `person/__init__.py` Person.
        "person" => &["device_trackers"],
        _ => &[],
    }
}

/// The attributes one entity doesn't record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unrecorded {
    /// Its domain's set.
    pub domain: &'static [&'static str],
    /// Its own set, if its integration declared one.
    pub entity: &'static [&'static str],
}

impl Unrecorded {
    pub fn of(entity_id: &EntityId, entity: &'static [&'static str]) -> Self {
        Self {
            domain: domain_unrecorded(entity_id.domain()),
            entity,
        }
    }

    /// Whether `key` is left out of the recorded attributes.
    pub fn excludes(&self, key: &str) -> bool {
        ALWAYS_UNRECORDED.contains(&key) || self.domain.contains(&key) || self.entity.contains(&key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Hass;

    /// The sets HA's own entity classes declared in the vector run
    /// (`bus_log[*].unrecorded_attributes`, sorted).
    #[test]
    fn sets_match_ha() {
        let cases: [(&str, &[&str]); 6] = [
            (
                "automation.motion_light",
                &["current", "id", "last_triggered", "max", "mode"],
            ),
            (
                "climate.air_conditioner",
                &[
                    "fan_modes",
                    "hvac_modes",
                    "max_humidity",
                    "max_temp",
                    "min_humidity",
                    "min_temp",
                    "preset_modes",
                    "swing_modes",
                    "target_humidity_step",
                    "target_temp_step",
                ],
            ),
            (
                "light.hallway_hallway",
                &[
                    "brightness",
                    "color_mode",
                    "color_temp_kelvin",
                    "effect",
                    "effect_list",
                    "hs_color",
                    "max_color_temp_kelvin",
                    "min_color_temp_kelvin",
                    "rgb_color",
                    "rgbw_color",
                    "rgbww_color",
                    "supported_color_modes",
                    "xy_color",
                ],
            ),
            ("person.owner", &["device_trackers"]),
            ("binary_sensor.hall_motion", &[]),
            ("weather.forecast_home", &[]),
        ];
        let hass = Hass::new();
        for (id, want) in cases {
            let id = EntityId::parse(id).unwrap();
            let u = hass.unrecorded(&id);
            let mut got: Vec<&str> = u.domain.iter().chain(u.entity).copied().collect();
            got.sort_unstable();
            assert_eq!(got, want, "{id}");
        }
        let sensor = hass.unrecorded(&EntityId::parse("sensor.x").unwrap());
        assert!(sensor.excludes("options") && sensor.excludes("restored"));
        assert!(!sensor.excludes("unit_of_measurement"));
        let vacuum = hass.unrecorded(&EntityId::parse("vacuum.roborock").unwrap());
        assert!(vacuum.excludes("fan_speed_list"));
        assert!(!vacuum.excludes("fan_speed"));
    }

    #[test]
    fn entity_sets_add_to_the_domain_set() {
        let hass = Hass::new();
        let sun = EntityId::parse("sun.sun").unwrap();
        assert!(!hass.unrecorded(&sun).excludes("elevation"));
        hass.set_unrecorded(sun.clone(), &["elevation"]);
        let u = hass.unrecorded(&sun);
        assert!(u.excludes("elevation") && u.excludes("attribution"));
        assert!(!u.excludes("friendly_name"));
    }
}
