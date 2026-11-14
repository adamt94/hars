//! Home Assistant's automations (`components/automation`,
//! `helpers/script.py`, `helpers/trigger.py`, `helpers/condition.py`), for
//! the config subset in [`config`].
//!
//! How it runs, as HA 2026.6.1 does:
//! - Triggers attach at `homeassistant_started` ([`Automations::started`]),
//!   or at once when turned on later. State changes reach them from the
//!   lossless state feed through listeners keyed by entity id; conditions
//!   read the live state machine, so a batch an integration writes without
//!   yielding is seen whole, as HA's `call_soon` dispatch sees it.
//! - A trigger whose conditions pass starts a run (one task) at once:
//!   `automation_triggered`, `last_triggered`, one state write; `single`
//!   drops a trigger while running (with HA's warning), `restart` stops the
//!   old run first.
//! - Time triggers sleep on the monotonic clock and re-check the wall clock
//!   on waking: a clock stepped back never fires early, one stepped forward
//!   fires late, as HA's timers do. Nothing polls.

pub mod config;
mod time;

use config::{Action, Condition, Config, Mode, Trigger, TriggerKind};
use ha_core::{
    Context, EntityId, Event, EventKind, Hass, Kind, Schema, ServiceError, State, SupportsResponse,
};
use ha_registry::Registries;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use tokio::task::JoinHandle;
use tokio::time::Instant;

pub const DOMAIN: &str = "automation";

/// HA's `CONTEXT_RECENT_TIME_SECONDS`: an entity keeps a call's context
/// for its writes this long.
const CONTEXT_RECENT: jiff::SignedDuration = jiff::SignedDuration::from_secs(5);

/// Python's `float(str)`: surrounding whitespace, `_` between digits,
/// exponents, `inf`/`infinity`/`nan` in any case.
pub fn py_float(s: &str) -> Option<f64> {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    let (sign, body) = match lower.strip_prefix('-') {
        Some(b) => (-1.0, b),
        None => (1.0, lower.strip_prefix('+').unwrap_or(&lower)),
    };
    match body {
        "inf" | "infinity" => return Some(sign * f64::INFINITY),
        "nan" => return Some(f64::NAN),
        _ => {}
    }
    // Underscores only between digits.
    let b = t.as_bytes();
    for (i, c) in b.iter().enumerate() {
        if *c == b'_'
            && !(i > 0 && i + 1 < b.len() && b[i - 1].is_ascii_digit() && b[i + 1].is_ascii_digit())
        {
            return None;
        }
    }
    let clean: String = t.chars().filter(|c| *c != '_').collect();
    if clean.is_empty()
        || !clean
            .bytes()
            .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        return None;
    }
    clean.parse().ok()
}

// ----- Listeners: state changes by entity id -----

type Callback = Arc<dyn Fn(&Event) + Send + Sync>;

/// A listener: (id, the first change it hears, callback).
type Entry = (u64, u64, Callback);

#[derive(Default)]
struct Listeners {
    next: Mutex<u64>,
    /// Per entity: (id, the first change it hears, callback).
    by_entity: Mutex<HashMap<String, Vec<Entry>>>,
}

/// A listener; dropping it unsubscribes.
struct Listening {
    listeners: Weak<Listeners>,
    entity: String,
    id: u64,
}

impl Drop for Listening {
    fn drop(&mut self) {
        if let Some(l) = self.listeners.upgrade() {
            let mut map = l.by_entity.lock();
            if let Some(v) = map.get_mut(&self.entity) {
                v.retain(|(id, _, _)| *id != self.id);
                if v.is_empty() {
                    map.remove(&self.entity);
                }
            }
        }
    }
}

impl Listeners {
    /// Listen to an entity's changes from `since` on (the state feed's
    /// sequence when attaching: a change made earlier but still queued
    /// isn't heard, as in HA, where it was dispatched before).
    fn listen(self: &Arc<Self>, entity: &str, since: u64, cb: Callback) -> Listening {
        let id = {
            let mut n = self.next.lock();
            *n += 1;
            *n
        };
        self.by_entity
            .lock()
            .entry(entity.to_owned())
            .or_default()
            .push((id, since, cb));
        Listening {
            listeners: Arc::downgrade(self),
            entity: entity.to_owned(),
            id,
        }
    }

    /// Every listener of the event's entity, in the order they attached.
    /// Whether there were any.
    fn dispatch(&self, seq: u64, ev: &Event) -> bool {
        let EventKind::StateChanged { entity_id, .. } = &ev.kind else {
            return false;
        };
        let cbs: Vec<Callback> = match self.by_entity.lock().get(entity_id.as_str()) {
            Some(v) => v
                .iter()
                .filter(|(_, since, _)| seq >= *since)
                .map(|(_, _, cb)| cb.clone())
                .collect(),
            None => return false,
        };
        for cb in cbs {
            cb(ev);
        }
        true
    }
}

