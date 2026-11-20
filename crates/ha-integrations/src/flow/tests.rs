//! The flow manager against the SwitchBot API of `switchbot_cloud`'s tests.
//! What a fresh set-up must leave in the registries and the state machine
//! is `switchbot_cloud/vectors/fresh_setup.json`: what Home Assistant
//! 2026.6.1 had after the same flow (`scripts/vectors/config_flow.py`).
//! The views' replies are compared with HA's in `ha-api`
//! (`config_entries/tests.rs`).

use super::*;
use crate::config_entries::{Change, EntryState, Listener};
use crate::light::Profiles;
use crate::switchbot_cloud::tests::{AC, BOT, Server, fake, fixture};
use ha_core::EntityId;
use ha_registry::Registry;

const TOKEN: &str = "token-one";

struct Home {
    hass: Hass,
    registry: Arc<Registry>,
    entries: Arc<ConfigEntries>,
    flows: Arc<Manager>,
    api: Arc<Server>,
    base: String,
}

async fn home() -> Home {
    let hass = Hass::new();
    let (base, api) = fake().await;
    let registry = Registry::in_memory(&hass, Default::default());
    let entries = ConfigEntries::in_memory(&hass, Vec::new());
    let components = Components::new(&hass, registry.clone(), Profiles::default());
    let flows = Manager::new(
        &hass,
        entries.clone(),
        components,
        Endpoints {
            switchbot: base.clone(),
        },
        Vec::new(),
        None,
    )
    .unwrap();
    Home {
        hass,
        registry,
        entries,
        flows,
        api,
        base,
    }
}

fn credentials(token: &str) -> Option<Vec<(String, Value)>> {
    Some(vec![
        ("api_token".to_owned(), json!(token)),
        ("api_key".to_owned(), json!("secret-one")),
    ])
}

/// Start a flow and answer its form: the result.
async fn add(home: &Home, token: &str) -> FlowResult {
    let FlowResult::Form { flow_id, .. } = home
        .flows
        .init("switchbot_cloud", SOURCE_USER)
        .await
        .unwrap()
    else {
        panic!("no form");
    };
    home.flows
        .configure(&flow_id, credentials(token))
        .await
        .unwrap()
}

fn entry_id(result: &FlowResult) -> String {
    let FlowResult::CreateEntry { entry, .. } = result else {
        panic!("no entry: {result:?}");
    };
    entry["entry_id"].as_str().unwrap().to_owned()
}

/// The registries' rows of the entry, as the generator writes HA's.
fn rows(registry: &Registry, entry_id: &str) -> (Vec<Value>, Vec<Value>) {
    rows_as(registry, entry_id, "<entry:1>")
}

/// [`rows`], with `entry_id` written as `name`.
fn rows_as(registry: &Registry, entry_id: &str, name: &str) -> (Vec<Value>, Vec<Value>) {
    let snap = registry.snapshot();
    let device_names: Vec<(String, String)> = snap
        .devices
        .iter()
        .map(|d| {
            (
                d.id.clone(),
                format!("<device:{}>", d.identifiers[0][1].as_str().unwrap()),
            )
        })
        .collect();
    let plain = |v: Value| -> Value {
        let mut text = v.to_string().replace(entry_id, name);
        for (id, name) in &device_names {
            text = text.replace(id, name);
        }
        let mut v: Value = serde_json::from_str(&text).unwrap();
        let row = v.as_object_mut().unwrap();
        for key in ["created_at", "modified_at"] {
            row.insert(key.into(), json!("<time>"));
        }
        if row["id"].as_str().is_some_and(|id| !id.starts_with('<')) {
            row.insert("id".into(), json!("<id>"));
        }
        v
    };
    let mut devices: Vec<Value> = snap
        .devices
        .iter()
        .map(|d| plain(serde_json::to_value(&**d).unwrap()))
        .collect();
    devices.sort_by_key(|d| d["identifiers"].to_string());
    let mut entities: Vec<Value> = snap
        .entities
        .iter()
        .map(|e| plain(serde_json::to_value(&**e).unwrap()))
        .collect();
    entities.sort_by_key(|e| e["entity_id"].as_str().unwrap().to_owned());
    (devices, entities)
}

fn state(hass: &Hass, id: &str) -> Value {
    match hass.states().get(&EntityId::parse(id).unwrap()) {
        Some(s) => json!({"state": s.state.as_str(), "attributes": s.attributes}),
        None => Value::Null,
    }
}

fn fresh() -> Value {
    serde_json::from_str(include_str!("../switchbot_cloud/vectors/fresh_setup.json")).unwrap()
}

/// The flow's steps, the entry it makes, and the devices, entities and
/// states HA has after the same flow.
#[tokio::test]
async fn a_flow_adds_switchbot_as_home_assistant_does() {
    let home = home().await;
    let changes = Arc::new(Mutex::new(Vec::new()));
    let listener: Listener = {
        let (changes, entries) = (changes.clone(), home.entries.clone());
        Box::new(move |change: Change, entry: &Map<String, Value>| {
            let id = entry["entry_id"].as_str().unwrap();
            changes
                .lock()
                .push((change, entries.runtime(id).map(|r| r.state)));
            true
        })
    };
    home.entries
        .subscribe(Arc::new(Mutex::new(true)), listener, || {});

    let form = home
        .flows
        .init("switchbot_cloud", SOURCE_USER)
        .await
        .unwrap();
    let FlowResult::Form {
        flow_id,
        handler,
        step_id,
        schema,
        errors,
        ..
    } = form
    else {
        panic!("no form");
    };
    assert_eq!((handler, step_id), ("switchbot_cloud", "user"));
    assert!(errors.is_empty());
    // `voluptuous_serialize` of `STEP_USER_DATA_SCHEMA`, as HA sends it.
    assert_eq!(
        schema_json(schema.as_deref()),
        json!([{"type": "string", "name": "api_token", "required": true},
               {"type": "string", "name": "api_key", "required": true}])
    );
    assert_eq!(flow_id.len(), 26);
    assert_eq!(home.flows.in_progress(), 1);
    assert!(
        home.api.take().is_empty(),
        "nothing is asked before the form is answered"
    );

    let result = home
        .flows
        .configure(&flow_id, credentials(TOKEN))
        .await
        .unwrap();
    let FlowResult::CreateEntry {
        title,
        version,
        minor_version,
        entry,
        ..
    } = &result
    else {
        panic!("no entry: {result:?}");
    };
    assert_eq!(
        (title.as_str(), *version, *minor_version),
        ("SwitchBot Cloud", 1, 1)
    );
    assert_eq!(entry["domain"], "switchbot_cloud");
    assert_eq!(entry["source"], "user");
    assert_eq!(entry["title"], "SwitchBot Cloud");
    assert_eq!(entry["unique_id"], TOKEN);
    assert_eq!(
        entry["data"],
        json!({"api_token": TOKEN, "api_key": "secret-one"})
    );
    assert_eq!(
        (&entry["version"], &entry["minor_version"]),
        (&json!(1), &json!(1))
    );
    let id = entry_id(&result);
    // The reply waited for the setup: the entry is loaded, the flow gone.
    assert_eq!(
        home.entries
            .runtime(&id)
            .map(|r| (r.state, r.integration_loaded)),
        Some((EntryState::Loaded, true))
    );
    assert_eq!(home.flows.in_progress(), 0);
    assert_eq!(
        *changes.lock(),
        [
            (Change::Added, Some(EntryState::NotLoaded)),
            (Change::Updated, Some(EntryState::SetupInProgress)),
            (Change::Updated, Some(EntryState::Loaded)),
        ]
    );
    assert_eq!(
        home.flows.started(),
        ["switchbot_cloud", "button", "climate", "sensor", "switch"]
    );

    // HA's registry rows and states for the same devices.
    let want = fresh();
    let (devices, entities) = rows(&home.registry, &id);
    assert_eq!(json!(devices), want["devices"]);
    assert_eq!(json!(entities), want["entities"]);
    for (entity_id, s) in want["states"].as_object().unwrap() {
        assert_eq!(state(&home.hass, entity_id), *s, "{entity_id}");
    }

    // A service aimed at the new device reaches it: the services read the
    // registry as it is now, not as it was at start-up.
    home.api.take();
    let device = home
        .registry
        .snapshot()
        .devices
        .iter()
        .find(|d| d.identifiers[0][1] == AC)
        .unwrap()
        .id
        .clone();
    home.hass
        .call_service(
            "switch",
            "turn_on",
            json!({"device_id": device}).as_object().unwrap().clone(),
            None,
            false,
            None,
        )
        .await
        .unwrap();
    let seen = home.api.take();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, format!("/v1.1/devices/{AC}/commands"));
    assert_eq!(state(&home.hass, "switch.air_conditioner")["state"], "on");
}

