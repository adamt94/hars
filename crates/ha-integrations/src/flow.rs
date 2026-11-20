//! Config flows: adding an integration from the UI. HA's
//! `data_entry_flow.py` (`FlowManager`, `FlowHandler`) and
//! `config_entries.py` (`ConfigEntriesFlowManager`, `ConfigFlow`).
//!
//! An integration registers a [`Handler`]; a flow is one [`Flow`] it makes,
//! whose fields are what the flow remembers between steps. Each step
//! returns a [`Step`]: a form to fill in, an abort, or the entry to create.
//! The [`Manager`] keeps the flows in progress, checks what a form is sent
//! against its schema, and on `CreateEntry` adds the entry, starts its
//! integration and waits for the first setup attempt, as HA's `async_add`
//! does before the flow's reply.
//!
//! Where `hars` differs from HA, on purpose:
//! - Only flows a user starts (source `user`). There is no discovery,
//!   import, reauth, reconfigure, options or subentry flow yet.
//! - A handler must abort when its unique id is configured
//!   ([`Cx::abort_if_unique_id_configured`]). If one asks for an entry
//!   anyway the flow aborts with `already_configured`; HA replaces the old
//!   entry and logs that the integration should have aborted.
//! - The flow leaves the list when its entry is made, before the entry's
//!   setup ends (HA removes it after).
//! - An integration with no way to stop (all but `switchbot_cloud` and
//!   `mobile_app`) is removed as HA removes one that can't unload: the
//!   entry and its registry rows go, and its entities stay until a
//!   restart (`require_restart`).
//! - A removal that comes while the entry's first setup still runs stops
//!   that setup and lets the flow's reply go; HA's waits for the setup
//!   (they share the entry's `setup_lock`).
//!
//! Nothing here runs, wakes or holds memory while no flow is open: a flow
//! lives from its first form until it is answered or deleted (the dialog
//! closing deletes it), with no timer. As in HA, a flow whose client
//! vanishes without deleting it stays until the next restart.

use crate::components::Components;
use crate::config_entries::{ConfigEntries, EntryState, Status};
use crate::{Endpoints, switchbot_cloud};
use ha_core::Hass;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::task::JoinHandle;

/// The flow handlers, by domain (`HANDLERS` in `config_entries.py`).
pub const HANDLERS: &[&Handler] = &[&switchbot_cloud::config_flow::HANDLER];

/// `SOURCE_USER`.
pub const SOURCE_USER: &str = "user";
/// `SOURCE_RECONFIGURE`: what the flow view starts when given an entry.
pub const SOURCE_RECONFIGURE: &str = "reconfigure";

pub type StepFuture<'a> = Pin<Box<dyn Future<Output = Step> + Send + 'a>>;

/// A config flow's class: `class …ConfigFlow(ConfigFlow, domain=DOMAIN)`.
pub struct Handler {
    pub domain: &'static str,
    /// The class's name, which HA's errors quote.
    pub class: &'static str,
    /// `VERSION` and `MINOR_VERSION`, written to the entry.
    pub version: u32,
    pub minor_version: u32,
    /// The steps it has (`async_step_<id>`); a flow starts at its source's.
    pub steps: &'static [&'static str],
    /// A new flow.
    pub new: fn() -> Box<dyn Flow>,
}

/// One flow in progress (HA's `ConfigFlow` instance).
pub trait Flow: Send {
    /// `async_step_<step_id>`: `input` is `None` when the step is shown
    /// (first, or again), and the form's values, checked against the
    /// form's schema, when it is answered.
    fn step<'a>(
        &'a mut self,
        cx: &'a mut Cx<'_>,
        step_id: &'a str,
        input: Option<Map<String, Value>>,
    ) -> StepFuture<'a>;
}

/// What a step sees of the rest of `hars`.
pub struct Cx<'a> {
    pub hass: &'a Hass,
    /// Where the integrations' services are.
    pub endpoints: &'a Endpoints,
    entries: &'a ConfigEntries,
    domain: &'static str,
    unique_id: &'a mut Option<String>,
}