/// A `state_changed` event's (old, new) states.
type Change<'a> = (Option<&'a Arc<State>>, Option<&'a Arc<State>>);

fn change(ev: &Event) -> Option<Change<'_>> {
    match &ev.kind {
        EventKind::StateChanged {
            old_state,
            new_state,
            ..
        } => Some((old_state.as_ref(), new_state.as_ref())),
        _ => None,
    }
}

// ----- Trigger variables and conditions -----

/// What a trigger hands the run (`trigger` variables). Only `id` and the
/// description are read by the supported config.
#[derive(Debug, Clone)]
pub struct TriggerVars {
    pub id: String,
    pub idx: String,
    pub platform: Option<&'static str>,
    pub description: Option<String>,
}

fn logger(entity_id: &EntityId) -> String {
    format!(
        "homeassistant.components.automation.{}",
        entity_id.as_str().trim_start_matches("automation.")
    )
}

/// `async_numeric_state` for one entity: `Ok(false)` for `unknown` or
/// `unavailable`, `Err` for an absent entity or a non-number.
fn numeric(hass: &Hass, eid: &str, below: Option<f64>, above: Option<f64>) -> Result<bool, String> {
    let state = EntityId::parse(eid)
        .ok()
        .and_then(|id| hass.states().get(&id))
        .ok_or_else(|| format!("In 'numeric_state' condition: unknown entity {eid}"))?;
    numeric_of(&state, below, above)
}

fn numeric_of(state: &State, below: Option<f64>, above: Option<f64>) -> Result<bool, String> {
    let s = state.state.as_str();
    if matches!(s, "unavailable" | "unknown") {
        return Ok(false);
    }
    let v = py_float(s).ok_or_else(|| {
        format!(
            "In 'numeric_state' condition: entity {} state '{s}' cannot be processed as a number",
            state.entity_id
        )
    })?;
    Ok(!(below.is_some_and(|b| v >= b) || above.is_some_and(|a| v <= a)))
}

