//! Process / Unix-socket transport, JSON-RPC client, and crash supervisor for
//! one installed plugin.
//!
//! Design properties (`docs/PERF.md`):
//! - **Lazy start:** nothing is spawned until a config flow or an enabled
//!   config entry asks for the plugin.
//! - **Zero idle wakeups:** when quiet, `hars` awaits `AsyncRead` on the
//!   plugin's pipe/socket and `mpsc::Receiver::recv()` on the outbox.
//! - **Bounded queues, pending calls & frames:** outbox bounded to 256
//!   messages; in-flight RPCs bounded to 64; incoming NDJSON lines bounded to
//!   256 KiB.
//! - **Crash & retry supervision:** if a plugin exits or an entry enters
//!   `setup_retry`, its entities become `unavailable` on disconnect and it
//!   retries on HA's backoff ladder (`5 s` doubling to `600 s`).

use super::entities::PluginEntityStore;
use super::manifest::{InstalledPlugin, Transport};
use super::protocol::{
    DeviceUpsertParams, EntityCallParams, EntityUpsertParams, EntrySetupParams, EntryStatusParams,
    EntryUnloadParams, FlowStepParams, FlowStepResult, IncomingMessage, InitializeParams,
    InitializeResult, LogParams, MAX_FRAME_BYTES, PROTOCOL_VERSION, RpcRequest, RpcResponse,
    StateSetParams, read_bounded_line,
};
use crate::components::Components;
use crate::config_entries::{EntryState, Status};
use ha_core::ServiceError;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, mpsc, oneshot, watch};

const OUTBOX_CAPACITY: usize = 256;
const MAX_PENDING_CALLS: usize = 64;
const INIT_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Runtime errors communicating with a plugin process.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("spawning plugin {domain}: {source}")]
    Spawn {
        domain: String,
        #[source]
        source: std::io::Error,
    },
    #[error("connecting to plugin {domain} socket {}: {source}", path.display())]
    Socket {
        domain: String,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("plugin {0} is not supported on this platform")]
    UnsupportedTransport(String),
    #[error("plugin {0} handshake failed: {1}")]
    Handshake(String, String),
    #[error("plugin {0} RPC error ({1}): {2}")]
    Rpc(String, i64, String),
    #[error("plugin {0} request timed out")]
    Timeout(String),
    #[error("plugin {0} has too many in-flight requests")]
    Busy(String),
    #[error("plugin {0} disconnected")]
    Disconnected(String),
    #[error("plugin {0} returned invalid payload: {1}")]
    InvalidPayload(String, String),
}

/// Active config entry record kept so the supervisor can retry `entry.setup`
/// on transient errors or after a crash, and `reload_plugin` can migrate it
/// onto a newly installed version.
#[derive(Clone)]
pub(crate) struct ActiveEntry {
    pub entry_id: String,
    pub title: String,
    pub data: Map<String, Value>,
    pub options: Map<String, Value>,
    pub status: Arc<Status>,
    /// True once the plugin called `entry.status` during the current setup
    /// attempt so `setup_entry` does not overwrite an explicit `setup_error`
    /// or `setup_retry` with `loaded`.
    pub status_reported: Arc<AtomicBool>,
    /// True when the entry should be retried by the supervisor even if the
    /// session remains connected.
    pub needs_retry: Arc<AtomicBool>,
}

type PendingReply = oneshot::Sender<Result<Value, RuntimeError>>;
type PendingMap = Arc<Mutex<HashMap<u64, PendingReply>>>;

/// Live connection to one running plugin instance.
struct Session {
    domain: String,
    next_id: AtomicU64,
    outbox: mpsc::Sender<String>,
    pending: PendingMap,
    closed: watch::Receiver<bool>,
    child: Mutex<Option<tokio::process::Child>>,
    reader_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    writer_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Session {
    async fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RuntimeError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = RpcRequest::new(id, method, params);
        let mut line = serde_json::to_string(&req)
            .map_err(|e| RuntimeError::InvalidPayload(self.domain.clone(), e.to_string()))?;
        line.push('\n');

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock();
            if pending.len() >= MAX_PENDING_CALLS {
                return Err(RuntimeError::Busy(self.domain.clone()));
            }
            pending.insert(id, tx);
        }
        // RAII guard ensures `pending.remove(&id)` runs on normal completion,
        // timeout, error, AND future drop/cancellation (e.g. aborted config
        // flow), so pending slots can never leak.
        struct PendingGuard {
            pending: PendingMap,
            id: u64,
        }
        impl Drop for PendingGuard {
            fn drop(&mut self) {
                self.pending.lock().remove(&self.id);
            }
        }
        let _guard = PendingGuard {
            pending: self.pending.clone(),
            id,
        };

