//! A port of the parts of `astral` 2.2 (the version HA 2026.6.1 pins) that
//! the sun integration uses: NOAA solar position and event times. Kept line
//! for line with `astral/sun.py` so the results match HA's.

use jiff::civil::Date;
use jiff::{SignedDuration, Timestamp};

const SUN_APPARENT_RADIUS: f64 = 32.0 / (60.0 * 2.0);

/// Where the sun is observed from. `elevation` is metres above the horizon.
#[derive(Debug, Clone, Copy)]
pub struct Observer {
    pub latitude: f64,
    pub longitude: f64,
    pub elevation: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Rising,
    Setting,
}

fn julianday(date: Date) -> f64 {
    let (mut y, mut m) = (date.year() as f64, date.month() as f64);
    let d = date.day() as f64;
    if m <= 2.0 {
        y -= 1.0;
        m += 12.0;
    }
    let a = (y / 100.0).floor();
    let b = 2.0 - a + (a / 4.0).floor();
    (365.25 * (y + 4716.0)).floor() + (30.6001 * (m + 1.0)).floor() + d + b - 1524.5
}

/// `minutes_to_timedelta`, with its truncations.
fn minutes_to_duration(minutes: f64) -> SignedDuration {
    let d = (minutes / 1440.0) as i64;
    let minutes = (minutes - d as f64 * 1440.0) * 60.0;
    let s = minutes as i64;
    let us = ((minutes - s as f64) * 1_000_000.0) as i64;
    SignedDuration::from_secs(d * 86400 + s) + SignedDuration::from_micros(us)
}

fn jday_to_jcentury(jd: f64) -> f64 {
    (jd - 2451545.0) / 36525.0
}

fn jcentury_to_jday(jc: f64) -> f64 {
    jc * 36525.0 + 2451545.0
}

fn geom_mean_long_sun(t: f64) -> f64 {
    let l0 = 280.46646 + t * (36000.76983 + 0.0003032 * t);
    // Python's `%` takes the sign of the divisor.
    l0.rem_euclid(360.0)
}

fn geom_mean_anomaly_sun(t: f64) -> f64 {
    357.52911 + t * (35999.05029 - 0.0001537 * t)
}

fn eccentric_location_earth_orbit(t: f64) -> f64 {
    0.016708634 - t * (4.2037e-05 + 1.267e-07 * t)
}

fn sun_eq_of_center(t: f64) -> f64 {
    let mrad = geom_mean_anomaly_sun(t).to_radians();
    let sinm = mrad.sin();
    let sin2m = (mrad + mrad).sin();
    let sin3m = (mrad + mrad + mrad).sin();
    sinm * (1.914602 - t * (0.004817 + 1.4e-05 * t))
        + sin2m * (0.019993 - 0.000101 * t)
        + sin3m * 0.000289
}

fn sun_true_long(t: f64) -> f64 {
    geom_mean_long_sun(t) + sun_eq_of_center(t)
}

fn sun_apparent_long(t: f64) -> f64 {
    let omega = 125.04 - 1934.136 * t;
    sun_true_long(t) - 0.00569 - 0.00478 * omega.to_radians().sin()
}

fn mean_obliquity_of_ecliptic(t: f64) -> f64 {
    let seconds = 21.448 - t * (46.815 + t * (0.00059 - t * 0.001813));
    23.0 + (26.0 + seconds / 60.0) / 60.0
}

fn obliquity_correction(t: f64) -> f64 {
    let omega = 125.04 - 1934.136 * t;
    mean_obliquity_of_ecliptic(t) + 0.00256 * omega.to_radians().cos()
}

fn sun_declination(t: f64) -> f64 {
    let e = obliquity_correction(t);
    let lambd = sun_apparent_long(t);
    (e.to_radians().sin() * lambd.to_radians().sin())
        .asin()
        .to_degrees()
}

fn var_y(t: f64) -> f64 {
    let y = (obliquity_correction(t).to_radians() / 2.0).tan();
    y * y
}

