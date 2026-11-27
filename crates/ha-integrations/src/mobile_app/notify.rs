//! `mobile_app`'s push notifications: HA's `mobile_app/notify.py` (the
//! legacy `notify.mobile_app_<device>` and `notify.notify` services, the
//! `MobileAppNotifyEntity` behind `notify.send_message`, cloud push through
//! the relay) and `push_notification.py` (the local push channel the app
//! opens over the WebSocket, with its confirm handshake).
//!
//! A notification goes over the app's local push channel when one is open,
//! else to the registration's `push_url` (the relay at
//! mobile-apps.home-assistant.io). With `support_confirm`, a channel
//! message carries a `hass_confirm_id`; unconfirmed after 10 s, the channel
//! is dropped and everything still unconfirmed goes by cloud push. Nothing
//! runs while nothing is being sent: a pending confirmation is one sleeping
//! task, aborted when confirmed, and the HTTP client is made per send.
//!
//! Where hars differs from HA, deliberately:
//!
//! - Service data arrives as `serde_json` objects, which keep their keys
//!   sorted, so a `data` object in a request body or a channel message has
//!   its keys sorted where HA keeps the caller's order. The keys around it
//!   are written in HA's order.
//! - Requests to the relay carry hars's `User-Agent`, not aiohttp's.
//! - When a relay answer is not JSON, the error is HA's (an unknown error),
//!   but its message is `serde_json`'s, not Python's.
//! - `notify.persistent_notification` (a `notify` component service) is not
//!   registered: hars has no persistent notifications to make. The other
//!   notify services are registered once there is a registration (HA's
//!   `default_config` sets `mobile_app` up, and so `notify.notify`, even
//!   with none).
//! - A cloud push that fails where HA's error escapes into a task (a
//!   confirm timer, a closed connection) is logged here as "mobile_app push
//!   notification fallback failed"; HA's asyncio logs it.
//! - Replacing a channel whose pending `send_message` then fails by cloud:
//!   the command fails, as in HA, but the connection keeps the failed
//!   subscription, so unsubscribing it succeeds where HA says not found.
//! - The legacy services write the notify entity's state with a new
//!   context; HA reuses the context of a `send_message` from the last 5 s.
//! - Assist's timer notifications (`timers.py`) are not sent: hars has no
//!   Assist.

use super::{DOMAIN, Registration};
use ha_core::{
    Context, EntityId, Hass, Kind, Schema, ServiceCall, ServiceError, ServiceResponse,
    SupportsResponse,
};
use ha_registry::Registry;
use parking_lot::{Mutex, RwLock};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::sync::{Arc, Weak};
use std::time::Duration;

/// `PUSH_CONFIRM_TIMEOUT`: how long a channel message waits for the app's
/// confirmation.
pub const PUSH_CONFIRM_TIMEOUT: Duration = Duration::from_secs(10);
/// `asyncio.timeout(10)` around the relay request and reading its answer.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// `ATTR_TITLE_DEFAULT`: the legacy services drop this title.
const TITLE_DEFAULT: &str = "Home Assistant";
const NOTIFY: &str = "notify";

/// Every loaded registration, by webhook id (shared with `MobileApp`).
pub(super) type Registrations = Arc<RwLock<HashMap<Arc<str>, Arc<Registration>>>>;
type Channels = Arc<Mutex<ChannelMap>>;

/// The registered channel of each app, and the generation of each app's
/// latest open (counted under the same lock), so an open that first had to
/// send its predecessor's pending messages by cloud registers only if no
/// newer open came meanwhile.
#[derive(Default)]
struct ChannelMap {
    live: HashMap<Arc<str>, Arc<PushChannel>>,
    latest: HashMap<Arc<str>, u64>,
    opens: u64,
}

/// A delayed open's entry in [`ChannelMap::latest`], removed when the
/// opening ends if it is still the latest.
struct Latest {
    channels: Channels,
    webhook_id: Arc<str>,
    generation: u64,
}

impl Drop for Latest {
    fn drop(&mut self) {
        let mut channels = self.channels.lock();
        if channels.latest.get(&self.webhook_id) == Some(&self.generation) {
            channels.latest.remove(&self.webhook_id);
        }
    }
}

impl std::ops::Deref for ChannelMap {
    type Target = HashMap<Arc<str>, Arc<PushChannel>>;
    fn deref(&self) -> &Self::Target {
        &self.live
    }
}

impl std::ops::DerefMut for ChannelMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.live
    }
}

/// What the relay answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayAnswer {
    pub status: u16,
    /// The `Content-Type` header, as sent (empty if none).
    pub content_type: String,
    pub body: Vec<u8>,
}

/// Why no answer came: a timeout, or anything aiohttp calls a
/// `ClientError` (connecting, TLS, HTTP).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayError {
    Timeout,
    Client(String),
}

pub type RelayFuture =
    std::pin::Pin<Box<dyn Future<Output = Result<RelayAnswer, RelayError>> + Send>>;

/// Where cloud pushes are POSTed: [`HttpRelay`], or a fake in tests.
pub trait Relay: Send + Sync + std::fmt::Debug {
    /// POST `body` (JSON) to `url`.
    fn post(&self, url: &str, body: String) -> RelayFuture;
}

/// The relay over HTTPS (`ha-http`), with a client made per push.
#[derive(Debug, Default)]
pub struct HttpRelay;

impl Relay for HttpRelay {
    fn post(&self, url: &str, body: String) -> RelayFuture {
        let url = url.to_owned();
        Box::pin(async move {
            let client = ha_http::Client::new(crate::USER_AGENT)
                .map_err(|e| RelayError::Client(e.to_string()))?
                .with_timeouts(SEND_TIMEOUT, SEND_TIMEOUT);
            match client
                .request(ha_http::Method::POST, &url, &[], Some(body.into()))
                .await
            {
                Ok(res) => Ok(RelayAnswer {
                    status: res.status.as_u16(),
                    content_type: res
                        .headers
                        .get(ha_http::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned(),
                    body: res.body.to_vec(),
                }),
                Err(ha_http::Error::Timeout(_)) => Err(RelayError::Timeout),
                Err(e) => Err(RelayError::Client(e.to_string())),
            }
        })
    }
}

/// What a cloud push needs: the relay and the clock.
#[derive(Debug, Clone)]
struct Cloud {
    relay: Arc<dyn Relay>,
    clock: ha_core::Clock,
}

/// What a notification carries (HA's `data` dict), written in HA's key
/// order: `message`, `title`, `data`, `hass_confirm_id`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Notification {
    pub message: String,
    pub title: Option<String>,
    pub data: Option<Value>,
    pub confirm_id: Option<String>,
}

impl Notification {
    fn fields(&self, out: &mut String) {
        out.push_str("\"message\":");
        push_json(out, &self.message);
        if let Some(title) = &self.title {
            out.push_str(",\"title\":");
            push_json(out, title);
        }
        if let Some(data) = &self.data {
            out.push_str(",\"data\":");
            push_json(out, data);
        }
        if let Some(id) = &self.confirm_id {
            out.push_str(",\"hass_confirm_id\":");
            push_json(out, id);
        }
    }

    /// The JSON object a channel message carries as its `event`.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{");
        self.fields(&mut out);
        out.push('}');
        out
    }
}

fn push_json<T: serde::Serialize + ?Sized>(out: &mut String, value: &T) {
    out.push_str(&serde_json::to_string(value).unwrap_or_else(|_| "null".into()));
}

/// The legacy services share one notification between their targets, as
/// HA's one `data` dict: a confirm id a channel adds rides along to the
/// targets after it.
type Shared = Arc<Mutex<Notification>>;

/// What an unconfirmed message falls back to: the legacy services log a
/// failed cloud push; `send_message`'s raises it to whoever waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fallback {
    Legacy,
    Entity,
}

struct Pending {
    id: String,
    notification: Shared,
    reg: Arc<Registration>,
    fallback: Fallback,
    timer: tokio::task::AbortHandle,
}

