//! `util/location.py`'s `distance` and `zone.async_in_zones`.

use ha_core::Hass;
use serde_json::Value;
use std::sync::Arc;

const AXIS_A: f64 = 6_378_137.0;
const FLATTENING: f64 = 1.0 / 298.257_223_563;
const AXIS_B: f64 = 6_356_752.314_245;
const MAX_ITERATIONS: usize = 200;
const CONVERGENCE_THRESHOLD: f64 = 1e-12;

/// HA's `distance`: Vincenty's inverse formula on WGS-84, in metres, from a
/// result rounded to 6 decimal places of a kilometre. `None` when it
/// doesn't converge (nearly antipodal points).
pub fn distance(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> Option<f64> {
    vincenty((lat1, lon1), (lat2, lon2)).map(|km| km * 1000.0)
}

fn vincenty(p1: (f64, f64), p2: (f64, f64)) -> Option<f64> {
    if p1.0 == p2.0 && p1.1 == p2.1 {
        return Some(0.0);
    }
    let u1 = ((1.0 - FLATTENING) * p1.0.to_radians().tan()).atan();
    let u2 = ((1.0 - FLATTENING) * p2.0.to_radians().tan()).atan();
    let l = (p2.1 - p1.1).to_radians();
    let mut lambda = l;
    let (sin_u1, cos_u1) = (u1.sin(), u1.cos());
    let (sin_u2, cos_u2) = (u2.sin(), u2.cos());
    let mut converged = None;
    for _ in 0..MAX_ITERATIONS {
        let (sin_lambda, cos_lambda) = (lambda.sin(), lambda.cos());
        let sin_sigma = ((cos_u2 * sin_lambda).powi(2)
            + (cos_u1 * sin_u2 - sin_u1 * cos_u2 * cos_lambda).powi(2))
        .sqrt();
        if sin_sigma == 0.0 {
            return Some(0.0);
        }
        let cos_sigma = sin_u1 * sin_u2 + cos_u1 * cos_u2 * cos_lambda;
        let sigma = sin_sigma.atan2(cos_sigma);
        let sin_alpha = cos_u1 * cos_u2 * sin_lambda / sin_sigma;
        let cos_sq_alpha = 1.0 - sin_alpha.powi(2);
        // Python's ZeroDivisionError branch.
        let cos2_sigma_m = if cos_sq_alpha == 0.0 {
            0.0
        } else {
            cos_sigma - 2.0 * sin_u1 * sin_u2 / cos_sq_alpha
        };
        let c = FLATTENING / 16.0 * cos_sq_alpha * (4.0 + FLATTENING * (4.0 - 3.0 * cos_sq_alpha));
        let prev = lambda;
        lambda = l
            + (1.0 - c)
                * FLATTENING
                * sin_alpha
                * (sigma
                    + c * sin_sigma
                        * (cos2_sigma_m + c * cos_sigma * (-1.0 + 2.0 * cos2_sigma_m.powi(2))));
        if (lambda - prev).abs() < CONVERGENCE_THRESHOLD {
            converged = Some((cos_sq_alpha, sin_sigma, cos_sigma, sigma, cos2_sigma_m));
            break;
        }
    }
    let (cos_sq_alpha, sin_sigma, cos_sigma, sigma, cos2_sigma_m) = converged?;
    let u_sq = cos_sq_alpha * (AXIS_A.powi(2) - AXIS_B.powi(2)) / AXIS_B.powi(2);
    let a = 1.0 + u_sq / 16384.0 * (4096.0 + u_sq * (-768.0 + u_sq * (320.0 - 175.0 * u_sq)));
    let b = u_sq / 1024.0 * (256.0 + u_sq * (-128.0 + u_sq * (74.0 - 47.0 * u_sq)));
    let delta_sigma = b
        * sin_sigma
        * (cos2_sigma_m
            + b / 4.0
                * (cos_sigma * (-1.0 + 2.0 * cos2_sigma_m.powi(2))
                    - b / 6.0
                        * cos2_sigma_m
                        * (-3.0 + 4.0 * sin_sigma.powi(2))
                        * (-3.0 + 4.0 * cos2_sigma_m.powi(2))));
    let s = AXIS_B * a * (sigma - delta_sigma) / 1000.0;
    Some(round6(s))
}

/// Python's `round(x, 6)`: correctly rounded, as formatting is.
fn round6(x: f64) -> f64 {
    format!("{x:.6}").parse().unwrap_or(x)
}

/// A zone's numbers, from its state's attributes.
fn zone_numbers(attrs: &serde_json::Map<String, Value>) -> Option<(f64, f64, f64)> {
    Some((
        attrs.get("latitude")?.as_f64()?,
        attrs.get("longitude")?.as_f64()?,
        attrs.get("radius")?.as_f64()?,
    ))
}

/// `async_in_zones`: the closest active (non-passive) zone containing the
/// point, and every zone containing it, by distance then radius. A point
/// is inside when `distance - radius < accuracy` (`radius` here).
pub fn in_zones(
    hass: &Hass,
    latitude: f64,
    longitude: f64,
    radius: f64,
) -> (Option<Arc<ha_core::State>>, Vec<String>) {
    let mut zones: Vec<Arc<ha_core::State>> = hass
        .states()
        .all()
        .into_iter()
        .filter(|s| s.entity_id.domain() == "zone" && s.state != "unavailable")
        .collect();
    zones.sort_by(|a, b| a.entity_id.as_str().cmp(b.entity_id.as_str()));
    let mut min_dist = f64::MAX;
    let mut closest: Option<(Arc<ha_core::State>, f64)> = None;
    let mut inside: Vec<(String, f64, f64)> = Vec::new();
    for zone in zones {
        let Some((lat, lon, zone_radius)) = zone_numbers(&zone.attributes) else {
            continue;
        };
        let Some(dist) = distance(latitude, longitude, lat, lon) else {
            continue;
        };
        // `not (dist - radius < accuracy)`: NaN is outside too.
        if (dist - zone_radius).partial_cmp(&radius) != Some(std::cmp::Ordering::Less) {
            continue;
        }
        inside.push((zone.entity_id.to_string(), dist, zone_radius));
        if zone
            .attributes
            .get("passive")
            .is_some_and(|p| p.as_bool().unwrap_or(false))
        {
            continue;
        }
        if let Some((_, closest_radius)) = &closest
            && !(dist < min_dist || (dist == min_dist && zone_radius < *closest_radius))
        {
            continue;
        }
        min_dist = dist;
        closest = Some((zone, zone_radius));
    }
    inside.sort_by(|a, b| {
        (a.1, a.2)
            .partial_cmp(&(b.1, b.2))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    (
        closest.map(|(z, _)| z),
        inside.into_iter().map(|(id, _, _)| id).collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// HA 2026.6.1's `distance` and inside test (`vectors/zone_math.json`).
    #[test]
    fn distances_as_ha() {
        let v: Value = serde_json::from_str(include_str!("vectors/zone_math.json")).unwrap();
        let cases = v["distance"].as_array().unwrap();
        assert!(cases.len() >= 50);
        for c in cases {
            let p = |k: &str, i: usize| c[k][i].as_f64().unwrap();
            let got = distance(p("p1", 0), p("p1", 1), p("p2", 0), p("p2", 1));
            // `vincenty_km` exactly; the file's `meters` went through a
            // shorter float repr in places (HA itself gives
            // 112.14399999999999 for the Sydney pair, as here).
            let km = c["vincenty_km"].as_f64();
            assert_eq!(got, km.map(|k| k * 1000.0), "{c}");
            if let (Some(g), Some(m)) = (got, c["meters"].as_f64()) {
                assert!((g - m).abs() < 1e-9, "{c}");
            }
        }
        for c in v["inside_test"].as_array().unwrap() {
            let f = |k: &str| c[k].as_f64().unwrap();
            let inside = f("zone_dist") - f("radius") < f("accuracy");
            assert_eq!(inside, c["inside"], "{c}");
        }
    }

    fn zone(hass: &Hass, id: &str, lat: f64, lon: f64, radius: f64, passive: bool) {
        hass.set_state(
            ha_core::EntityId::parse(id).unwrap(),
            "0",
            json!({"latitude": lat, "longitude": lon, "radius": radius, "passive": passive})
                .as_object()
                .unwrap()
                .clone(),
        );
    }

    #[test]
    fn closest_active_zone_and_the_rest_by_distance() {
        let hass = Hass::new();
        zone(&hass, "zone.home", 0.5, 0.5, 100.0, false);
        zone(&hass, "zone.big", 0.5, 0.5, 1000.0, false);
        zone(&hass, "zone.near", 0.5003, 0.5, 100.0, true);
        zone(&hass, "zone.far", 1.5, 0.5, 100.0, false);
        let (closest, all) = in_zones(&hass, 0.5002, 0.5, 0.0);
        // Equal distance: the smaller zone wins; passive zones never do.
        assert_eq!(closest.unwrap().entity_id.as_str(), "zone.home");
        assert_eq!(all, ["zone.near", "zone.home", "zone.big"]);
        let (closest, all) = in_zones(&hass, 2.5, 0.5, 0.0);
        assert!(closest.is_none() && all.is_empty());
    }
}