fn check(hass: &Hass, c: &Condition, vars: &TriggerVars) -> Result<bool, String> {
    match c {
        Condition::NumericState {
            entity_ids,
            below,
            above,
        } => {
            for e in entity_ids {
                if !numeric(hass, e, *below, *above)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Condition::State { entity_ids, states } => {
            for e in entity_ids {
                let s = EntityId::parse(e)
                    .ok()
                    .and_then(|id| hass.states().get(&id))
                    .ok_or_else(|| format!("In 'state' condition: unknown entity {e}"))?;
                if !states.iter().any(|x| x == s.state.as_str()) {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Condition::Time { after, before } => {
            let now = time::local_now(hass).time();
            let after = after.unwrap_or(jiff::civil::Time::midnight());
            let before = before.unwrap_or(jiff::civil::Time::MAX);
            Ok(if after < before {
                after <= now && now < before
            } else {
                !(before <= now && now < after)
            })
        }
        Condition::Trigger { ids } => Ok(ids.contains(&vars.id)),
    }
}

/// `ConditionsChecker`: AND, stopping at the first false; an error counts
/// as false, with HA's warning when nothing else was false.
fn conditions_pass(hass: &Hass, alias: &str, conds: &[Condition], vars: &TriggerVars) -> bool {
    let mut errors = Vec::new();
    for c in conds {
        match check(hass, c, vars) {
            Ok(true) => {}
            Ok(false) => return false,
            Err(e) => errors.push(e),
        }
    }
    if !errors.is_empty() {
        tracing::warn!(
            logger = "homeassistant.helpers.condition",
            "Error evaluating condition in '{alias}':\n{}",
            errors.join("\n")
        );
        return false;
    }
    true
}

// ----- Automations -----

struct Run {
    id: u64,
    handle: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Live {
    enabled: bool,
    last_triggered: Option<jiff::Timestamp>,
    runs: Vec<Run>,
    next_run: u64,
    /// Listeners and timers of the attached triggers.
    attached: Vec<Attached>,
    /// The context the entity writes with, and when it was set.
    context: Option<(Context, jiff::Timestamp)>,
}

enum Attached {
    Listening(#[allow(dead_code)] Listening),
    Task(JoinHandle<()>),
    /// A state trigger's pending `for` timer.
    Pending(PendingFor),
}

impl Drop for Attached {
    fn drop(&mut self) {
        match self {
            Attached::Task(h) => h.abort(),
            Attached::Pending(p) => {
                if let Some((_, h)) = p.lock().take() {
                    h.abort();
                }
            }
            Attached::Listening(_) => {}
        }
    }
}

/// A state trigger's pending `for`: the value waited on, and its timer.
type PendingFor = Arc<Mutex<Option<(String, JoinHandle<()>)>>>;

/// How a run is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fire {
    /// An attached trigger: only while enabled, conditions checked.
    Trigger,
    /// `automation.trigger`: whether enabled or not.
    Service { skip_conditions: bool },
}

pub struct Automation {
    shared: Weak<Shared>,
    pub entity_id: EntityId,
    pub name: String,
    pub unique_id: Option<String>,
    /// The config as loaded, as JSON text in its YAML key order (`None`
    /// if HA couldn't encode it either).
    pub raw: Option<String>,
    /// The validated config, or why it isn't valid (`unavailable`).
    config: Result<Config, String>,
    live: Mutex<Live>,
}

struct Shared {
    hass: Hass,
    listeners: Arc<Listeners>,
    automations: Mutex<Vec<Arc<Automation>>>,
}

/// Every automation, its listeners and its services.
#[derive(Clone)]
pub struct Automations {
    shared: Arc<Shared>,
}

impl Automation {
    fn hass(&self) -> Option<Hass> {
        self.shared.upgrade().map(|s| s.hass.clone())
    }

    fn mode(&self) -> Mode {
        self.config.as_ref().map_or(Mode::Single, |c| c.mode)
    }

    fn attributes(&self, live: &Live) -> Map<String, Value> {
        let mut a = Map::new();
        if let Some(id) = &self.unique_id {
            a.insert("id".into(), json!(id));
        }
        if self.config.is_ok() {
            a.insert(
                "last_triggered".into(),
                json!(live.last_triggered.map(ha_core::isoformat)),
            );
            a.insert("mode".into(), json!(self.mode().as_str()));
            a.insert("current".into(), json!(live.runs.len()));
        }
        a.insert("friendly_name".into(), json!(self.name));
        a
    }

    /// Write the entity's state, with `context` or the entity's recent one.
    fn write(&self, live: &mut Live, context: Option<Context>) {
        let Some(hass) = self.hass() else { return };
        let state = match (&self.config, live.enabled) {
            (Err(_), _) => "unavailable",
            (Ok(_), true) => "on",
            (Ok(_), false) => "off",
        };
        // `async_set_context`'s expiry, on the wall clock.
        let now = hass.clock().now();
        let context = context.or_else(|| {
            live.context
                .as_ref()
                .filter(|(_, at)| now.duration_since(*at) <= CONTEXT_RECENT)
                .map(|(c, _)| c.clone())
        });
        hass.set_state_with_context(
            self.entity_id.clone(),
            state,
            self.attributes(live),
            context,
        );
    }

    fn set_context(live: &mut Live, ctx: &Context, now: jiff::Timestamp) {
        live.context = Some((ctx.clone(), now));
    }

    /// Attach the triggers (enabled, valid, not yet attached).
    fn attach(self: &Arc<Self>) {
        let (Ok(cfg), Some(shared)) = (&self.config, self.shared.upgrade()) else {
            return;
        };
        let mut live = self.live.lock();
        if !live.enabled || !live.attached.is_empty() {
            return;
        }
        for t in &cfg.triggers {
            self.attach_trigger(&shared, t, &mut live.attached);
        }
        tracing::info!(
            logger = logger(&self.entity_id),
            "Initialized trigger {}",
            self.name
        );
    }

    fn attach_trigger(
        self: &Arc<Self>,
        shared: &Arc<Shared>,
        t: &Trigger,
        out: &mut Vec<Attached>,
    ) {
        let me = Arc::downgrade(self);
        let vars = |description: String, platform| TriggerVars {
            id: t.id.clone(),
            idx: t.idx.to_string(),
            platform: Some(platform),
            description: Some(description),
        };
        match &t.kind {
            TriggerKind::State {
                entity_ids,
                to,
                for_,
                ..
            } => {
                for e in entity_ids {
                    let (me, to, for_) = (me.clone(), to.clone(), *for_);
                    let v = vars(format!("state of {e}"), "state");
                    // With `for`: the pending firing (value, timer); any
                    // other value (or removal) cancels it.
                    let pending: PendingFor = Arc::default();
                    out.push(Attached::Pending(pending.clone()));
                    out.push(Attached::Listening(shared.listeners.listen(
                        e,
                        shared.hass.states().seq(),
                        Arc::new(move |ev| {
                            let Some((old, new)) = change(ev) else { return };
                            let new_s = new.map(|n| n.state.to_string());
                            {
                                let mut p = pending.lock();
                                if p.as_ref().is_some_and(|(v, _)| Some(v) != new_s.as_ref())
                                    && let Some((_, h)) = p.take()
                                {
                                    h.abort();
                                }
                            }
                            let Some(new) = new else { return };
                            let old = old.map(|s| s.state.as_str());
                            let new_s = new.state.as_str();
                            let to_ok = to.as_ref().is_none_or(|t| t.iter().any(|x| x == new_s));
                            if !to_ok || old == Some(new_s) {
                                return;
                            }
                            match for_ {
                                None => {
                                    if let Some(a) = me.upgrade() {
                                        a.fired(v.clone(), Some(&new.context), Fire::Trigger);
                                    }
                                }
                                Some(d) => {
                                    let (me, v, ctx) = (me.clone(), v.clone(), new.context.clone());
                                    let p2 = pending.clone();
                                    let h = tokio::spawn(async move {
                                        tokio::time::sleep(d).await;
                                        p2.lock().take();
                                        if let Some(a) = me.upgrade() {
                                            a.fired(v, Some(&ctx), Fire::Trigger);
                                        }
                                    });
                                    *pending.lock() = Some((new_s.to_owned(), h));
                                }
                            }
                        }),
                    )));
                }
            }
            TriggerKind::NumericState {
                entity_ids,
                below,
                above,
            } => {
                let hass = &shared.hass;
                let armed: Arc<Mutex<HashSet<String>>> = Arc::default();
                for e in entity_ids {
                    // At attach: armed unless it matches already; an error
                    // (absent, not a number) leaves it unarmed.
                    match numeric(hass, e, *below, *above) {
                        Ok(false) => {
                            armed.lock().insert(e.clone());
                        }
                        Ok(true) => {}
                        Err(err) => tracing::warn!(
                            logger =
                                "homeassistant.components.homeassistant.triggers.numeric_state",
                            "Error initializing '{}' trigger: {err}",
                            self.name
                        ),
                    }
                    let (me, armed, e2) = (me.clone(), armed.clone(), e.clone());
                    let (below, above) = (*below, *above);
                    let v = vars(format!("numeric state of {e}"), "numeric_state");
                    let name = self.name.clone();
                    out.push(Attached::Listening(shared.listeners.listen(
                        e,
                        shared.hass.states().seq(),
                        Arc::new(move |ev| {
                            let Some((_, Some(new))) = change(ev) else { return };
                            match numeric_of(new, below, above) {
                                Err(err) => tracing::warn!(
                                    logger = "homeassistant.components.homeassistant.triggers.numeric_state",
                                    "Error in '{name}' trigger: {err}"
                                ),
                                Ok(false) => {
                                    armed.lock().insert(e2.clone());
                                }
                                Ok(true) => {
                                    if armed.lock().remove(&e2)
                                        && let Some(a) = me.upgrade()
                                    {
                                        a.fired(v.clone(), Some(&new.context), Fire::Trigger);
                                    }
                                }
                            }
                        }),
                    )));
                }
            }
            TriggerKind::Time { at } => {
                for at in at {
                    let (me, at) = (me.clone(), *at);
                    let v = vars("time".into(), "time");
                    let hass = shared.hass.clone();
                    out.push(Attached::Task(tokio::spawn(async move {
                        // HA's tracker: a random 50-500 ms past the second,
                        // fixed for the tracker's life.
                        let micro = rand::random_range(50_000..=500_000);
                        let mut from = hass.clock().now();
                        loop {
                            let Some(next) = time::next_local(&hass, from, at, micro) else {
                                return;
                            };
                            time::sleep_until_wall(&hass, next).await;
                            let Some(a) = me.upgrade() else { return };
                            a.fired(v.clone(), None, Fire::Trigger);
                            from = hass.clock().now() + jiff::SignedDuration::from_secs(1);
                        }
                    })));
                }
            }
        }
    }

    /// `async_trigger`: conditions (unless skipped), then the mode, the
    /// event, `last_triggered` and a write, then the run. Returns the run
    /// (to await, for `automation.trigger`).
    fn fired(
        self: &Arc<Self>,
        vars: TriggerVars,
        parent: Option<&Context>,
        how: Fire,
    ) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let (Ok(cfg), Some(hass)) = (&self.config, self.hass()) else {
            return None;
        };
        // `_async_trigger_if_enabled`: a trigger that raced `turn_off`.
        if how == Fire::Trigger && !self.live.lock().enabled {
            return None;
        }
        let skip_conditions = how
            == (Fire::Service {
                skip_conditions: true,
            });
        let log = logger(&self.entity_id);
        if let Some(d) = &vars.description {
            tracing::debug!(logger = log, "Automation trigger 'None' triggered by {d}");
        }
        let now = hass.clock().now();
        let mut ctx = Context::at(now);
        ctx.parent_id = parent.map(|p| p.id);
        if !skip_conditions && !conditions_pass(&hass, &self.name, &cfg.conditions, &vars) {
            tracing::debug!(logger = log, "Conditions not met, aborting automation.");
            return None;
        }
        let mut live = self.live.lock();
        Self::set_context(&mut live, &ctx, now);
        if !live.runs.is_empty() {
            match cfg.mode {
                Mode::Single => {
                    tracing::warn!(logger = log, "{}: Already running", self.name);
                    return None;
                }
                Mode::Restart => {
                    tracing::info!(logger = log, "{}: Restarting", self.name);
                    for r in live.runs.drain(..) {
                        if let Some(h) = r.handle {
                            h.abort();
                        }
                    }
                }
            }
        }
        let mut data = json!({"name": self.name, "entity_id": self.entity_id.as_str()});
        if let Some(d) = &vars.description {
            data["source"] = json!(d);
        }
        hass.bus().fire(Event::new(
            EventKind::Custom {
                name: "automation_triggered".into(),
                data,
            },
            now,
            ctx.clone(),
        ));
        live.last_triggered = Some(hass.clock().now());
        live.next_run += 1;
        let run_id = live.next_run;
        live.runs.push(Run {
            id: run_id,
            handle: None,
        });
        self.write(&mut live, Some(ctx.clone()));
        tracing::info!(logger = log, "{}: Running automation actions", self.name);
        let me = self.clone();
        // Dropped when the run ends however it ends (a stopped run's task
        // is dropped), which is what `automation.trigger` waits for.
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let _done = done_tx;
            let Ok(cfg) = &me.config else { return };
            if let Err(e) = me.run_actions(&cfg.actions, &vars, &ctx, None).await {
                tracing::error!(
                    logger = "homeassistant.components.automation",
                    "Error while executing automation {}: {e}",
                    me.entity_id
                );
            }
            let mut live = me.live.lock();
            live.runs.retain(|r| r.id != run_id);
            me.write(&mut live, None);
        });
        if let Some(r) = live.runs.iter_mut().find(|r| r.id == run_id) {
            r.handle = Some(handle);
        }
        Some(done_rx)
    }

    fn run_actions<'a>(
        self: &'a Arc<Self>,
        actions: &'a [Action],
        vars: &'a TriggerVars,
        ctx: &'a Context,
        choose: Option<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let Some(hass) = self.hass() else {
                return Ok(());
            };
            let log = logger(&self.entity_id);
            let prefix = choose.unwrap_or_default();
            for (step, action) in actions.iter().enumerate() {
                match action {
                    Action::Call {
                        domain,
                        service,
                        data,
                        target,
                    } => {
                        tracing::info!(
                            logger = log,
                            "{}: {prefix}Executing step call service",
                            self.name
                        );
                        let target = (!target.is_empty()).then(|| target.clone());
                        if let Err(e) = hass
                            .call_service(
                                domain,
                                service,
                                data.clone(),
                                target,
                                false,
                                Some(ctx.clone()),
                            )
                            .await
                        {
                            let msg = match &e {
                                ServiceError::Invalid(m) => m.clone(),
                                e => e.to_string(),
                            };
                            tracing::error!(
                                logger = log,
                                "{}: {prefix}Error executing script. Error for call_service at pos {}: {msg}",
                                self.name,
                                step + 1
                            );
                            return Err(msg);
                        }
                    }
                    Action::Choose { options, default } => {
                        let chosen = options.iter().enumerate().find(|(_, (conds, _))| {
                            conditions_pass(&hass, &self.name, conds, vars)
                        });
                        match chosen {
                            Some((i, (_, seq))) => {
                                let p = format!("Choose at step {}: choice {}: ", step + 1, i + 1);
                                self.run_actions(seq, vars, ctx, Some(p)).await?;
                            }
                            None if !default.is_empty() => {
                                let p = format!("Choose at step {}: default: ", step + 1);
                                self.run_actions(default, vars, ctx, Some(p)).await?;
                            }
                            None => {}
                        }
                    }
                    Action::WaitForTrigger { triggers } => {
                        tracing::info!(
                            logger = log,
                            "{}: {prefix}Executing step wait for trigger",
                            self.name
                        );
                        self.wait_for(triggers).await;
                    }
                }
            }
            Ok(())
        })
    }

    /// `wait_for_trigger` with state triggers: subscribed now (a state
    /// already matching doesn't count), done when one matches (for its
    /// `for`, kept through attribute-only writes, reset by any other value).
    async fn wait_for(&self, triggers: &[Trigger]) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        // (trigger, entity, old value, new value; `None` = removed)
        type Msg = (usize, String, Option<String>, Option<String>);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Msg>();
        let mut guards = Vec::new();
        for (i, t) in triggers.iter().enumerate() {
            let TriggerKind::State { entity_ids, .. } = &t.kind else {
                continue;
            };
            for e in entity_ids {
                let (tx, e2) = (tx.clone(), e.clone());
                guards.push(shared.listeners.listen(
                    e,
                    shared.hass.states().seq(),
                    Arc::new(move |ev| {
                        if let Some((old, new)) = change(ev) {
                            let _ = tx.send((
                                i,
                                e2.clone(),
                                old.map(|s| s.state.to_string()),
                                new.map(|s| s.state.to_string()),
                            ));
                        }
                    }),
                ));
            }
        }
        tracing::info!(
            logger = logger(&self.entity_id),
            "{}: Initialized trigger",
            self.name
        );
        drop(tx);
        // Pending `for`s, per trigger and entity: (trigger, entity, value,
        // deadline).
        let mut timers: Vec<(usize, String, String, Instant)> = Vec::new();
        loop {
            let next = timers.iter().map(|t| t.3).min();
            let sleep = async {
                match next {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                msg = rx.recv() => {
                    let Some((i, entity, old, new)) = msg else { return };
                    // Another value (or removal) cancels this entity's `for`.
                    timers.retain(|(j, e, v, _)| !(*j == i && *e == entity && Some(v) != new.as_ref()));
                    let Some(new) = new else { continue };
                    let TriggerKind::State { to, for_, .. } = &triggers[i].kind else { continue };
                    let to_ok = to.as_ref().is_none_or(|t| t.contains(&new));
                    if to_ok && old.as_deref() != Some(new.as_str()) {
                        match for_ {
                            None => return,
                            Some(d) => timers.push((i, entity, new, Instant::now() + *d)),
                        }
                    }
                }
                () = sleep => return,
            }
        }
    }

    fn stop_runs(&self, live: &mut Live) {
        for r in live.runs.drain(..) {
            if let Some(h) = r.handle {
                h.abort();
            }
        }
    }
}