#[derive(Default)]
struct ChannelState {
    /// `on_teardown` has run: the channel is out of the registry.
    torn_down: bool,
    /// `pending_confirms`, in the order they were sent.
    pending: Vec<Pending>,
}

/// `PushChannel`: the app's open `push_notification_channel`.
pub struct PushChannel {
    webhook_id: Arc<str>,
    support_confirm: bool,
    /// Queues one channel event (the notification's JSON) to the app.
    send: Box<dyn Fn(&str) + Send + Sync>,
    channels: Weak<Mutex<ChannelMap>>,
    cloud: Cloud,
    state: Mutex<ChannelState>,
}

impl std::fmt::Debug for PushChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushChannel")
            .field("webhook_id", &self.webhook_id)
            .field("support_confirm", &self.support_confirm)
            .finish_non_exhaustive()
    }
}

/// Locking: the channel map's lock is held while a channel is looked up and
/// sent to, torn down, confirmed, or opened, so none of these interleave
/// (HA runs each in one go on its event loop). A channel's state lock is
/// only ever taken inside it, never the other way round, and neither is
/// held across an await.
impl PushChannel {
    /// `async_send_notification`, with the map lock held. `false` if the
    /// channel was torn down: the caller sends by cloud instead.
    fn notify(self: &Arc<Self>, n: &Shared, reg: Arc<Registration>, fallback: Fallback) -> bool {
        if !self.support_confirm {
            if self.state.lock().torn_down {
                return false;
            }
            let json = n.lock().to_json();
            (self.send)(&json);
            return true;
        }
        if self.state.lock().torn_down {
            return false;
        }
        // `random_uuid_hex`.
        let id = format!("{:032x}", rand::random::<u128>());
        let json = {
            let mut n = n.lock();
            n.confirm_id = Some(id.clone());
            n.to_json()
        };
        {
            let mut state = self.state.lock();
            let me = self.clone();
            let key = id.clone();
            // Spawned under the lock: it can't look before it is recorded.
            let timer = tokio::spawn(async move {
                tokio::time::sleep(PUSH_CONFIRM_TIMEOUT).await;
                if let Err(e) = me.push_failed(&key).await {
                    tracing::error!("mobile_app push notification fallback failed: {e}");
                }
            })
            .abort_handle();
            state.pending.push(Pending {
                id,
                notification: n.clone(),
                reg,
                fallback,
                timer,
            });
        }
        (self.send)(&json);
        true
    }

    /// `handle_push_failed`: unconfirmed in time. The channel is dropped
    /// (everything else pending goes by cloud first), then this one goes.
    async fn push_failed(&self, id: &str) -> Result<(), ServiceError> {
        let pending = {
            let mut state = self.state.lock();
            let Some(i) = state.pending.iter().position(|p| p.id == id) else {
                return Ok(());
            };
            state.pending.remove(i)
        };
        if let Some(rest) = self.begin_teardown() {
            run_fallbacks(&self.cloud, rest).await?;
        }
        fallback(&self.cloud, pending).await
    }

    /// `async_confirm_notification`.
    fn confirm(&self, id: &str) -> bool {
        let mut state = self.state.lock();
        match state.pending.iter().position(|p| p.id == id) {
            Some(i) => {
                state.pending.remove(i).timer.abort();
                true
            }
            None => false,
        }
    }

    /// The synchronous half of `async_teardown`: out of the registry (if
    /// it is still this channel there), and everything pending taken, its
    /// timers stopped. `None` if already torn down.
    fn begin_teardown(&self) -> Option<Vec<Pending>> {
        match self.channels.upgrade() {
            Some(channels) => self.teardown_locked(&mut channels.lock()),
            None => self.teardown_locked(&mut ChannelMap::default()),
        }
    }

    /// [`PushChannel::begin_teardown`] with the map lock held.
    fn teardown_locked(&self, channels: &mut ChannelMap) -> Option<Vec<Pending>> {
        let mut state = self.state.lock();
        if state.torn_down {
            return None;
        }
        state.torn_down = true;
        if channels
            .get(&self.webhook_id)
            .is_some_and(|c| std::ptr::eq(Arc::as_ptr(c), self))
        {
            channels.remove(&self.webhook_id);
        }
        let pending = std::mem::take(&mut state.pending);
        for p in &pending {
            p.timer.abort();
        }
        Some(pending)
    }

    /// The app unsubscribed or its connection closed: `async_teardown` as
    /// a task (HA's `hass.async_create_task`). What was pending goes by
    /// cloud; a failure is logged.
    pub fn close(&self) {
        if let Some(pending) = self.begin_teardown()
            && !pending.is_empty()
        {
            let cloud = self.cloud.clone();
            tokio::spawn(async move {
                if let Err(e) = run_fallbacks(&cloud, pending).await {
                    tracing::error!("mobile_app push notification fallback failed: {e}");
                }
            });
        }
    }

    fn mark_torn_down(&self) {
        self.state.lock().torn_down = true;
    }
}

/// `asyncio.gather` of the pending fallbacks: all run; the first error is
/// returned.
async fn run_fallbacks(cloud: &Cloud, pending: Vec<Pending>) -> Result<(), ServiceError> {
    crate::light::join_all(pending.into_iter().map(|p| fallback(cloud, p)))
        .await
        .into_iter()
        .collect()
}

async fn fallback(cloud: &Cloud, p: Pending) -> Result<(), ServiceError> {
    let n = p.notification.lock().clone();
    match p.fallback {
        Fallback::Legacy => send_remote_logged(cloud, &p.reg, &n).await,
        Fallback::Entity => send_cloud(cloud, &p.reg, &n).await,
    }
}

/// The push services and channels of every registration.
/// Registered `MobileAppNotifyEntity` adapter keyed by `(webhook_id, reg)`.
type NotifyAdapter = (Arc<str>, Arc<Registration>, Arc<dyn crate::notify::Notify>);

pub struct Notifier {
    hass: Hass,
    registry: Arc<Registry>,
    registrations: Registrations,
    channels: Channels,
    cloud: Cloud,
    /// `registered_targets`: legacy service name to webhook id.
    targets: Mutex<BTreeMap<String, Arc<str>>>,
    /// Shared `Components` when running inside `hars` (lazily acquires
    /// `Components::notifiers()` on first `register_services`).
    entities: Option<Arc<crate::components::Components>>,
    /// The `Notifiers` registry `MobileAppNotifyEntity` adapters are attached to.
    notifiers: std::sync::OnceLock<Arc<crate::notify::Notifiers>>,
    /// Registered `MobileAppNotifyEntity` adapters keyed by `webhook_id`.
    adapters: Mutex<Vec<NotifyAdapter>>,
    me: Weak<Notifier>,
}

struct MobileAppNotifyEntity {
    notifier: Weak<Notifier>,
    reg: Arc<Registration>,
}

impl crate::notify::Notify for MobileAppNotifyEntity {
    fn entity_id(&self) -> Option<EntityId> {
        let notifier = self.notifier.upgrade()?;
        notify_entity(&notifier.registry, &self.reg)
    }

    fn available(&self) -> bool {
        self.notifier.upgrade().is_some()
    }

    fn supported_features(&self) -> u32 {
        crate::notify::feature::TITLE
    }

    fn send_message<'a>(
        &'a self,
        message: &'a str,
        title: Option<&'a str>,
        context: Context,
    ) -> crate::light::BoxFuture<'a> {
        let Some(notifier) = self.notifier.upgrade() else {
            return Box::pin(async { Ok(()) });
        };
        let reg = self.reg.clone();
        let n = Notification {
            message: message.to_owned(),
            title: title.map(str::to_owned),
            ..Default::default()
        };
        Box::pin(async move { notifier.entity_send(reg, n, context).await })
    }
}

impl std::fmt::Debug for Notifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notifier").finish_non_exhaustive()
    }
}

/// What opening a channel left to do: the old channel's pending messages
/// go by cloud first, and the new channel is registered only if that
/// worked (HA's handler awaits the old channel's teardown, and an error
/// from it is the command's error).
pub type Opening = std::pin::Pin<Box<dyn Future<Output = Result<(), ServiceError>> + Send>>;

