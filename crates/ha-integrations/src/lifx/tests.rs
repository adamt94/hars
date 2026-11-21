//! Against the real bulb's replies and vectors made by running HA
//! 2026.6.1 with aiolifx 1.2.1 against a fake bulb (see `vectors/`).

use super::*;
use crate::light::Profiles;
use ha_registry::Registries;
use proto::Out;

/// What an entry's task does just after it has claimed its bulb (the
/// set-up lock held): a test pauses one there.
type Hook = Arc<dyn Fn(&str) + Send + Sync>;

static CLAIMED: parking_lot::Mutex<Option<Hook>> = parking_lot::Mutex::new(None);

pub(super) fn claimed(entry_id: &str) {
    let hook = CLAIMED.lock().clone();
    if let Some(hook) = hook {
        hook(entry_id);
    }
}

const SERIAL: [u8; 6] = [0xd0, 0x73, 0xd5, 0x01, 0x02, 0x03];
const UID: &str = "d0:73:d5:01:02:03";

/// A received message: (type, payload).
type Got = (u16, Vec<u8>);

/// A fake Color 1000 on 127.0.0.1: answers Gets from its state, acks Sets
/// and applies them at once (the transition model doesn't matter here).
struct FakeBulb {
    port: u16,
    state: Arc<Mutex<(u16, [u16; 4])>>,
    got: Arc<Mutex<Vec<Got>>>,
    silent: Arc<AtomicBool>,
}

async fn fake_bulb(power: u16, color: [u16; 4]) -> FakeBulb {
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = sock.local_addr().unwrap().port();
    let state = Arc::new(Mutex::new((power, color)));
    let got: Arc<Mutex<Vec<Got>>> = Arc::default();
    let silent = Arc::new(AtomicBool::new(false));
    let (st, log, quiet) = (state.clone(), got.clone(), silent.clone());
    tokio::spawn(async move {
        let mut buf = [0u8; 512];
        loop {
            let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                return;
            };
            let Some((h, p)) = proto::decode(&buf[..n]) else {
                continue;
            };
            log.lock().push((h.ty, p.to_vec()));
            if quiet.load(Ordering::Relaxed) {
                continue;
            }
            let flags = buf[22];
            let reply = |ty: u16, payload: &[u8]| {
                Out {
                    source: h.source,
                    target: SERIAL,
                    seq: h.seq,
                    ack: false,
                    res: true,
                    ty,
                    payload,
                }
                .encode()
            };
            let mut out = Vec::new();
            match h.ty {
                ty::LIGHT_SET_POWER => st.lock().0 = u16::from_le_bytes([p[0], p[1]]),
                ty::LIGHT_SET_WAVEFORM_OPTIONAL if p[1] == 0 => {
                    let mut s = st.lock();
                    for i in 0..4 {
                        if p[21 + i] != 0 {
                            s.1[i] = u16::from_le_bytes([p[2 + 2 * i], p[3 + 2 * i]]);
                        }
                    }
                }
                _ => {}
            }
            if flags & 2 != 0 {
                out.push(reply(ty::ACK, &[]));
            }
            let s = *st.lock();
            match h.ty {
                ty::GET_HOST_FIRMWARE => {
                    let mut p = vec![0u8; 16];
                    p.extend_from_slice(&65558u32.to_le_bytes());
                    out.push(reply(ty::STATE_HOST_FIRMWARE, &p));
                }
                ty::GET_VERSION => {
                    let mut p = 1u32.to_le_bytes().to_vec();
                    p.extend_from_slice(&22u32.to_le_bytes());
                    p.extend_from_slice(&0u32.to_le_bytes());
                    out.push(reply(ty::STATE_VERSION, &p));
                }
                ty::GET_GROUP => {
                    let mut p = vec![0u8; 16];
                    let mut label = b"Kitchen".to_vec();
                    label.resize(32, 0);
                    p.extend(label);
                    p.extend_from_slice(&0u64.to_le_bytes());
                    out.push(reply(ty::STATE_GROUP, &p));
                }
                ty::LIGHT_GET => {
                    let mut p = Vec::new();
                    for c in s.1 {
                        p.extend_from_slice(&c.to_le_bytes());
                    }
                    p.extend_from_slice(&0i16.to_le_bytes());
                    p.extend_from_slice(&s.0.to_le_bytes());
                    let mut label = b"Kitchen Light".to_vec();
                    label.resize(32, 0);
                    p.extend(label);
                    p.extend_from_slice(&0u64.to_le_bytes());
                    out.push(reply(ty::LIGHT_STATE, &p));
                }
                _ => {}
            }
            for o in out {
                let _ = sock.send_to(&o, from).await;
            }
        }
    });
    FakeBulb {
        port,
        state,
        got,
        silent,
    }
}