impl Cx<'_> {
    /// `async_set_unique_id(unique_id, raise_on_progress=False)`.
    pub fn set_unique_id(&mut self, unique_id: &str) {
        *self.unique_id = Some(unique_id.to_owned());
    }

    /// `_abort_if_unique_id_configured`: the abort, if an entry of this
    /// integration has the flow's unique id.
    pub fn abort_if_unique_id_configured(&self) -> Option<Step> {
        let unique_id = self.unique_id.as_deref()?;
        self.entries
            .find_unique(self.domain, unique_id)
            .map(|_| Step::abort("already_configured"))
    }
}

/// A form's field: a key of its `vol.Schema` and what it accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: &'static str,
    /// `vol.Required` (else `vol.Optional`).
    pub required: bool,
    pub kind: Kind,
}

/// What a field accepts. A flow that needs a choice (`vol.In`) adds its
/// kind here, with its `voluptuous_serialize` form and its check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// `str`.
    Str,
}

impl Field {
    /// `vol.Required(name): str`.
    pub const fn required_str(name: &'static str) -> Self {
        Self {
            name,
            required: true,
            kind: Kind::Str,
        }
    }

    /// `voluptuous_serialize.convert` of the field.
    fn json(&self) -> Value {
        match self.kind {
            Kind::Str => json!({"type": "string", "name": self.name, "required": self.required}),
        }
    }

    /// The field's check of a value: its error message.
    fn check(&self, value: &Value) -> Result<(), &'static str> {
        match self.kind {
            Kind::Str if value.is_string() => Ok(()),
            Kind::Str => Err("expected str"),
        }
    }
}

/// A form's `data_schema` as the flow views send it: `[]` for none.
pub fn schema_json(schema: Option<&[Field]>) -> Value {
    Value::Array(schema.unwrap_or_default().iter().map(Field::json).collect())
}

/// `data_schema(user_input)` and `_map_error_to_schema_errors`: the values
/// by key, or `InvalidData.schema_errors` (a field's message under its
/// name, the rest listed under `base`). `input` is in the order sent.
fn validate(
    schema: &[Field],
    input: Vec<(String, Value)>,
) -> Result<Map<String, Value>, Map<String, Value>> {
    let mut errors = Map::new();
    let mut base = Vec::new();
    let mut values = Map::new();
    for (key, value) in input {
        match schema.iter().find(|f| f.name == key) {
            Some(field) => match field.check(&value) {
                Ok(()) => {
                    values.insert(key, value);
                }
                Err(message) => {
                    errors.insert(key, json!(message));
                }
            },
            None => base.push(json!(format!("extra keys not allowed @ data['{key}']"))),
        }
    }
    for field in schema.iter().filter(|f| f.required) {
        if !values.contains_key(field.name) && !errors.contains_key(field.name) {
            errors.insert(field.name.into(), json!("required key not provided"));
        }
    }
    if !base.is_empty() {
        errors.insert("base".into(), Value::Array(base));
    }
    match errors.is_empty() {
        true => Ok(values),
        false => Err(errors),
    }
}

/// What a step returns (HA's `async_show_form`, `async_create_entry` and
/// `async_abort`).
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Form {
        /// The step the form's answer goes to.
        step_id: &'static str,
        /// `None`: a form with nothing to fill in (a button to press).
        schema: Option<Vec<Field>>,
        /// By field name, or `base` for the whole form: translation keys.
        errors: Vec<(&'static str, &'static str)>,
        description_placeholders: Option<Map<String, Value>>,
    },
    /// The entry to make, with the flow's unique id.
    CreateEntry {
        title: String,
        data: Map<String, Value>,
    },
    Abort {
        reason: &'static str,
    },
}

impl Step {
    /// A form, with `error` for the whole of it (`errors["base"]`) if the
    /// last answer failed.
    pub fn form(step_id: &'static str, schema: Vec<Field>, error: Option<&'static str>) -> Self {
        Self::Form {
            step_id,
            schema: Some(schema),
            errors: error.map(|e| ("base", e)).into_iter().collect(),
            description_placeholders: None,
        }
    }