impl Notifier {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn new(
        hass: &Hass,
        registry: Arc<Registry>,
        registrations: Registrations,
        relay: Arc<dyn Relay>,
    ) -> Arc<Self> {
        Self::with_components(hass, registry, registrations, relay, None)
    }

    pub(super) fn with_components(
        hass: &Hass,
        registry: Arc<Registry>,
        registrations: Registrations,
        relay: Arc<dyn Relay>,
        entities: Option<Arc<crate::components::Components>>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            hass: hass.clone(),
            registry,
            registrations,
            channels: Default::default(),
            cloud: Cloud {
                relay,
                clock: hass.clock().clone(),
            },
            targets: Mutex::new(BTreeMap::new()),
            entities,
            notifiers: std::sync::OnceLock::new(),
            adapters: Mutex::new(Vec::new()),
            me: me.clone(),
        })
    }

    /// `handle_push_notification_channel` once checked: replace the app's
    /// channel with one sending through `send`. Returns the channel (for
    /// the connection to close) and, when the old channel had messages
    /// waiting for confirmation, what is left to do before it counts.
    pub fn open_channel(
        &self,
        webhook_id: &str,
        support_confirm: bool,
        send: Box<dyn Fn(&str) + Send + Sync>,
    ) -> (Arc<PushChannel>, Option<Opening>) {
        let webhook_id: Arc<str> = webhook_id.into();
        let channel = Arc::new(PushChannel {
            webhook_id: webhook_id.clone(),
            support_confirm,
            send,
            channels: Arc::downgrade(&self.channels),
            cloud: self.cloud.clone(),
            state: Mutex::new(ChannelState::default()),
        });
        // Lookup, teardown of the channel displaced and insert in one step,
        // so two opens at once can't both miss each other.
        let mut channels = self.channels.lock();
        channels.opens += 1;
        let generation = channels.opens;
        channels.latest.insert(webhook_id.clone(), generation);
        let old = channels.get(&webhook_id).cloned();
        let pending = old
            .and_then(|old| old.teardown_locked(&mut channels))
            .unwrap_or_default();
        if pending.is_empty() {
            // Registered now: no open is on its way any more (an older one
            // still waiting finds no entry and stands down).
            channels.latest.remove(&webhook_id);
            channels.insert(webhook_id, channel.clone());
            return (channel, None);
        }
        drop(channels);
        let new = channel.clone();
        let cloud = self.cloud.clone();
        // Clears this open's entry however the opening ends (done, failed or
        // dropped), unless a newer open has replaced it.
        let latest = Latest {
            channels: self.channels.clone(),
            webhook_id,
            generation,
        };
        let opening = Box::pin(async move {
            if let Err(e) = run_fallbacks(&cloud, pending).await {
                new.mark_torn_down();
                return Err(e);
            }
            // Unless the app gave up on it meanwhile, or opened another
            // since, registered or still on its way (the newest open wins;
            // HA would overwrite a newer channel without tearing it down).
            let mut channels = latest.channels.lock();
            let mut state = new.state.lock();
            let newest = channels.latest.get(&latest.webhook_id) == Some(&latest.generation);
            if !state.torn_down && newest {
                channels.insert(latest.webhook_id.clone(), new.clone());
            } else {
                state.torn_down = true;
            }
            // Before `latest` drops, which takes the map lock again.
            drop(state);
            drop(channels);
            drop(latest);
            Ok(())
        });
        (channel, Some(opening))
    }

    /// Send over the app's channel, looked up and sent to under the map
    /// lock; `false` if it has none (the caller goes by cloud).
    fn send_local(
        &self,
        webhook_id: &str,
        n: &Shared,
        reg: &Arc<Registration>,
        fallback: Fallback,
    ) -> bool {
        let channels = self.channels.lock();
        channels
            .get(webhook_id)
            .is_some_and(|c| c.notify(n, reg.clone(), fallback))
    }

    /// `handle_push_notification_confirm` once checked: `false` when the
    /// app has no channel or nothing waits under `confirm_id`.
    pub fn confirm(&self, webhook_id: &str, confirm_id: &str) -> bool {
        let channels = self.channels.lock();
        channels
            .get(webhook_id)
            .is_some_and(|c| c.confirm(confirm_id))
    }

    /// `push_registrations`: device name to webhook id for every
    /// registration that can receive push, in load order (a later name
    /// replaces an earlier one's id, as a dict does).
    fn push_registrations(&self) -> Vec<(String, Arc<str>)> {
        let mut regs: Vec<Arc<Registration>> = self
            .registrations
            .read()
            .values()
            .filter(|r| supports_push(r))
            .cloned()
            .collect();
        regs.sort_unstable_by_key(|r| r.seq);
        let mut out: Vec<(String, Arc<str>)> = Vec::with_capacity(regs.len());
        for reg in regs {
            let name = reg.device_name().unwrap_or_default();
            match out.iter_mut().find(|(n, _)| *n == name) {
                Some(slot) => slot.1 = reg.webhook_id.clone(),
                None => out.push((name, reg.webhook_id.clone())),
            }
        }
        out
    }

    /// `async_register_services` (run at start and as each entry loads or
    /// unloads, HA's `notify.async_reload`): one `notify.mobile_app_<device>`
    /// per push registration, stale ones removed, and `notify.notify`. Also
    /// the `notify` component's `send_message`.
    pub fn register_services(&self) {
        let services = self.hass.services();
        let mut registered = self.targets.lock();
        let mut stale: BTreeSet<String> = registered.keys().cloned().collect();
        for (name, hook) in self.push_registrations() {
            let target = ha_core::slugify(&format!("{DOMAIN}_{name}"));
            stale.remove(&target);
            if registered.get(&target) == Some(&hook) {
                continue;
            }
            registered.insert(target.clone(), hook);
            self.register_legacy(target);
        }
        for name in stale {
            registered.remove(&name);
            services.remove(NOTIFY, &name);
        }
        drop(registered);
        if !services.has_service(NOTIFY, NOTIFY) {
            self.register_legacy(NOTIFY.into());
        }
        // Attach `MobileAppNotifyEntity` adapters onto the shared `notify`
        // entity component (which registers `notify.send_message` once).
        let notifiers = self
            .notifiers
            .get_or_init(|| match &self.entities {
                Some(components) => components.notifiers(),
                None => {
                    let n = Arc::new(crate::notify::Notifiers::default());
                    n.register_services(&self.hass, self.registry.clone());
                    n
                }
            })
            .clone();
        let mut regs: Vec<Arc<Registration>> = self
            .registrations
            .read()
            .values()
            .filter(|r| supports_push(r))
            .cloned()
            .collect();
        regs.sort_unstable_by_key(|r| r.seq);
        let mut adapters = self.adapters.lock();
        // Diff in place so registering, reloading, or removing one phone never
        // briefly detaches unrelated phones from the shared `Notifiers` list.
        adapters.retain(|(hook, old_reg, adapter)| {
            let keep = regs
                .iter()
                .any(|r| r.webhook_id == *hook && Arc::ptr_eq(r, old_reg));
            if !keep {
                notifiers.remove_arc(adapter);
            }
            keep
        });
        for reg in regs {
            if adapters
                .iter()
                .any(|(hook, cur, _)| *hook == reg.webhook_id && Arc::ptr_eq(cur, &reg))
            {
                continue;
            }
            let adapter: Arc<dyn crate::notify::Notify> = Arc::new(MobileAppNotifyEntity {
                notifier: self.me.clone(),
                reg: reg.clone(),
            });
            notifiers.add(adapter.clone());
            adapters.push((reg.webhook_id.clone(), reg, adapter));
        }
    }

    /// A legacy service (`NOTIFY_SERVICE_SCHEMA`).
    fn register_legacy(&self, service: String) {
        let me = self.me.clone();
        self.hass.services().register_with_schema(
            NOTIFY,
            service,
            Some(
                Schema::new()
                    .required("message", Kind::String)
                    .optional("title", Kind::String)
                    .optional("target", Kind::List(Box::new(Kind::String)))
                    .optional("data", Kind::Dict),
            ),
            SupportsResponse::None,
            move |call| {
                let me = me.upgrade();
                async move {
                    match me {
                        Some(me) => me.legacy(call).await,
                        None => Ok(None),
                    }
                }
            },
        );
    }

    /// `_async_notify_message_service` and
    /// `MobileAppNotificationService.async_send_message`.
    async fn legacy(&self, call: ServiceCall) -> Result<Option<ServiceResponse>, ServiceError> {
        let d = &call.data;
        let message = d
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        // An empty title is dropped first, then HA's default one.
        let title = d
            .get("title")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty() && *t != TITLE_DEFAULT)
            .map(str::to_owned);
        let registered = self.targets.lock().get(call.service.as_str()).cloned();
        let mut targets: Vec<Arc<str>> = match registered {
            Some(hook) => vec![hook],
            None => d
                .get("target")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(Arc::from).collect())
                .unwrap_or_default(),
        };
        if targets.is_empty() {
            targets = self
                .push_registrations()
                .into_iter()
                .map(|(_, h)| h)
                .collect();
        }
        let shared: Shared = Arc::new(Mutex::new(Notification {
            message,
            title,
            data: d.get("data").cloned(),
            confirm_id: None,
        }));
        let mut failed: Vec<Arc<str>> = Vec::new();
        for target in targets {
            let Some(reg) = self.registrations.read().get(&*target).cloned() else {
                return Err(ServiceError::Unknown(key_error(&target)));
            };
            if self.send_local(&target, &shared, &reg, Fallback::Legacy) {
                self.record(&reg, None);
                continue;
            }
            if !has_push_url(&reg) {
                failed.push(target);
                continue;
            }
            let n = shared.lock().clone();
            send_remote_logged(&self.cloud, &reg, &n).await?;
            self.record(&reg, None);
        }
        if failed.is_empty() {
            return Ok(None);
        }
        Err(ServiceError::Failed(format!(
            "Device(s) with webhook id(s) {} not connected to local push notifications",
            failed.join(", ")
        )))
    }

    /// `MobileAppNotifyEntity.async_send_message`, then
    /// `_async_record_notification`.
    async fn entity_send(
        &self,
        reg: Arc<Registration>,
        n: Notification,
        context: Context,
    ) -> Result<(), ServiceError> {
        let context_set = self.hass.clock().instant();
        let shared = Arc::new(Mutex::new(n));
        let sent = self.send_local(&reg.webhook_id, &shared, &reg, Fallback::Entity);
        if !sent && has_push_url(&reg) {
            let n = shared.lock().clone();
            send_cloud(&self.cloud, &reg, &n).await?;
        } else if !sent {
            return Err(raised(
                "device_not_connected_for_local_push_notifications",
                &reg.title,
            ));
        }
        // Match HA's `CONTEXT_RECENT_TIME_SECONDS = 5` (`helpers/entity.py`).
        let elapsed = self
            .hass
            .clock()
            .instant()
            .saturating_duration_since(context_set);
        let ctx = (elapsed <= Duration::from_secs(5)).then_some(context);
        self.record(&reg, ctx);
        Ok(())
    }

    /// `_async_record_notification`: the notify entity's state becomes now.
    fn record(&self, reg: &Arc<Registration>, context: Option<Context>) {
        // Drop writes from a removed or re-paired registration so an in-flight
        // send can never overwrite its replacement entity.
        let is_live = self
            .registrations
            .read()
            .get(&reg.webhook_id)
            .is_some_and(|cur| Arc::ptr_eq(cur, reg));
        if !is_live {
            return;
        }
        let Some(entity_id) = notify_entity(&self.registry, reg) else {
            return;
        };
        let now = ha_core::isoformat(self.hass.clock().now());
        let attributes = notify_attributes(&self.registry, reg);
        self.hass
            .set_state_with_context(entity_id, now, attributes, context);
    }
}