/// What the check of the credentials shows, and that the same flow goes
/// on afterwards.
#[tokio::test]
async fn bad_credentials_show_the_form_again() {
    let home = home().await;
    let FlowResult::Form { flow_id, .. } = home
        .flows
        .init("switchbot_cloud", SOURCE_USER)
        .await
        .unwrap()
    else {
        panic!("no form");
    };
    let json = "application/json";
    let cases: [(crate::switchbot_cloud::tests::Reply, &str); 4] = [
        (
            (403, json, r#"{"message":"Unauthorized"}"#.into()),
            "invalid_auth",
        ),
        (
            (500, json, r#"{"message":"down"}"#.into()),
            "cannot_connect",
        ),
        (
            (
                200,
                json,
                r#"{"statusCode":190,"body":{},"message":"error"}"#.into(),
            ),
            "cannot_connect",
        ),
        ((200, "text/html", "<html>down</html>".into()), "unknown"),
    ];
    for (reply, error) in cases {
        home.api.set("GET", "/v1.1/devices", reply);
        let result = home
            .flows
            .configure(&flow_id, credentials(TOKEN))
            .await
            .unwrap();
        let FlowResult::Form {
            errors, step_id, ..
        } = result
        else {
            panic!("no form");
        };
        assert_eq!((step_id, errors), ("user", vec![("base", error)]));
    }
    // Shown again (a GET), the form has no error.
    let FlowResult::Form { errors, .. } = home.flows.configure(&flow_id, None).await.unwrap()
    else {
        panic!("no form");
    };
    assert!(errors.is_empty());
    assert_eq!(home.entries.list(), Some(Vec::new()));
    assert_eq!(home.flows.in_progress(), 1);
}

/// The form's schema refuses what HA's does, with its messages.
#[tokio::test]
async fn the_form_is_checked_before_the_step() {
    let home = home().await;
    let FlowResult::Form { flow_id, .. } = home
        .flows
        .init("switchbot_cloud", SOURCE_USER)
        .await
        .unwrap()
    else {
        panic!("no form");
    };
    let answer = |input: Value| {
        let input: Vec<(String, Value)> = input
            .as_array()
            .unwrap()
            .iter()
            .map(|pair| (pair[0].as_str().unwrap().to_owned(), pair[1].clone()))
            .collect();
        let (flows, flow_id) = (home.flows.clone(), flow_id.clone());
        async move {
            match flows.configure(&flow_id, Some(input)).await {
                Err(Error::InvalidData(errors)) => Value::Object(errors),
                other => panic!("{other:?}"),
            }
        }
    };
    assert_eq!(
        answer(json!([["api_key", "x"]])).await,
        json!({"api_token": "required key not provided"})
    );
    assert_eq!(
        answer(json!([
            ["api_token", 5],
            ["api_key", "x"],
            ["zz", 1],
            ["aa", 2]
        ]))
        .await,
        json!({"api_token": "expected str",
               "base": ["extra keys not allowed @ data['zz']",
                        "extra keys not allowed @ data['aa']"]})
    );
    assert!(
        home.api.take().is_empty(),
        "a refused form asks the API nothing"
    );
    assert_eq!(home.flows.in_progress(), 1);
}

/// A second flow for the same account aborts; one for another account
/// makes a second entry. Flows that don't exist are refused.
#[tokio::test]
async fn one_entry_per_account() {
    let home = home().await;
    let first = entry_id(&add(&home, TOKEN).await);
    let FlowResult::Abort {
        handler, reason, ..
    } = add(&home, TOKEN).await
    else {
        panic!("no abort");
    };
    assert_eq!((handler, reason), ("switchbot_cloud", "already_configured"));
    let second = entry_id(&add(&home, "token-two").await);
    assert_ne!(first, second);
    assert_eq!(home.entries.list().unwrap().len(), 2);
    assert_eq!(home.flows.in_progress(), 0);

    assert_eq!(
        home.flows.init("nope", SOURCE_USER).await,
        Err(Error::UnknownHandler)
    );
    assert_eq!(
        home.flows.init("switchbot_cloud", SOURCE_RECONFIGURE).await,
        Err(Error::UnknownStep(
            "Handler SwitchBotCloudConfigFlow doesn't support step reconfigure".into()
        ))
    );
    assert_eq!(
        home.flows.configure("nope", None).await,
        Err(Error::UnknownFlow)
    );
    assert_eq!(home.flows.abort("nope"), Err(Error::UnknownFlow));
    assert_eq!(home.flows.in_progress(), 0);
}

/// A flow that is left (the dialog closed) is dropped, and an empty list
/// holds no memory.
#[tokio::test]
async fn an_abandoned_flow_is_dropped() {
    let home = home().await;
    assert_eq!(home.flows.flows.lock().capacity(), 0);
    let mut ids = Vec::new();
    for _ in 0..3 {
        let FlowResult::Form { flow_id, .. } = home
            .flows
            .init("switchbot_cloud", SOURCE_USER)
            .await
            .unwrap()
        else {
            panic!("no form");
        };
        ids.push(flow_id);
    }
    assert_eq!(home.flows.in_progress(), 3);
    for id in &ids {
        assert_eq!(home.flows.abort(id), Ok(()));
        assert_eq!(home.flows.abort(id), Err(Error::UnknownFlow));
    }
    assert_eq!(home.flows.in_progress(), 0);
    assert_eq!(home.flows.flows.lock().capacity(), 0);
    // An answer that comes after the dialog closed finds no flow.
    assert_eq!(
        home.flows.configure(&ids[0], credentials(TOKEN)).await,
        Err(Error::UnknownFlow)
    );
    assert_eq!(home.entries.list(), Some(Vec::new()));
}

/// The entry's changes as `config_entries/subscribe` tells them, sent on
/// as (change, state, reason, `supports_unload`).
type Told = (&'static str, &'static str, Option<String>, bool);

fn told(home: &Home) -> tokio::sync::mpsc::UnboundedReceiver<Told> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let entries = home.entries.clone();
    let listener: Listener = Box::new(move |change: Change, entry: &Map<String, Value>| {
        let r = entries
            .runtime(entry["entry_id"].as_str().unwrap())
            .unwrap();
        tx.send((
            change.as_str(),
            r.state.as_str(),
            r.reason,
            r.integration_loaded,
        ))
        .is_ok()
    });
    home.entries
        .subscribe(Arc::new(Mutex::new(true)), listener, || {});
    rx
}

/// A setup that can't read a device: the entry is made, reports
/// `setup_retry` with HA's reason, and loads when HA's first retry (5 s
/// later) works, with the changes HA tells its subscribers
/// (`switchbot_cloud/vectors/retry.json`; HA's second `setup_in_progress`
/// in a row is its webhook id, which `hars` doesn't add).
#[tokio::test(start_paused = true)]
async fn an_entry_that_is_not_ready_is_retried() {
    let home = home().await;
    let mut changes = told(&home);
    // Real time for the sockets, until the retry's wait.
    tokio::time::resume();
    home.hass.start();
    let error = r#"{"statusCode":190,"body":{},"message":"error"}"#;
    let path = format!("/v1.1/devices/{BOT}/status");
    home.api
        .set("GET", &path, (200, "application/json", error.into()));
    let id = entry_id(&add(&home, TOKEN).await);
    assert_eq!(
        home.entries.runtime(&id),
        Some(crate::config_entries::Runtime {
            state: EntryState::SetupRetry,
            reason: Some("Error communicating with API: ".into()),
            integration_loaded: true,
        })
    );
    assert!(home.registry.snapshot().devices.is_empty());
    home.api.set(
        "GET",
        &path,
        (200, "application/json", fixture("bot").to_string()),
    );
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(6)).await;
    tokio::time::resume();
    let vector: Value =
        serde_json::from_str(include_str!("../switchbot_cloud/vectors/retry.json")).unwrap();
    let mut want: Vec<Told> = Vec::new();
    for c in vector["changes"].as_array().unwrap() {
        let leak = |v: &Value| -> &'static str { v.as_str().unwrap().to_owned().leak() };
        let change = (
            leak(&c["type"]),
            leak(&c["state"]),
            c["reason"].as_str().map(str::to_owned),
            c["supports_unload"].as_bool().unwrap(),
        );
        if want.last() != Some(&change) {
            want.push(change);
        }
    }
    let mut got = Vec::new();
    while got.len() < want.len() {
        let change = tokio::time::timeout(std::time::Duration::from_secs(5), changes.recv())
            .await
            .expect("the retry never finished")
            .unwrap();
        got.push(change);
    }
    assert_eq!(got, want);
    assert_eq!(home.entries.runtime(&id).unwrap().state, EntryState::Loaded);
    assert_eq!(home.registry.snapshot().devices.len(), 3);
}