/// The registry's entity id for `unique_id`, else `slugify(name)` made
/// unique; `None` if the registry has it disabled.
fn entity_id_for(
    hass: &Hass,
    registries: &Registries,
    unique_id: Option<&str>,
    name: &str,
    taken: &HashSet<String>,
) -> Option<(EntityId, Option<String>)> {
    let entry = unique_id.and_then(|u| {
        registries.entities.iter().find(|e| {
            e.platform == DOMAIN && e.unique_id.as_ref().and_then(Value::as_str) == Some(u)
        })
    });
    if let Some(e) = entry {
        if e.disabled_by.is_some() {
            return None;
        }
        return EntityId::parse(&e.entity_id)
            .ok()
            .map(|id| (id, e.name.clone()));
    }
    let base = format!("{DOMAIN}.{}", ha_components::slugify(name));
    let used = |id: &str| {
        taken.contains(id)
            || registries.entities.iter().any(|e| e.entity_id == id)
            || EntityId::parse(id).is_ok_and(|i| hass.states().get(&i).is_some())
    };
    let id = (1..)
        .map(|n| {
            if n == 1 {
                base.clone()
            } else {
                format!("{base}_{n}")
            }
        })
        .find(|id| !used(id))?;
    EntityId::parse(&id).ok().map(|id| (id, None))
}