fn registries() -> Registries {
    Registries {
        entities: serde_json::from_str(include_str!("vectors/entity_registry_lifx.json")).unwrap(),
        devices: serde_json::from_str(include_str!("vectors/device_registry_lifx.json")).unwrap(),
        ..Default::default()
    }
}

/// HA's delays, but a quick poll-failure (5 × 40 ms) for tests that need
/// one. The settle and debounce keep HA's values: vectors are timed.
fn timing(fast_fail: bool) -> Timing {
    let mut t = Timing::HA;
    if fast_fail {
        t.command_try = Duration::from_millis(40);
        t.poll_try = Duration::from_millis(40);
        t.identify = Duration::from_millis(50);
    }
    t
}

async fn setup(f: &FakeBulb, t: Timing) -> (Hass, Arc<Bulb>, Arc<Components>) {
    let hass = Hass::new();
    let components = Components::new(&hass, Arc::new(registries()), Profiles::default());
    let found = connect("127.0.0.1", ("127.0.0.1", f.port), UID, t)
        .await
        .unwrap();
    let (b, _) = bulb(&components, "Kitchen Light", "127.0.0.1", found, true, t);
    (hass, b, components)
}

fn state(hass: &Hass, id: &str) -> Value {
    match hass.states().get(&EntityId::parse(id).unwrap()) {
        Some(s) => json!({"state": s.state.as_str(), "attributes": s.attributes}),
        None => Value::Null,
    }
}

const LIGHT: &str = "light.kitchen_kitchen_light";

#[tokio::test]
async fn sets_up_as_ha() {
    let f = fake_bulb(65535, [0, 0, 39321, 2700]).await;
    let (hass, _b, _) = setup(&f, timing(false)).await;
    // HA's states from the real bulb's replies.
    assert_eq!(
        state(&hass, LIGHT),
        json!({"state": "on", "attributes": {"min_color_temp_kelvin":2500,"max_color_temp_kelvin":9000,
            "effect_list":["effect_colorloop","effect_pulse","effect_stop"],"supported_color_modes":["color_temp","hs"],
            "effect":null,"color_mode":"color_temp","brightness":153,"color_temp_kelvin":2700,
            "hs_color":[28.395,65.723],"rgb_color":[255,167,87],"xy_color":[0.525,0.388],
            "friendly_name":"Kitchen Light","supported_features":36}})
    );
    assert_eq!(
        state(&hass, "button.kitchen_kitchen_light_restart"),
        json!({"state": "unknown", "attributes": {"device_class":"restart","friendly_name":"Kitchen Light Restart"}})
    );
    assert_eq!(
        state(&hass, "button.kitchen_kitchen_light_identify"),
        json!({"state": "unknown", "attributes": {"device_class":"identify","friendly_name":"Kitchen Light Identify"}})
    );
    // The disabled RSSI sensor isn't made.
    assert_eq!(
        state(&hass, "sensor.kitchen_kitchen_light_rssi"),
        Value::Null
    );
    let types: Vec<u16> = f.got.lock().iter().map(|g| g.0).collect();
    assert_eq!(types, [14, 32, 51, 101]);
}

#[tokio::test]
async fn serial_mismatch_retries() {
    let f = fake_bulb(65535, [0, 0, 39321, 2700]).await;
    let at = ("127.0.0.1", f.port);
    let r = connect("192.0.2.30", at, "d0:73:d5:00:00:01", timing(false)).await;
    let why = "Unexpected device found at 192.0.2.30; expected d0:73:d5:00:00:01, found d0:73:d5:01:02:03";
    assert_eq!(
        r.err(),
        Some(SetupError::Retry {
            reason: Some(why.into()),
            log: why.into()
        })
    );
    f.silent.store(true, Ordering::Relaxed);
    let r = connect("127.0.0.1", at, UID, timing(true)).await;
    // A refresh that times out is HA's `ConfigEntryNotReady()`: no reason.
    assert_eq!(
        r.err(),
        Some(SetupError::Retry {
            reason: None,
            log: "get_hostfirmware, get_version, get_group timed out after 5 attempts".into()
        })
    );
    // Five tries of all three, each a new message.
    assert_eq!(f.got.lock().iter().filter(|g| g.0 == 14).count(), 6);
}

