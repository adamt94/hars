//! Local wall time for time triggers and time conditions, in the
//! configured zone, as HA's `dt_util` and `async_track_time_change`.

use ha_core::Hass;
use jiff::civil::Time;
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

fn tz(hass: &Hass) -> TimeZone {
    TimeZone::get(&hass.config().time_zone).unwrap_or(TimeZone::UTC)
}

/// `dt_util.now()`.
pub fn local_now(hass: &Hass) -> Zoned {
    hass.clock().now().to_zoned(tz(hass))
}

/// `find_next_time_expression_time(now, [s], [m], [h])` then
/// `.replace(microsecond=micro)`: the first local `at` (at a whole
/// second) not before `from`'s second, skipping a day where it doesn't
/// exist (a DST gap); in a fold, each occurrence in turn.
pub fn next_local(hass: &Hass, from: Timestamp, at: Time, micro: i32) -> Option<Timestamp> {
    let tz = tz(hass);
    // Matched at whole seconds: the current second still counts (it then
    // fires at once if the offset has passed).
    let floor = Timestamp::from_second(from.as_second()).ok()?;
    let mut date = from.to_zoned(tz.clone()).date();
    for _ in 0..3 {
        // A gap skips the day; a fold has two occurrences.
        let amb = tz.to_ambiguous_timestamp(date.to_datetime(at));
        let candidates: Vec<Timestamp> = match amb.offset() {
            jiff::tz::AmbiguousOffset::Gap { .. } => vec![],
            jiff::tz::AmbiguousOffset::Fold { .. } => [amb.earlier().ok(), amb.later().ok()]
                .into_iter()
                .flatten()
                .collect(),
            jiff::tz::AmbiguousOffset::Unambiguous { .. } => {
                amb.earlier().ok().into_iter().collect()
            }
        };
        if let Some(ts) = candidates.into_iter().find(|ts| *ts >= floor) {
            return ts
                .checked_add(jiff::SignedDuration::from_micros(micro.into()))
                .ok();
        }
        date = date.tomorrow().ok()?;
    }
    None
}

/// Sleep until the wall clock reads `at`: on the monotonic clock, checking
/// the wall again on each wake (a clock stepped back sleeps on; one stepped
/// forward wakes at the old deadline, late, as HA's timers do).
pub async fn sleep_until_wall(hass: &Hass, at: Timestamp) {
    let clock = hass.clock().clone();
    loop {
        let wait = at.duration_since(clock.now());
        if wait <= jiff::SignedDuration::ZERO {
            return;
        }
        let wait = std::time::Duration::try_from(wait).unwrap_or_default();
        tokio::time::sleep_until(clock.instant() + wait).await;
    }
}