/// The automations in a loaded `configuration.yaml`: every `automation`
/// or `automation <anything>` key (`config_per_platform`), a list of them or
/// one; a falsy value is none.
pub fn from_config(config: &ha_core::yaml::Yaml) -> Vec<Value> {
    from_config_yaml(config)
        .iter()
        .map(|y| y.to_json())
        .collect()
}

/// HA's `raw_config` shows its validators' changes to nested mappings:
/// an action's legacy `service` becomes `action`, moved last. Top-level
/// keys, triggers and conditions are as written.
pub fn migrate_actions(item: &ha_core::yaml::Yaml) -> ha_core::yaml::Yaml {
    use ha_core::yaml::Yaml;
    fn action(a: &Yaml) -> Yaml {
        let Yaml::Map(pairs) = a else {
            return a.clone();
        };
        let mut out: Vec<(Yaml, Yaml)> = Vec::new();
        let mut service = None;
        let has_action = pairs.iter().any(|(k, _)| k.as_str() == Some("action"));
        for (k, v) in pairs {
            match k.as_str() {
                Some("service") if !has_action => service = Some(v.clone()),
                Some("sequence" | "default" | "then" | "else") => out.push((k.clone(), actions(v))),
                Some("choose") => out.push((k.clone(), choose(v))),
                _ => out.push((k.clone(), v.clone())),
            }
        }
        if let Some(s) = service {
            out.push((Yaml::Str("action".into()), s));
        }
        Yaml::Map(out)
    }
    fn actions(v: &Yaml) -> Yaml {
        match v {
            Yaml::Seq(items) => Yaml::Seq(items.iter().map(action).collect()),
            single => action(single),
        }
    }
    fn choose(v: &Yaml) -> Yaml {
        let option = |o: &Yaml| match o {
            Yaml::Map(pairs) => Yaml::Map(
                pairs
                    .iter()
                    .map(|(k, v)| match k.as_str() {
                        Some("sequence") => (k.clone(), actions(v)),
                        _ => (k.clone(), v.clone()),
                    })
                    .collect(),
            ),
            o => o.clone(),
        };
        match v {
            Yaml::Seq(items) => Yaml::Seq(items.iter().map(option).collect()),
            single => option(single),
        }
    }
    let Yaml::Map(pairs) = item else {
        return item.clone();
    };
    Yaml::Map(
        pairs
            .iter()
            .map(|(k, v)| match k.as_str() {
                Some("actions" | "action") => (k.clone(), actions(v)),
                _ => (k.clone(), v.clone()),
            })
            .collect(),
    )
}