/// A second account that lists the same devices: its entry loads, and the
/// devices and entities stay the first entry's, each running once, as in
/// HA (`switchbot_cloud/vectors/two_accounts.json`).
#[tokio::test]
async fn a_second_account_does_not_take_the_devices() {
    let home = home().await;
    let first = entry_id(&add(&home, TOKEN).await);
    let second = entry_id(&add(&home, "token-two").await);
    assert_eq!(
        home.entries.runtime(&second).map(|r| r.state),
        Some(EntryState::Loaded)
    );
    let want: Value =
        serde_json::from_str(include_str!("../switchbot_cloud/vectors/two_accounts.json")).unwrap();
    let (devices, entities) = rows(&home.registry, &first);
    assert_eq!(json!(devices), want["devices"]);
    assert_eq!(json!(entities), want["entities"]);
    for (entity_id, s) in want["states"].as_object().unwrap() {
        assert_eq!(state(&home.hass, entity_id), *s, "{entity_id}");
    }
    // One press, one command.
    home.api.take();
    let press = json!({"entity_id": "button.towel_switch"});
    home.hass
        .call_service(
            "button",
            "press",
            press.as_object().unwrap().clone(),
            None,
            false,
            None,
        )
        .await
        .unwrap();
    assert_eq!(home.api.take().len(), 1);
    // Removing the second entry leaves the first one's devices running.
    assert_eq!(home.flows.remove_entry(&second).await, Some(false));
    assert_eq!(home.registry.snapshot().entities.len(), 6);
    assert_eq!(
        state(&home.hass, "sensor.hub_2_9d_temperature")["state"],
        "21.7"
    );
}

/// A command still waiting for the API when its entry is removed writes
/// nothing when the answer comes: no state comes back.
#[tokio::test]
async fn a_command_in_flight_writes_nothing_after_removal() {
    let home = home().await;
    let id = entry_id(&add(&home, TOKEN).await);
    home.api.take();
    let hold = Arc::new(tokio::sync::Notify::new());
    *home.api.hold.lock() = Some(hold.clone());
    let call = {
        let hass = home.hass.clone();
        tokio::spawn(async move {
            let data = json!({"entity_id": "switch.air_conditioner"});
            hass.call_service(
                "switch",
                "turn_on",
                data.as_object().unwrap().clone(),
                None,
                false,
                None,
            )
            .await
        })
    };
    // The command has reached the API, which holds its answer.
    while home.api.count() < 1 {
        home.api.arrived.notified().await;
    }
    assert_eq!(home.flows.remove_entry(&id).await, Some(false));
    hold.notify_one();
    call.await.unwrap().unwrap();
    assert_eq!(state(&home.hass, "switch.air_conditioner"), Value::Null);
}