fn eq_of_time(t: f64) -> f64 {
    let l0 = geom_mean_long_sun(t);
    let e = eccentric_location_earth_orbit(t);
    let m = geom_mean_anomaly_sun(t);
    let y = var_y(t);
    let sin2l0 = (2.0 * l0.to_radians()).sin();
    let sinm = m.to_radians().sin();
    let cos2l0 = (2.0 * l0.to_radians()).cos();
    let sin4l0 = (4.0 * l0.to_radians()).sin();
    let sin2m = (2.0 * m.to_radians()).sin();
    let etime = y * sin2l0 - 2.0 * e * sinm + 4.0 * e * y * sinm * cos2l0
        - 0.5 * y * y * sin4l0
        - 1.25 * e * e * sin2m;
    etime.to_degrees() * 4.0
}

/// `None` where Python's `acos` raises a math domain error: the sun never
/// reaches that zenith that day.
fn hour_angle(latitude: f64, declination: f64, zenith: f64, direction: Direction) -> Option<f64> {
    let (lat, dec, zen) = (
        latitude.to_radians(),
        declination.to_radians(),
        zenith.to_radians(),
    );
    let h = (zen.cos() - lat.sin() * dec.sin()) / (lat.cos() * dec.cos());
    if !(-1.0..=1.0).contains(&h) {
        return None;
    }
    let ha = h.acos();
    Some(if direction == Direction::Setting {
        -ha
    } else {
        ha
    })
}

fn adjust_to_horizon(elevation: f64) -> f64 {
    if elevation <= 0.0 {
        return 0.0;
    }
    let r = 6356900.0;
    (r / (r + elevation)).acos().to_degrees()
}

fn refraction_at_zenith(zenith: f64) -> f64 {
    let elevation = 90.0 - zenith;
    if elevation >= 85.0 {
        return 0.0;
    }
    let te = elevation.to_radians().tan();
    let correction = if elevation > 5.0 {
        58.1 / te - 0.07 / (te * te * te) + 8.6e-05 / (te * te * te * te * te)
    } else if elevation > -0.575 {
        let step1 = -12.79 + elevation * 0.711;
        let step2 = 103.4 + elevation * step1;
        let step3 = -518.2 + elevation * step2;
        1735.0 + elevation * step3
    } else {
        -20.774 / te
    };
    correction / 3600.0
}

fn midnight_utc(date: Date) -> Timestamp {
    date.to_zoned(jiff::tz::TimeZone::UTC)
        .expect("valid date")
        .timestamp()
}

fn time_of_transit(
    o: &Observer,
    date: Date,
    zenith: f64,
    direction: Direction,
) -> Option<Timestamp> {
    let latitude = o.latitude.clamp(-89.8, 89.8);
    let adjustment_for_elevation = if o.elevation > 0.0 {
        adjust_to_horizon(o.elevation)
    } else {
        0.0
    };
    let adjustment_for_refraction = refraction_at_zenith(zenith + adjustment_for_elevation);

    let mut t = jday_to_jcentury(julianday(date));
    let dec = sun_declination(t);
    let ha = hour_angle(
        latitude,
        dec,
        zenith + adjustment_for_elevation - adjustment_for_refraction,
        direction,
    )?;
    let delta = -o.longitude - ha.to_degrees();
    let time_utc = 720.0 + 4.0 * delta - eq_of_time(t);

    t = jday_to_jcentury(jcentury_to_jday(t) + time_utc / 1440.0);
    let dec = sun_declination(t);
    let ha = hour_angle(
        latitude,
        dec,
        zenith + adjustment_for_elevation + adjustment_for_refraction,
        direction,
    )?;
    let delta = -o.longitude - ha.to_degrees();
    let time_utc = 720.0 + 4.0 * delta - eq_of_time(t);
    midnight_utc(date)
        .checked_add(minutes_to_duration(time_utc))
        .ok()
}

/// `int()` of the hours, minutes and seconds, with astral's carries.
fn hms(date: Date, time_utc_hours: f64, may_go_forward: bool) -> Timestamp {
    let mut date = date;
    let mut hour = time_utc_hours as i64;
    let mut minute = ((time_utc_hours - hour as f64) * 60.0) as i64;
    let mut second = (((time_utc_hours - hour as f64) * 60.0 - minute as f64) * 60.0) as i64;
    if second > 59 {
        second -= 60;
        minute += 1;
    } else if second < 0 {
        second += 60;
        minute -= 1;
    }
    if minute > 59 {
        minute -= 60;
        hour += 1;
    } else if minute < 0 {
        minute += 60;
        hour -= 1;
    }
    if may_go_forward && hour > 23 {
        hour -= 24;
        date = date.tomorrow().expect("in range");
    } else if hour < 0 {
        hour += 24;
        date = date.yesterday().expect("in range");
    }
    midnight_utc(date) + SignedDuration::from_secs(hour * 3600 + minute * 60 + second)
}