/// `supports_push`: cloud push, or the local push channel.
pub fn supports_push(reg: &Registration) -> bool {
    reg.with_data(|data| {
        data.get("app_data")
            .and_then(Value::as_object)
            .is_some_and(|a| {
                (a.contains_key("push_token") && a.contains_key("push_url"))
                    || a.contains_key("push_websocket_channel")
            })
    })
}

/// `ATTR_PUSH_URL in entry.data[ATTR_APP_DATA]`.
fn has_push_url(reg: &Registration) -> bool {
    reg.with_data(|d| {
        d.get("app_data")
            .and_then(Value::as_object)
            .is_some_and(|a| a.contains_key("push_url"))
    })
}

/// The registration's enabled notify row's entity, when it supports push.
fn notify_entity(registry: &Registry, reg: &Registration) -> Option<EntityId> {
    if !supports_push(reg) {
        return None;
    }
    let device_id = reg.device_id()?;
    let row = registry
        .entity_by_unique_id(NOTIFY, DOMAIN, &device_id)
        .filter(|r| r.disabled_by.is_none())?;
    EntityId::parse(&row.entity_id).ok()
}

/// `has_entity_name` with no name: the device's name; and `TITLE`.
fn notify_attributes(registry: &Registry, reg: &Registration) -> Map<String, Value> {
    let mut a = Map::new();
    let row = registry.entity_by_unique_id(NOTIFY, DOMAIN, &reg.device_id().unwrap_or_default());
    let name = row
        .and_then(|r| r.name.clone())
        .or_else(|| reg.device_name());
    if let Some(name) = name {
        a.insert("friendly_name".into(), json!(name));
    }
    a.insert("supported_features".into(), json!(1));
    a
}

/// The registrations' `notify` entities (`MobileAppNotifyEntity`), made
/// when the app can receive push. Their state is the last notification's
/// time, restored (`NotifyEntity.async_internal_added_to_hass`).
pub fn setup_entities(hass: &Hass, registry: &Registry, registrations: &[Arc<Registration>]) {
    let restore = hass.restore();
    for reg in registrations {
        let Some(entity_id) = notify_entity(registry, reg) else {
            continue;
        };
        restore.register(entity_id.as_str(), None);
        let state = restore
            .last_state(entity_id.as_str())
            .and_then(|s| s.get("state").and_then(Value::as_str).map(str::to_owned))
            .filter(|s| s != "unavailable")
            .unwrap_or_else(|| "unknown".into());
        hass.set_state(entity_id, state, notify_attributes(registry, reg));
    }
}

/// The entry unloads: its notify entity goes (its state kept for restore).
pub fn unload_entity(hass: &Hass, registry: &Registry, reg: &Registration) {
    if let Some(entity_id) = reg
        .device_id()
        .and_then(|d| registry.entity_by_unique_id(NOTIFY, DOMAIN, &d))
        .and_then(|r| EntityId::parse(&r.entity_id).ok())
    {
        hass.restore().unregister(hass, entity_id.as_str());
        hass.remove_state(&entity_id);
    }
}

/// HA's `HomeAssistantError` with a `mobile_app` translation; `str()` of
/// it is the English message.
fn raised(key: &str, device_name: &str) -> ServiceError {
    let template = match key {
        "device_not_connected_for_local_push_notifications" => {
            "Device {device_name} is not connected for local push notifications"
        }
        "rate_limit_exceeded_sending_notification" => {
            "Rate limit exceeded sending notification to {device_name}"
        }
        "timeout_sending_notification" => "Timeout sending notification to {device_name}",
        _ => "Error sending notification to {device_name}",
    };
    let mut p = Map::new();
    p.insert("device_name".into(), json!(device_name));
    ServiceError::translated(
        DOMAIN,
        key,
        Some(p),
        template.replace("{device_name}", device_name),
    )
}

/// `str(KeyError(key))`.
fn key_error(key: &str) -> String {
    let mut out = String::new();
    ha_core::py::repr_str(&mut out, key);
    out
}