/// A delete that comes while the entry's first setup is still asking the
/// API waits for that attempt, as HA's removal waits on the entry's setup
/// lock: the flow's reply says what the setup made (a loaded entry), and
/// only then is the entry removed.
#[tokio::test]
async fn removing_an_entry_during_its_setup_waits_for_the_setup() {
    let home = home().await;
    let FlowResult::Form { flow_id, .. } = home
        .flows
        .init("switchbot_cloud", SOURCE_USER)
        .await
        .unwrap()
    else {
        panic!("no form");
    };
    let mut changes = told(&home);
    // Every request is held until let go.
    let hold = Arc::new(tokio::sync::Notify::new());
    *home.api.hold.lock() = Some(hold.clone());
    let answer = {
        let flows = home.flows.clone();
        tokio::spawn(async move { flows.configure(&flow_id, credentials(TOKEN)).await })
    };
    // The flow's own check of the credentials is let through...
    while home.api.count() < 1 {
        home.api.arrived.notified().await;
    }
    hold.notify_one();
    // ...the entry is added, and its setup's first request stays held.
    loop {
        let change = changes.recv().await.unwrap();
        if change.1 == "setup_in_progress" {
            break;
        }
    }
    while home.api.count() < 2 {
        home.api.arrived.notified().await;
    }
    let id = home.entries.list().unwrap()[0]["entry_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let removal = {
        let (flows, id) = (home.flows.clone(), id.clone());
        tokio::spawn(async move { flows.remove_entry(&id).await })
    };
    // The removal has nothing to do but wait: the entry stays, in setup.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(
        !removal.is_finished(),
        "the removal didn't wait for the setup"
    );
    assert_eq!(
        home.entries.runtime(&id).map(|r| r.state),
        Some(EntryState::SetupInProgress)
    );
    // The API answers from here on, and the setup ends.
    *home.api.hold.lock() = None;
    hold.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), answer)
        .await
        .expect("the flow's reply never came")
        .unwrap()
        .unwrap();
    // The reply is of the entry as its setup left it, though it may be
    // gone by the time the reply is read.
    let FlowResult::CreateEntry { runtime, entry, .. } = result else {
        panic!("no entry");
    };
    assert_eq!(entry["entry_id"], json!(id));
    let runtime = runtime.expect("the setup's state");
    assert_eq!(
        (runtime.state, runtime.integration_loaded),
        (EntryState::Loaded, true)
    );
    assert_eq!(removal.await.unwrap(), Some(false));
    assert_eq!(home.entries.list(), Some(Vec::new()));
    assert_eq!(home.entries.runtime(&id), None);
    assert_eq!(home.flows.tasks(), 0);
    let snap = home.registry.snapshot();
    assert!(snap.devices.is_empty() && snap.entities.is_empty());
    assert!(home.hass.states().all().is_empty());
}