/// HA's light-service vectors (rows 1-36 bar the effects and the longest
/// transitions): the messages sent, in order, and the state after.
#[tokio::test]
async fn light_services_send_what_ha_sends() {
    let vectors: Value =
        serde_json::from_str(include_str!("vectors/service_vectors.json")).unwrap();
    let cases: Vec<Value> = vectors
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| {
            let name = case["name"].as_str().unwrap();
            case["call"]["domain"] == "light"
                && !name.contains("effect")
                && !name.contains("automation")
                && !case["call"]["data"]
                    .get("transition")
                    .is_some_and(|t| t.as_f64() > Some(0.5))
        })
        .cloned()
        .collect();
    assert!(cases.len() >= 25, "{}", cases.len());
    // Each against its own bulb, all at once.
    let runs = cases.into_iter().map(|case| tokio::spawn(light_case(case)));
    for r in runs.collect::<Vec<_>>() {
        r.await.unwrap();
    }
}

async fn light_case(case: Value) {
    let name = case["name"].as_str().unwrap();
    let call = &case["call"];
    let init = &case["initial_bulb"];
    let c: Vec<u16> = init["color"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u16)
        .collect();
    let f = fake_bulb(
        init["power"].as_u64().unwrap() as u16,
        c.try_into().unwrap(),
    )
    .await;
    let (hass, _b, _c) = setup(&f, timing(false)).await;
    f.got.lock().clear();
    let data = call["data"].as_object().unwrap().clone();
    hass.call_service(
        "light",
        call["service"].as_str().unwrap(),
        data,
        None,
        false,
        None,
    )
    .await
    .unwrap_or_else(|e| panic!("{name}: {e}"));
    // The settle, then the debounced refresh.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let got: Vec<(u16, String)> = f
        .got
        .lock()
        .iter()
        .map(|(t, p)| (*t, p.iter().map(|b| format!("{b:02x}")).collect()))
        .collect();
    let want: Vec<(u16, String)> = case["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let hex = m["hex"].as_str().unwrap();
            (m["type"].as_u64().unwrap() as u16, hex[72..].to_owned())
        })
        .collect();
    assert_eq!(got, want, "{name}");
    assert_eq!(state(&hass, LIGHT), case["state_after"], "{name}");
}

/// `light.turn_on` with an effect calls `lifx.<effect>`; one that doesn't
/// exist is a not-found naming the child service.
#[tokio::test]
async fn effects_call_the_lifx_service() {
    let f = fake_bulb(65535, [0, 0, 39321, 2700]).await;
    let (hass, _b, _c) = setup(&f, timing(false)).await;
    f.got.lock().clear();
    let data = json!({"entity_id": LIGHT, "effect": "effect_stop"})
        .as_object()
        .cloned()
        .unwrap();
    let r = hass
        .call_service("light", "turn_on", data, None, false, None)
        .await;
    assert_eq!(
        r.err(),
        Some(ServiceError::NotFound {
            domain: "lifx".into(),
            service: "effect_stop".into()
        })
    );
    assert!(f.got.lock().is_empty());
}