/// Solar noon (UTC) on `date`.
pub fn noon(o: &Observer, date: Date) -> Timestamp {
    let jc = jday_to_jcentury(julianday(date));
    let time_utc = (720.0 - 4.0 * o.longitude - eq_of_time(jc)) / 60.0;
    hms(date, time_utc, true)
}

/// Solar midnight (UTC) for `date`.
pub fn midnight(o: &Observer, date: Date) -> Timestamp {
    let jd = julianday(date);
    let newt = jday_to_jcentury(jd + 0.5 + -o.longitude / 360.0);
    let time_utc = (-o.longitude * 4.0 - eq_of_time(newt)) / 60.0;
    hms(date, time_utc, false)
}

/// Dawn: the sun `depression` degrees below the horizon, rising.
pub fn dawn(o: &Observer, date: Date, depression: f64) -> Option<Timestamp> {
    time_of_transit(o, date, 90.0 + depression, Direction::Rising)
}

pub fn dusk(o: &Observer, date: Date, depression: f64) -> Option<Timestamp> {
    time_of_transit(o, date, 90.0 + depression, Direction::Setting)
}

pub fn sunrise(o: &Observer, date: Date) -> Option<Timestamp> {
    time_of_transit(o, date, 90.0 + SUN_APPARENT_RADIUS, Direction::Rising)
}

pub fn sunset(o: &Observer, date: Date) -> Option<Timestamp> {
    time_of_transit(o, date, 90.0 + SUN_APPARENT_RADIUS, Direction::Setting)
}

/// `zenith_and_azimuth` for a UTC time, with refraction.
fn zenith_and_azimuth(o: &Observer, at: Timestamp) -> (f64, f64) {
    let latitude = o.latitude.clamp(-89.8, 89.8);
    let utc = at.to_zoned(jiff::tz::TimeZone::UTC);
    let (h, m, s) = (utc.hour() as f64, utc.minute() as f64, utc.second() as f64);
    // Whole seconds only, as astral reads the datetime's fields.
    let timenow = h + m / 60.0 + s / 3600.0;
    let t = jday_to_jcentury(julianday(utc.date()) + timenow / 24.0);
    let dec = sun_declination(t);
    let solar_time_fix = eq_of_time(t) - 4.0 * -o.longitude;
    let mut true_solar_time = h * 60.0 + m + s / 60.0 + solar_time_fix;
    while true_solar_time > 1440.0 {
        true_solar_time -= 1440.0;
    }
    let mut hourangle = true_solar_time / 4.0 - 180.0;
    if hourangle < -180.0 {
        hourangle += 360.0;
    }
    let harad = hourangle.to_radians();
    let csz = (latitude.to_radians().sin() * dec.to_radians().sin()
        + latitude.to_radians().cos() * dec.to_radians().cos() * harad.cos())
    .clamp(-1.0, 1.0);
    let zenith = csz.acos().to_degrees();
    let az_denom = latitude.to_radians().cos() * zenith.to_radians().sin();
    let mut azimuth = if az_denom.abs() > 0.001 {
        let az_rad = ((latitude.to_radians().sin() * zenith.to_radians().cos()
            - dec.to_radians().sin())
            / az_denom)
            .clamp(-1.0, 1.0);
        let a = 180.0 - az_rad.acos().to_degrees();
        if hourangle > 0.0 { -a } else { a }
    } else if latitude > 0.0 {
        180.0
    } else {
        0.0
    };
    if azimuth < 0.0 {
        azimuth += 360.0;
    }
    (zenith - refraction_at_zenith(zenith), azimuth)
}

/// Degrees above the horizon, with refraction.
pub fn elevation(o: &Observer, at: Timestamp) -> f64 {
    90.0 - zenith_and_azimuth(o, at).0
}

pub fn azimuth(o: &Observer, at: Timestamp) -> f64 {
    zenith_and_azimuth(o, at).1
}
