//! Out-of-process plugin runtime for `hars` (`docs/adr/0011-plugin-system.md`).
//!
//! Plugins live under `<config>/plugins/<domain>/` and communicate with `hars`
//! over newline-delimited JSON-RPC 2.0 (`stdin`/`stdout` for managed static
//! binaries, or a Unix domain socket under `<config>/plugins/run/` for sidecar
//! containers).
//!
//! Key guarantees:
//! - **Zero cost when unused:** if `<config>/plugins/` does not exist or has
//!   no enabled config entries, no plugin processes or background tasks run.
//! - **Zero restarts:** installing or removing a plugin through the built-in
//!   `hars_plugins` config flow or CLI takes effect immediately.
//! - **Strict scoping:** a plugin can only touch devices, entities, and states
//!   belonging to its own active config entries and declared entity domains.

pub mod entities;
pub mod manifest;
pub mod protocol;
pub mod runtime;
pub mod store;

pub use manifest::{Capabilities, InstalledPlugin, ManifestError, PluginManifest, Transport};
pub use store::{
    Catalog, CatalogPlugin, DEFAULT_CATALOG_URL, InstallSource, STORE_DOMAIN, StoreError,
};

use crate::components::Components;
use crate::config_entries::Status;
use crate::flow::{Cx, Field, Flow, Kind, Step, StepFuture};
use parking_lot::RwLock;
use protocol::{FlowStepResult, PluginField, PluginFieldKind};
use runtime::PluginHost;
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

/// Summary of an installed plugin (for CLI listings and UI dropdowns).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSummary {
    pub domain: String,
    pub name: String,
    pub version: String,
    pub transport: &'static str,
    pub config_flow: bool,
}

/// Manages every installed plugin under `<config>/plugins/`.
pub struct PluginManager {
    plugins_dir: PathBuf,
    hosts: RwLock<HashMap<String, Arc<PluginHost>>>,
    catalog_url: RwLock<String>,
    allow_insecure_http: AtomicBool,
    /// Per-domain weak async lifecycle locks so disk swaps, reloads, removals,
    /// and entry unloads serialize per domain without retaining entries after
    /// an operation finishes (including failed installs).
    domain_locks: parking_lot::Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

impl std::fmt::Debug for PluginManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginManager")
            .field("plugins_dir", &self.plugins_dir)
            .finish_non_exhaustive()
    }
}

