//! What a SwitchBot Cloud entry writes at each refresh for its infrared
//! remotes, which are not polled: every 600 s their climate and switch
//! report the state they have, unchanged unless a command came between.
//!
//! Run with `cargo bench -p ha-integrations`; compare with main as
//! docs/PERF.md says (`--save-baseline main` there, `--baseline main`
//! here, with the same `target/criterion`).

use criterion::{Criterion, criterion_group, criterion_main};
use ha_core::Hass;
use ha_integrations::switchbot_cloud::{api::Api, setup};
use ha_registry::Registries;
use std::hint::black_box;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One infrared air conditioner, as the API lists it.
const DEVICES: &str = r#"{"statusCode":100,"body":{"deviceList":[],"infraredRemoteList":[{"deviceId":"01-202301010000-00000001","deviceName":"Air Conditioner","remoteType":"DIY Air Conditioner","hubDeviceId":"AABBCC000002"}]},"message":"success"}"#;

/// The device list once, on a loopback port: the base URL.
async fn api() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf).await;
        let res = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{DEVICES}",
            DEVICES.len()
        );
        let _ = s.write_all(res.as_bytes()).await;
    });
    base
}

fn report(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let hass = Hass::new();
    let entry = rt.block_on(async {
        let client = ha_http::Client::builder("bench").build().unwrap();
        let api = Api::new(client, api().await, "token".into(), "secret".into());
        setup(&hass, &Registries::default(), api).await.unwrap()
    });
    // The first report writes the states; each one after is unchanged.
    rt.block_on(entry.poll());
    assert_eq!(hass.states().len(), 2);
    c.bench_function("switchbot_report/remote_unchanged", |b| {
        b.iter(|| rt.block_on(black_box(&entry).poll()))
    });
}

criterion_group!(benches, report);
criterion_main!(benches);