#[tokio::test]
async fn buttons_as_ha() {
    let f = fake_bulb(65535, [0, 0, 39321, 2700]).await;
    let (hass, _b, _c) = setup(&f, timing(true)).await;
    let vectors: Value =
        serde_json::from_str(include_str!("vectors/service_vectors.json")).unwrap();
    let case = |n: &str| {
        vectors
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == n)
            .unwrap()
            .clone()
    };
    let payloads = |c: &Value| -> Vec<(u16, String)> {
        c["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["type"] != 101)
            .map(|m| {
                (
                    m["type"].as_u64().unwrap() as u16,
                    m["hex"].as_str().unwrap()[72..].to_owned(),
                )
            })
            .collect()
    };
    let sent = |f: &FakeBulb| -> Vec<(u16, String)> {
        f.got
            .lock()
            .iter()
            .filter(|g| g.0 != 101)
            .map(|(t, p)| (*t, p.iter().map(|b| format!("{b:02x}")).collect()))
            .collect()
    };
    let press = |id: &'static str| {
        let hass = hass.clone();
        async move {
            let data = json!({ "entity_id": id }).as_object().cloned().unwrap();
            hass.call_service("button", "press", data, None, false, None)
                .await
                .map(drop)
        }
    };
    for (button, vector, power) in [
        (
            "button.kitchen_kitchen_light_identify",
            "identify_while_on",
            65535,
        ),
        (
            "button.kitchen_kitchen_light_identify",
            "identify_while_off",
            0,
        ),
        ("button.kitchen_kitchen_light_restart", "restart", 65535),
    ] {
        f.state.lock().0 = power;
        _b.cached.lock().power = power;
        f.got.lock().clear();
        press(button).await.unwrap();
        // The reboot isn't waited for.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(sent(&f), payloads(&case(vector)), "{vector}");
        assert!(
            state(&hass, button)["state"]
                .as_str()
                .unwrap()
                .ends_with("+00:00")
        );
    }
    // A silent bulb: identify's bare timeout.
    f.silent.store(true, Ordering::Relaxed);
    f.state.lock().0 = 65535;
    _b.cached.lock().power = 65535;
    match press("button.kitchen_kitchen_light_identify").await {
        Err(ServiceError::Unknown(m)) => assert_eq!(
            m,
            "functools.partial(<bound method Light.set_waveform_optional of <aiolifx.aiolifx.Light object at 0x0>>, value={'transient': True, 'color': [0, 0, 1, 3500], 'skew_ratio': 0, 'period': 1000, 'cycles': 3, 'waveform': 1, 'set_hue': True, 'set_saturation': True, 'set_brightness': True, 'set_kelvin': True}, rapid=False) timed out after 5 attempts"
        ),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn silence_makes_it_unavailable_then_it_recovers() {
    let f = fake_bulb(65535, [0, 0, 39321, 2700]).await;
    let (hass, b, _c) = setup(&f, timing(true)).await;
    f.silent.store(true, Ordering::Relaxed);
    f.got.lock().clear();
    b.refresh().await;
    assert_eq!(state(&hass, LIGHT)["state"], "unavailable");
    assert_eq!(
        state(&hass, LIGHT)["attributes"],
        json!({"min_color_temp_kelvin":2500,"max_color_temp_kelvin":9000,
            "effect_list":["effect_colorloop","effect_pulse","effect_stop"],
            "supported_color_modes":["color_temp","hs"],"friendly_name":"Kitchen Light","supported_features":36})
    );
    assert_eq!(
        state(&hass, "button.kitchen_kitchen_light_restart")["state"],
        "unavailable"
    );
    // Five tries, each a new LightGet.
    assert_eq!(f.got.lock().iter().filter(|g| g.0 == 101).count(), 5);
    // A command to a silent bulb fails as HA's does.
    b.ok.store(true, Ordering::Relaxed);
    let r = b.set_state(Some(true), Params::default()).await;
    assert_eq!(
        r,
        Err(ServiceError::Failed(
            "Timeout setting power for None".into()
        ))
    );
    f.silent.store(false, Ordering::Relaxed);
    f.state.lock().0 = 0;
    b.ok.store(false, Ordering::Relaxed);
    b.refresh().await;
    assert_eq!(state(&hass, LIGHT)["state"], "off");
    assert_eq!(
        state(&hass, "button.kitchen_kitchen_light_restart")["state"],
        "unknown"
    );
}

#[tokio::test]
async fn retries_complete_on_a_late_reply() {
    // Replies to the first try arriving during the second still count.
    let f = fake_bulb(65535, [0, 0, 39321, 2700]).await;
    let conn = Conn::open("127.0.0.1", f.port).await.unwrap();
    let r = conn
        .execute(
            &[Op::get(ty::LIGHT_GET, ty::LIGHT_STATE)],
            5,
            Duration::from_millis(200),
        )
        .await
        .unwrap();
    assert_eq!(proto::LightState::parse(&r[0].1).unwrap().power, 65535);
    // Seq wraps at 128.
    for _ in 0..130 {
        conn.next_seq();
    }
    assert_eq!(*conn.seq.lock(), (1 + 130) % 128);
}

/// A reply to an earlier try after aiolifx forgot it (18 s on) is
/// ignored; within that it completes the request.
#[tokio::test]
async fn late_replies_count_only_within_the_request_lifetime() {
    // A bulb that answers only the first request, 150 ms late.
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = sock.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut buf = [0u8; 512];
        let mut first = true;
        loop {
            let (n, from) = sock.recv_from(&mut buf).await.unwrap();
            let (h, _) = proto::decode(&buf[..n]).unwrap();
            if !std::mem::take(&mut first) {
                continue;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
            let mut p = vec![0u8; 12];
            p.extend([0u8; 40]);
            let reply = Out {
                source: h.source,
                target: SERIAL,
                seq: h.seq,
                ack: false,
                res: true,
                ty: ty::LIGHT_STATE,
                payload: &p,
            }
            .encode();
            let _ = sock.send_to(&reply, from).await;
        }
    });
    let get = [Op::get(ty::LIGHT_GET, ty::LIGHT_STATE)];
    let conn = Conn::open("127.0.0.1", port)
        .await
        .unwrap()
        .with_lifetime(Duration::from_millis(100));
    let r = conn.execute(&get, 3, Duration::from_millis(100)).await;
    assert_eq!(r.err(), Some(vec![0]));
}