/// `_async_send_remote_message_target`: a cloud push whose
/// `HomeAssistantError` is logged (a warning for the rate limit, else an
/// error); any other error is raised.
async fn send_remote_logged(
    cloud: &Cloud,
    reg: &Registration,
    n: &Notification,
) -> Result<(), ServiceError> {
    match send_cloud(cloud, reg, n).await {
        Err(ServiceError::Translated(r)) => {
            if r.key == "rate_limit_exceeded_sending_notification" {
                tracing::warn!("{}", r.message);
            } else {
                tracing::error!("{}", r.message);
            }
            Ok(())
        }
        other => other,
    }
}

/// `_send_message`: POST the notification to the registration's
/// `push_url` within 10 s, and judge the relay's answer.
async fn send_cloud(
    cloud: &Cloud,
    reg: &Registration,
    n: &Notification,
) -> Result<(), ServiceError> {
    let (url, body) = reg
        .with_data(|d| relay_request(d, n))
        .map_err(|key| ServiceError::Unknown(key_error(key)))?;
    let title = reg.title.as_str();
    let client_error = |why: &str| {
        tracing::debug!(
            error = why,
            "Error sending notification to {title} [{url}]:"
        );
        Err(raised("error_sending_notification", title))
    };
    match tokio::time::timeout(SEND_TIMEOUT, cloud.relay.post(&url, body)).await {
        // aiohttp's `json()` refuses another type (a `ClientError`).
        Ok(Ok(answer)) if !is_json(&answer.content_type) => {
            client_error(&format!("unexpected mimetype: {}", answer.content_type))
        }
        Ok(Ok(answer)) => relay_answer(title, answer.status, &answer.body, cloud.clock.now()),
        Err(_) | Ok(Err(RelayError::Timeout)) => Err(raised("timeout_sending_notification", title)),
        Ok(Err(RelayError::Client(e))) => client_error(&e),
    }
}

/// The relay's URL and the request body: the notification, then
/// `push_token` and `registration_info`, in HA's order. `Err` names the
/// key that's missing (HA's `KeyError`).
fn relay_request(
    d: &Map<String, Value>,
    n: &Notification,
) -> Result<(String, String), &'static str> {
    let get = |k: &'static str| d.get(k).ok_or(k);
    let (app_id, app_version, webhook_id) =
        (get("app_id")?, get("app_version")?, get("webhook_id")?);
    let app_data = d
        .get("app_data")
        .and_then(Value::as_object)
        .ok_or("app_data")?;
    let url = app_data.get("push_url").ok_or("push_url")?;
    let token = app_data.get("push_token").ok_or("push_token")?;
    let mut body = String::from("{");
    n.fields(&mut body);
    body.push_str(",\"push_token\":");
    push_json(&mut body, token);
    body.push_str(",\"registration_info\":{\"app_id\":");
    push_json(&mut body, app_id);
    body.push_str(",\"app_version\":");
    push_json(&mut body, app_version);
    body.push_str(",\"webhook_id\":");
    push_json(&mut body, webhook_id);
    if let Some(os) = d.get("os_version") {
        body.push_str(",\"os_version\":");
        push_json(&mut body, os);
    }
    body.push_str("}}");
    Ok((url.as_str().unwrap_or_default().to_owned(), body))
}

/// aiohttp's `json()` check: the lower-cased `Content-Type` must match
/// `^application/(?:[\w.+-]+?\+)?json`.
fn is_json(content_type: &str) -> bool {
    let ct = content_type.to_ascii_lowercase();
    let Some(rest) = ct.strip_prefix("application/") else {
        return false;
    };
    if rest.starts_with("json") {
        return true;
    }
    let word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '.' | '+' | '-');
    rest.char_indices()
        .take_while(|&(_, c)| word(c))
        .any(|(i, c)| i > 0 && c == '+' && rest[i..].starts_with("+json"))
}

/// Python's type name for a JSON value.
fn py_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `str(value)` in an f-string, for the log.
fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() => ha_core::py::repr_float(f),
            _ => n.to_string(),
        },
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The rest of `_send_message` once the relay answered with JSON:
/// aiohttp's `response.json()`, `log_rate_limits`, then the status. A 2xx
/// (200, 201, 202) is a success; 429 is the rate limit error; anything else
/// is `error_sending_notification`. Python errors the answer's shape causes
/// (HA lets them escape) are unknown errors with Python's message.
fn relay_answer(
    title: &str,
    status: u16,
    body: &[u8],
    now: jiff::Timestamp,
) -> Result<(), ServiceError> {
    // `bytes.strip()`; nothing left is `None`.
    let trimmed = body.trim_ascii();
    let result: Value = if trimmed.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(trimmed).map_err(|e| ServiceError::Unknown(e.to_string()))?
    };
    // `ATTR_PUSH_RATE_LIMITS in resp`, then `resp[ATTR_PUSH_RATE_LIMITS]`.
    let found = match &result {
        Value::Object(m) => m.get("rateLimits"),
        Value::Array(a) => a.iter().any(|v| *v == "rateLimits").then_some(&result),
        Value::String(s) => s.contains("rateLimits").then_some(&result),
        other => {
            return Err(ServiceError::Unknown(format!(
                "argument of type '{}' is not a container or iterable",
                py_type(other)
            )));
        }
    };
    if let Some(rl) = found {
        if !result.is_object() {
            return Err(ServiceError::Unknown(subscript_error(&result)));
        }
        tracing::debug!("{}", rate_limit_line(title, rl, now)?);
    }
    if matches!(status, 200..=202) {
        return Ok(());
    }
    let Value::Object(m) = &result else {
        return Err(ServiceError::Unknown(format!(
            "'{}' object has no attribute 'get'",
            py_type(&result)
        )));
    };
    let message = match m.get("message") {
        Some(message) => external_message(message)?,
        None => {
            let fallback = m
                .get("errorMessage")
                .map(py_str)
                .unwrap_or_else(|| "Unknown error".into());
            format!("Internal server error, please try again later: {fallback}")
        }
    };
    tracing::debug!("Error sending notification to {title}: {message}");
    if status == 429 {
        return Err(raised("rate_limit_exceeded_sending_notification", title));
    }
    Err(raised("error_sending_notification", title))
}

/// The relay's own `message`, ended with punctuation and marked as
/// external, as HA logs it; the errors `message[-1]` raises.
fn external_message(message: &Value) -> Result<String, ServiceError> {
    match message {
        Value::String(s) => {
            let Some(last) = s.chars().last() else {
                return Err(ServiceError::Unknown("string index out of range".into()));
            };
            let mut s = s.clone();
            if !matches!(last, '.' | '?' | '!') {
                s.push('.');
            }
            s.push_str(" This message is generated externally to Home Assistant.");
            Ok(s)
        }
        // `list += str` extends the list; nothing is raised.
        Value::Array(a) if a.is_empty() => {
            Err(ServiceError::Unknown("list index out of range".into()))
        }
        Value::Array(_) => Ok(py_str(message)),
        // `dict[-1]`.
        Value::Object(_) => Err(ServiceError::Unknown("-1".into())),
        other => Err(ServiceError::Unknown(format!(
            "'{}' object is not subscriptable",
            py_type(other)
        ))),
    }
}

/// `value[str]` on something that isn't a dict.
fn subscript_error(value: &Value) -> String {
    match value {
        Value::Array(_) => "list indices must be integers or slices, not str".into(),
        Value::String(_) => "string indices must be integers, not 'str'".into(),
        other => format!("'{}' object is not subscriptable", py_type(other)),
    }
}

/// `log_rate_limits`'s line for `rateLimits`, at `now`: the counts and
/// `str(resets_at - utcnow())` without its microseconds. The errors HA's
/// lookups raise on a malformed answer are returned.
pub fn rate_limit_line(
    title: &str,
    rate_limits: &Value,
    now: jiff::Timestamp,
) -> Result<String, ServiceError> {
    let unknown = |m: String| ServiceError::Unknown(m);
    let Value::Object(rl) = rate_limits else {
        return Err(unknown(subscript_error(rate_limits)));
    };
    let field = |k: &str| rl.get(k).ok_or_else(|| unknown(key_error(k)));
    let resets_at = match field("resetsAt")? {
        Value::String(s) => parse_datetime(s)?,
        _ => return Err(unknown("argument must be str".into())),
    };
    let (successful, maximum, errors) = (field("successful")?, field("maximum")?, field("errors")?);
    let until = resets_at.as_microsecond() as i128 - now.as_microsecond() as i128;
    Ok(format!(
        "mobile_app push notification rate limits for {title}: {} sent, {} allowed, {} errors, resets in {}",
        percent_d(successful),
        percent_d(maximum),
        percent_d(errors),
        timedelta(until),
    ))
}

