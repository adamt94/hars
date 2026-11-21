//! What a Hue entry's set-up does about the registries at every start of
//! `hars`: it goes over the bridge's devices and entities and makes or
//! updates their rows. On an install that has them (any start but the
//! first) every row already matches.
//!
//! The bridge here has the recorded resources eight times over (about 100
//! devices and 400 entities), in a registry that also holds 300 devices
//! and 3,000 entities of other integrations.
//!
//! Run with `cargo bench -p ha-integrations --bench hue_register`; compare
//! with main as docs/PERF.md says.

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use ha_core::Hass;
use ha_integrations::hue::register_rows;
use ha_registry::{Device, Entity, Registries, Registry};
use serde_json::{Value, json};
use std::hint::black_box;
use std::sync::Arc;

const RESOURCES: &str = include_str!("../src/hue/vectors/imported/resource.json");
const COPIES: usize = 8;

/// The recorded bridge, with every device, room, scene and service of it
/// `COPIES` times under ids, names and addresses of their own.
fn resources() -> Vec<Value> {
    let recorded: Value = serde_json::from_str(RESOURCES).unwrap();
    let recorded = recorded["data"].as_array().unwrap();
    // One bridge, and one home it is the root of.
    let single = |r: &Value| matches!(r["type"].as_str(), Some("bridge" | "bridge_home"));
    let ids: Vec<&str> = recorded
        .iter()
        .filter(|r| !single(r))
        .filter_map(|r| r["id"].as_str())
        .collect();
    let mut all = recorded.clone();
    for copy in 1..COPIES {
        for r in recorded.iter().filter(|r| !single(r)) {
            // The bridge's own device stays the only one.
            if r["services"]
                .as_array()
                .is_some_and(|s| s.iter().any(|s| s["rtype"] == "bridge"))
            {
                continue;
            }
            let mut text = r.to_string();
            for id in &ids {
                if text.contains(id) {
                    text = text.replace(id, &format!("{copy:08x}{}", &id[8..]));
                }
            }
            text = text.replace("00:17:88:01:", &format!("00:17:88:{:02x}:", copy + 1));
            let mut r: Value = serde_json::from_str(&text).unwrap();
            if let Some(name) = r["metadata"]["name"].as_str() {
                r["metadata"]["name"] = json!(format!("{name} {copy}"));
            }
            all.push(r);
        }
    }
    all
}

/// The rows of other integrations.
fn others() -> Registries {
    let devices = (0..300)
        .map(|i| {
            Arc::new(Device {
                id: format!("{i:032x}"),
                config_entries: vec!["other".into()],
                identifiers: vec![json!(["other", format!("device-{i}")])],
                name: Some(format!("Device {i}")),
                ..Default::default()
            })
        })
        .collect();
    let entities = (0..3000)
        .map(|i| {
            Arc::new(Entity {
                entity_id: format!("sensor.other_{i}"),
                id: Some(format!("{:032x}", 1_000_000 + i)),
                platform: "other".into(),
                unique_id: Some(json!(format!("other-{i}"))),
                config_entry_id: Some("other".into()),
                device_id: Some(format!("{:032x}", i % 300)),
                ..Default::default()
            })
        })
        .collect();
    Registries {
        devices,
        entities,
        ..Default::default()
    }
}

fn register(c: &mut Criterion) {
    let hass = Hass::new();
    let resources = resources();
    let registry = Registry::in_memory(&hass, others());
    register_rows(&hass, &registry, "hue", resources.clone());
    let made = registry.snapshot();
    let (devices, entities) = (made.devices.len() - 300, made.entities.len() - 3000);
    println!("hue rows: {devices} devices, {entities} entities");
    assert!(devices > 80 && entities > 300);
    // Any start but the first: every row is there and matches.
    c.bench_function("hue_register/rows_that_match", |b| {
        b.iter_batched(
            || resources.clone(),
            |resources| register_rows(&hass, black_box(&registry), "hue", resources),
            BatchSize::LargeInput,
        )
    });
    // Nothing was copied or swapped in for it.
    assert!(Arc::ptr_eq(&made, &registry.snapshot()));
    // The first start: every row is made.
    c.bench_function("hue_register/rows_made", |b| {
        b.iter_batched(
            || (Registry::in_memory(&hass, others()), resources.clone()),
            |(registry, resources)| register_rows(&hass, &registry, "hue", resources),
            BatchSize::LargeInput,
        )
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20);
    targets = register
}
criterion_main!(benches);
