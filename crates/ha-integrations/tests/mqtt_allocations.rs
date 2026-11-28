//! What one ordinary MQTT reading costs in allocations: a QoS 0
//! message of one byte, for one discovered sensor with no template,
//! that changes nothing (the same value again). The routing from the
//! socket to the sensor is the hot path PERF.md's rules are about.
//!
//! Its own test binary, so its counting allocator sees only this test
//! (as `ha-core`'s `template_memory.rs`).

// The counting allocator must implement `GlobalAlloc`, an unsafe trait; it
// only forwards to the system allocator and counts. Test code only.
#![allow(unsafe_code)]

use ha_core::{EntityId, Hass};
use ha_integrations::mqtt;
use serde_json::json;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.realloc(p, layout, new) }
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.dealloc(p, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn publish(out: &mut Vec<u8>, topic: &str, payload: &[u8]) {
    let len = 2 + topic.len() + 1 + payload.len();
    out.push(0x30);
    let mut n = len;
    loop {
        let b = (n % 128) as u8;
        n /= 128;
        out.push(if n > 0 { b | 0x80 } else { b });
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&(topic.len() as u16).to_be_bytes());
    out.extend_from_slice(topic.as_bytes());
    out.push(0); // MQTT 5: no properties.
    out.extend_from_slice(payload);
}

#[tokio::test(flavor = "current_thread")]
async fn a_reading_that_changes_nothing_allocates_nothing() {
    let hass = Hass::new();
    hass.start();
    let registry = ha_registry::Registry::in_memory(&hass, Default::default());
    let components = ha_integrations::components::Components::new(
        &hass,
        registry,
        ha_integrations::light::Profiles::default(),
    );
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    let slot = Arc::new(parking_lot::Mutex::new(Some(theirs)));
    let dial: mqtt::client::Dial = Arc::new(move |_, _| {
        let stream = slot.lock().take();
        Box::pin(async move {
            stream
                .map(|s| Box::new(s) as Box<dyn mqtt::client::Stream>)
                .ok_or_else(|| std::io::ErrorKind::ConnectionRefused.into())
        })
    });
    let entry = json!({"entry_id": "e", "domain": "mqtt", "title": "b", "version": 2,
                       "minor_version": 1, "options": {},
                       "data": {"broker": "b", "port": 1883, "protocol": "5"}});
    tokio::spawn(mqtt::run(
        components,
        entry.as_object().unwrap().clone(),
        None,
        dial,
        hass.setup_guard(),
        ha_integrations::config_entries::Status::none(),
    ));
    let (mut read, mut write) = tokio::io::split(ours);
    // The broker: take whatever comes, answer the CONNECT.
    let mut first = [0u8; 1];
    read.read_exact(&mut first).await.unwrap();
    tokio::spawn(async move {
        let mut sink = vec![0u8; 1 << 16];
        while read.read(&mut sink).await.is_ok_and(|n| n > 0) {}
    });
    write.write_all(&[0x20, 3, 0, 0, 0]).await.unwrap();
    let mut out = Vec::new();
    publish(
        &mut out,
        "homeassistant/sensor/s/config",
        br#"{"stat_t": "s/state", "uniq_id": "s1", "name": "S"}"#,
    );
    publish(&mut out, "s/state", b"1");
    write.write_all(&out).await.unwrap();
    let id = EntityId::parse("sensor.s").unwrap();
    let state = |want: &str| hass.states().get(&id).is_some_and(|s| s.state == want);
    while !state("1") {
        tokio::task::yield_now().await;
    }
    // Readings that change nothing, then one that does, to know when
    // they are all in.
    let n = 10_000;
    let mut batch = Vec::with_capacity(n * 12);
    for _ in 0..n {
        publish(&mut batch, "s/state", b"1");
    }
    publish(&mut batch, "s/state", b"2");
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    write.write_all(&batch).await.unwrap();
    while !state("2") {
        tokio::task::yield_now().await;
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    eprintln!(
        "{allocations} allocations for {n} readings that changed nothing and one that did: \
         {:.2} a reading",
        allocations as f64 / n as f64
    );
    // The one changed write (its state, event and attributes) and the
    // reads' buffer growing are all that may allocate.
    assert!(allocations < 100, "{allocations} allocations");
}