    pub fn abort(reason: &'static str) -> Self {
        Self::Abort { reason }
    }
}

/// A step's result with its flow (HA's `FlowResult`), for the views.
#[derive(Debug, Clone, PartialEq)]
pub enum FlowResult {
    Form {
        flow_id: String,
        handler: &'static str,
        step_id: &'static str,
        schema: Option<Vec<Field>>,
        errors: Vec<(&'static str, &'static str)>,
        description_placeholders: Option<Map<String, Value>>,
    },
    CreateEntry {
        flow_id: String,
        handler: &'static str,
        title: String,
        version: u32,
        minor_version: u32,
        /// The entry as stored, after its first setup attempt.
        entry: Map<String, Value>,
    },
    Abort {
        flow_id: String,
        handler: &'static str,
        reason: &'static str,
    },
}

/// Why the manager refused.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// `UnknownHandler`: no integration of that name has a flow.
    UnknownHandler,
    /// `UnknownFlow`: no flow of that id is in progress.
    UnknownFlow,
    /// `UnknownStep`, with HA's message.
    UnknownStep(String),
    /// `InvalidData`: its `schema_errors`.
    InvalidData(Map<String, Value>),
}

/// A flow in progress, as `async_progress` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InProgress {
    pub flow_id: String,
    pub handler: &'static str,
    /// The context's `source`.
    pub source: &'static str,
    pub step_id: &'static str,
}

struct Progress {
    flow_id: String,
    handler: &'static Handler,
    /// The context's `unique_id`.
    unique_id: Option<String>,
    /// `cur_step`: the form being shown.
    step_id: &'static str,
    schema: Option<Vec<Field>>,
    /// Held while one of its steps runs.
    flow: Arc<tokio::sync::Mutex<Box<dyn Flow>>>,
}

/// HA's `hass.config_entries.flow`, and what starts and stops the entries
/// it makes.
pub struct Manager {
    hass: Hass,
    entries: Arc<ConfigEntries>,
    components: Arc<Components>,
    endpoints: Endpoints,
    /// Empty, with nothing allocated, while no flow is open.
    flows: Mutex<Vec<Progress>>,
    /// Domains started here, for `get_config`'s `components`.
    started: Mutex<Vec<&'static str>>,
    /// The tasks of the entries that can be stopped, by entry id.
    tasks: Mutex<Vec<(String, JoinHandle<()>)>>,
    /// The companion apps' registrations, whose entries can be stopped.
    mobile_app: Option<Arc<crate::mobile_app::MobileApp>>,
    /// HA's `entry.setup_lock`, for all entries: an entry is added with
    /// its task, or removed, one at a time, so a removal never misses a
    /// task that is about to start.
    lifecycle: tokio::sync::Mutex<()>,
}

/// What a request does to a flow.
enum Input {
    /// Show its current step (first, or again).
    Show,
    /// Answer its form, with the body's members in the order sent.
    Answer(Vec<(String, Value)>),
}

impl std::fmt::Debug for Manager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Manager").finish_non_exhaustive()
    }
}