/// Removing the entry stops its integration: the entities, their states
/// and the registry rows go, and nothing polls afterwards.
#[tokio::test]
async fn removing_an_entry_stops_it() {
    let home = home().await;
    let id = entry_id(&add(&home, TOKEN).await);
    assert_eq!(home.flows.tasks(), 1);
    let changes = Arc::new(Mutex::new(Vec::new()));
    let listener: Listener = {
        let (changes, entries) = (changes.clone(), home.entries.clone());
        Box::new(move |change: Change, entry: &Map<String, Value>| {
            let id = entry["entry_id"].as_str().unwrap();
            changes
                .lock()
                .push((change, entries.runtime(id).map(|r| r.state)));
            true
        })
    };
    home.entries
        .subscribe(Arc::new(Mutex::new(true)), listener, || {});

    assert_eq!(home.flows.remove_entry("nope").await, None);
    // `require_restart` is false: it was stopped.
    assert_eq!(home.flows.remove_entry(&id).await, Some(false));
    assert_eq!(home.flows.remove_entry(&id).await, None);
    assert_eq!(
        *changes.lock(),
        [
            (Change::Updated, Some(EntryState::UnloadInProgress)),
            (Change::Updated, Some(EntryState::NotLoaded)),
            (Change::Removed, Some(EntryState::NotLoaded)),
        ]
    );
    assert_eq!(home.entries.list(), Some(Vec::new()));
    assert_eq!(home.entries.runtime(&id), None);
    assert_eq!(home.flows.tasks(), 0);
    let snap = home.registry.snapshot();
    assert!(snap.devices.is_empty() && snap.entities.is_empty());
    for entity_id in fresh()["states"].as_object().unwrap().keys() {
        assert_eq!(state(&home.hass, entity_id), Value::Null, "{entity_id}");
    }
    // Its services no longer find the entities.
    home.api.take();
    home.hass
        .call_service(
            "switch",
            "turn_on",
            json!({"entity_id": "switch.air_conditioner"})
                .as_object()
                .unwrap()
                .clone(),
            None,
            false,
            None,
        )
        .await
        .unwrap();
    assert!(home.api.take().is_empty());

    // Added again, the devices come back with their entity ids.
    let again = entry_id(&add(&home, TOKEN).await);
    assert_ne!(again, id);
    let (_, entities) = rows(&home.registry, &again);
    let ids = |rows: &Value| -> Vec<String> {
        rows.as_array()
            .unwrap()
            .iter()
            .map(|e| e["entity_id"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(ids(&json!(entities)), ids(&fresh()["entities"]));
}

/// Two accounts that list the same devices, set up at the same moment:
/// each entity still has one owner and runs once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_accounts_at_once_share_no_entity() {
    for _ in 0..8 {
        let home = home().await;
        let (a, b) = tokio::join!(add(&home, TOKEN), add(&home, "token-two"));
        let (a, b) = (entry_id(&a), entry_id(&b));
        let snap = home.registry.snapshot();
        assert_eq!(snap.entities.len(), 6);
        assert_eq!(snap.devices.len(), 3);
        for e in snap.entities.iter() {
            let owner = e.config_entry_id.as_deref().unwrap();
            assert!(owner == a || owner == b);
        }
        home.api.take();
        let press = json!({"entity_id": "button.towel_switch"});
        home.hass
            .call_service(
                "button",
                "press",
                press.as_object().unwrap().clone(),
                None,
                false,
                None,
            )
            .await
            .unwrap();
        assert_eq!(home.api.take().len(), 1, "one press, one command");
        // Removing both leaves nothing running.
        assert_eq!(home.flows.remove_entry(&a).await, Some(false));
        assert_eq!(home.flows.remove_entry(&b).await, Some(false));
        assert!(home.hass.states().all().is_empty());
    }
}

/// A delete that lands at any point of the entry's setup (while it asks
/// the API, while it makes its rows and entities, or just after) leaves
/// nothing of the entry: no row, no state, no entity behind a service.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delete_at_any_point_of_setup_leaves_nothing() {
    for yields in 0..24 {
        let home = home().await;
        let mut changes = told(&home);
        let adding = {
            let flows = home.flows.clone();
            tokio::spawn(async move {
                let FlowResult::Form { flow_id, .. } =
                    flows.init("switchbot_cloud", SOURCE_USER).await.unwrap()
                else {
                    panic!("no form");
                };
                flows.configure(&flow_id, credentials(TOKEN)).await
            })
        };
        loop {
            let change = changes.recv().await.unwrap();
            if change.1 == "setup_in_progress" {
                break;
            }
        }
        for _ in 0..yields {
            tokio::task::yield_now().await;
        }
        let id = home.entries.list().unwrap()[0]["entry_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(home.flows.remove_entry(&id).await, Some(false));
        assert!(matches!(
            adding.await.unwrap(),
            Ok(FlowResult::CreateEntry { .. })
        ));
        assert_eq!(
            home.entries.list(),
            Some(Vec::new()),
            "after {yields} yields"
        );
        assert!(home.hass.states().all().is_empty(), "after {yields} yields");
        let snap = home.registry.snapshot();
        assert!(snap.entities.is_empty() && snap.devices.is_empty());
        assert_eq!(home.flows.tasks(), 0);
        // No entity is left behind a service.
        home.api.take();
        let data = json!({"entity_id": "switch.air_conditioner"});
        home.hass
            .call_service(
                "switch",
                "turn_on",
                data.as_object().unwrap().clone(),
                None,
                false,
                None,
            )
            .await
            .unwrap();
        assert_eq!(home.api.count(), 0, "after {yields} yields");
    }
}

/// A delete whose request goes away half way is still carried out.
#[tokio::test]
async fn a_delete_that_is_dropped_still_finishes() {
    let home = home().await;
    let id = entry_id(&add(&home, TOKEN).await);
    {
        let removal = home.flows.remove_entry(&id);
        tokio::pin!(removal);
        // Started (polled once), then dropped, as a disconnect drops it.
        tokio::select! {
            biased;
            _ = &mut removal => {}
            _ = std::future::ready(()) => {}
        }
    }
    // The next delete waits its turn and finds the entry gone.
    assert_eq!(home.flows.remove_entry(&id).await, None);
    assert_eq!(home.entries.list(), Some(Vec::new()));
    assert_eq!(home.flows.tasks(), 0);
    assert!(home.hass.states().all().is_empty());
    assert!(home.registry.snapshot().entities.is_empty());
}

/// A disabled entry runs nothing, so removing it needs no restart (HA
/// unloads nothing for an entry that isn't loaded); the placeholder
/// states start-up wrote for its registry rows go with the rows.
#[tokio::test]
async fn removing_a_disabled_entry_needs_no_restart() {
    let home = home().await;
    let id = entry_id(&add(&home, TOKEN).await);
    let rows = (*home.registry.snapshot()).clone();
    let mut stored = home.entries.list().unwrap();
    stored[0].insert("disabled_by".into(), json!("user"));

    // The same install after a restart, with the entry disabled.
    let hass = Hass::new();
    let registry = Registry::in_memory(&hass, rows);
    let entries = ConfigEntries::in_memory(&hass, stored.clone());
    let started = crate::start(
        &hass,
        registry.clone().into(),
        Some(&stored),
        Profiles::default(),
        &Endpoints {
            switchbot: home.base.clone(),
        },
    );
    assert!(started.tasks.is_empty());
    registry.snapshot().write_unavailable_states(&hass);
    assert_eq!(
        state(&hass, "switch.air_conditioner")["attributes"]["restored"],
        true
    );
    let flows = Manager::new(
        &hass,
        entries.clone(),
        started.components,
        Endpoints {
            switchbot: home.base.clone(),
        },
        started.tasks,
        None,
    )
    .unwrap();
    assert_eq!(flows.remove_entry(&id).await, Some(false));
    assert!(hass.states().all().is_empty());
    assert!(registry.snapshot().entities.is_empty());
    assert_eq!(entries.list(), Some(Vec::new()));
}

/// Removing a companion app's entry stops its registration: the webhook
/// no longer answers as registered (a deleted phone can't keep writing),
/// and no restart is needed, as HA unloads `mobile_app`.
#[tokio::test]
async fn removing_a_companion_app_entry_stops_its_webhook() {
    use crate::mobile_app::tests::{WEBHOOK, entry as app_entry};
    use crate::mobile_app::{MobileApp, Webhook};
    let hass = Hass::new();
    let stored = vec![app_entry(true)];
    let id = stored[0]["entry_id"].as_str().unwrap().to_owned();
    let entries = ConfigEntries::in_memory(&hass, stored.clone());
    let registry = Registry::in_memory(&hass, Default::default());
    let app = Arc::new(MobileApp::new(&stored, Vec::new()));
    app.setup(&hass, registry.clone(), entries.clone(), None);
    assert!(matches!(app.webhook(WEBHOOK), Webhook::Registered(_)));
    let flows = Manager::new(
        &hass,
        entries.clone(),
        Components::new(&hass, registry, Profiles::default()),
        Endpoints::default(),
        Vec::new(),
        Some(app.clone()),
    )
    .unwrap();
    assert_eq!(flows.remove_entry(&id).await, Some(false));
    assert!(!matches!(app.webhook(WEBHOOK), Webhook::Registered(_)));
    assert!(app.is_empty());
    assert_eq!(entries.list(), Some(Vec::new()));
}

/// An entry whose integration has no way to stop is removed as HA removes
/// one that can't unload: `require_restart`.
#[tokio::test]
async fn an_entry_that_cannot_stop_needs_a_restart() {
    let home = home().await;
    let sun = home
        .entries
        .new_entry("sun", "import", "Sun", None, Map::new());
    let id = sun["entry_id"].as_str().unwrap().to_owned();
    home.entries.add(sun);
    assert_eq!(home.flows.remove_entry(&id).await, Some(true));
    assert_eq!(home.entries.list(), Some(Vec::new()));
}

/// The entry and its registry rows are what a restart starts from: the
/// same entity ids, with nothing made twice.
#[tokio::test]
async fn a_restart_keeps_the_entity_ids() {
    let home = home().await;
    let id = entry_id(&add(&home, TOKEN).await);
    let stored = home.entries.list().unwrap();
    let before = home.registry.snapshot();

    let hass = Hass::new();
    let registry = Registry::in_memory(&hass, (*before).clone());
    let mut events = hass.bus().subscribe();
    let started = crate::start(
        &hass,
        registry.clone().into(),
        Some(&stored),
        Profiles::default(),
        &Endpoints {
            switchbot: home.base.clone(),
        },
    );
    hass.setups_done().await;
    assert_eq!(started.tasks.len(), 1);
    assert_eq!(started.tasks[0].entry_id, id);
    for (entity_id, s) in fresh()["states"].as_object().unwrap() {
        assert_eq!(state(&hass, entity_id), *s, "{entity_id}");
    }
    // The rows are as they were: the setup found them and changed nothing.
    let after = registry.snapshot();
    assert_eq!(
        serde_json::to_value(&after.devices).unwrap(),
        serde_json::to_value(&before.devices).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&after.entities).unwrap(),
        serde_json::to_value(&before.entities).unwrap()
    );
    while let Ok(event) = events.try_recv() {
        if let ha_core::EventKind::Custom { name, .. } = &event.kind {
            assert!(!name.ends_with("registry_updated"), "{name}");
        }
    }
}

/// The three stores on disk, as a restart reads them.
struct Disk {
    dir: std::path::PathBuf,
}

impl Disk {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("hars-flow-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".storage")).unwrap();
        Self { dir }
    }

    /// `hars` started on the directory: its entries, registry and flows
    /// (`None` when they aren't offered), with the entries started.
    fn start(
        &self,
        hass: &Hass,
        base: &str,
    ) -> (Arc<ConfigEntries>, Arc<Registry>, Option<Arc<Manager>>) {
        let entries = ConfigEntries::load(hass, &self.dir);
        let loaded = ha_registry::Registries::load(&self.dir).unwrap();
        let registry = Registry::new(hass, &self.dir, loaded);
        let lookup = entries.clone();
        registry.set_entry_lookup(Arc::new(move |id: &str| lookup.info(id)));
        let endpoints = Endpoints {
            switchbot: base.to_owned(),
        };
        let stored = entries.list();
        let started = crate::start(
            hass,
            registry.clone().into(),
            stored.as_deref(),
            Profiles::default(),
            &endpoints,
        );
        let flows = Manager::new(
            hass,
            entries.clone(),
            started.components,
            endpoints,
            started.tasks,
            None,
        );
        (entries, registry, flows)
    }

    fn file(&self, key: &str) -> String {
        std::fs::read_to_string(self.dir.join(".storage").join(key)).unwrap()
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// What a flow adds and what a removal takes away reach all three stores:
/// after each, the files are flushed and read again as a restart reads
/// them, and the entry, its devices and its entities agree.
#[tokio::test]
async fn a_restart_from_disk_finds_what_a_flow_and_a_removal_left() {
    let disk = Disk::new("stores");
    let (base, _api) = fake().await;
    let hass = Hass::new();
    let (entries, registry, flows) = disk.start(&hass, &base);
    let flows = flows.expect("new stores can be written");
    let FlowResult::Form { flow_id, .. } =
        flows.init("switchbot_cloud", SOURCE_USER).await.unwrap()
    else {
        panic!("no form");
    };
    let id = entry_id(&flows.configure(&flow_id, credentials(TOKEN)).await.unwrap());
    entries.flush().await;
    registry.flush().await;

    // A restart: the entry starts from the files, with the same entities.
    let hass = Hass::new();
    let (entries, registry, flows) = disk.start(&hass, &base);
    hass.setups_done().await;
    assert_eq!(entries.list().unwrap().len(), 1);
    assert_eq!(entries.list().unwrap()[0]["entry_id"], json!(id));
    let snap = registry.snapshot();
    assert_eq!((snap.devices.len(), snap.entities.len()), (3, 6));
    assert!(
        snap.entities
            .iter()
            .all(|e| e.config_entry_id.as_deref() == Some(id.as_str()))
    );
    for (entity_id, s) in fresh()["states"].as_object().unwrap() {
        assert_eq!(state(&hass, entity_id), *s, "{entity_id}");
    }
    // Removed, flushed and read again: nothing points at the entry.
    let flows = flows.expect("its stores can be written");
    assert_eq!(flows.remove_entry(&id).await, Some(false));
    entries.flush().await;
    registry.flush().await;
    let hass = Hass::new();
    let (entries, registry, _) = disk.start(&hass, &base);
    assert_eq!(entries.list(), Some(Vec::new()));
    let snap = registry.snapshot();
    assert!(snap.devices.is_empty() && snap.entities.is_empty());
    assert!(
        !disk
            .file("core.entity_registry")
            .contains(&format!("\"config_entry_id\":\"{id}\""))
    );
}

/// A registry file of another minor version is never overwritten, so
/// what a removal (or a flow) changed in it would be lost at a restart
/// while the entry's own file was saved: the devices and entities would
/// come back pointing at an entry that is gone. So neither is offered.
#[tokio::test]
async fn flows_and_removal_need_all_three_stores_writable() {
    for key in ["core.entity_registry", "core.device_registry"] {
        let disk = Disk::new(key);
        let (base, _api) = fake().await;
        let hass = Hass::new();
        let (entries, registry, flows) = disk.start(&hass, &base);
        let flows = flows.unwrap();
        let FlowResult::Form { flow_id, .. } =
            flows.init("switchbot_cloud", SOURCE_USER).await.unwrap()
        else {
            panic!("no form");
        };
        let id = entry_id(&flows.configure(&flow_id, credentials(TOKEN)).await.unwrap());
        entries.flush().await;
        registry.flush().await;
        // The same install, its registry file as an older release left it.
        let text = disk.file(key);
        let minor = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("\"minor_version\": "))
            .and_then(|v| v.trim_end_matches(',').parse::<u32>().ok())
            .unwrap();
        let older = text.replacen(
            &format!("\"minor_version\": {minor}"),
            &format!("\"minor_version\": {}", minor - 1),
            1,
        );
        std::fs::write(disk.dir.join(".storage").join(key), &older).unwrap();

        let hass = Hass::new();
        let (entries, registry, flows) = disk.start(&hass, &base);
        assert_eq!(registry.read_only(), Some(key));
        assert!(flows.is_none(), "{key}: flows and removal were offered");
        // Nothing can have been half removed: the three files still agree.
        hass.setups_done().await;
        entries.flush().await;
        registry.flush().await;
        let hass = Hass::new();
        let (entries, registry, _) = disk.start(&hass, &base);
        assert_eq!(entries.list().unwrap()[0]["entry_id"], json!(id));
        assert_eq!(registry.snapshot().entities.len(), 6);
        assert_eq!(disk.file(key), older, "{key} was overwritten");
    }
}

/// An account that was disabled, its devices and entities with it
/// (`disabled_by: config_entry`), then a second account that lists the
/// same devices: the rows become the second account's and are enabled, as
/// in HA (`switchbot_cloud/vectors/disabled_account.json`: the stores
/// before and after).
#[tokio::test]
async fn an_enabled_account_takes_over_a_disabled_ones_devices() {
    let vector: Value = serde_json::from_str(include_str!(
        "../switchbot_cloud/vectors/disabled_account.json"
    ))
    .unwrap();
    let before = &vector["before"];
    let stored: Vec<Map<String, Value>> =
        serde_json::from_value(before["entries"].clone()).unwrap();
    assert_eq!(stored[0]["disabled_by"], "user");
    let rows = ha_registry::Registries {
        devices: serde_json::from_value(before["devices"].clone()).unwrap(),
        entities: serde_json::from_value(before["entities"].clone()).unwrap(),
        ..Default::default()
    };
    // What the account's disable disabled, and what the user had: a Bot
    // (its entities with it) and the Hub's humidity sensor.
    let disabled = |rows: &Value, key: &str| -> Vec<(String, Value)> {
        rows.as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r[key].as_str().unwrap().to_owned(),
                    r["disabled_by"].clone(),
                )
            })
            .collect()
    };
    let by_user: Vec<(String, Value)> = disabled(&before["entities"], "entity_id")
        .into_iter()
        .chain(disabled(&before["devices"], "name"))
        .filter(|(_, by)| by != "config_entry")
        .collect();
    assert_eq!(
        by_user,
        [
            ("button.towel_switch".to_owned(), json!("device")),
            ("sensor.hub_2_9d_humidity".to_owned(), json!("user")),
            ("sensor.towel_switch_battery".to_owned(), json!("device")),
            ("Towel switch".to_owned(), json!("user")),
        ]
    );
    let hass = Hass::new();
    let (base, _api) = fake().await;
    let registry = Registry::in_memory(&hass, rows);
    let entries = ConfigEntries::in_memory(&hass, stored.clone());
    let lookup = entries.clone();
    registry.set_entry_lookup(Arc::new(move |id: &str| lookup.info(id)));
    let endpoints = Endpoints { switchbot: base };
    let started = crate::start(
        &hass,
        registry.clone().into(),
        Some(&stored),
        Profiles::default(),
        &endpoints,
    );
    assert!(started.tasks.is_empty(), "a disabled entry isn't started");
    let flows = Manager::new(
        &hass,
        entries.clone(),
        started.components,
        endpoints,
        started.tasks,
        None,
    )
    .unwrap();
    let FlowResult::Form { flow_id, .. } =
        flows.init("switchbot_cloud", SOURCE_USER).await.unwrap()
    else {
        panic!("no form");
    };
    let second = entry_id(
        &flows
            .configure(&flow_id, credentials("token-two"))
            .await
            .unwrap(),
    );
    let after = &vector["after"];
    let (devices, entities) = rows_as(&registry, &second, "<entry:2>");
    assert_eq!(json!(devices), after["devices"]);
    assert_eq!(json!(entities), after["entities"]);
    for (entity_id, s) in after["states"].as_object().unwrap() {
        assert_eq!(state(&hass, entity_id), *s, "{entity_id}");
    }
    // The user's disables outlive the takeover: still disabled in the
    // rows (compared above), and nothing runs for them.
    let still: Vec<(String, Value)> = disabled(&json!(entities), "entity_id")
        .into_iter()
        .chain(disabled(&json!(devices), "name"))
        .filter(|(_, by)| !by.is_null())
        .collect();
    assert_eq!(still, by_user);
    for (entity_id, _) in &by_user[..3] {
        assert_eq!(state(&hass, entity_id), Value::Null, "{entity_id}");
    }
    assert_eq!(hass.states().all().len(), 3);
}