/// [`from_config`], as loaded (for the raw configs the editor shows).
pub fn from_config_yaml(config: &ha_core::yaml::Yaml) -> Vec<ha_core::yaml::Yaml> {
    use ha_core::yaml::Yaml;
    let Yaml::Map(pairs) = config else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (k, v) in pairs {
        let Some(k) = k.as_str() else { continue };
        if k != DOMAIN && !k.starts_with("automation ") {
            continue;
        }
        if !v.is_truthy() {
            continue;
        }
        match v {
            Yaml::Seq(items) => out.extend(items.iter().cloned()),
            item => out.push(item.clone()),
        }
    }
    out
}

impl Automations {
    /// Make the automations (the items of HA's `automation:` config) and
    /// write their states, restored as HA restores them. Triggers attach at
    /// [`Automations::started`].
    pub fn setup(hass: &Hass, registries: &Registries, items: &[Value]) -> Self {
        let raw: Vec<Option<String>> = items.iter().map(|v| Some(v.to_string())).collect();
        Self::setup_raw(hass, registries, items, &raw)
    }

    /// [`Automations::setup`] from YAML as loaded, keeping each item's raw
    /// config in its YAML key order for `automation/config`.
    pub fn setup_yaml(hass: &Hass, registries: &Registries, items: &[ha_core::yaml::Yaml]) -> Self {
        let json: Vec<Value> = items.iter().map(|y| y.to_json()).collect();
        let raw: Vec<Option<String>> = items
            .iter()
            .map(|y| migrate_actions(y).to_json_text())
            .collect();
        Self::setup_raw(hass, registries, &json, &raw)
    }