/// A second command drops the first one's pending debounced refresh, as
/// `async_set_updated_data` does: one re-read, after the second.
#[tokio::test]
async fn a_new_command_drops_the_pending_refresh() {
    let f = fake_bulb(65535, [0, 0, 39321, 2700]).await;
    let (_hass, b, _c) = setup(&f, timing(false)).await;
    f.got.lock().clear();
    let b2 = b.clone();
    let first = tokio::spawn(async move { b2.set_state(Some(true), Params::default()).await });
    tokio::time::sleep(Duration::from_millis(400)).await;
    b.set_state(Some(true), Params::default()).await.unwrap();
    first.await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let gets = f.got.lock().iter().filter(|g| g.0 == ty::LIGHT_GET).count();
    assert_eq!(gets, 1);
}

/// A command that is waiting to be sent again when its entry is removed
/// ends there: the service call holds the bulb and its socket itself, and
/// would go on trying (HA's 5 tries) and then send what follows. Here the
/// light is turned off while the bulb says nothing, and the entry is
/// stopped during the wait. Nothing reaches the bulb after, and the
/// entities, their states and the polling are gone.
#[tokio::test]
async fn a_command_ends_when_its_entry_is_removed() {
    let f = fake_bulb(65535, [0, 0, 39321, 2700]).await;
    let (hass, b, components) = setup(&f, timing(false)).await;
    let coordinator_polls = || f.got.lock().iter().filter(|g| g.0 == ty::LIGHT_GET).count();
    assert!(state(&hass, LIGHT) != Value::Null);
    f.silent.store(true, Ordering::Relaxed);
    let light = LifxLight(b.clone());
    let call = tokio::spawn(async move {
        use crate::light::Light;
        light.turn_off(Default::default()).await
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !f.got.lock().iter().any(|g| g.0 == ty::LIGHT_SET_POWER) {
        assert!(tokio::time::Instant::now() < deadline, "nothing was sent");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    b.stop();
    // The call ends at once, not after its next try (3 s later).
    let ended = tokio::time::timeout(Duration::from_millis(250), call).await;
    assert!(matches!(ended, Ok(Ok(Err(_)))), "{ended:?}");
    let sent = f.got.lock().len();
    let polls = coordinator_polls();
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(f.got.lock().len(), sent, "something reached the bulb after");
    assert_eq!(coordinator_polls(), polls);
    for id in [LIGHT, "button.kitchen_kitchen_light_restart"] {
        assert_eq!(state(&hass, id), Value::Null, "{id}");
    }
    // No longer a target: a call for every light reaches nothing.
    let all = json!({"entity_id": "all"}).as_object().unwrap().clone();
    let _ = hass
        .call_service("light", "turn_on", all, None, false, None)
        .await;
    assert_eq!(f.got.lock().len(), sent);
    drop(components);
    // A press of a button it still holds reaches nothing either.
    LifxButton(b.clone(), 0).press().await.unwrap();
    assert_eq!(f.got.lock().len(), sent);
    // And its socket is closed, though the bulb is still held: its port
    // is free again.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(std::net::UdpSocket::bind(("0.0.0.0", b.conn.local().port())).is_ok());
}

/// A bulb's address may be IPv6, as aiolifx's endpoint takes it.
#[tokio::test]
async fn a_bulb_at_an_ipv6_address_is_reached() {
    let Ok(bulb) = tokio::net::UdpSocket::bind("[::1]:0").await else {
        return; // No IPv6 loopback here.
    };
    let port = bulb.local_addr().unwrap().port();
    let conn = Conn::open("::1", port).await.unwrap();
    conn.fire_and_forget(ty::SET_REBOOT).await;
    let mut buf = [0u8; 64];
    let got = tokio::time::timeout(Duration::from_secs(2), bulb.recv(&mut buf)).await;
    let n = got.unwrap().unwrap();
    assert_eq!(proto::decode(&buf[..n]).unwrap().0.ty, ty::SET_REBOOT);
}

/// A message that a reply of another kind ended (aiolifx drops it from
/// what it waits for) isn't sent again by aiolifx; HA's next try sends a
/// new one.
#[tokio::test]
async fn a_message_a_wrong_reply_ended_is_not_sent_again() {
    let bulb = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = bulb.local_addr().unwrap().port();
    let got: Arc<Mutex<Vec<u8>>> = Arc::default();
    let seqs = got.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 512];
        while let Ok((n, from)) = bulb.recv_from(&mut buf).await {
            let (h, _) = proto::decode(&buf[..n]).unwrap();
            seqs.lock().push(h.seq);
            // A `StateGroup` on the same sequence: not what was asked.
            let wrong = Out {
                source: h.source,
                target: SERIAL,
                seq: h.seq,
                ack: false,
                res: true,
                ty: ty::STATE_GROUP,
                payload: &[0; 56],
            }
            .encode();
            let _ = bulb.send_to(&wrong, from).await;
        }
    });
    let resend = conn::Resend {
        every: Duration::from_millis(40),
        sends: 3,
    };
    let conn = Conn::open_with("127.0.0.1", port, resend).await.unwrap();
    let op = Op::get(ty::GET_HOST_FIRMWARE, ty::STATE_HOST_FIRMWARE);
    let r = conn.execute(&[op], 2, Duration::from_millis(240)).await;
    assert!(r.is_err());
    // One send for each of HA's two tries, each its own sequence.
    assert_eq!(*got.lock(), [1, 2]);
}