/// Start-up's placeholder pass and a removal at the same time: the pass
/// is writing `unavailable` for every registered entity nothing provides
/// when a disabled entry is deleted. Whichever comes first, no state is
/// left for a row that is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delete_during_the_placeholder_pass_leaves_no_state() {
    for _ in 0..5 {
        let hass = Hass::new();
        // Many rows of an entry that stays, then one of the entry that
        // goes: the pass is still busy with the first when the removal
        // has come and gone.
        let row = |i: usize, entry: &str| {
            Arc::new(ha_registry::Entity {
                entity_id: format!("sensor.row_{i}"),
                platform: "switchbot_cloud".into(),
                unique_id: Some(json!(format!("row-{i}"))),
                config_entry_id: Some(entry.to_owned()),
                ..Default::default()
            })
        };
        let mut entities: Vec<_> = (0..4000).map(|i| row(i, "stays")).collect();
        entities.push(row(4000, "goes"));
        let registry = Registry::in_memory(
            &hass,
            ha_registry::Registries {
                entities,
                ..Default::default()
            },
        );
        let entry = |id: &str| {
            json!({"entry_id": id, "domain": "switchbot_cloud", "disabled_by": "user",
                   "data": {}, "title": id})
            .as_object()
            .unwrap()
            .clone()
        };
        let entries = ConfigEntries::in_memory(&hass, vec![entry("stays"), entry("goes")]);
        let flows = Manager::new(
            &hass,
            entries,
            Components::new(&hass, registry.clone(), Profiles::default()),
            Endpoints::default(),
            Vec::new(),
            None,
        )
        .unwrap();
        let pass = {
            let (hass, registry) = (hass.clone(), registry.clone());
            tokio::task::spawn_blocking(move || registry.write_unavailable_states(&hass))
        };
        // The pass has begun (it has read the rows and written some).
        while hass.states().all().is_empty() {
            std::hint::spin_loop();
        }
        assert_eq!(flows.remove_entry("goes").await, Some(false));
        pass.await.unwrap();
        let gone = EntityId::parse("sensor.row_4000").unwrap();
        assert!(
            hass.states().get(&gone).is_none(),
            "a placeholder was written for a row that was removed"
        );
        assert_eq!(hass.states().all().len(), 4000);
    }
}

