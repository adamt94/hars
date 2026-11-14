//! Light profiles (`light_profiles.csv`): HA's built-in four, then the
//! config directory's file, which can also set `<entity_id>.default` and
//! `group.all_lights.default`.

use super::Params;
use std::path::Path;

/// One profile: the colour (as hs, from its xy), brightness, transition.
#[derive(Debug, Clone, PartialEq)]
struct Profile {
    hs: Option<(f64, f64)>,
    brightness: Option<i64>,
    transition: Option<f64>,
}

/// HA's own `components/light/light_profiles.csv`.
const BUILT_IN: &str = "id,x,y,brightness
relax,0.5119,0.4147,144
concentrate,0.5119,0.4147,219
energize,0.368,0.3686,203
reading,0.4448,0.4066,240
";

#[derive(Debug, Clone, PartialEq)]
pub struct Profiles(Vec<(String, Profile)>);

impl Default for Profiles {
    fn default() -> Self {
        let mut p = Self(Vec::new());
        p.read("built-in", BUILT_IN);
        p
    }
}

impl Profiles {
    /// The built-in profiles, then `<config>/light_profiles.csv` if there is
    /// one (blocking; at start-up).
    pub fn load(config_dir: &Path) -> Self {
        let mut p = Self::default();
        let path = config_dir.join("light_profiles.csv");
        match std::fs::read_to_string(&path) {
            Ok(text) => p.read(&path.display().to_string(), &text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::error!("reading {}: {e}", path.display()),
        }
        p
    }

    /// Rows after the header. As in HA, an invalid row is logged and ends
    /// that file: the rows after it are not read.
    pub(super) fn read(&mut self, source: &str, text: &str) {
        for line in text.lines().skip(1) {
            if line.is_empty() {
                continue;
            }
            match parse_row(line) {
                Some((name, profile)) => match self.0.iter_mut().find(|(n, _)| *n == name) {
                    Some((_, p)) => *p = profile,
                    None => self.0.push((name, profile)),
                },
                None => {
                    tracing::error!("Error parsing light profile row '{line}' from {source}");
                    return;
                }
            }
        }
    }

    fn get(&self, name: &str) -> Option<&Profile> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, p)| p)
    }

    /// `apply_profile`: its colour unless one was given, and its brightness
    /// and transition unless given.
    pub fn apply(&self, name: &str, p: &mut Params) {
        let Some(profile) = self.get(name) else {
            return;
        };
        if let Some(hs) = profile.hs
            && !p.has_color()
        {
            p.hs_color = Some(hs);
        }
        if p.brightness.is_none() {
            p.brightness = profile.brightness;
        }
        if p.transition.is_none() {
            p.transition = profile.transition;
        }
    }

    /// `apply_default`: `<entity_id>.default`, then
    /// `group.all_lights.default`.
    pub fn apply_default(&self, entity_id: &str, on: bool, p: &mut Params) {
        for id in [entity_id, "group.all_lights"] {
            let name = format!("{id}.default");
            let Some(profile) = self.get(&name) else {
                continue;
            };
            if !on || p.is_empty() {
                self.apply(&name, p);
            } else if p.transition.is_none() {
                p.transition = profile.transition;
            }
        }
    }
}

/// `Profile.SCHEMA`: `name, x, y, brightness[, transition]`; an empty field
/// is `None`.
fn parse_row(line: &str) -> Option<(String, Profile)> {
    let cells: Vec<&str> = line.split(',').collect();
    if !(4..=5).contains(&cells.len()) {
        return None;
    }
    fn opt(s: &str) -> Option<Option<&str>> {
        Some(if s.is_empty() { None } else { Some(s) })
    }
    let small_float = |s: &str| -> Option<Option<f64>> {
        match opt(s)? {
            None => Some(None),
            Some(v) => v
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|f| (0.0..=1.0).contains(f))
                .map(Some),
        }
    };
    let x = small_float(cells[1])?;
    let y = small_float(cells[2])?;
    let brightness = match opt(cells[3])? {
        None => None,
        Some(v) => Some(
            v.trim()
                .parse::<i64>()
                .ok()
                .filter(|b| (0..=255).contains(b))?,
        ),
    };
    let transition = match cells.get(4).copied() {
        None | Some("") => None,
        Some(v) => Some(v.trim().parse::<f64>().ok()?.clamp(0.0, 6553.0)),
    };
    let hs = match (x, y) {
        (Some(x), Some(y)) => Some(super::color::xy_to_hs(x, y)),
        _ => None,
    };
    Some((
        cells[0].to_owned(),
        Profile {
            hs,
            brightness,
            transition,
        },
    ))
}