    fn setup_raw(
        hass: &Hass,
        registries: &Registries,
        items: &[Value],
        raw: &[Option<String>],
    ) -> Self {
        let shared = Arc::new(Shared {
            hass: hass.clone(),
            listeners: Arc::default(),
            automations: Mutex::default(),
        });
        // The state feed, to the listeners.
        {
            let mut feed = hass.subscribe_state_changes();
            let listeners = Arc::downgrade(&shared.listeners);
            tokio::spawn(async move {
                while let Some((seq, ev)) = feed.recv_seq().await {
                    let Some(l) = listeners.upgrade() else { return };
                    // A run started here gets to its first step (HA starts
                    // it as an eager task) before the next change is seen.
                    if l.dispatch(seq, &ev) {
                        drop(l);
                        tokio::task::yield_now().await;
                    }
                }
            });
        }
        let restore = hass.restore();
        let mut taken = HashSet::new();
        let mut seen_ids = HashSet::new();
        for (n, item) in items.iter().enumerate() {
            let config = config::parse(item);
            let alias = item.get("alias").and_then(Value::as_str).map(str::to_owned);
            let name = alias.unwrap_or_else(|| format!("automation {n}"));
            let unique_id = match item.get("id") {
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Number(num)) => Some(num.to_string()),
                _ => None,
            };
            if let Some(u) = &unique_id
                && !seen_ids.insert(u.clone())
            {
                tracing::error!(
                    logger = "homeassistant.components.automation",
                    "Platform automation does not generate unique IDs. ID {u} already exists - ignoring"
                );
                continue;
            }
            let Some((entity_id, registry_name)) =
                entity_id_for(hass, registries, unique_id.as_deref(), &name, &taken)
            else {
                continue;
            };
            taken.insert(entity_id.as_str().to_owned());
            let name = registry_name.unwrap_or(name);
            if let Err(e) = &config {
                tracing::error!(
                    logger = "homeassistant.components.automation",
                    "Automation with alias '{name}' could not be validated and has been disabled: {e}"
                );
            }
            // Restore: on/off and `last_triggered`; `initial_state` wins.
            let mut live = Live::default();
            if config.is_ok() {
                let last = restore.last_state(entity_id.as_str());
                live.enabled = match &last {
                    Some(s) => s["state"] == "on",
                    None => true,
                };
                if let Some(Ok(c)) = Some(&config)
                    && let Some(i) = c.initial_state
                {
                    live.enabled = i;
                }
                if last.is_none() {
                    tracing::debug!(
                        logger = logger(&entity_id),
                        "Automation {entity_id} not in state storage, state True from default is used"
                    );
                }
                live.last_triggered = last
                    .as_ref()
                    .and_then(|s| s["attributes"]["last_triggered"].as_str())
                    .and_then(|t| t.parse::<jiff::Timestamp>().ok());
                restore.register(entity_id.as_str(), None);
            }
            let a = Arc::new(Automation {
                shared: Arc::downgrade(&shared),
                entity_id,
                name,
                unique_id,
                raw: raw[n].clone(),
                config,
                live: Mutex::new(live),
            });
            {
                let mut live = a.live.lock();
                a.write(&mut live, None);
            }
            shared.automations.lock().push(a);
        }
        let me = Self { shared };
        me.register_services();
        me
    }

    /// The automation with this entity id.
    pub fn by_entity(&self, entity_id: &str) -> Option<Arc<Automation>> {
        self.list()
            .into_iter()
            .find(|a| a.entity_id.as_str() == entity_id)
    }

    pub fn list(&self) -> Vec<Arc<Automation>> {
        self.shared.automations.lock().clone()
    }

    /// `homeassistant_started`: attach every enabled automation's
    /// triggers, then write its state again.
    pub fn started(&self) {
        for a in self.list() {
            a.attach();
            let mut live = a.live.lock();
            a.write(&mut live, None);
        }
    }

    /// `homeassistant_stop`: stop every run (each then reads `current: 0`).
    pub fn stop(&self) {
        for a in self.list() {
            let mut live = a.live.lock();
            if !live.runs.is_empty() {
                a.stop_runs(&mut live);
                a.write(&mut live, None);
            }
        }
    }

    fn targets(&self, data: &Map<String, Value>) -> Vec<Arc<Automation>> {
        let ids = ha_core::ServiceCall::entity_ids_of(data.get("entity_id"));
        self.list()
            .into_iter()
            .filter(|a| a.config.is_ok())
            .filter(|a| match &ids {
                Ok(ha_core::EntityTarget::All) => true,
                Ok(ha_core::EntityTarget::Ids(ids)) => ids.contains(&a.entity_id),
                Err(_) => false,
            })
            .collect()
    }

    fn register_services(&self) {
        let hass = &self.shared.hass;
        let weak = Arc::downgrade(&self.shared);
        let get = move || weak.upgrade().map(|shared| Automations { shared });
        for name in ["turn_on", "turn_off", "toggle", "trigger"] {
            let schema = match name {
                "turn_off" => Schema::entity_service().optional("stop_actions", Kind::Boolean),
                "trigger" => Schema::entity_service()
                    .optional("skip_condition", Kind::StrictBool)
                    .optional("variables", Kind::Dict),
                _ => Schema::entity_service(),
            };
            let get = get.clone();
            hass.services().register_with_schema(
                DOMAIN,
                name,
                Some(schema),
                SupportsResponse::None,
                move |call| {
                    let me = get();
                    async move {
                        let Some(me) = me else { return Ok(None) };
                        let mut runs = Vec::new();
                        for a in me.targets(&call.data) {
                            match name {
                                "trigger" => {
                                    let skip = call
                                        .data
                                        .get("skip_condition")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(true);
                                    let vars = TriggerVars {
                                        id: String::new(),
                                        idx: String::new(),
                                        platform: None,
                                        description: None,
                                    };
                                    runs.extend(a.fired(
                                        vars,
                                        Some(&call.context),
                                        Fire::Service {
                                            skip_conditions: skip,
                                        },
                                    ));
                                }
                                _ => {
                                    let on = match name {
                                        "turn_on" => true,
                                        "turn_off" => false,
                                        _ => !a.live.lock().enabled,
                                    };
                                    let stop = name != "turn_off"
                                        || call.data.get("stop_actions").and_then(Value::as_bool)
                                            != Some(false);
                                    me.set_enabled(&a, on, stop, &call.context);
                                }
                            }
                        }
                        for r in runs {
                            let _ = r.await;
                        }
                        Ok(None)
                    }
                },
            );
        }
    }

    fn set_enabled(&self, a: &Arc<Automation>, on: bool, stop_actions: bool, ctx: &Context) {
        {
            let mut live = a.live.lock();
            live.enabled = on;
            if !on {
                live.attached.clear();
                if stop_actions {
                    a.stop_runs(&mut live);
                }
            }
            Automation::set_context(&mut live, ctx, self.shared.hass.clock().now());
        }
        if on && self.shared.hass.core_state() == ha_core::CoreState::Running {
            a.attach();
        }
        let mut live = a.live.lock();
        a.write(&mut live, Some(ctx.clone()));
    }
}

#[cfg(test)]
mod tests;