/// Flows nobody answers don't pile up: at most `MAX_FLOWS` are kept (the
/// longest untouched goes when one more starts), one left alone for
/// `EXPIRES` goes when the next starts, and the list of flows no user
/// started is made without copying the others.
#[tokio::test(start_paused = true)]
async fn abandoned_flows_are_capped_and_expire() {
    let home = home().await;
    let start = || async {
        let FlowResult::Form { flow_id, .. } = home
            .flows
            .init("switchbot_cloud", SOURCE_USER)
            .await
            .unwrap()
        else {
            panic!("no form");
        };
        // Each a moment after the last, so "longest untouched" is clear.
        tokio::time::advance(Duration::from_secs(1)).await;
        flow_id
    };
    let mut ids = Vec::new();
    for _ in 0..MAX_FLOWS {
        ids.push(start().await);
    }
    assert_eq!(home.flows.in_progress(), MAX_FLOWS);
    // The first is shown again: it is no longer the longest untouched.
    assert!(home.flows.configure(&ids[0], None).await.is_ok());
    let newest = start().await;
    assert_eq!(home.flows.in_progress(), MAX_FLOWS);
    assert!(home.flows.configure(&ids[0], None).await.is_ok());
    assert_eq!(
        home.flows.configure(&ids[1], None).await,
        Err(Error::UnknownFlow),
        "the longest untouched flow was kept"
    );
    assert!(home.flows.configure(&newest, None).await.is_ok());
    // A user's flows are not what `config_entries/flow/progress` lists.
    assert!(home.flows.progress().is_empty());

    // An hour on, the next flow to start finds the rest expired.
    tokio::time::advance(EXPIRES).await;
    assert_eq!(
        home.flows.in_progress(),
        MAX_FLOWS,
        "nothing wakes to prune"
    );
    let fresh = start().await;
    assert_eq!(home.flows.in_progress(), 1);
    assert!(home.flows.configure(&fresh, None).await.is_ok());
    assert_eq!(
        home.flows.configure(&newest, None).await,
        Err(Error::UnknownFlow)
    );
    home.flows.abort(&fresh).unwrap();
    assert_eq!(home.flows.flows.lock().capacity(), 0);
}