/// HA's coordinator refreshes on its interval only while an entity listens:
/// a bulb whose entities are all disabled isn't polled after its set-up.
#[tokio::test]
async fn a_bulb_with_no_enabled_entity_is_not_polled() {
    for disabled in [false, true] {
        let f = fake_bulb(65535, [0, 0, 39321, 2700]).await;
        let hass = Hass::new();
        let mut registries = registries();
        if disabled {
            for row in &mut registries.entities {
                let mut off = (**row).clone();
                off.disabled_by = Some("user".into());
                *row = Arc::new(off);
            }
        }
        let components = Components::new(&hass, Arc::new(registries), Profiles::default());
        let t = Timing {
            poll: Duration::from_millis(50),
            ..timing(false)
        };
        let found = connect("127.0.0.1", ("127.0.0.1", f.port), UID, t)
            .await
            .unwrap();
        let (_b, coordinator) = bulb(&components, "Kitchen Light", "127.0.0.1", found, true, t);
        tokio::time::sleep(Duration::from_millis(1000)).await;
        coordinator.abort();
        let polls = f.got.lock().iter().filter(|g| g.0 == ty::LIGHT_GET).count();
        match disabled {
            true => assert_eq!(polls, 1, "only set-up's"),
            false => assert!(polls > 3, "{polls}"),
        }
    }
}

