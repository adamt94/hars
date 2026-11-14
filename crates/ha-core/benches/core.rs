//! Hot-path benchmarks: state writes and `state_changed` fan-out.
//!
//! Run with `cargo bench -p ha-core`; record notable results in docs/PERF.md.

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use ha_core::{Attributes, EntityId, Hass};
use serde_json::json;
use std::hint::black_box;

/// A realistic light: the attributes a Hue colour-temperature bulb reports.
fn light_attributes(brightness: u64) -> Attributes {
    json!({
        "brightness": brightness,
        "color_mode": "color_temp",
        "color_temp_kelvin": 2700,
        "min_color_temp_kelvin": 2202,
        "max_color_temp_kelvin": 6535,
        "supported_color_modes": ["color_temp", "xy"],
        "friendly_name": "Kitchen ceiling",
        "supported_features": 40,
    })
    .as_object()
    .unwrap()
    .clone()
}

fn state_write(c: &mut Criterion) {
    let mut g = c.benchmark_group("state_write");
    let hass = Hass::new();
    let id = EntityId::parse("light.kitchen_ceiling").unwrap();

    let mut on = false;
    g.bench_function("changed/no_attributes", |b| {
        b.iter(|| {
            on = !on;
            drop(black_box(hass.set_state(
                id.clone(),
                if on { "on" } else { "off" },
                Attributes::new(),
            )))
        })
    });
    hass.set_state(id.clone(), "on", Attributes::new());
    g.bench_function("unchanged/no_attributes", |b| {
        b.iter(|| black_box(hass.set_state(id.clone(), "on", Attributes::new())))
    });

    // Building the attribute map is the caller's cost, so it is kept out.
    let mut brightness = 0;
    g.bench_function("changed/light_attributes", |b| {
        b.iter_batched(
            || {
                brightness = (brightness + 1) % 255;
                light_attributes(brightness)
            },
            // Drop the result inside the timing: for a changed write that
            // frees the replaced state, which would otherwise be deferred.
            |attrs| drop(black_box(hass.set_state(id.clone(), "on", attrs))),
            BatchSize::SmallInput,
        )
    });
    hass.set_state(id.clone(), "on", light_attributes(1));
    g.bench_function("unchanged/light_attributes", |b| {
        b.iter_batched(
            || light_attributes(1),
            // Drop the result inside the timing: for a changed write that
            // frees the replaced state, which would otherwise be deferred.
            |attrs| drop(black_box(hass.set_state(id.clone(), "on", attrs))),
            BatchSize::SmallInput,
        )
    });
    g.finish();
}

/// One changed write delivered to `n` lossless feeds (each drained, so the
/// queue cost is included) plus one bus subscriber.
fn fan_out(c: &mut Criterion) {
    let mut g = c.benchmark_group("fan_out");
    for n in [0usize, 1, 4, 16] {
        let hass = Hass::new();
        let mut bus = hass.bus().subscribe();
        let mut feeds: Vec<_> = (0..n).map(|_| hass.subscribe_state_changes()).collect();
        let id = EntityId::parse("sensor.power").unwrap();
        let mut v = 0u32;
        g.bench_with_input(BenchmarkId::new("feeds", n), &n, |b, _| {
            b.iter(|| {
                v = v.wrapping_add(1);
                hass.set_state(id.clone(), v.to_string(), Attributes::new());
                for feed in &mut feeds {
                    black_box(feed.try_recv());
                }
                black_box(bus.try_recv().ok());
            })
        });
    }
    g.finish();
}

/// `subscribe_entities` filters run once per state change per subscription.
fn entity_filter(c: &mut Criterion) {
    use ha_core::entityfilter::{EntityFilter, FilterLists, Glob};
    let mut g = c.benchmark_group("entity_filter");
    let glob = Glob::new("sensor.phone_*_level");
    g.bench_function("glob/match", |b| {
        b.iter(|| black_box(glob.matches(black_box("sensor.phone_battery_level"))))
    });
    let lists = |domains: &[&str], globs: &[&str], entities: &[&str]| FilterLists {
        domains: domains.iter().map(|s| s.to_string()).collect(),
        entities: entities.iter().map(|s| s.to_string()).collect(),
        entity_globs: globs.iter().map(|s| s.to_string()).collect(),
    };
    // The iOS app's filter.
    let app = EntityFilter::new(lists(&["zone", "person"], &[], &[]), lists(&[], &[], &[]));
    g.bench_function("app/reject", |b| {
        b.iter(|| black_box(app.matches(black_box("sensor.phone_battery_level"))))
    });
    // Case 4 with globs on both sides.
    let mixed = EntityFilter::new(
        lists(&["light"], &["sensor.phone_*"], &["switch.a"]),
        lists(&[], &["*_battery_*"], &["light.b"]),
    );
    g.bench_function("mixed/glob_path", |b| {
        b.iter(|| black_box(mixed.matches(black_box("sensor.phone_battery_level"))))
    });
    g.finish();
}

criterion_group!(benches, state_write, fan_out, entity_filter);
criterion_main!(benches);