/// A flow whose answer is being checked is never the one dropped to make
/// room, however many forms are opened meanwhile: its answer comes back
/// and its entry is made. When every flow is at work, a new one is
/// refused rather than started outside the limit.
#[tokio::test]
async fn a_flow_at_work_is_not_dropped_to_make_room() {
    let home = home().await;
    let start = || async {
        match home.flows.init("switchbot_cloud", SOURCE_USER).await {
            Ok(FlowResult::Form { flow_id, .. }) => Ok(flow_id),
            Ok(other) => panic!("{other:?}"),
            Err(e) => Err(e),
        }
    };
    let hold = Arc::new(tokio::sync::Notify::new());
    *home.api.hold.lock() = Some(hold.clone());
    // The first flow's credentials are with the API, which holds its
    // answer.
    let first = start().await.unwrap();
    let answer = {
        let (flows, first) = (home.flows.clone(), first.clone());
        tokio::spawn(async move { flows.configure(&first, credentials(TOKEN)).await })
    };
    while home.api.count() < 1 {
        home.api.arrived.notified().await;
    }
    // Sixteen more forms: the oldest idle ones go, never the one at work.
    let mut others = Vec::new();
    for _ in 0..MAX_FLOWS {
        others.push(start().await.unwrap());
    }
    assert_eq!(home.flows.in_progress(), MAX_FLOWS);
    assert_eq!(
        home.flows.configure(&others[0], None).await,
        Err(Error::UnknownFlow),
        "an idle flow made room"
    );
    // Every remaining flow put to work too: no room can be made.
    let mut answers = Vec::new();
    for (n, id) in others[1..].iter().enumerate() {
        let (flows, id) = (home.flows.clone(), id.clone());
        answers.push(tokio::spawn(async move {
            flows
                .configure(&id, credentials(&format!("token-{n}")))
                .await
        }));
    }
    while home.api.count() < MAX_FLOWS {
        home.api.arrived.notified().await;
    }
    assert_eq!(start().await, Err(Error::Busy));
    assert_eq!(home.flows.in_progress(), MAX_FLOWS);

    // The API answers: every submission ends as it should.
    *home.api.hold.lock() = None;
    hold.notify_waiters();
    for _ in 0..MAX_FLOWS {
        hold.notify_one();
    }
    let result = answer.await.unwrap();
    assert!(
        matches!(result, Ok(FlowResult::CreateEntry { .. })),
        "the first submission was lost: {result:?}"
    );
    for answer in answers {
        assert!(matches!(
            answer.await.unwrap(),
            Ok(FlowResult::CreateEntry { .. })
        ));
    }
    assert_eq!(home.entries.list().unwrap().len(), MAX_FLOWS);
    assert_eq!(home.flows.in_progress(), 0);
    // With room again, a flow starts.
    assert!(start().await.is_ok());
}

/// The entry's setup lock is its task's, not the request's that added it:
/// an entry started at start-up has one too, and a removal waits for its
/// first attempt as it does for a flow's.
#[tokio::test]
async fn removing_a_boot_time_entry_waits_for_its_first_attempt() {
    let home = home().await;
    let id = entry_id(&add(&home, TOKEN).await);
    let stored = home.entries.list().unwrap();
    let rows = (*home.registry.snapshot()).clone();

    // The same install starting again, the API slow to answer.
    let hass = Hass::new();
    let registry = Registry::in_memory(&hass, rows);
    let entries = ConfigEntries::in_memory(&hass, stored.clone());
    let endpoints = Endpoints {
        switchbot: home.base.clone(),
    };
    home.api.take();
    let hold = Arc::new(tokio::sync::Notify::new());
    *home.api.hold.lock() = Some(hold.clone());
    let started = crate::start(
        &hass,
        registry.clone().into(),
        Some(&stored),
        Profiles::default(),
        &endpoints,
    );
    let flows = Manager::new(
        &hass,
        entries.clone(),
        started.components,
        endpoints,
        started.tasks,
        None,
    )
    .unwrap();
    while home.api.count() < 1 {
        home.api.arrived.notified().await;
    }
    let removal = {
        let (flows, id) = (flows.clone(), id.clone());
        tokio::spawn(async move { flows.remove_entry(&id).await })
    };
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(
        !removal.is_finished(),
        "the removal didn't wait for the entry's first attempt"
    );
    assert_eq!(entries.list().unwrap().len(), 1);
    *home.api.hold.lock() = None;
    hold.notify_one();
    assert_eq!(removal.await.unwrap(), Some(false));
    assert_eq!(entries.list(), Some(Vec::new()));
    assert_eq!(flows.tasks(), 0);
    assert!(hass.states().all().is_empty());
    assert!(registry.snapshot().entities.is_empty());
}

/// The client that added an entry disconnects while its setup still
/// runs: the setup goes on, and still holds the entry's lock, so a
/// removal waits for it all the same.
#[tokio::test]
async fn a_removal_waits_for_a_setup_whose_request_went_away() {
    let home = home().await;
    let FlowResult::Form { flow_id, .. } = home
        .flows
        .init("switchbot_cloud", SOURCE_USER)
        .await
        .unwrap()
    else {
        panic!("no form");
    };
    let mut changes = told(&home);
    let hold = Arc::new(tokio::sync::Notify::new());
    *home.api.hold.lock() = Some(hold.clone());
    let answer = {
        let flows = home.flows.clone();
        tokio::spawn(async move { flows.configure(&flow_id, credentials(TOKEN)).await })
    };
    while home.api.count() < 1 {
        home.api.arrived.notified().await;
    }
    hold.notify_one();
    loop {
        let change = changes.recv().await.unwrap();
        if change.1 == "setup_in_progress" {
            break;
        }
    }
    while home.api.count() < 2 {
        home.api.arrived.notified().await;
    }
    // The request is dropped, as a disconnect drops it.
    answer.abort();
    let _ = answer.await;
    let id = home.entries.list().unwrap()[0]["entry_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let removal = {
        let (flows, id) = (home.flows.clone(), id.clone());
        tokio::spawn(async move { flows.remove_entry(&id).await })
    };
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(
        !removal.is_finished(),
        "the removal didn't wait for a setup nobody waits on"
    );
    assert_eq!(
        home.entries.runtime(&id).map(|r| r.state),
        Some(EntryState::SetupInProgress)
    );
    *home.api.hold.lock() = None;
    hold.notify_one();
    assert_eq!(removal.await.unwrap(), Some(false));
    assert_eq!(home.entries.list(), Some(Vec::new()));
    assert!(home.hass.states().all().is_empty());
    assert!(home.registry.snapshot().entities.is_empty());
}

/// What a flow added and what a removal took away are on disk when the
/// answer comes, in all three stores: nobody waits for their timers (1 s
/// for the entries, 10 s for a registry, 180 s while starting), during
/// which a crash would leave rows pointing at a missing entry.
#[tokio::test]
async fn the_stores_are_written_before_the_answer() {
    let disk = Disk::new("saved-now");
    let (base, _api) = fake().await;
    let hass = Hass::new();
    let (_entries, _registry, flows) = disk.start(&hass, &base);
    let flows = flows.unwrap();
    let FlowResult::Form { flow_id, .. } =
        flows.init("switchbot_cloud", SOURCE_USER).await.unwrap()
    else {
        panic!("no form");
    };
    let id = entry_id(&flows.configure(&flow_id, credentials(TOKEN)).await.unwrap());
    let owned = format!("\"config_entry_id\":\"{id}\"");
    // Read straight from the files, with no flush and no time passing.
    assert!(disk.file("core.config_entries").contains(&id));
    assert_eq!(disk.file("core.entity_registry").matches(&owned).count(), 6);
    assert_eq!(disk.file("core.device_registry").matches(&id).count(), 9);

    assert_eq!(flows.remove_entry(&id).await, Some(false));
    assert!(!disk.file("core.config_entries").contains(&id));
    assert!(!disk.file("core.entity_registry").contains(&owned));
    let devices: Value = serde_json::from_str(&disk.file("core.device_registry")).unwrap();
    assert_eq!(devices["data"]["devices"], json!([]));
}
