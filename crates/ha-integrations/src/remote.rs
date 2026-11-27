//! The `remote` entity component (`components/remote/__init__.py`): `turn_on`,
//! `turn_off`, `toggle`, `send_command`, `learn_command`, and `delete_command`.

use crate::light::BoxFuture;
use crate::switch::select;
use ha_core::{EntityId, Hass, Kind, Range, Schema, ServiceError, SupportsResponse};
use parking_lot::RwLock;
use serde_json::Value;
use std::sync::Arc;

pub const DOMAIN: &str = "remote";

/// `RemoteEntityFeature` (`components/remote/__init__.py`).
pub mod feature {
    pub const LEARN_COMMAND: u32 = 1;
    pub const DELETE_COMMAND: u32 = 2;
    pub const ACTIVITY: u32 = 4;
}

pub const DEFAULT_NUM_REPEATS: u64 = 1;
pub const DEFAULT_HOLD_SECS: f64 = 0.0;

/// Validated arguments for `remote.send_command`.
#[derive(Debug, Clone, PartialEq)]
pub struct SendCommand {
    pub command: Vec<String>,
    pub device: Option<String>,
    pub num_repeats: u64,
    pub delay_secs: Option<f64>,
    pub hold_secs: f64,
}

/// Validated arguments for `remote.learn_command`.
#[derive(Debug, Clone, PartialEq)]
pub struct LearnCommand {
    pub device: Option<String>,
    pub command: Option<Vec<String>>,
    pub command_type: Option<String>,
    pub alternative: Option<bool>,
    pub timeout: Option<u64>,
}

/// Validated arguments for `remote.delete_command`.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteCommand {
    pub command: Vec<String>,
    pub device: Option<String>,
}

/// A method the entity doesn't implement (Python's `NotImplementedError`).
fn unsupported<'a>() -> BoxFuture<'a> {
    Box::pin(async { Err(ServiceError::Unknown(String::new())) })
}

/// A remote entity a platform runs (`RemoteEntity`).
pub trait Remote: Send + Sync {
    fn entity_id(&self) -> &EntityId;
    fn available(&self) -> bool;
    fn is_on(&self) -> bool;

    fn turn_on(&self, _activity: Option<&str>) -> BoxFuture<'_> {
        unsupported()
    }
    fn turn_off(&self, _activity: Option<&str>) -> BoxFuture<'_> {
        unsupported()
    }
    fn toggle(&self, activity: Option<&str>) -> BoxFuture<'_> {
        if self.is_on() {
            self.turn_off(activity)
        } else {
            self.turn_on(activity)
        }
    }
    fn send_command(&self, _cmd: SendCommand) -> BoxFuture<'_> {
        unsupported()
    }
    fn learn_command(&self, _cmd: LearnCommand) -> BoxFuture<'_> {
        unsupported()
    }
    fn delete_command(&self, _cmd: DeleteCommand) -> BoxFuture<'_> {
        unsupported()
    }
}

/// The running remote entities, for the services.
#[derive(Default)]
pub struct Remotes {
    items: RwLock<Vec<Arc<dyn Remote>>>,
}

fn string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

impl Remotes {
    pub fn add(&self, remote: Arc<dyn Remote>) {
        let mut items = self.items.write();
        items.retain(|r| r.entity_id() != remote.entity_id());
        items.push(remote);
    }

    pub fn remove(&self, entity_id: &EntityId) {
        self.items.write().retain(|r| r.entity_id() != entity_id);
    }