/// The broadcast's socket and a flow's socket to a bulb are gone when the
/// flow's step answers: nothing of a flow listens or waits afterwards.
#[tokio::test]
async fn a_flows_sockets_are_gone_when_its_step_answers() {
    let broadcast = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let at = Where {
        broadcast: broadcast.local_addr().unwrap(),
        ..Where::default()
    };
    let scan = tokio::spawn(async move { scan(&at, Duration::from_millis(100), None).await });
    let mut buf = [0u8; 64];
    let (n, from) = broadcast.recv_from(&mut buf).await.unwrap();
    let (h, _) = proto::decode(&buf[..n]).unwrap();
    assert_eq!((h.ty, h.seq, h.target), (ty::GET_SERVICE, 0, [0; 6]));
    assert!(scan.await.unwrap().is_empty());
    // Its port is free again.
    assert!(std::net::UdpSocket::bind(("0.0.0.0", from.port())).is_ok());
}

/// With a device or entity store that can't be written (an install from an
/// older Home Assistant), no row is made: rows made now would be gone after
/// a restart while rows that point at them were saved.
#[tokio::test]
async fn nothing_is_registered_in_a_store_that_cant_be_written() {
    let f = fake_bulb(65535, [0, 0, 39321, 2700]).await;
    let found = connect("127.0.0.1", ("127.0.0.1", f.port), UID, timing(false))
        .await
        .unwrap();
    let hass = Hass::new();
    let older = ha_registry::STORES
        .iter()
        .find(|s| s.0 == ha_registry::DEVICE_KEY)
        .unwrap()
        .2
        - 1;
    let registries = Registries {
        device_file: ha_registry::FileRest {
            minor: older,
            ..Default::default()
        },
        ..Default::default()
    };
    let dir = std::env::temp_dir().join(format!("hars-lifx-read-only-{}", std::process::id()));
    let registry = ha_registry::Registry::new(&hass, &dir, registries);
    register::register(&hass, &registry, "entry", &found, false, false);
    let after = registry.snapshot();
    assert!(after.devices.is_empty() && after.entities.is_empty() && after.areas.is_empty());
    // And on a store that can be: the device, the light, both buttons and
    // the RSSI sensor, disabled.
    let registry = ha_registry::Registry::in_memory(&hass, Registries::default());
    register::register(&hass, &registry, "entry", &found, false, false);
    let after = registry.snapshot();
    assert_eq!(after.devices.len(), 1);
    let rssi = after
        .entities
        .iter()
        .find(|e| e.entity_id.ends_with("_rssi"))
        .unwrap();
    assert_eq!(rssi.disabled_by.as_deref(), Some("integration"));
    assert_eq!(after.entities.len(), 4);
}

/// Rediscovery found the bulb at B, where its entry had A; a flow writes C
/// between rediscovery reading the entry and writing it. C stands: the
/// address is looked at and written in one step, under the entries' lock.
/// And so does A written back after C: what counts is that the entry's data
/// changed since rediscovery read it, not what it says now.
#[test]
fn a_newer_host_is_not_overwritten_by_rediscovery() {
    use crate::config_entries::ConfigEntries;
    let hass = Hass::new();
    let host = |h: &str| json!({"host": h}).as_object().unwrap().clone();
    let entries = ConfigEntries::in_memory(&hass, Vec::new());
    let mut entry = entries.new_entry("lifx", "user", "Bulb", Some(UID), host("192.0.2.1"));
    entry.insert("entry_id".into(), json!("e"));
    entries.add(entry);
    let stored = |entries: &ConfigEntries| entries.get("e").unwrap()["data"].clone();
    // A, then C before the write.
    let (_, read) = entries.data_value("e", "host").unwrap();
    let flow = entries.clone();
    crate::config_entries::tests::set_before_write(move || {
        flow.update_data("e", host("192.0.2.3"));
    });
    assert!(!save_host(&entries, "e", read, "192.0.2.2"));
    assert_eq!(stored(&entries), json!({"host": "192.0.2.3"}));
    // A, then C, then A again before the write.
    let flow = entries.clone();
    flow.update_data("e", host("192.0.2.1"));
    let (was, read) = entries.data_value("e", "host").unwrap();
    assert_eq!(was, "192.0.2.1");
    crate::config_entries::tests::set_before_write(move || {
        flow.update_data("e", host("192.0.2.3"));
        flow.update_data("e", host("192.0.2.1"));
    });
    assert!(!save_host(&entries, "e", read, "192.0.2.2"));
    assert_eq!(stored(&entries), json!({"host": "192.0.2.1"}));
    // With nothing written meanwhile, rediscovery's address is saved, and
    // only the host.
    let (_, read) = entries.data_value("e", "host").unwrap();
    assert!(save_host(&entries, "e", read, "192.0.2.2"));
    assert_eq!(stored(&entries), json!({"host": "192.0.2.2"}));
}