        // Cover BOTH queueing into `outbox` and awaiting the response with the
        // same timeout so a stalled plugin can never hang callers or shutdown.
        let result = tokio::time::timeout(timeout, async {
            if self.outbox.send(line).await.is_err() {
                return Err(RuntimeError::Disconnected(self.domain.clone()));
            }
            match rx.await {
                Ok(res) => res,
                Err(_) => Err(RuntimeError::Disconnected(self.domain.clone())),
            }
        })
        .await;

        match result {
            Ok(res) => res,
            Err(_) => Err(RuntimeError::Timeout(self.domain.clone())),
        }
    }

    fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    async fn wait_closed(&self) {
        let mut rx = self.closed.clone();
        let _ = rx.wait_for(|c| *c).await;
    }

    async fn shutdown(&self) {
        if !self.is_closed() {
            // Bounded send + reply timeout: if the outbox is full because the
            // plugin stopped reading, proceed straight to abort + kill.
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut line) = serde_json::to_string(&RpcRequest::new(id, "shutdown", json!({})))
            {
                line.push('\n');
                let (tx, rx) = oneshot::channel();
                self.pending.lock().insert(id, tx);
                if self.outbox.try_send(line).is_ok() {
                    let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, rx).await;
                }
                self.pending.lock().remove(&id);
            }
        }
        let child = self.child.lock().take();
        if let Some(mut child) = child {
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        let reader = self.reader_task.lock().take();
        if let Some(t) = reader {
            t.abort();
        }
        let writer = self.writer_task.lock().take();
        if let Some(t) = writer {
            t.abort();
        }
    }
}

/// Manages the lifecycle of a single installed plugin domain.
pub(crate) struct PluginHost {
    plugin: Arc<InstalledPlugin>,
    plugins_dir: PathBuf,
    /// Set once `Components` is available (at `ha_integrations::start`).
    entities: Mutex<Option<Arc<PluginEntityStore>>>,
    /// Currently running session, if any.
    session: tokio::sync::Mutex<Option<Arc<Session>>>,
    /// Enabled config entries currently running on this plugin.
    entries: Mutex<HashMap<String, ActiveEntry>>,
    /// Number of active config flows holding the plugin open.
    active_flows: Mutex<usize>,
    /// Crash/retry supervisor task, running only while `entries` is non-empty.
    supervisor: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Wakes the supervisor immediately when an entry transitions to
    /// `needs_retry` on an open connection.
    retry_notify: Notify,
    /// Set once this host has been replaced or removed so lingering flows or
    /// supervisors can never reconnect it.
    retired: AtomicBool,
    /// Serializes `unload_entry` against an in-flight supervisor `entry.setup`
    /// retry so entry removal never returns while a retry RPC is still running.
    entry_ops: tokio::sync::Mutex<()>,
}