    /// Register all 6 `remote.*` services on `hass`.
    ///
    /// Note: in HA 2026.6.1 (`components/remote/__init__.py`), none of the 6
    /// services pass `required_features` to `async_register_entity_service`.
    pub fn register_services(
        self: &Arc<Self>,
        hass: &Hass,
        registries: impl Into<ha_registry::Lookup>,
    ) {
        let registries = registries.into();
        let entity = Schema::entity_service;
        let pos_int = || {
            Kind::Int(Range {
                min: Some(0.0),
                max: None,
            })
        };

        let services: [(&'static str, Schema); 6] = [
            ("turn_on", entity().optional("activity", Kind::String)),
            ("turn_off", entity().optional("activity", Kind::String)),
            ("toggle", entity().optional("activity", Kind::String)),
            (
                "send_command",
                entity()
                    .required("command", Kind::List(Box::new(Kind::String)))
                    .optional("device", Kind::String)
                    .optional("num_repeats", pos_int())
                    .optional("delay_secs", Kind::Float(Range::ANY))
                    .optional("hold_secs", Kind::Float(Range::ANY)),
            ),
            (
                "learn_command",
                entity()
                    .optional("device", Kind::String)
                    .optional("command", Kind::List(Box::new(Kind::String)))
                    .optional("command_type", Kind::String)
                    .optional("alternative", Kind::Boolean)
                    .optional("timeout", pos_int()),
            ),
            (
                "delete_command",
                entity()
                    .required("command", Kind::List(Box::new(Kind::String)))
                    .optional("device", Kind::String),
            ),
        ];

        for (name, schema) in services {
            let weak = Arc::downgrade(self);
            let reg = registries.clone();
            hass.services().register_with_schema(
                DOMAIN,
                name,
                Some(schema),
                SupportsResponse::None,
                move |call| {
                    let weak = weak.clone();
                    let reg = reg.clone();
                    async move {
                        let Some(remotes) = weak.upgrade() else {
                            return Ok(None);
                        };
                        let target = call.entity_ids()?;
                        let indirect = reg.get().target_entities(&call.data);
                        let list = select(
                            &remotes.items.read(),
                            |r| r.entity_id(),
                            |r| r.available(),
                            &target,
                            &indirect,
                        );
                        let data = &call.data;
                        let calls = list.iter().map(|r| async move {
                            match name {
                                "turn_on" => {
                                    r.turn_on(data.get("activity").and_then(Value::as_str))
                                        .await
                                }
                                "turn_off" => {
                                    r.turn_off(data.get("activity").and_then(Value::as_str))
                                        .await
                                }
                                "toggle" => {
                                    r.toggle(data.get("activity").and_then(Value::as_str)).await
                                }
                                "send_command" => {
                                    let cmd = SendCommand {
                                        command: string_list(data.get("command")),
                                        device: data
                                            .get("device")
                                            .and_then(Value::as_str)
                                            .map(str::to_owned),
                                        num_repeats: data
                                            .get("num_repeats")
                                            .and_then(Value::as_u64)
                                            .unwrap_or(DEFAULT_NUM_REPEATS),
                                        delay_secs: data.get("delay_secs").and_then(Value::as_f64),
                                        hold_secs: data
                                            .get("hold_secs")
                                            .and_then(Value::as_f64)
                                            .unwrap_or(DEFAULT_HOLD_SECS),
                                    };
                                    r.send_command(cmd).await
                                }
                                "learn_command" => {
                                    let cmd = LearnCommand {
                                        device: data
                                            .get("device")
                                            .and_then(Value::as_str)
                                            .map(str::to_owned),
                                        command: data.get("command").map(|v| string_list(Some(v))),
                                        command_type: data
                                            .get("command_type")
                                            .and_then(Value::as_str)
                                            .map(str::to_owned),
                                        alternative: data
                                            .get("alternative")
                                            .and_then(Value::as_bool),
                                        timeout: data.get("timeout").and_then(Value::as_u64),
                                    };
                                    r.learn_command(cmd).await
                                }
                                "delete_command" => {
                                    let cmd = DeleteCommand {
                                        command: string_list(data.get("command")),
                                        device: data
                                            .get("device")
                                            .and_then(Value::as_str)
                                            .map(str::to_owned),
                                    };
                                    r.delete_command(cmd).await
                                }
                                _ => Ok(()),
                            }
                        });
                        crate::light::join_all(calls)
                            .await
                            .into_iter()
                            .collect::<Result<Vec<()>, _>>()?;
                        Ok(None)
                    }
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use serde_json::json;

    struct DummyRemote {
        id: EntityId,
        available: bool,
        is_on: bool,
        fail_send: bool,
        calls: Mutex<Vec<String>>,
    }

    impl Remote for DummyRemote {
        fn entity_id(&self) -> &EntityId {
            &self.id
        }
        fn available(&self) -> bool {
            self.available
        }
        fn is_on(&self) -> bool {
            self.is_on
        }
        fn turn_on(&self, activity: Option<&str>) -> BoxFuture<'_> {
            self.calls.lock().push(format!("turn_on:{activity:?}"));
            Box::pin(async { Ok(()) })
        }
        fn turn_off(&self, activity: Option<&str>) -> BoxFuture<'_> {
            self.calls.lock().push(format!("turn_off:{activity:?}"));
            Box::pin(async { Ok(()) })
        }
        fn send_command(&self, cmd: SendCommand) -> BoxFuture<'_> {
            self.calls
                .lock()
                .push(format!("send:{:?}:{}", cmd.command, cmd.num_repeats));
            let fail = self.fail_send;
            Box::pin(async move {
                if fail {
                    Err(ServiceError::Failed("send failed".into()))
                } else {
                    Ok(())
                }
            })
        }
        fn learn_command(&self, cmd: LearnCommand) -> BoxFuture<'_> {
            self.calls.lock().push(format!("learn:{:?}", cmd.timeout));
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn unavailable_remote_is_skipped_and_no_cycle() {
        let hass = Hass::new();
        let remotes = Arc::new(Remotes::default());
        remotes.register_services(&hass, Arc::new(ha_registry::Registries::default()));

        let offline = Arc::new(DummyRemote {
            id: EntityId::parse("remote.offline").unwrap(),
            available: false,
            is_on: false,
            fail_send: false,
            calls: Mutex::new(Vec::new()),
        });
        remotes.add(offline.clone());
        hass.call_service(
            "remote",
            "turn_on",
            json!({"entity_id": "remote.offline"})
                .as_object()
                .cloned()
                .unwrap(),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        assert!(offline.calls.lock().is_empty());

        let weak = Arc::downgrade(&remotes);
        drop((hass, remotes));
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn large_u64_ints_preserved_and_multi_target_runs_all() {
        let hass = Hass::new();
        let remotes = Arc::new(Remotes::default());
        remotes.register_services(&hass, Arc::new(ha_registry::Registries::default()));

        let first = Arc::new(DummyRemote {
            id: EntityId::parse("remote.first").unwrap(),
            available: true,
            is_on: false,
            fail_send: true,
            calls: Mutex::new(Vec::new()),
        });
        let second = Arc::new(DummyRemote {
            id: EntityId::parse("remote.second").unwrap(),
            available: true,
            is_on: false,
            fail_send: false,
            calls: Mutex::new(Vec::new()),
        });
        remotes.add(first.clone());
        remotes.add(second.clone());

        let big: u64 = 9_223_372_036_854_775_808; // i64::MAX as u64 + 1
        let err = hass
            .call_service(
                "remote",
                "send_command",
                json!({
                    "entity_id": ["remote.first", "remote.second"],
                    "command": "Power",
                    "num_repeats": big
                })
                .as_object()
                .cloned()
                .unwrap(),
                None,
                false,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err, ServiceError::Failed("send failed".into()));
        assert_eq!(*first.calls.lock(), vec![format!("send:[\"Power\"]:{big}")]);
        assert_eq!(
            *second.calls.lock(),
            vec![format!("send:[\"Power\"]:{big}")]
        );

        hass.call_service(
            "remote",
            "learn_command",
            json!({"entity_id": "remote.second", "timeout": big})
                .as_object()
                .cloned()
                .unwrap(),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            second.calls.lock().last().map(String::as_str),
            Some(format!("learn:Some({big})").as_str())
        );
    }
}