impl PluginManager {
    /// Scan `<config_dir>/plugins/` and load every valid `plugin.json`.
    /// Missing directory or individual invalid plugins are non-fatal (logged,
    /// leaving valid plugins available). No plugin process is spawned here.
    pub fn load(config_dir: &Path) -> Arc<Self> {
        let plugins_dir = config_dir.join("plugins");
        let mut hosts = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(&plugins_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if name.starts_with('.') || name == "run" {
                    continue;
                }
                match InstalledPlugin::load(&path) {
                    Ok(installed) => {
                        let domain = installed.domain().to_owned();
                        tracing::info!(
                            plugin = %domain,
                            version = %installed.manifest.version,
                            "discovered plugin"
                        );
                        hosts.insert(
                            domain,
                            PluginHost::new(Arc::new(installed), plugins_dir.clone()),
                        );
                    }
                    Err(e) => {
                        tracing::error!("skipping plugin at {}: {e}", path.display());
                    }
                }
            }
        }
        let catalog_url = std::env::var("HARS_PLUGIN_CATALOG")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_CATALOG_URL.to_owned());
        Arc::new(Self {
            plugins_dir,
            hosts: RwLock::new(hosts),
            catalog_url: RwLock::new(catalog_url),
            allow_insecure_http: AtomicBool::new(store::env_allow_insecure_http()),
            domain_locks: parking_lot::Mutex::new(HashMap::new()),
        })
    }

    fn domain_lock(&self, domain: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.domain_locks.lock();
        map.retain(|_, w| w.strong_count() > 0);
        if let Some(existing) = map.get(domain).and_then(Weak::upgrade) {
            return existing;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        map.insert(domain.to_owned(), Arc::downgrade(&lock));
        lock
    }

    /// Where installed plugins live (`<config>/plugins`).
    pub fn plugins_dir(&self) -> &Path {
        &self.plugins_dir
    }

    pub fn catalog_url(&self) -> String {
        self.catalog_url.read().clone()
    }

    /// Override the catalog URL (used in tests and custom deployments).
    pub fn set_catalog_url(&self, url: impl Into<String>) {
        *self.catalog_url.write() = url.into();
    }

    pub fn allow_insecure_http(&self) -> bool {
        self.allow_insecure_http.load(Ordering::Acquire)
    }

    /// Allow `http://` and loopback URLs for plugin downloads (for tests and
    /// local development).
    pub fn set_allow_insecure_http(&self, allow: bool) {
        self.allow_insecure_http.store(allow, Ordering::Release);
    }

    /// Summaries of all installed plugins, sorted by domain.
    pub fn list(&self) -> Vec<PluginSummary> {
        let mut out: Vec<PluginSummary> = self
            .hosts
            .read()
            .values()
            .map(|h| {
                let m = &h.plugin().manifest;
                PluginSummary {
                    domain: m.domain.clone(),
                    name: m.name.clone(),
                    version: m.version.clone(),
                    transport: match m.transport {
                        Transport::Stdio { .. } => "stdio",
                        Transport::UnixSocket { .. } => "unix_socket",
                    },
                    config_flow: m.config_flow,
                }
            })
            .collect();
        out.sort_by(|a, b| a.domain.cmp(&b.domain));
        out
    }

    /// All installed plugin metadata snapshots, sorted by domain.
    pub fn installed(&self) -> Vec<Arc<InstalledPlugin>> {
        let mut out: Vec<Arc<InstalledPlugin>> = self
            .hosts
            .read()
            .values()
            .map(|h| h.plugin().clone())
            .collect();
        out.sort_by(|a, b| a.domain().cmp(b.domain()));
        out
    }

    /// Look up metadata for one installed plugin by domain.
    pub fn get(&self, domain: &str) -> Option<Arc<InstalledPlugin>> {
        self.hosts.read().get(domain).map(|h| h.plugin().clone())
    }

    /// Domains of installed plugins that provide a config flow.
    pub fn flow_domains(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .hosts
            .read()
            .values()
            .filter(|h| h.plugin().manifest.config_flow)
            .map(|h| h.plugin().domain().to_owned())
            .collect();
        out.sort();
        out
    }

    /// Hot-load or replace a plugin from `<plugins_dir>/<domain>/` without
    /// restarting `hars`. Serialized per domain with installs, removals, and
    /// entry unloads so a concurrently deleted entry is never replayed.
    pub async fn reload_plugin(&self, domain: &str) -> Result<Arc<InstalledPlugin>, ManifestError> {
        let lock = self.domain_lock(domain);
        let _guard = lock.lock().await;
        self.reload_plugin_locked(domain).await
    }

    async fn reload_plugin_locked(
        &self,
        domain: &str,
    ) -> Result<Arc<InstalledPlugin>, ManifestError> {
        let dir = self.plugins_dir.join(domain);
        let load_dir = dir.clone();
        let installed = tokio::task::spawn_blocking(move || InstalledPlugin::load(&load_dir))
            .await
            .map_err(|e| ManifestError::Io {
                path: dir,
                source: std::io::Error::other(e),
            })??;
        let installed = Arc::new(installed);
        let new_host = PluginHost::new(installed.clone(), self.plugins_dir.clone());
        let old = self
            .hosts
            .write()
            .insert(domain.to_owned(), new_host.clone());
        if let Some(old) = old {
            let (entries, components) = old.retire_and_drain(false).await;
            if let Some(components) = components {
                for active in entries {
                    let still_exists = active
                        .status
                        .entry()
                        .is_none_or(|(store, id)| store.get(id).is_some());
                    if still_exists {
                        new_host
                            .setup_active_entry(active, components.clone())
                            .await;
                    }
                }
            }
        }
        Ok(installed)
    }

    /// Install a plugin from a local directory and register it live. Holds the
    /// per-domain lock across BOTH the disk swap and `reload_plugin` on a
    /// detached task so overlapping upgrades or flow cancellation can never
    /// desynchronize disk and runtime.
    pub async fn install_from_dir(
        self: &Arc<Self>,
        source: &Path,
    ) -> Result<Arc<InstalledPlugin>, StoreError> {
        let this = self.clone();
        let src = source.to_path_buf();
        tokio::spawn(async move {
            let probe_src = src.clone();
            let domain = tokio::task::spawn_blocking(move || {
                InstalledPlugin::load(&probe_src).map(|p| p.domain().to_owned())
            })
            .await
            .map_err(|e| StoreError::Io(std::io::Error::other(e)))??;
            let lock = this.domain_lock(&domain);
            let _guard = lock.lock().await;
            let dir = this.plugins_dir.clone();
            let _ = tokio::task::spawn_blocking(move || store::install_from_dir(&src, &dir))
                .await
                .map_err(|e| StoreError::Io(std::io::Error::other(e)))??;
            Ok(this.reload_plugin_locked(&domain).await?)
        })
        .await
        .map_err(|e| StoreError::Io(std::io::Error::other(e)))?
    }

    /// Install a plugin from a curated catalog entry and register it live.
    /// Downloads and verifies artifacts before taking the per-domain lock,
    /// then holds the lock across BOTH the disk swap and `reload_plugin` on a
    /// detached task so slow network fetches do not stall unrelated operations
    /// and flow cancellation can never desynchronize disk and runtime.
    pub async fn install_from_catalog_entry(
        self: &Arc<Self>,
        entry: &CatalogPlugin,
    ) -> Result<Arc<InstalledPlugin>, StoreError> {
        let this = self.clone();
        let entry = entry.clone();
        tokio::spawn(async move {
            let target = store::current_target();
            let fetched = store::fetch_catalog_plugin_with_policy(
                &entry,
                &target,
                this.allow_insecure_http(),
            )
            .await?;
            let domain = fetched.manifest.domain.clone();
            let lock = this.domain_lock(&domain);
            let _guard = lock.lock().await;
            let _ = store::commit_fetched_plugin(fetched, &this.plugins_dir).await?;
            Ok(this.reload_plugin_locked(&domain).await?)
        })
        .await
        .map_err(|e| StoreError::Io(std::io::Error::other(e)))?
    }

    /// Install a plugin from a bundle JSON URL and register it live. Downloads
    /// and verifies artifacts before taking the per-domain lock, then holds
    /// the lock across BOTH the disk swap and `reload_plugin` on a detached
    /// task.
    pub async fn install_from_url(
        self: &Arc<Self>,
        url: &str,
    ) -> Result<Arc<InstalledPlugin>, StoreError> {
        let this = self.clone();
        let url = url.to_owned();
        tokio::spawn(async move {
            let entry = store::fetch_bundle_with_policy(&url, this.allow_insecure_http()).await?;
            let target = store::current_target();
            let fetched = store::fetch_catalog_plugin_with_policy(
                &entry,
                &target,
                this.allow_insecure_http(),
            )
            .await?;
            let domain = fetched.manifest.domain.clone();
            let lock = this.domain_lock(&domain);
            let _guard = lock.lock().await;
            let _ = store::commit_fetched_plugin(fetched, &this.plugins_dir).await?;
            Ok(this.reload_plugin_locked(&domain).await?)
        })
        .await
        .map_err(|e| StoreError::Io(std::io::Error::other(e)))?
    }

    /// Stop, deactivate all live entries/adapters, and uninstall a plugin from
    /// disk and memory under its per-domain lock.
    pub async fn remove_plugin(self: &Arc<Self>, domain: &str) -> Result<bool, StoreError> {
        if !manifest::valid_domain(domain) || manifest::is_reserved_domain(domain) {
            return Err(StoreError::NotFound(domain.to_owned()));
        }
        let this = self.clone();
        let domain = domain.to_owned();
        tokio::spawn(async move {
            let lock = this.domain_lock(&domain);
            let _guard = lock.lock().await;
            // Retire and drain BEFORE removing the host from `self.hosts` so
            // any concurrent `unload_entry` waits on `domain_lock` instead of
            // taking the non-plugin fast path while teardown is still running.
            let old = this.hosts.read().get(&domain).cloned();
            if let Some(host) = old {
                let _ = host.retire_and_drain(true).await;
            }
            this.hosts.write().remove(&domain);
            let dir = this.plugins_dir.clone();
            let domain_clone = domain.clone();
            tokio::task::spawn_blocking(move || store::remove_from_dir(&dir, &domain_clone))
                .await
                .map_err(|e| StoreError::Io(std::io::Error::other(e)))?
        })
        .await
        .map_err(|e| StoreError::Io(std::io::Error::other(e)))?
    }

    /// Explicitly unload a config entry from its current plugin host and wait
    /// until `entry.unload` and entity adapter removal complete. Fast-paths
    /// immediately (no lock) for non-plugin domains so built-in entry removals
    /// can never be delayed by a slow plugin upgrade.
    pub(crate) async fn unload_entry(&self, domain: &str, entry_id: &str) {
        if !self.hosts.read().contains_key(domain) {
            return;
        }
        let lock = self.domain_lock(domain);
        let _guard = lock.lock().await;
        let host = self.hosts.read().get(domain).cloned();
        if let Some(host) = host {
            host.unload_entry(entry_id).await;
        }
    }

    /// Create a `Flow` instance for the built-in `hars_plugins` store.
    pub(crate) fn make_store_flow(self: &Arc<Self>) -> Box<dyn Flow> {
        Box::new(store::StoreFlow::new(self.clone()))
    }

    /// Create a `Flow` instance backed by an installed plugin domain.
    pub(crate) fn make_plugin_flow(
        &self,
        domain: &str,
        flow_id: String,
    ) -> Option<(u32, Box<dyn Flow>)> {
        let host = self.hosts.read().get(domain)?.clone();
        if !host.plugin().manifest.config_flow {
            return None;
        }
        let version = host.plugin().manifest.capabilities.config_entry_version;
        host.flow_opened();
        Some((
            version,
            Box::new(PluginFlow {
                host,
                flow_id,
                closed: false,
            }),
        ))
    }

    /// Spawn a task that sets up `entry` on its plugin and keeps running
    /// until `flow::Manager` aborts it (and then explicitly awaits
    /// [`Self::unload_entry`]). Resolves the current host and runs initial
    /// setup under the per-domain lifecycle lock so concurrent hot-reload or
    /// uninstall can never strand the entry on a retired host.
    pub(crate) fn spawn_entry(
        self: &Arc<Self>,
        domain: &str,
        components: &Arc<Components>,
        entry: Map<String, Value>,
        status: Status,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if !self.hosts.read().contains_key(domain) {
            return None;
        }
        let this = self.clone();
        let domain = domain.to_owned();
        let components = components.clone();
        let guard = components.hass().setup_guard();
        Some(tokio::spawn(async move {
            {
                let lock = this.domain_lock(&domain);
                let _guard = lock.lock().await;
                let still_exists = status
                    .entry()
                    .is_none_or(|(store, id)| store.get(id).is_some());
                if still_exists {
                    let host = this.hosts.read().get(&domain).cloned();
                    if let Some(host) = host {
                        host.setup_entry(entry, components, status).await;
                    } else {
                        status.set(crate::config_entries::EntryState::NotLoaded, None);
                    }
                }
            }
            guard.done();
            std::future::pending::<()>().await;
        }))
    }

    /// Gracefully stop every running plugin concurrently within a hard ceiling
    /// so server shutdown never hangs on a stalled child process.
    pub async fn shutdown_all(&self) {
        let hosts: Vec<Arc<PluginHost>> = self.hosts.read().values().cloned().collect();
        if hosts.is_empty() {
            return;
        }
        let mut set = tokio::task::JoinSet::new();
        for host in hosts {
            set.spawn(async move {
                host.shutdown().await;
            });
        }
        let _ = tokio::time::timeout(Duration::from_secs(4), async {
            while set.join_next().await.is_some() {}
        })
        .await;
        set.abort_all();
    }
}