/// `dt_util.parse_datetime(s)` subtracted from an aware `utcnow()`: a
/// time without an offset, or none at all, is a `TypeError`.
fn parse_datetime(s: &str) -> Result<jiff::Timestamp, ServiceError> {
    if let Ok(t) = s.parse::<jiff::Timestamp>() {
        return Ok(t);
    }
    if s.parse::<jiff::civil::DateTime>().is_ok() {
        return Err(ServiceError::Unknown(
            "can't subtract offset-naive and offset-aware datetimes".into(),
        ));
    }
    Err(ServiceError::Unknown(
        "unsupported operand type(s) for -: 'NoneType' and 'datetime.datetime'".into(),
    ))
}

/// `"%d" % value`.
fn percent_d(v: &Value) -> String {
    match v {
        Value::Bool(b) => u8::from(*b).to_string(),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => i.to_string(),
            (_, Some(u), _) => u.to_string(),
            (_, _, Some(f)) => format!("{}", f.trunc()),
            _ => n.to_string(),
        },
        other => py_str(other),
    }
}

/// `str(timedelta(microseconds=us)).split(".")[0]`: `[-]D day[s], H:MM:SS`.
fn timedelta(us: i128) -> String {
    const DAY: i128 = 86_400 * 1_000_000;
    let days = us.div_euclid(DAY);
    let secs = us.rem_euclid(DAY) / 1_000_000;
    let mut out = String::new();
    if days != 0 {
        let plural = if days.abs() != 1 { "s" } else { "" };
        let _ = write!(out, "{days} day{plural}, ");
    }
    let _ = write!(
        out,
        "{}:{:02}:{:02}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timedelta_is_pythons() {
        let s = 1_000_000;
        assert_eq!(timedelta(0), "0:00:00");
        assert_eq!(timedelta(5 * s + 500_000), "0:00:05");
        assert_eq!(timedelta(-10 * s), "-1 day, 23:59:50");
        assert_eq!(timedelta((2 * 86_400 + 3_662) * s), "2 days, 1:01:02");
    }

    #[test]
    fn json_content_types_as_aiohttp() {
        for (ct, ok) in [
            ("application/json", true),
            ("Application/JSON; charset=utf-8", true),
            ("application/problem+json", true),
            ("application/vnd.api+json", true),
            ("application/jsonp", true),
            ("application/+json", false),
            ("text/json", false),
            ("text/plain", false),
            ("", false),
        ] {
            assert_eq!(is_json(ct), ok, "{ct}");
        }
    }

    fn reg(name: &str, hook: &str, app_data: Value) -> Arc<Registration> {
        let entry = json!({"entry_id": hook, "title": name, "data": {
            "webhook_id": hook, "device_name": name, "device_id": hook, "app_data": app_data}});
        Arc::new(Registration::from_entry(entry.as_object().unwrap()).unwrap())
    }

    /// `async_register_services` as entries load and unload (HA's
    /// `notify.async_reload`): one service per device name (the later
    /// registration's), none without push, stale ones removed;
    /// `notify.notify` and `send_message` stay.
    #[tokio::test]
    async fn services_follow_the_registrations() {
        let hass = Hass::new();
        let registry = Registry::in_memory(&hass, Default::default());
        let cloud = json!({"push_token": "t", "push_url": "https://relay.test/push"});
        let regs: Registrations = Default::default();
        for r in [
            reg("My Phone", "a", cloud.clone()),
            reg("Tablet", "b", json!({"push_websocket_channel": true})),
            reg("Plain", "c", json!({})),
            reg("My Phone", "d", cloud),
        ] {
            regs.write().insert(r.webhook_id.clone(), r);
        }
        let n = Notifier::new(&hass, registry, regs.clone(), Arc::new(HttpRelay));
        let services = || -> Vec<String> {
            hass.services()
                .services()
                .into_iter()
                .filter(|(d, _)| d == NOTIFY)
                .flat_map(|(_, s)| s.into_iter().map(|(name, _)| name.to_string()))
                .collect()
        };
        let target = |s: &str| n.targets.lock().get(s).map(|h| h.to_string());

        n.register_services();
        assert_eq!(
            services(),
            [
                "mobile_app_my_phone",
                "mobile_app_tablet",
                "notify",
                "send_message"
            ]
        );
        assert_eq!(target("mobile_app_my_phone").as_deref(), Some("d"));

        regs.write().remove("d");
        regs.write().remove("b");
        n.register_services();
        assert_eq!(
            services(),
            ["mobile_app_my_phone", "notify", "send_message"]
        );
        assert_eq!(target("mobile_app_my_phone").as_deref(), Some("a"));
        assert_eq!(target("mobile_app_tablet"), None);
    }

    /// A relay that answers every push with 200 and counts them.
    #[derive(Debug, Default)]
    struct Counting(std::sync::atomic::AtomicUsize);

    impl Relay for Counting {
        fn post(&self, _: &str, _: String) -> RelayFuture {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {
                Ok(RelayAnswer {
                    status: 200,
                    content_type: "application/json".into(),
                    body: b"{}".to_vec(),
                })
            })
        }
    }

    impl Counting {
        fn count(&self) -> usize {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// One app (webhook `w`) with cloud push, its services registered.
    fn phone_notifier() -> (Hass, Arc<Notifier>, Arc<Counting>) {
        let relay = Arc::new(Counting::default());
        let (hass, n) = phone_notifier_with(relay.clone());
        (hass, n, relay)
    }

    fn phone_notifier_with(relay: Arc<dyn Relay>) -> (Hass, Arc<Notifier>) {
        let hass = Hass::new();
        hass.start();
        let registry = Registry::in_memory(&hass, Default::default());
        let entry = json!({"entry_id": "e", "title": "Phone", "data": {
            "webhook_id": "w", "device_name": "Phone", "device_id": "d", "app_id": "a",
            "app_version": "1", "os_version": "2",
            "app_data": {"push_token": "t", "push_url": "https://relay.test/push"}}});
        let reg = Arc::new(Registration::from_entry(entry.as_object().unwrap()).unwrap());
        let regs: Registrations = Default::default();
        regs.write().insert(reg.webhook_id.clone(), reg);
        let n = Notifier::new(&hass, registry, regs, relay);
        n.register_services();
        (hass, n)
    }

    /// A relay whose answers wait for the test to open their gate, one per
    /// push, in order.
    #[derive(Debug, Default)]
    struct Gated(Mutex<std::collections::VecDeque<tokio::sync::oneshot::Receiver<()>>>);

    impl Relay for Gated {
        fn post(&self, _: &str, _: String) -> RelayFuture {
            let gate = self.0.lock().pop_front().expect("a gate per push");
            Box::pin(async move {
                let _ = gate.await;
                Ok(RelayAnswer {
                    status: 200,
                    content_type: "application/json".into(),
                    body: b"{}".to_vec(),
                })
            })
        }
    }

    /// Review regression: an older open still sending its predecessor's
    /// pending messages by cloud must not register over a newer one. A
    /// replaces X (X had a message pending), B opens and gets a message,
    /// C replaces B; A's pushes finish first, then C's: C, the newest, is
    /// the channel, and A is torn down.
    #[tokio::test]
    async fn a_delayed_older_open_does_not_take_over() {
        let relay = Arc::new(Gated::default());
        let (gate_a, wait_a) = tokio::sync::oneshot::channel();
        let (gate_c, wait_c) = tokio::sync::oneshot::channel();
        relay.0.lock().extend([wait_a, wait_c]);
        let (hass, n) = phone_notifier_with(relay);
        let probes: Vec<Arc<Probe>> = (0..4).map(|_| Arc::new(Probe::default())).collect();
        let [x, a, b, c] = [0, 1, 2, 3].map(|i| probes[i].clone());

        let (_, none) = open(&n, true, &x);
        assert!(none.is_none());
        notify(&hass, 0).await;
        let (a_channel, opening_a) = open(&n, true, &a);
        let opening_a = tokio::spawn(opening_a.expect("X had a message pending"));
        tokio::task::yield_now().await;
        let (b_channel, none) = open(&n, true, &b);
        assert!(none.is_none());
        notify(&hass, 1).await;
        assert_eq!(b.events.lock().len(), 1);
        let (c_channel, opening_c) = open(&n, true, &c);
        let opening_c = tokio::spawn(opening_c.expect("B had a message pending"));
        tokio::task::yield_now().await;

        gate_a.send(()).unwrap();
        opening_a.await.unwrap().unwrap();
        gate_c.send(()).unwrap();
        opening_c.await.unwrap().unwrap();

        let live = n.channels.lock().get("w").cloned().expect("a channel");
        assert!(
            Arc::ptr_eq(&live, &c_channel),
            "the newest open is the channel"
        );
        for old in [&a_channel, &b_channel] {
            assert!(old.state.lock().torn_down);
        }
        notify(&hass, 2).await;
        assert_eq!(c.events.lock().len(), 1);
        assert!(a.events.lock().is_empty());
        assert!(
            n.channels.lock().latest.is_empty(),
            "no open left on its way"
        );
    }

    /// Opens leave nothing behind in `latest`, whether they register at
    /// once, after their fallbacks, or their opening is dropped unrun.
    #[tokio::test]
    async fn opens_leave_no_generation_behind() {
        let relay = Arc::new(Gated::default());
        let (gate, wait) = tokio::sync::oneshot::channel();
        relay.0.lock().push_back(wait);
        let (hass, n) = phone_notifier_with(relay);
        let probe = || Arc::new(Probe::default());

        let (_, none) = open(&n, true, &probe());
        assert!(none.is_none());
        assert!(n.channels.lock().latest.is_empty());

        notify(&hass, 0).await;
        let (_, opening) = open(&n, true, &probe());
        let opening = opening.expect("a message pending");
        assert_eq!(n.channels.lock().latest.len(), 1);
        drop(opening);
        assert!(n.channels.lock().latest.is_empty(), "dropped unrun");

        // Nothing is registered now, so this one registers at once.
        let (_, none) = open(&n, true, &probe());
        assert!(none.is_none());
        notify(&hass, 1).await;
        let (_, opening) = open(&n, true, &probe());
        let opening = tokio::spawn(opening.expect("a message pending"));
        tokio::task::yield_now().await;
        gate.send(()).unwrap();
        opening.await.unwrap().unwrap();
        assert!(
            n.channels.lock().latest.is_empty(),
            "registered after fallback"
        );
    }

    /// What one channel's connection was sent, and how many of those went
    /// to a channel already torn down (never allowed).
    #[derive(Default)]
    struct Probe {
        events: Mutex<Vec<Value>>,
        channel: std::sync::OnceLock<Weak<PushChannel>>,
        dead: std::sync::atomic::AtomicUsize,
    }

    fn open(
        n: &Notifier,
        confirm: bool,
        probe: &Arc<Probe>,
    ) -> (Arc<PushChannel>, Option<Opening>) {
        let p = probe.clone();
        let send = Box::new(move |event: &str| {
            if let Some(c) = p.channel.get().and_then(Weak::upgrade)
                && c.state.lock().torn_down
            {
                p.dead.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            p.events.lock().push(serde_json::from_str(event).unwrap());
        });
        let (channel, opening) = n.open_channel("w", confirm, send);
        let _ = probe.channel.set(Arc::downgrade(&channel));
        (channel, opening)
    }

    async fn notify(hass: &Hass, i: usize) {
        let mut data = Map::new();
        data.insert("message".into(), json!(format!("m{i}")));
        hass.call_service(NOTIFY, "mobile_app_phone", data, None, false, None)
            .await
            .unwrap();
    }

    /// Run `f` until it holds (spawned fallbacks finish on other workers).
    async fn eventually(f: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !f() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("settled");
    }

    /// Every channel a test opened, and what its connection was sent.
    type Opened = Arc<Mutex<Vec<(Arc<PushChannel>, Arc<Probe>)>>>;

    /// Open a channel as the WS command does, waiting for what replacing
    /// the old one left to do.
    async fn open_awaited(n: &Notifier, confirm: bool, opened: &Opened) {
        let probe = Arc::new(Probe::default());
        let (channel, opening) = open(n, confirm, &probe);
        opened.lock().push((channel, probe));
        if let Some(opening) = opening {
            opening.await.unwrap();
        }
    }

    /// After a run of `sends` notifications: none reached a torn-down
    /// channel; every channel opened is either the registered one or torn
    /// down (none overwritten and left live); and each notification was
    /// delivered once. Without confirm that is one channel event or one
    /// cloud push. With confirm (nothing confirms during the run), what
    /// reached a channel since torn down went by cloud, as in HA, and what
    /// reached the live channel can still be confirmed.
    async fn delivered_once(
        n: &Notifier,
        relay: &Counting,
        opened: &Opened,
        confirm: bool,
        sends: usize,
    ) {
        let live = n.channels.lock().get("w").cloned();
        let opened = opened.lock().clone();
        let mut live_events = Vec::new();
        let mut events = 0;
        for (channel, probe) in &opened {
            assert_eq!(
                probe.dead.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "sent to a dead channel"
            );
            let registered = live.as_ref().is_some_and(|l| Arc::ptr_eq(l, channel));
            assert_ne!(
                registered,
                channel.state.lock().torn_down,
                "registered xor torn down"
            );
            events += probe.events.lock().len();
            if registered {
                live_events = probe.events.lock().clone();
            }
        }
        if confirm {
            for e in &live_events {
                assert!(n.confirm("w", e["hass_confirm_id"].as_str().unwrap()));
            }
            let want = sends - live_events.len();
            eventually(|| relay.count() >= want).await;
            assert_eq!(relay.count(), want);
        } else {
            assert_eq!(events + relay.count(), sends);
        }
    }

    /// Review regression: notifications racing the app's channel being
    /// closed or replaced, over and over, in both confirm modes, are each
    /// delivered once: never queued on a torn-down channel, never lost,
    /// never both on a live channel and by cloud.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sends_racing_teardown_deliver_once() {
        const SENDERS: usize = 4;
        const EACH: usize = 50;
        for confirm in [false, true] {
            for replace in [false, true] {
                for _ in 0..10 {
                    let (hass, n, relay) = phone_notifier();
                    let opened: Opened = Default::default();
                    open_awaited(&n, confirm, &opened).await;
                    let senders: Vec<_> = (0..SENDERS)
                        .map(|_| {
                            let hass = hass.clone();
                            tokio::spawn(async move {
                                for i in 0..EACH {
                                    notify(&hass, i).await;
                                }
                            })
                        })
                        .collect();
                    let mutator = {
                        let (n, opened) = (n.clone(), opened.clone());
                        tokio::spawn(async move {
                            for _ in 0..EACH {
                                if !replace {
                                    let last = opened.lock().last().unwrap().0.clone();
                                    last.close();
                                    tokio::task::yield_now().await;
                                }
                                open_awaited(&n, confirm, &opened).await;
                                tokio::task::yield_now().await;
                            }
                        })
                    };
                    for s in senders {
                        s.await.unwrap();
                    }
                    mutator.await.unwrap();
                    delivered_once(&n, &relay, &opened, confirm, SENDERS * EACH).await;
                }
            }
        }
    }

    /// Review regression: connections opening the app's channel at once,
    /// with notifications sent and confirmed between the opens: one
    /// channel ends up registered and every other is torn down, so nothing
    /// is confirmed on a channel that was overwritten and then sent again
    /// by its timer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_opens_leave_one_live_channel() {
        const OPENERS: usize = 4;
        const EACH: usize = 25;
        const SENDS: usize = 100;
        for _ in 0..10 {
            let (hass, n, relay) = phone_notifier();
            let opened: Opened = Default::default();
            let mut tasks: Vec<_> = (0..OPENERS)
                .map(|_| {
                    let (n, opened) = (n.clone(), opened.clone());
                    tokio::spawn(async move {
                        for _ in 0..EACH {
                            open_awaited(&n, true, &opened).await;
                            tokio::task::yield_now().await;
                        }
                    })
                })
                .collect();
            tasks.push(tokio::spawn(async move {
                for i in 0..SENDS {
                    notify(&hass, i).await;
                    tokio::task::yield_now().await;
                }
            }));
            for t in tasks {
                t.await.unwrap();
            }
            assert!(n.channels.lock().get("w").is_some());
            delivered_once(&n, &relay, &opened, true, SENDS).await;
        }
    }

    /// The relay over real HTTP: a JSON POST, and the answer as it came; a
    /// refused connection is a client error.
    #[tokio::test]
    async fn http_relay_posts_json() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut req = Vec::new();
            let mut buf = [0; 4096];
            // Headers, then a body of `content-length`.
            loop {
                let n = s.read(&mut buf).await.unwrap();
                req.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&req).to_lowercase();
                if let Some(end) = text.find("\r\n\r\n") {
                    let len: usize = text
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if req.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            let body = br#"{"message":"slow down"}"#;
            let head = format!(
                "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            s.write_all(head.as_bytes()).await.unwrap();
            s.write_all(body).await.unwrap();
            String::from_utf8(req).unwrap()
        });
        let sent = r#"{"message":"Hi ✓","push_token":"t"}"#;
        let answer = HttpRelay
            .post(
                &format!("http://{addr}/api/sendPushNotification"),
                sent.into(),
            )
            .await
            .unwrap();
        assert_eq!(
            answer,
            RelayAnswer {
                status: 429,
                content_type: "application/json; charset=utf-8".into(),
                body: br#"{"message":"slow down"}"#.to_vec(),
            }
        );
        let req = server.await.unwrap();
        assert!(
            req.starts_with("POST /api/sendPushNotification HTTP/1.1\r\n"),
            "{req}"
        );
        let lower = req.to_lowercase();
        assert!(
            lower.contains("\r\ncontent-type: application/json\r\n"),
            "{req}"
        );
        assert!(lower.contains(&format!(
            "\r\nuser-agent: {}\r\n",
            crate::USER_AGENT.to_lowercase()
        )));
        assert!(req.ends_with(&format!("\r\n\r\n{sent}")), "{req}");

        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = closed.local_addr().unwrap();
        drop(closed);
        let refused = HttpRelay
            .post(&format!("http://{addr}/push"), "{}".into())
            .await;
        assert!(matches!(refused, Err(RelayError::Client(_))), "{refused:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn adapter_diff_keeps_unrelated_phones_and_repair_and_context_expiry_guard() {
        #[derive(Debug)]
        struct GatedRelay {
            gate: Arc<tokio::sync::Notify>,
            started: Arc<tokio::sync::Notify>,
        }
        impl Relay for GatedRelay {
            fn post(&self, _: &str, _: String) -> RelayFuture {
                let gate = self.gate.clone();
                let started = self.started.clone();
                Box::pin(async move {
                    started.notify_one();
                    gate.notified().await;
                    Ok(RelayAnswer {
                        status: 200,
                        content_type: "application/json".into(),
                        body: b"{}".to_vec(),
                    })
                })
            }
        }

        let hass = Hass::new();
        hass.start();
        let registry = Registry::in_memory(
            &hass,
            ha_registry::Registries {
                entities: vec![
                    Arc::new(ha_registry::Entity {
                        entity_id: "notify.phone_a".into(),
                        platform: DOMAIN.into(),
                        unique_id: Some(json!("dev_a")),
                        config_entry_id: Some("ea".into()),
                        ..Default::default()
                    }),
                    Arc::new(ha_registry::Entity {
                        entity_id: "notify.phone_b".into(),
                        platform: DOMAIN.into(),
                        unique_id: Some(json!("dev_b")),
                        config_entry_id: Some("eb".into()),
                        ..Default::default()
                    }),
                ],
                ..Default::default()
            },
        );
        let mk = |entry_id: &str, name: &str, hook: &str, dev: &str| {
            let entry = json!({"entry_id": entry_id, "title": name, "data": {
                "webhook_id": hook, "device_name": name, "device_id": dev,
                "app_id": "a", "app_version": "1", "os_version": "2",
                "app_data": {"push_token": "t", "push_url": "https://relay.test/push"}}});
            Arc::new(Registration::from_entry(entry.as_object().unwrap()).unwrap())
        };
        let regs: Registrations = Default::default();
        let reg_a = mk("ea", "Phone A", "hook_a", "dev_a");
        regs.write().insert(reg_a.webhook_id.clone(), reg_a.clone());

        let gate = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(tokio::sync::Notify::new());
        let relay = Arc::new(GatedRelay {
            gate: gate.clone(),
            started: started.clone(),
        });
        let notifier = Notifier::new(&hass, registry.clone(), regs.clone(), relay);
        notifier.register_services();

        // 1. Adding & removing Phone B never removes/replaces Phone A's adapter pointer.
        let adapter_a_before = notifier.adapters.lock()[0].2.clone();
        let reg_b = mk("eb", "Phone B", "hook_b", "dev_b");
        regs.write().insert(reg_b.webhook_id.clone(), reg_b.clone());
        notifier.register_services();
        assert!(
            notifier
                .adapters
                .lock()
                .iter()
                .any(|(_, _, a)| Arc::ptr_eq(a, &adapter_a_before)),
            "adding Phone B must keep Phone A's adapter untouched"
        );
        regs.write().remove("hook_b");
        notifier.register_services();
        assert!(
            notifier
                .adapters
                .lock()
                .iter()
                .any(|(_, _, a)| Arc::ptr_eq(a, &adapter_a_before)),
            "removing Phone B must keep Phone A's adapter untouched"
        );

        // 2. Context expires after >5s simulated clock advance.
        let eid_a = EntityId::parse("notify.phone_a").unwrap();
        let orig_ctx = Context::new();
        let orig_ctx_id = orig_ctx.id;
        let hass_send = hass.clone();
        let t1 = tokio::spawn(async move {
            hass_send
                .call_service(
                    NOTIFY,
                    "send_message",
                    json!({"entity_id": "notify.phone_a", "message": "slow"})
                        .as_object()
                        .cloned()
                        .unwrap(),
                    None,
                    false,
                    Some(orig_ctx),
                )
                .await
        });
        started.notified().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        gate.notify_one();
        t1.await.unwrap().unwrap();
        let st1 = hass.states().get(&eid_a).unwrap();
        assert_ne!(st1.context.id, orig_ctx_id, "context must expire after >5s");

        // 3. Re-pairing Phone A while a send is in flight must not overwrite the replacement state.
        let hass_send2 = hass.clone();
        let t2 = tokio::spawn(async move {
            hass_send2
                .call_service(
                    NOTIFY,
                    "send_message",
                    json!({"entity_id": "notify.phone_a", "message": "stale"})
                        .as_object()
                        .cloned()
                        .unwrap(),
                    None,
                    false,
                    None,
                )
                .await
        });
        started.notified().await;
        // Simulate re-pair: retire old registration, swap in a fresh one for dev_a, reset state to "unknown".
        regs.write().remove("hook_a");
        let reg_a2 = mk("ea2", "Phone A", "hook_a2", "dev_a");
        regs.write().insert(reg_a2.webhook_id.clone(), reg_a2);
        notifier.register_services();
        hass.set_state(eid_a.clone(), "unknown", Map::new());
        tokio::time::advance(Duration::from_secs(1)).await;
        gate.notify_one();
        t2.await.unwrap().unwrap();
        assert_eq!(
            hass.states().get(&eid_a).unwrap().state.as_str(),
            "unknown",
            "in-flight send for retired registration must not overwrite replacement"
        );
    }
}