impl Manager {
    /// `components` are the entity components start-up made, and `tasks`
    /// the entries it started that can be stopped ([`crate::Started`]).
    pub fn new(
        hass: &Hass,
        entries: Arc<ConfigEntries>,
        components: Arc<Components>,
        endpoints: Endpoints,
        tasks: Vec<(String, JoinHandle<()>)>,
        mobile_app: Option<Arc<crate::mobile_app::MobileApp>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            hass: hass.clone(),
            entries,
            components,
            endpoints,
            flows: Mutex::new(Vec::new()),
            started: Mutex::new(Vec::new()),
            tasks: Mutex::new(tasks),
            mobile_app,
            lifecycle: tokio::sync::Mutex::new(()),
        })
    }

    /// The domains whose entries were started here.
    pub fn started(&self) -> Vec<&'static str> {
        self.started.lock().clone()
    }

    /// `async_progress`.
    pub fn progress(&self) -> Vec<InProgress> {
        self.flows
            .lock()
            .iter()
            .map(|p| InProgress {
                flow_id: p.flow_id.clone(),
                handler: p.handler.domain,
                source: SOURCE_USER,
                step_id: p.step_id,
            })
            .collect()
    }

    /// `async_init`: a new flow for `handler`, at its source's step.
    pub async fn init(&self, handler: &str, source: &'static str) -> Result<FlowResult, Error> {
        let handler = HANDLERS
            .iter()
            .find(|h| h.domain == handler)
            .ok_or(Error::UnknownHandler)?;
        // `_raise_if_step_does_not_exist`, which drops the flow again.
        let Some(step_id) = handler.steps.iter().find(|s| **s == source) else {
            return Err(Error::UnknownStep(format!(
                "Handler {} doesn't support step {source}",
                handler.class
            )));
        };
        // `ulid_now()`.
        let flow_id = ha_core::ContextId::at(self.hass.clock().now()).to_string();
        self.flows.lock().push(Progress {
            flow_id: flow_id.clone(),
            handler,
            unique_id: None,
            step_id,
            schema: None,
            flow: Arc::new(tokio::sync::Mutex::new((handler.new)())),
        });
        self.step(&flow_id, Input::Show).await
    }

    /// `async_configure`: the form's answer (in the order sent), or
    /// `None` to show the current step again.
    pub async fn configure(
        &self,
        flow_id: &str,
        input: Option<Vec<(String, Value)>>,
    ) -> Result<FlowResult, Error> {
        self.step(flow_id, input.map_or(Input::Show, Input::Answer))
            .await
    }

    /// `async_abort`: the dialog closed.
    pub fn abort(&self, flow_id: &str) -> Result<(), Error> {
        let mut flows = self.flows.lock();
        let i = flows
            .iter()
            .position(|p| p.flow_id == flow_id)
            .ok_or(Error::UnknownFlow)?;
        remove(&mut flows, i);
        Ok(())
    }

    /// `_async_configure` and `_async_handle_step`: check the answer
    /// against the form being shown, run the step, and keep its result.
    async fn step(&self, flow_id: &str, input: Input) -> Result<FlowResult, Error> {
        let find = |flows: &[Progress]| flows.iter().position(|p| p.flow_id == flow_id);
        let flow = {
            let flows = self.flows.lock();
            let i = find(&flows).ok_or(Error::UnknownFlow)?;
            flows[i].flow.clone()
        };
        // One request at a time for a flow, each seeing the flow as the
        // one before left it: a request that waited here for another that
        // ended the flow finds none, and asks its device or service nothing.
        let mut flow = flow.lock().await;
        let (handler, step_id, schema, mut unique_id) = {
            let flows = self.flows.lock();
            let p = &flows[find(&flows).ok_or(Error::UnknownFlow)?];
            (p.handler, p.step_id, p.schema.clone(), p.unique_id.clone())
        };
        let input = match (input, schema) {
            (Input::Answer(input), Some(schema)) => {
                Some(validate(&schema, input).map_err(Error::InvalidData)?)
            }
            // A form without a schema takes what it is sent.
            (Input::Answer(input), None) => Some(input.into_iter().collect()),
            (Input::Show, _) => None,
        };
        let step = {
            let mut cx = Cx {
                hass: &self.hass,
                endpoints: &self.endpoints,
                entries: &self.entries,
                domain: handler.domain,
                unique_id: &mut unique_id,
            };
            flow.step(&mut cx, step_id, input).await
        };
        let domain = handler.domain;
        let abort = |reason| FlowResult::Abort {
            flow_id: flow_id.to_owned(),
            handler: domain,
            reason,
        };
        // Held from adding an entry until its task can be found and
        // stopped, so a removal comes before both or after both.
        let adding = match step {
            Step::CreateEntry { .. } => Some(self.lifecycle.lock().await),
            _ => None,
        };
        let (title, entry) = {
            let mut flows = self.flows.lock();
            let Some(i) = find(&flows) else {
                // Deleted while the step ran: `UnknownFlow`, unless the
                // step aborted anyway.
                return match step {
                    Step::Abort { reason } => Ok(abort(reason)),
                    _ => Err(Error::UnknownFlow),
                };
            };
            let (title, data) = match step {
                Step::Form {
                    step_id,
                    schema,
                    errors,
                    description_placeholders,
                } => {
                    let p = &mut flows[i];
                    p.unique_id = unique_id;
                    p.step_id = step_id;
                    p.schema.clone_from(&schema);
                    return Ok(FlowResult::Form {
                        flow_id: flow_id.to_owned(),
                        handler: domain,
                        step_id,
                        schema,
                        errors,
                        description_placeholders,
                    });
                }
                Step::Abort { reason } => {
                    remove(&mut flows, i);
                    return Ok(abort(reason));
                }
                Step::CreateEntry { title, data } => (title, data),
            };
            // `async_finish_flow`, as far as adding the entry, in one go so
            // two flows can't both add the same account.
            remove(&mut flows, i);
            if let Some(unique_id) = &unique_id {
                if self.entries.find_unique(domain, unique_id).is_some() {
                    return Ok(abort("already_configured"));
                }
                // The handler's other flows for the same thing end.
                flows.retain(|p| {
                    p.handler.domain != domain || p.unique_id.as_ref() != Some(unique_id)
                });
                if flows.is_empty() {
                    *flows = Vec::new();
                }
            }
            let mut entry =
                self.entries
                    .new_entry(domain, SOURCE_USER, &title, unique_id.as_deref(), data);
            entry.insert("version".into(), json!(handler.version));
            entry.insert("minor_version".into(), json!(handler.minor_version));
            self.entries.add_tracked(entry.clone());
            (title, entry)
        };
        let entry_id = entry
            .get("entry_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        self.entries
            .set_state(&entry_id, EntryState::SetupInProgress, None);
        let started = self.setup_entry(&entry);
        drop(adding);
        // The flow is over: a request that queued behind this one finds
        // that out now, not when the entry's setup ends.
        drop(flow);
        // `async_add` awaits `async_setup`: the reply waits for the first
        // setup attempt (or the entry's removal), so the entry's devices
        // are there when it comes.
        if started {
            self.entries.setup_attempted(&entry_id).await;
        } else {
            self.entries
                .set_state(&entry_id, EntryState::NotLoaded, None);
        }
        Ok(FlowResult::CreateEntry {
            flow_id: flow_id.to_owned(),
            handler: domain,
            title,
            version: handler.version,
            minor_version: handler.minor_version,
            entry: self.entries.get(&entry_id).unwrap_or(entry),
        })
    }

    /// `async_setup` for an entry made while running: start its
    /// integration, which reports the entry's state. `false` for a domain
    /// that can't be started here.
    fn setup_entry(&self, entry: &Map<String, Value>) -> bool {
        let Some(entry_id) = entry.get("entry_id").and_then(Value::as_str) else {
            return false;
        };
        let status = Status::new(self.entries.clone(), entry_id);
        let (task, domains) = match entry.get("domain").and_then(Value::as_str) {
            Some(switchbot_cloud::DOMAIN) => (
                crate::spawn_switchbot(&self.components, entry.clone(), &self.endpoints, status),
                switchbot_cloud::COMPONENTS,
            ),
            _ => return false,
        };
        self.tasks.lock().push((entry_id.to_owned(), task));
        let mut started = self.started.lock();
        for domain in domains {
            if !started.contains(domain) {
                started.push(domain);
            }
        }
        true
    }

    /// `async_remove`: stop the entry's integration, remove what the
    /// registries hold for it, then the entry. `Some(require_restart)`:
    /// `true` when its integration can't be stopped, so its entities stay
    /// until a restart (HA's answer for an integration that can't
    /// unload). `None` if there is no such entry.
    ///
    /// The work runs on a task of its own: a request that goes away half
    /// way (its client disconnected) doesn't leave an entry half removed.
    pub async fn remove_entry(self: &Arc<Self>, entry_id: &str) -> Option<bool> {
        let (this, entry_id) = (self.clone(), entry_id.to_owned());
        tokio::spawn(async move { this.remove(&entry_id).await })
            .await
            .ok()
            .flatten()
    }

    async fn remove(&self, entry_id: &str) -> Option<bool> {
        let _removing = self.lifecycle.lock().await;
        let entry = self.entries.get(entry_id)?;
        let task = {
            let mut tasks = self.tasks.lock();
            let i = tasks.iter().position(|(id, _)| id == entry_id);
            i.map(|i| tasks.swap_remove(i).1)
        };
        // What can be stopped: an entry with a task here, and a companion
        // app's registration. A disabled entry runs nothing.
        let app = self.mobile_app.as_ref().filter(|_| {
            entry.get("domain").and_then(Value::as_str) == Some(crate::mobile_app::DOMAIN)
        });
        let disabled = entry.get("disabled_by").is_some_and(|d| !d.is_null());
        let unloads = !disabled && (task.is_some() || app.is_some());
        // `async_unload`: a loaded entry unloads; one that isn't (its
        // setup failed, or waits to be tried again) only stops.
        let loaded = self
            .entries
            .runtime(entry_id)
            .is_none_or(|r| r.state == EntryState::Loaded);
        if unloads && loaded {
            self.entries
                .set_state(entry_id, EntryState::UnloadInProgress, None);
        }
        // Stopped before its rows are looked up, so nothing of it is made
        // after: once the task is gone its entities write nothing (see its
        // `run`), and no state comes back.
        let ran = task.is_some();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        if let Some(app) = app {
            app.stop_entry(&entry).await;
        }
        {
            // No other entry is set up while this one's entities, states
            // and rows go.
            let _one = switchbot_cloud::SETUP.lock();
            let rows: Vec<ha_core::EntityId> = self
                .components
                .registries()
                .entities
                .iter()
                .filter(|e| e.config_entry_id.as_deref() == Some(entry_id))
                .filter_map(|e| ha_core::EntityId::parse(&e.entity_id).ok())
                .collect();
            if ran {
                for id in &rows {
                    match id.domain() {
                        "button" => self.components.buttons().remove(id),
                        "climate" => self.components.climates().remove(id),
                        "switch" => self.components.switches().remove(id),
                        _ => {}
                    }
                    // `RestoreEntity.async_internal_will_remove_from_hass`:
                    // its last state is kept for when it comes back.
                    self.hass.restore().unregister(&self.hass, id.as_str());
                    self.hass.remove_state(id);
                }
            }
            if unloads {
                self.entries
                    .set_state(entry_id, EntryState::NotLoaded, None);
            }
            // `_async_clean_up`: the registries' rows. With a row goes the
            // placeholder state start-up wrote for an entity nothing
            // provides (the entity registry's `cleanup_restored_states`),
            // and any state of an entry stopped here (a notification that
            // was on its way to a phone can write one late).
            if let Some(registry) = self.components.registry() {
                registry.clear_config_entry(entry_id);
            }
            for id in &rows {
                let state = self.hass.states().get(id);
                if state.is_some_and(|s| unloads || s.attributes.contains_key("restored")) {
                    self.hass.remove_state(id);
                }
            }
        }
        self.entries.remove(entry_id);
        Some(!(unloads || disabled))
    }

    #[cfg(test)]
    pub(crate) fn tasks(&self) -> usize {
        self.tasks.lock().len()
    }
}

/// Drop flow `i`; an empty list gives its memory back.
fn remove(flows: &mut Vec<Progress>, i: usize) {
    flows.remove(i);
    if flows.is_empty() {
        *flows = Vec::new();
    }
}

#[cfg(test)]
mod tests;