impl PluginHost {
    pub fn new(plugin: Arc<InstalledPlugin>, plugins_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            plugin,
            plugins_dir,
            entities: Mutex::new(None),
            session: tokio::sync::Mutex::new(None),
            entries: Mutex::new(HashMap::new()),
            active_flows: Mutex::new(0),
            supervisor: Mutex::new(None),
            retry_notify: Notify::new(),
            retired: AtomicBool::new(false),
            entry_ops: tokio::sync::Mutex::new(()),
        })
    }

    pub fn plugin(&self) -> &Arc<InstalledPlugin> {
        &self.plugin
    }

    /// Attach `Components` so the plugin can register devices and entities.
    pub fn bind_components(self: &Arc<Self>, components: Arc<Components>) {
        let mut guard = self.entities.lock();
        if guard.is_some() {
            return;
        }
        let weak = Arc::downgrade(self);
        let call_fn: super::entities::CallPluginFn = Arc::new(move |params: EntityCallParams| {
            let weak = weak.clone();
            Box::pin(async move {
                let Some(host) = weak.upgrade() else {
                    return Err(ServiceError::Unknown("plugin stopped".into()));
                };
                host.call_entity(params).await
            })
        });
        *guard = Some(Arc::new(PluginEntityStore::new(
            self.plugin.clone(),
            components,
            call_fn,
        )));
    }

    fn entity_store(&self) -> Option<Arc<PluginEntityStore>> {
        self.entities.lock().clone()
    }

    /// Ensure a live, handshaked session exists (lazy start). Refuses once the
    /// host has been retired (replaced or uninstalled).
    async fn ensure_session(self: &Arc<Self>) -> Result<Arc<Session>, RuntimeError> {
        let mut guard = self.session.lock().await;
        if self.retired.load(Ordering::Acquire) {
            return Err(RuntimeError::Disconnected(self.plugin.domain().to_owned()));
        }
        if let Some(s) = guard.as_ref()
            && !s.is_closed()
        {
            return Ok(s.clone());
        }
        let session = self.connect().await?;
        if self.retired.load(Ordering::Acquire) {
            session.shutdown().await;
            return Err(RuntimeError::Disconnected(self.plugin.domain().to_owned()));
        }
        *guard = Some(session.clone());
        Ok(session)
    }

    async fn connect(self: &Arc<Self>) -> Result<Arc<Session>, RuntimeError> {
        let domain = self.plugin.domain().to_owned();
        // Persistent per-plugin data lives outside `<plugins>/<domain>/` so
        // upgrades never wipe saved credentials or pairing state.
        let data_dir = self.plugins_dir.join(".data").join(&domain);
        let mk_dir = data_dir.clone();
        let _ = tokio::task::spawn_blocking(move || std::fs::create_dir_all(&mk_dir)).await;

        let session = match &self.plugin.manifest.transport {
            Transport::Stdio { .. } => {
                let (cmd_path, args) = self
                    .plugin
                    .stdio_command()
                    .ok_or_else(|| RuntimeError::UnsupportedTransport(domain.clone()))?;
                let mut cmd = tokio::process::Command::new(&cmd_path);
                cmd.args(args)
                    .current_dir(&self.plugin.dir)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true);
                let mut child = cmd.spawn().map_err(|source| RuntimeError::Spawn {
                    domain: domain.clone(),
                    source,
                })?;
                let stdin = child.stdin.take().expect("piped stdin");
                let stdout = child.stdout.take().expect("piped stdout");
                if let Some(stderr) = child.stderr.take() {
                    let log_domain = domain.clone();
                    tokio::spawn(async move {
                        let mut lines = BufReader::new(stderr);
                        while let Ok(Some(line)) = read_bounded_line(&mut lines, 8192).await {
                            if !line.trim().is_empty() {
                                tracing::warn!(plugin = %log_domain, "{line}");
                            }
                        }
                    });
                }
                self.attach_io(BufReader::new(stdout), stdin, Some(child))
            }
            Transport::UnixSocket { .. } => {
                #[cfg(unix)]
                {
                    let sock_path = self
                        .plugin
                        .unix_socket_path(&self.plugins_dir)
                        .ok_or_else(|| RuntimeError::UnsupportedTransport(domain.clone()))?;
                    let stream =
                        tokio::net::UnixStream::connect(&sock_path)
                            .await
                            .map_err(|source| RuntimeError::Socket {
                                domain: domain.clone(),
                                path: sock_path,
                                source,
                            })?;
                    let (read_half, write_half) = stream.into_split();
                    self.attach_io(BufReader::new(read_half), write_half, None)
                }
                #[cfg(not(unix))]
                {
                    return Err(RuntimeError::UnsupportedTransport(domain));
                }
            }
        };

        // Handshake (`initialize`). Every failure path shuts down `session`
        // so neither I/O tasks nor sidecar sockets leak.
        let init_params = serde_json::to_value(InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            hars_version: env!("CARGO_PKG_VERSION").into(),
            domain: domain.clone(),
            data_dir: data_dir.display().to_string(),
        })
        .expect("serialisable");
        let raw = match session.call("initialize", init_params, INIT_TIMEOUT).await {
            Ok(v) => v,
            Err(e) => {
                session.shutdown().await;
                return Err(RuntimeError::Handshake(domain, e.to_string()));
            }
        };
        let res: InitializeResult = match serde_json::from_value(raw) {
            Ok(r) => r,
            Err(e) => {
                session.shutdown().await;
                return Err(RuntimeError::Handshake(
                    domain,
                    format!("invalid initialize result: {e}"),
                ));
            }
        };
        if res.protocol_version != PROTOCOL_VERSION {
            session.shutdown().await;
            return Err(RuntimeError::Handshake(
                domain,
                format!(
                    "protocol version mismatch: plugin {}, hars {PROTOCOL_VERSION}",
                    res.protocol_version
                ),
            ));
        }
        tracing::info!(
            plugin = %self.plugin.domain(),
            version = %self.plugin.manifest.version,
            "plugin connected"
        );
        Ok(session)
    }

    fn attach_io<R, W>(
        self: &Arc<Self>,
        mut reader: R,
        mut writer: W,
        child: Option<tokio::process::Child>,
    ) -> Arc<Session>
    where
        R: AsyncBufRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let domain = self.plugin.domain().to_owned();
        let (outbox_tx, mut outbox_rx) = mpsc::channel::<String>(OUTBOX_CAPACITY);
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (closed_tx, closed_rx) = watch::channel(false);

        let writer_domain = domain.clone();
        let writer_closed = closed_tx.clone();
        let writer_task = tokio::spawn(async move {
            while let Some(line) = outbox_rx.recv().await {
                if let Err(e) = writer.write_all(line.as_bytes()).await {
                    tracing::debug!(plugin = %writer_domain, "write failed: {e}");
                    break;
                }
                if let Err(e) = writer.flush().await {
                    tracing::debug!(plugin = %writer_domain, "flush failed: {e}");
                    break;
                }
            }
            writer_closed.send_replace(true);
        });

        let host_weak = Arc::downgrade(self);
        let reader_pending = pending.clone();
        let reader_outbox = outbox_tx.clone();
        let reader_domain = domain.clone();
        let reader_task = tokio::spawn(async move {
            loop {
                match read_bounded_line(&mut reader, MAX_FRAME_BYTES).await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let msg: IncomingMessage = match serde_json::from_str(&line) {
                            Ok(m) => m,
                            Err(e) => {
                                tracing::warn!(plugin = %reader_domain, "invalid JSON-RPC frame: {e}");
                                continue;
                            }
                        };
                        if let Some(method) = msg.method {
                            let reply = match host_weak.upgrade() {
                                Some(host) => host.handle_plugin_message(&method, msg.params),
                                None => Err((-32000, "host stopped".into())),
                            };
                            if let Some(id) = msg.id {
                                let resp = match reply {
                                    Ok(val) => RpcResponse::ok(id, val),
                                    Err((code, message)) => RpcResponse::err(id, code, message),
                                };
                                if let Ok(mut s) = serde_json::to_string(&resp) {
                                    s.push('\n');
                                    let _ = reader_outbox.try_send(s);
                                }
                            } else if let Err((_, message)) = reply {
                                tracing::warn!(
                                    plugin = %reader_domain,
                                    method = %method,
                                    "notification rejected: {message}"
                                );
                            }
                        } else if let Some(id) = msg.id.and_then(|v| v.as_u64())
                            && let Some(tx) = reader_pending.lock().remove(&id)
                        {
                            let result = if let Some(err) = msg.error {
                                Err(RuntimeError::Rpc(
                                    reader_domain.clone(),
                                    err.code,
                                    err.message,
                                ))
                            } else {
                                Ok(msg.result.unwrap_or(Value::Null))
                            };
                            let _ = tx.send(result);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(plugin = %reader_domain, "read error: {e}");
                        break;
                    }
                }
            }
            let drained: Vec<_> = reader_pending.lock().drain().collect();
            for (_, tx) in drained {
                let _ = tx.send(Err(RuntimeError::Disconnected(reader_domain.clone())));
            }
            closed_tx.send_replace(true);
        });

        Arc::new(Session {
            domain,
            next_id: AtomicU64::new(1),
            outbox: outbox_tx,
            pending,
            closed: closed_rx,
            child: Mutex::new(child),
            reader_task: Mutex::new(Some(reader_task)),
            writer_task: Mutex::new(Some(writer_task)),
        })
    }

    /// Dispatch a plugin -> `hars` notification or request, enforcing domain
    /// and capability boundaries.
    fn handle_plugin_message(&self, method: &str, params: Value) -> Result<Value, (i64, String)> {
        if self.retired.load(Ordering::Acquire) {
            return Err((-32000, "plugin retired".into()));
        }
        match method {
            "log" => {
                let p: LogParams = serde_json::from_value(params)
                    .map_err(|e| (-32602, format!("invalid log params: {e}")))?;
                let d = self.plugin.domain();
                match p.level.as_str() {
                    "error" => tracing::error!(plugin = %d, "{}", p.message),
                    "warn" | "warning" => tracing::warn!(plugin = %d, "{}", p.message),
                    "debug" => tracing::debug!(plugin = %d, "{}", p.message),
                    _ => tracing::info!(plugin = %d, "{}", p.message),
                }
                Ok(json!({ "ok": true }))
            }
            "device.upsert" => {
                let p: DeviceUpsertParams = serde_json::from_value(params)
                    .map_err(|e| (-32602, format!("invalid device.upsert params: {e}")))?;
                let store = self
                    .entity_store()
                    .ok_or_else(|| (-32000, "components not bound".into()))?;
                let device_id = store.upsert_device(p).map_err(|e| (-32001, e))?;
                Ok(json!({ "device_id": device_id }))
            }
            "entity.upsert" => {
                let p: EntityUpsertParams = serde_json::from_value(params)
                    .map_err(|e| (-32602, format!("invalid entity.upsert params: {e}")))?;
                let store = self
                    .entity_store()
                    .ok_or_else(|| (-32000, "components not bound".into()))?;
                let entity_id = store.upsert_entity(p).map_err(|e| (-32001, e))?;
                Ok(json!({ "entity_id": entity_id }))
            }
            "state.set" => {
                let p: StateSetParams = serde_json::from_value(params)
                    .map_err(|e| (-32602, format!("invalid state.set params: {e}")))?;
                let store = self
                    .entity_store()
                    .ok_or_else(|| (-32000, "components not bound".into()))?;
                store.set_state(p).map_err(|e| (-32001, e))?;
                Ok(json!({ "ok": true }))
            }
            "entry.status" => {
                let p: EntryStatusParams = serde_json::from_value(params)
                    .map_err(|e| (-32602, format!("invalid entry.status params: {e}")))?;
                let notify_retry = {
                    let entries = self.entries.lock();
                    let active = entries
                        .get(&p.entry_id)
                        .ok_or_else(|| (-32001, format!("entry {:?} not active", p.entry_id)))?;
                    match p.state.as_str() {
                        "loaded" => {
                            active.status_reported.store(true, Ordering::Release);
                            active.needs_retry.store(false, Ordering::Release);
                            active.status.set(EntryState::Loaded, None);
                            false
                        }
                        "setup_retry" => {
                            active.status_reported.store(true, Ordering::Release);
                            active.needs_retry.store(true, Ordering::Release);
                            active.status.set(
                                EntryState::SetupRetry,
                                Some(p.reason.unwrap_or_else(|| "plugin retry".into())),
                            );
                            true
                        }
                        "setup_error" => {
                            active.status_reported.store(true, Ordering::Release);
                            active.needs_retry.store(false, Ordering::Release);
                            active.status.set(
                                EntryState::SetupError,
                                Some(p.reason.unwrap_or_else(|| "plugin error".into())),
                            );
                            false
                        }
                        other => {
                            return Err((-32602, format!("unknown entry status {other:?}")));
                        }
                    }
                };
                if notify_retry {
                    self.retry_notify.notify_one();
                }
                Ok(json!({ "ok": true }))
            }
            other => Err((-32601, format!("unknown method {other:?}"))),
        }
    }

    /// Run one config flow step inside the plugin.
    pub async fn flow_step(
        self: &Arc<Self>,
        flow_id: &str,
        step_id: &str,
        input: Option<Map<String, Value>>,
    ) -> Result<FlowStepResult, RuntimeError> {
        let session = self.ensure_session().await?;
        let params = serde_json::to_value(FlowStepParams {
            flow_id: flow_id.to_owned(),
            step_id: step_id.to_owned(),
            input,
        })
        .expect("serialisable");
        let raw = session.call("flow.step", params, CALL_TIMEOUT).await?;
        serde_json::from_value(raw)
            .map_err(|e| RuntimeError::InvalidPayload(self.plugin.domain().into(), e.to_string()))
    }

    pub fn flow_opened(&self) {
        *self.active_flows.lock() += 1;
    }

    pub async fn flow_closed(self: &Arc<Self>, flow_id: &str) {
        let stop = {
            let mut flows = self.active_flows.lock();
            *flows = flows.saturating_sub(1);
            *flows == 0 && self.entries.lock().is_empty()
        };
        if let Some(session) = self.session.lock().await.clone()
            && !session.is_closed()
        {
            let _ = session
                .call(
                    "flow.cancel",
                    json!({ "flow_id": flow_id }),
                    Duration::from_secs(2),
                )
                .await;
        }
        if stop {
            self.shutdown().await;
        }
    }

    /// Set up a config entry on this plugin and start crash/retry supervision.
    pub async fn setup_entry(
        self: &Arc<Self>,
        entry: Map<String, Value>,
        components: Arc<Components>,
        status: Status,
    ) {
        let entry_id = entry
            .get("entry_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let title = entry
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let data = entry
            .get("data")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let options = entry
            .get("options")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let active = ActiveEntry {
            entry_id,
            title,
            data,
            options,
            status: Arc::new(status),
            status_reported: Arc::new(AtomicBool::new(false)),
            needs_retry: Arc::new(AtomicBool::new(false)),
        };
        self.setup_active_entry(active, components).await;
    }

    /// Set up (or re-setup after hot reload) an [`ActiveEntry`] on this host.
    pub(crate) async fn setup_active_entry(
        self: &Arc<Self>,
        active: ActiveEntry,
        components: Arc<Components>,
    ) {
        {
            let _op = self.entry_ops.lock().await;
            self.bind_components(components.clone());
            active.status_reported.store(false, Ordering::Release);
            active.needs_retry.store(false, Ordering::Release);
            let entry_id = active.entry_id.clone();
            self.entries.lock().insert(entry_id.clone(), active.clone());
            if let Some(store) = self.entity_store() {
                store.activate_entry(&entry_id);
            }

            let res = self
                .send_entry_setup(
                    &active.entry_id,
                    &active.title,
                    active.data.clone(),
                    active.options.clone(),
                )
                .await;
            match res {
                Ok(()) => {
                    // Respect any explicit `setup_error` / `setup_retry` the plugin
                    // reported via `entry.status` during `entry.setup`.
                    if !active.status_reported.load(Ordering::Acquire) {
                        active.needs_retry.store(false, Ordering::Release);
                        active.status.set(EntryState::Loaded, None);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        plugin = %self.plugin.domain(),
                        entry = %entry_id,
                        "setup failed: {e}"
                    );
                    active.needs_retry.store(true, Ordering::Release);
                    active
                        .status
                        .set(EntryState::SetupRetry, Some(e.to_string()));
                    self.retry_notify.notify_one();
                }
            }
        }
        self.ensure_supervisor(components.hass().clone());
    }

    async fn send_entry_setup(
        self: &Arc<Self>,
        entry_id: &str,
        title: &str,
        data: Map<String, Value>,
        options: Map<String, Value>,
    ) -> Result<(), RuntimeError> {
        let session = self.ensure_session().await?;
        let params = serde_json::to_value(EntrySetupParams {
            entry_id: entry_id.to_owned(),
            title: title.to_owned(),
            data,
            options,
        })
        .expect("serialisable");
        session.call("entry.setup", params, CALL_TIMEOUT).await?;
        Ok(())
    }

    /// Unload a config entry from this plugin and stop the process if nothing
    /// else uses it. Closes entity/state admission BEFORE sending
    /// `entry.unload` so no in-flight plugin notification can recreate
    /// registry rows or states during or after removal.
    pub async fn unload_entry(self: &Arc<Self>, entry_id: &str) {
        let _op = self.entry_ops.lock().await;
        let removed = self.entries.lock().remove(entry_id);
        if removed.is_none() {
            return;
        }
        if let Some(store) = self.entity_store() {
            store.deactivate_entry(entry_id);
        }
        if let Some(session) = self.session.lock().await.clone()
            && !session.is_closed()
        {
            let params = serde_json::to_value(EntryUnloadParams {
                entry_id: entry_id.to_owned(),
            })
            .expect("serialisable");
            let _ = session
                .call("entry.unload", params, Duration::from_secs(3))
                .await;
        }
        let idle = {
            let flows = self.active_flows.lock();
            *flows == 0 && self.entries.lock().is_empty()
        };
        if idle {
            let sup = self.supervisor.lock().take();
            if let Some(sup) = sup {
                sup.abort();
            }
            self.shutdown().await;
        }
    }

    /// Permanently retire this host (used on hot-reload or uninstall): stops
    /// any reconnection, closes admission and deactivates all entity
    /// adapters/states, shuts down the session/supervisor, optionally marks
    /// entries `NotLoaded`, and returns the previously active entries + bound
    /// `Components` (so a hot reload can re-attach them to the replacement
    /// host).
    pub(crate) async fn retire_and_drain(
        self: &Arc<Self>,
        mark_not_loaded: bool,
    ) -> (Vec<ActiveEntry>, Option<Arc<Components>>) {
        self.retired.store(true, Ordering::Release);
        let sup = self.supervisor.lock().take();
        if let Some(sup) = sup {
            sup.abort();
        }
        let _op = self.entry_ops.lock().await;
        let drained: Vec<ActiveEntry> = self.entries.lock().drain().map(|(_, v)| v).collect();
        let components = if let Some(store) = self.entity_store() {
            let comps = store.components();
            store.deactivate_all();
            Some(comps)
        } else {
            None
        };
        self.shutdown().await;
        if mark_not_loaded {
            for e in &drained {
                e.status.set(EntryState::NotLoaded, None);
            }
        }
        (drained, components)
    }

    async fn call_entity(self: &Arc<Self>, params: EntityCallParams) -> Result<(), ServiceError> {
        if self.retired.load(Ordering::Acquire) {
            return Err(ServiceError::Unknown(format!(
                "plugin {} was reloaded or removed",
                self.plugin.domain()
            )));
        }
        let session = self
            .session
            .lock()
            .await
            .clone()
            .filter(|s| !s.is_closed())
            .ok_or_else(|| {
                ServiceError::Unknown(format!("plugin {} is not running", self.plugin.domain()))
            })?;
        if !self.entries.lock().contains_key(&params.entry_id) {
            return Ok(());
        }
        let val = serde_json::to_value(params).expect("serialisable");
        session
            .call("entity.call", val, CALL_TIMEOUT)
            .await
            .map(|_| ())
            .map_err(|e| ServiceError::Unknown(e.to_string()))
    }

    /// Supervise the running plugin while it has active entries: handles both
    /// process/socket crashes (marking entities `unavailable`) and entries
    /// stuck in `setup_retry` on an open connection, using HA's backoff ladder.
    fn ensure_supervisor(self: &Arc<Self>, hass: ha_core::Hass) {
        let mut sup = self.supervisor.lock();
        if sup.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        let weak = Arc::downgrade(self);
        *sup = Some(tokio::spawn(async move {
            let mut attempt: u32 = 0;
            loop {
                let Some(host) = weak.upgrade() else { return };
                if host.retired.load(Ordering::Acquire) || host.entries.lock().is_empty() {
                    return;
                }
                let session = host.session.lock().await.clone();
                let connected = session.as_ref().is_some_and(|s| !s.is_closed());
                let any_retry = host
                    .entries
                    .lock()
                    .values()
                    .any(|e| e.needs_retry.load(Ordering::Acquire));

                if connected && !any_retry {
                    attempt = 0;
                    let session = session.unwrap();
                    tokio::select! {
                        _ = session.wait_closed() => {}
                        _ = host.retry_notify.notified() => {}
                    }
                    continue;
                }

                let disconnected = session.as_ref().is_none_or(|s| s.is_closed());
                if disconnected {
                    if let Some(store) = host.entity_store() {
                        store.mark_all_unavailable();
                    }
                    let snapshot: Vec<ActiveEntry> =
                        host.entries.lock().values().cloned().collect();
                    for e in &snapshot {
                        e.needs_retry.store(true, Ordering::Release);
                        e.status
                            .set(EntryState::SetupRetry, Some("plugin process exited".into()));
                    }
                    tracing::warn!(
                        plugin = %host.plugin.domain(),
                        attempt = attempt + 1,
                        "plugin exited unexpectedly; scheduling restart"
                    );
                }

                let backoff = Duration::from_secs((5u64 << attempt.min(7)).min(600));
                if let Some(session) = session.filter(|s| !s.is_closed()) {
                    // Connected, retrying entries: wake early if the socket closes.
                    tokio::select! {
                        _ = hass.setup_retry_wait(backoff) => {}
                        _ = session.wait_closed() => continue,
                    }
                } else {
                    hass.setup_retry_wait(backoff).await;
                }
                attempt = attempt.saturating_add(1);

                let Some(host) = weak.upgrade() else { return };
                if host.retired.load(Ordering::Acquire) {
                    return;
                }
                let to_retry: Vec<ActiveEntry> = host
                    .entries
                    .lock()
                    .values()
                    .filter(|e| e.needs_retry.load(Ordering::Acquire))
                    .cloned()
                    .collect();
                if to_retry.is_empty() {
                    continue;
                }
                match host.ensure_session().await {
                    Ok(session) => {
                        for e in to_retry {
                            // Hold `entry_ops` around each retry RPC so
                            // `unload_entry` waits for an in-flight setup
                            // before sending `entry.unload` and returning.
                            let _op = host.entry_ops.lock().await;
                            if host.retired.load(Ordering::Acquire)
                                || !host.entries.lock().contains_key(&e.entry_id)
                            {
                                continue;
                            }
                            e.status_reported.store(false, Ordering::Release);
                            let res = host
                                .send_entry_setup(
                                    &e.entry_id,
                                    &e.title,
                                    e.data.clone(),
                                    e.options.clone(),
                                )
                                .await;
                            if host.retired.load(Ordering::Acquire)
                                || !host.entries.lock().contains_key(&e.entry_id)
                            {
                                if !session.is_closed() {
                                    let params = serde_json::to_value(EntryUnloadParams {
                                        entry_id: e.entry_id.clone(),
                                    })
                                    .expect("serialisable");
                                    let _ = session
                                        .call("entry.unload", params, Duration::from_secs(2))
                                        .await;
                                }
                                continue;
                            }
                            match res {
                                Ok(()) => {
                                    if !e.status_reported.load(Ordering::Acquire) {
                                        e.needs_retry.store(false, Ordering::Release);
                                        e.status.set(EntryState::Loaded, None);
                                    }
                                }
                                Err(err) => {
                                    e.needs_retry.store(true, Ordering::Release);
                                    e.status.set(EntryState::SetupRetry, Some(err.to_string()));
                                }
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!(plugin = %host.plugin.domain(), "restart failed: {err}");
                        for e in to_retry {
                            e.status.set(EntryState::SetupRetry, Some(err.to_string()));
                        }
                    }
                }
            }
        }));
    }

    /// Gracefully stop the plugin and its supervisor.
    pub async fn shutdown(&self) {
        let sup = self.supervisor.lock().take();
        if let Some(sup) = sup {
            sup.abort();
        }
        let session = self.session.lock().await.take();
        if let Some(session) = session {
            session.shutdown().await;
        }
    }
}