/// Two entries for one bulb. The first claims it and is paused right there
/// (a test signal); the second, started then, waits for the first's rows
/// and entities: the first is the device's primary entry and keeps every
/// live row, and the second takes only the disabled RSSI row, as in HA.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_entry_that_claims_a_bulb_sets_it_up_before_another() {
    use crate::config_entries::{ConfigEntries, EntryState, Status};
    let f = fake_bulb(65535, [0, 0, 39321, 2700]).await;
    let hass = Hass::new();
    let entries = ConfigEntries::in_memory(&hass, Vec::new());
    for (id, host) in [("dup-first", "192.0.2.40"), ("dup-second", "192.0.2.41")] {
        let data = json!({"host": host}).as_object().unwrap().clone();
        let mut entry = entries.new_entry("lifx", "user", "Kitchen Light", Some(UID), data);
        entry.insert("entry_id".into(), json!(id));
        entries.add(entry);
    }
    let registry = ha_registry::Registry::in_memory(&hass, Registries::default());
    let lookup = entries.clone();
    registry.set_entry_lookup(Arc::new(move |id: &str| lookup.info(id)));
    let components = Components::new(&hass, registry.clone(), Profiles::default());
    let bulb_at: std::net::SocketAddr = ([127, 0, 0, 1], f.port).into();
    let at = Where {
        stand_ins: vec![
            ("192.0.2.40".into(), bulb_at),
            ("192.0.2.41".into(), bulb_at),
        ],
        timing: Timing {
            rediscover_after: Duration::from_secs(3600),
            ..timing(true)
        },
        ..Where::default()
    };
    // The first is held just after its claim until let go.
    let (claimed_tx, claimed_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = parking_lot::Mutex::new(go_rx);
    let claimed_tx = parking_lot::Mutex::new(claimed_tx);
    *CLAIMED.lock() = Some(Arc::new(move |entry_id: &str| {
        if entry_id == "dup-first" {
            let _ = claimed_tx.lock().send(());
            tokio::task::block_in_place(|| {
                let _ = go_rx.lock().recv_timeout(Duration::from_secs(5));
            });
        }
    }));
    let start = |id: &str| {
        let entry = entries.get(id).unwrap();
        tokio::spawn(run(
            components.clone(),
            entry,
            at.clone(),
            hass.setup_guard(),
            Status::new(entries.clone(), id),
        ))
    };
    let first = start("dup-first");
    tokio::task::spawn_blocking(move || claimed_rx.recv_timeout(Duration::from_secs(5)))
        .await
        .unwrap()
        .unwrap();
    let second = start("dup-second");
    // The second gets as far as it can while the first is held (a sleep on
    // the blocking pool: the paused tasks hold worker threads).
    tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(300)))
        .await
        .unwrap();
    go_tx.send(()).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while ["dup-first", "dup-second"]
        .iter()
        .any(|id| entries.runtime(id).map(|r| r.state) != Some(EntryState::Loaded))
    {
        assert!(tokio::time::Instant::now() < deadline, "not both loaded");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    *CLAIMED.lock() = None;
    let rows = registry.snapshot();
    let device = &rows.devices[0];
    assert_eq!(device.primary_config_entry.as_deref(), Some("dup-first"));
    let mut owners = device.config_entries.clone();
    owners.sort();
    assert_eq!(owners, ["dup-first", "dup-second"]);
    for e in &rows.entities {
        let owner = match e.entity_id.ends_with("_rssi") {
            true => "dup-second",
            false => "dup-first",
        };
        assert_eq!(e.config_entry_id.as_deref(), Some(owner), "{}", e.entity_id);
    }
    first.abort();
    second.abort();
}