/// Adapts a plugin's `flow.step` RPC into `hars`'s [`Flow`] trait.
struct PluginFlow {
    host: Arc<PluginHost>,
    flow_id: String,
    closed: bool,
}

impl Drop for PluginFlow {
    fn drop(&mut self) {
        if !self.closed {
            self.closed = true;
            let host = self.host.clone();
            let flow_id = self.flow_id.clone();
            tokio::spawn(async move {
                host.flow_closed(&flow_id).await;
            });
        }
    }
}

impl Flow for PluginFlow {
    fn step<'a>(
        &'a mut self,
        cx: &'a mut Cx<'_>,
        step_id: &'a str,
        input: Option<Map<String, Value>>,
    ) -> StepFuture<'a> {
        Box::pin(async move {
            let res = match self.host.flow_step(&self.flow_id, step_id, input).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        plugin = %self.host.plugin().domain(),
                        "flow step {step_id:?} failed: {e}"
                    );
                    return Step::Abort {
                        reason: Cow::Borrowed("unknown"),
                    };
                }
            };
            match res {
                FlowStepResult::Form {
                    step_id,
                    schema,
                    errors,
                    description_placeholders,
                } => Step::Form {
                    step_id: Cow::Owned(step_id),
                    schema: schema.map(|fields| fields.into_iter().map(convert_field).collect()),
                    errors: errors.map(|map| {
                        map.into_iter()
                            .filter_map(|(k, v)| {
                                v.as_str()
                                    .map(|s| (Cow::Owned(k), Cow::Owned(s.to_owned())))
                            })
                            .collect()
                    }),
                    description_placeholders,
                },
                FlowStepResult::CreateEntry {
                    title,
                    unique_id,
                    data,
                } => {
                    if let Some(uid) = unique_id {
                        cx.set_unique_id(&uid);
                        if let Some(abort) = cx.abort_if_unique_id_configured() {
                            return abort;
                        }
                    }
                    Step::CreateEntry { title, data }
                }
                FlowStepResult::Abort { reason } => Step::Abort {
                    reason: Cow::Owned(reason),
                },
            }
        })
    }
}

fn convert_field(f: PluginField) -> Field {
    let kind = match f.kind {
        PluginFieldKind::String => Kind::Str,
        PluginFieldKind::Select { options } => Kind::Select(options),
    };
    Field {
        name: Cow::Owned(f.name),
        required: f.required,
        kind,
        default: f.default.map(Cow::Owned),
    }
}

#[cfg(test)]
mod tests;
