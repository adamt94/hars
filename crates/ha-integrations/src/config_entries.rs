//! `.storage/core.config_entries` (1.5) at runtime: the entries as loaded,
//! HA's `async_add` and `async_remove` (config flows, onboarding) and
//! `async_update_entry` for integrations that change their data (the
//! companion app's registration), saved 1 s after the last change
//! (`config_entries.py`'s `SAVE_DELAY`).
//!
//! What subscribers hear (`SIGNAL_CONFIG_ENTRY_CHANGED`): an entry added,
//! updated (its data, or where its setup is) or removed.
//!
//! An entry's state (`ConfigEntryState`) is kept only for entries whose
//! setup reports it: those a config flow added while `hars` runs. The API
//! shows the others as `loaded` while their integration runs.

use ha_core::Hass;
use ha_core::storage::{DelayedSave, Store};
use parking_lot::RwLock;
use serde::Serialize;
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub const KEY: &str = "core.config_entries";
const MINOR: u32 = 5;
const SAVE_DELAY: Duration = Duration::from_secs(1);

/// `ConfigEntryState`, as far as `hars` gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryState {
    NotLoaded,
    SetupInProgress,
    Loaded,
    SetupError,
    SetupRetry,
    UnloadInProgress,
}

impl EntryState {
    /// The enum's value, as the API sends it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotLoaded => "not_loaded",
            Self::SetupInProgress => "setup_in_progress",
            Self::Loaded => "loaded",
            Self::SetupError => "setup_error",
            Self::SetupRetry => "setup_retry",
            Self::UnloadInProgress => "unload_in_progress",
        }
    }
}

/// Where an entry's setup is, for the entries that report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runtime {
    pub state: EntryState,
    /// `ConfigEntry.reason`: why the setup is being retried.
    pub reason: Option<String>,
    /// The integration was loaded for it (a setup attempt has ended), which
    /// is when HA knows `supports_unload`.
    pub integration_loaded: bool,
}

/// `ConfigEntryChange`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Added,
    Removed,
    Updated,
}

impl Change {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Updated => "updated",
        }
    }
}

/// An entry's first setup attempt, owned by the task that makes it: HA's
/// `entry.setup_lock` for that attempt (a removal waits for it), and, for
/// a config flow, where to say how the attempt ended. It ends when the
/// task reports the attempt's end ([`Status::set`]), or when the task
/// itself ends or is stopped; a request that went away changes nothing.
pub struct FirstAttempt {
    _lock: Option<tokio::sync::OwnedMutexGuard<()>>,
    result: Option<tokio::sync::oneshot::Sender<Option<Runtime>>>,
}

impl FirstAttempt {
    /// A setup lock, taken: the lock for whoever must wait for the
    /// attempt, and the attempt for the entry's task.
    pub fn start() -> (Arc<tokio::sync::Mutex<()>>, Self) {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let held = lock.clone().try_lock_owned().ok();
        (
            lock,
            Self {
                _lock: held,
                result: None,
            },
        )
    }

    /// Also send where the attempt left the entry, before the lock is
    /// released (so whoever waited on the lock can't have changed it).
    pub fn tell(mut self, result: tokio::sync::oneshot::Sender<Option<Runtime>>) -> Self {
        self.result = Some(result);
        self
    }
}

/// How an entry's setup says where it is. [`Status::none`] records
/// nothing: the entries started at start-up, whose state the API works
/// out.
pub struct Status {
    entry: Option<(Arc<ConfigEntries>, String)>,
    first: parking_lot::Mutex<Option<FirstAttempt>>,
}

impl Status {
    pub fn none() -> Self {
        Self {
            entry: None,
            first: Default::default(),
        }
    }

    pub fn new(entries: Arc<ConfigEntries>, entry_id: &str) -> Self {
        Self {
            entry: Some((entries, entry_id.to_owned())),
            first: Default::default(),
        }
    }

    /// With the entry's first setup attempt, which ends at the first
    /// [`Status::set`] that says so.
    pub fn with_first(self, first: FirstAttempt) -> Self {
        *self.first.lock() = Some(first);
        self
    }

    /// The entries and this entry's id, if it reports to them.
    pub(crate) fn entry(&self) -> Option<(&Arc<ConfigEntries>, &str)> {
        self.entry
            .as_ref()
            .map(|(entries, id)| (entries, id.as_str()))
    }

    pub fn set(&self, state: EntryState, reason: Option<String>) {
        if let Some((entries, entry_id)) = &self.entry {
            entries.set_state(entry_id, state, reason);
        }
        // The first attempt has ended: its result, then its lock.
        if matches!(
            state,
            EntryState::Loaded | EntryState::SetupError | EntryState::SetupRetry
        ) && let Some(first) = self.first.lock().take()
            && let Some(result) = first.result
        {
            let runtime = self
                .entry
                .as_ref()
                .and_then(|(entries, entry_id)| entries.runtime(entry_id));
            let _ = result.send(runtime);
        }
    }
}

/// A subscription's `live` flag: false once unsubscribed or closed.
pub type Live = Arc<parking_lot::Mutex<bool>>;
/// Told each change with the entry as it is then; `false` drops it.
pub type Listener = Box<dyn Fn(Change, &Map<String, Value>) -> bool + Send + Sync>;

pub struct ConfigEntries {
    hass: Hass,
    /// `None`: there was no file (a fresh install).
    entries: RwLock<Option<Vec<Map<String, Value>>>>,
    save: Option<Arc<DelayedSave>>,
    /// The file couldn't be read as this version, so it is never
    /// written: nothing added now would outlive a restart.
    read_only: bool,
    /// By entry id, for entries whose setup reports its state. Empty until
    /// a config flow adds an entry.
    runtime: parking_lot::Mutex<Vec<(String, Runtime)>>,
    /// `config_entries/subscribe`rs. Empty while nobody subscribes.
    listeners: parking_lot::Mutex<Vec<(Live, Listener)>>,
    /// How many times each entry's data has been changed here, by entry
    /// id: a value read with its revision is still the one written last
    /// only while the revision is the same. Changed with the entries'
    /// write lock held. Empty until an entry's data changes.
    revisions: parking_lot::Mutex<Vec<(String, u64)>>,
}

impl std::fmt::Debug for ConfigEntries {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigEntries").finish_non_exhaustive()
    }
}

fn store(dir: &Path) -> Store {
    // Private (0600, not HA's 0644): it holds the app's secret and tokens
    // (spec-registry-writes.md decision D3). HA reads it either way.
    Store::new(dir, KEY, 1, MINOR, true)
}

#[derive(Default, serde::Deserialize)]
struct File {
    #[serde(default)]
    entries: Vec<Map<String, Value>>,
}

/// Tell the listeners a change, dropping those that have gone.
/// An entry the user chose to ignore (`SOURCE_IGNORE`): it is set up
/// never, and stands in nobody's way.
pub(crate) fn ignored(entry: &Map<String, Value>) -> bool {
    entry.get("source").and_then(Value::as_str) == Some("ignore")
}

/// Whether an entry is one that gets set up (`ConfigEntry.async_setup`
/// and `async_migrate`): not disabled, not ignored, and not written by a
/// newer version of its config flow than `hars` runs. The rule for
/// start-up, and for who may be the one entry for a unique id.
pub(crate) fn eligible(entry: &Map<String, Value>) -> bool {
    entry.get("disabled_by").is_none_or(Value::is_null)
        && !ignored(entry)
        && !crate::entry_too_new(entry)
}

fn tell(listeners: &mut Vec<(Live, Listener)>, change: Change, entry: &Map<String, Value>) {
    if listeners.is_empty() {
        return;
    }
    listeners.retain(|(_, listener)| listener(change, entry));
    if listeners.is_empty() {
        *listeners = Vec::new();
    }
}

impl ConfigEntries {
    /// Read the store (blocking) and save changes back to it. One that
    /// can't be read is logged and treated as no entries, so nothing
    /// starts, and it is never overwritten.
    pub fn load(hass: &Hass, config_dir: &Path) -> Arc<Self> {
        let (entries, writable) = match store(config_dir).load_with_minor::<File>() {
            Ok(Some((file, minor))) => (Some(file.entries), minor == MINOR),
            Ok(None) => (None, true),
            Err(e) => {
                tracing::error!("{e}; starting no integrations");
                (Some(Vec::new()), false)
            }
        };
        Arc::new_cyclic(|weak: &std::sync::Weak<Self>| {
            let (weak, dir) = (weak.clone(), config_dir.to_owned());
            let save = writable.then(|| {
                DelayedSave::spawn(KEY, move || match weak.upgrade() {
                    Some(this) => this.write(&dir),
                    None => Ok(()),
                })
            });
            if !writable {
                tracing::warn!("{KEY} is not at minor version {MINOR}; it won't be written");
            }
            Self {
                hass: hass.clone(),
                entries: RwLock::new(entries),
                save,
                read_only: !writable,
                runtime: Default::default(),
                listeners: Default::default(),
                revisions: Default::default(),
            }
        })
    }

    /// Kept in memory only (tests).
    pub fn in_memory(hass: &Hass, entries: Vec<Map<String, Value>>) -> Arc<Self> {
        Arc::new(Self {
            hass: hass.clone(),
            entries: RwLock::new(Some(entries)),
            save: None,
            read_only: false,
            runtime: Default::default(),
            listeners: Default::default(),
            revisions: Default::default(),
        })
    }

    /// Whether the entries can't be saved (see [`Self::load`]): then
    /// nothing should be added from the UI.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// The entries; `None` on a fresh install.
    pub fn list(&self) -> Option<Vec<Map<String, Value>>> {
        self.entries.read().clone()
    }

    /// The entry with this id.
    pub fn get(&self, entry_id: &str) -> Option<Map<String, Value>> {
        let guard = self.entries.read();
        guard
            .iter()
            .flatten()
            .find(|e| e.get("entry_id").and_then(Value::as_str) == Some(entry_id))
            .cloned()
    }

    /// `async_add`: a new entry, saved; subscribers hear `added`.
    pub fn add(&self, entry: Map<String, Value>) {
        self.insert(entry, false);
    }

    /// [`Self::add`] for an entry whose setup will report its state: it
    /// starts `not_loaded`, as HA's new entry does.
    pub fn add_tracked(&self, entry: Map<String, Value>) {
        self.insert(entry, true);
    }

    /// The listeners are held from before a change until they have heard
    /// it, here and wherever an entry changes: someone subscribing sees
    /// the change in its first list or as news, never both or neither.
    fn insert(&self, entry: Map<String, Value>, tracked: bool) {
        let mut listeners = self.listeners.lock();
        if let (true, Some(id)) = (tracked, entry.get("entry_id").and_then(Value::as_str)) {
            self.runtime.lock().push((
                id.to_owned(),
                Runtime {
                    state: EntryState::NotLoaded,
                    reason: None,
                    integration_loaded: false,
                },
            ));
        }
        self.entries
            .write()
            .get_or_insert_with(Vec::new)
            .push(entry.clone());
        self.schedule();
        tell(&mut listeners, Change::Added, &entry);
    }

    /// `async_remove`'s end: the entry, if it was there, leaves the list
    /// and the file, subscribers hear `removed` with the state it ended
    /// in, and that state is forgotten. Call it once what belonged to the entry is cleaned up
    /// (`_async_clean_up` tells last).
    pub fn remove(&self, entry_id: &str) -> Option<Map<String, Value>> {
        let mut listeners = self.listeners.lock();
        let removed = {
            let mut guard = self.entries.write();
            let list = guard.as_mut()?;
            let i = list
                .iter()
                .position(|e| e.get("entry_id").and_then(Value::as_str) == Some(entry_id))?;
            self.forget_revision(entry_id);
            list.remove(i)
        };
        self.schedule();
        tell(&mut listeners, Change::Removed, &removed);
        {
            let mut runtime = self.runtime.lock();
            runtime.retain(|(e, _)| e != entry_id);
            if runtime.is_empty() {
                // Nothing is held while no entry reports its state.
                *runtime = Vec::new();
            }
        }
        Some(removed)
    }

    /// `_async_remove` for an entry that a new one replaces: it leaves the
    /// list and the file now, and nobody is told yet. HA cleans up what
    /// belonged to it, and tells, once the new entry is added and set up:
    /// [`Self::tell_removed`] then.
    pub fn take(&self, entry_id: &str) -> Option<Map<String, Value>> {
        let _listeners = self.listeners.lock();
        let removed = {
            let mut guard = self.entries.write();
            let list = guard.as_mut()?;
            let i = list
                .iter()
                .position(|e| e.get("entry_id").and_then(Value::as_str) == Some(entry_id))?;
            self.forget_revision(entry_id);
            list.remove(i)
        };
        self.schedule();
        Some(removed)
    }

    /// The end of a removal begun with [`Self::take`]: subscribers hear
    /// `removed`, and the entry's state is forgotten.
    pub fn tell_removed(&self, entry: &Map<String, Value>) {
        let mut listeners = self.listeners.lock();
        tell(&mut listeners, Change::Removed, entry);
        let id = entry.get("entry_id").and_then(Value::as_str);
        let mut runtime = self.runtime.lock();
        runtime.retain(|(e, _)| Some(e.as_str()) != id);
        if runtime.is_empty() {
            *runtime = Vec::new();
        }
    }

    /// Hear every change from now on, while `live` is true. `first` runs
    /// before any of them: it reads the entries as they are (its first
    /// list), and no change can come between that and the listening.
    pub fn subscribe(&self, live: Live, listener: Listener, first: impl FnOnce()) {
        let mut listeners = self.listeners.lock();
        listeners.retain(|(live, _)| *live.lock());
        first();
        listeners.push((live, listener));
    }

    /// Drop the listeners whose subscription has ended (call when one
    /// ends, after its `live` is false and released), so a closed
    /// connection leaves nothing behind.
    pub fn prune(&self) {
        let mut listeners = self.listeners.lock();
        listeners.retain(|(live, _)| *live.lock());
        if listeners.is_empty() {
            *listeners = Vec::new();
        }
    }

    /// Where the entry's setup is, if it reports it.
    pub fn runtime(&self, entry_id: &str) -> Option<Runtime> {
        self.runtime
            .lock()
            .iter()
            .find(|(e, _)| e == entry_id)
            .map(|(_, r)| r.clone())
    }

    /// `ConfigEntry._async_set_state`: subscribers hear `updated`, and
    /// nothing for an entry that is gone.
    pub fn set_state(&self, entry_id: &str, state: EntryState, reason: Option<String>) {
        let mut listeners = self.listeners.lock();
        let Some(entry) = self.get(entry_id) else {
            return;
        };
        {
            let mut runtime = self.runtime.lock();
            // An entry being unloaded was loaded.
            let ended = matches!(
                state,
                EntryState::Loaded
                    | EntryState::SetupError
                    | EntryState::SetupRetry
                    | EntryState::UnloadInProgress
            );
            match runtime.iter_mut().find(|(e, _)| e == entry_id) {
                Some((_, r)) => {
                    r.state = state;
                    r.reason = reason;
                    r.integration_loaded |= ended;
                }
                None => runtime.push((
                    entry_id.to_owned(),
                    Runtime {
                        state,
                        reason,
                        integration_loaded: ended,
                    },
                )),
            }
        }
        tell(&mut listeners, Change::Updated, &entry);
    }

    /// The entry of `domain` with this `unique_id`: one that is set up
    /// before an ignored one, so an ignored entry never hides it.
    pub fn find_unique(&self, domain: &str, unique_id: &str) -> Option<Map<String, Value>> {
        let guard = self.entries.read();
        let has = |e: &&Map<String, Value>| {
            e.get("domain").and_then(Value::as_str) == Some(domain)
                && e.get("unique_id").and_then(Value::as_str) == Some(unique_id)
        };
        let all = || guard.iter().flatten().filter(has);
        all()
            .find(|e| !ignored(e))
            .or_else(|| all().next())
            .cloned()
    }

    /// `_async_current_ids(include_ignore=False)`: the unique ids of
    /// `domain`'s entries, but for the ignored ones.
    pub fn unique_ids(&self, domain: &str) -> Vec<String> {
        let guard = self.entries.read();
        guard
            .iter()
            .flatten()
            .filter(|e| e.get("domain").and_then(Value::as_str) == Some(domain))
            .filter(|e| !ignored(e))
            .filter_map(|e| e.get("unique_id")?.as_str().map(str::to_owned))
            .collect()
    }

    /// Whether an entry of `domain` has `value` under `key`
    /// (`_async_abort_entries_match`): in its options, else in its data
    /// (`ChainMap(entry.options, entry.data)`), ignored entries left out.
    pub fn has_data(&self, domain: &str, key: &str, value: &str) -> bool {
        let guard = self.entries.read();
        guard.iter().flatten().any(|e| {
            let of = |part: &str| e.get(part).and_then(|d| d.get(key));
            e.get("domain").and_then(Value::as_str) == Some(domain)
                && !ignored(e)
                && of("options").or_else(|| of("data")).and_then(Value::as_str) == Some(value)
        })
    }

    /// An entry becomes the one entry of `domain` for `unique_id`
    /// (`async_update_entry(entry, unique_id=…)` for an imported entry
    /// that never had a unique id, or has a stale one), looked up and
    /// taken in one step: of two entries that find out at the same moment
    /// that they are for the same thing, one gets it.
    ///
    /// False if another entry that gets set up ([`eligible`]) is the one:
    /// it has the unique id and this entry doesn't, or both have it and
    /// the other is listed first. The caller then doesn't start this
    /// entry. An entry that is disabled or ignored runs nothing, so it
    /// is in nobody's way: the usable entry of an install that has the
    /// same bridge twice, once disabled, is set up.
    pub fn claim_unique_id(&self, domain: &str, entry_id: &str, unique_id: &str) -> bool {
        let mut listeners = self.listeners.lock();
        {
            let mut guard = self.entries.write();
            let Some(list) = guard.as_mut() else {
                return true;
            };
            let has = |e: &Map<String, Value>| {
                e.get("domain").and_then(Value::as_str) == Some(domain)
                    && e.get("unique_id").and_then(Value::as_str) == Some(unique_id)
            };
            let Some(me) = list
                .iter()
                .position(|e| e.get("entry_id").and_then(Value::as_str) == Some(entry_id))
            else {
                return true;
            };
            let mine = has(&list[me]);
            let taken = list
                .iter()
                .enumerate()
                .any(|(i, e)| i != me && has(e) && eligible(e) && (!mine || i < me));
            if taken {
                return false;
            }
            if mine {
                return true;
            }
            let entry = &mut list[me];
            entry.insert("unique_id".into(), Value::String(unique_id.to_owned()));
            entry.insert(
                "modified_at".into(),
                Value::String(ha_core::isoformat(self.hass.clock().now())),
            );
        }
        self.schedule();
        if let Some(entry) = self.get(entry_id) {
            tell(&mut listeners, Change::Updated, &entry);
        }
        true
    }

    /// A new entry as `ConfigEntry(…)` and `as_dict` make it (a ULID id).
    pub fn new_entry(
        &self,
        domain: &str,
        source: &str,
        title: &str,
        unique_id: Option<&str>,
        data: Map<String, Value>,
    ) -> Map<String, Value> {
        let now = self.hass.clock().now();
        let iso = ha_core::isoformat(now);
        let mut e = Map::new();
        e.insert("created_at".into(), Value::String(iso.clone()));
        e.insert("data".into(), Value::Object(data));
        e.insert("discovery_keys".into(), Value::Object(Map::new()));
        e.insert("disabled_by".into(), Value::Null);
        e.insert("domain".into(), Value::String(domain.into()));
        e.insert(
            "entry_id".into(),
            Value::String(ha_core::ContextId::at(now).to_string()),
        );
        e.insert("minor_version".into(), Value::from(1));
        e.insert("modified_at".into(), Value::String(iso));
        e.insert("options".into(), Value::Object(Map::new()));
        e.insert("pref_disable_new_entities".into(), Value::Bool(false));
        e.insert("pref_disable_polling".into(), Value::Bool(false));
        e.insert("source".into(), Value::String(source.into()));
        e.insert("subentries".into(), Value::Array(Vec::new()));
        e.insert("title".into(), Value::String(title.into()));
        e.insert(
            "unique_id".into(),
            unique_id.map_or(Value::Null, |u| Value::String(u.into())),
        );
        e.insert("version".into(), Value::from(1));
        e
    }

    fn schedule(&self) {
        if let Some(save) = &self.save {
            save.schedule(SAVE_DELAY);
        }
    }

    /// An entry's domain and whether it's disabled, for the registries.
    pub fn info(&self, entry_id: &str) -> Option<ha_registry::EntryInfo> {
        let guard = self.entries.read();
        let entry = guard
            .iter()
            .flatten()
            .find(|e| e.get("entry_id").and_then(Value::as_str) == Some(entry_id))?;
        Some(ha_registry::EntryInfo {
            domain: entry.get("domain")?.as_str()?.to_owned(),
            disabled: entry.get("disabled_by").is_some_and(|d| !d.is_null()),
        })
    }

    /// An entry's data revision: changed with each change of its data.
    fn revision(&self, entry_id: &str) -> u64 {
        let revisions = self.revisions.lock();
        revisions
            .iter()
            .find(|(id, _)| id == entry_id)
            .map_or(0, |(_, r)| *r)
    }

    /// The entry's data changed (called with the entries' write lock held).
    fn bump(&self, entry_id: &str) {
        let mut revisions = self.revisions.lock();
        match revisions.iter_mut().find(|(id, _)| id == entry_id) {
            Some((_, r)) => *r += 1,
            None => revisions.push((entry_id.to_owned(), 1)),
        }
    }

    /// The entry left the list (called with the entries' write lock held):
    /// its revision goes with it, and the list's memory once it is empty.
    fn forget_revision(&self, entry_id: &str) {
        let mut revisions = self.revisions.lock();
        revisions.retain(|(id, _)| id != entry_id);
        if revisions.is_empty() {
            *revisions = Vec::new();
        }
    }

    /// Test access to how many entries have a revision.
    #[cfg(test)]
    pub(crate) fn revision_count(&self) -> usize {
        self.revisions.lock().len()
    }

    /// `data[key]` of an entry, with the data's revision, read together.
    pub fn data_value(&self, entry_id: &str, key: &str) -> Option<(String, u64)> {
        let guard = self.entries.read();
        let entry = guard
            .iter()
            .flatten()
            .find(|e| e.get("entry_id").and_then(Value::as_str) == Some(entry_id))?;
        let value = entry.get("data")?.get(key)?.as_str()?.to_owned();
        Some((value, self.revision(entry_id)))
    }

    /// One field of an entry's data, `key`, becomes `value`, if the data is
    /// still at `revision` (as [`ConfigEntries::data_value`] read it): looked
    /// at and changed under one write lock, so data written meanwhile (a
    /// flow's address, even one changed back to what it was) is never
    /// overwritten. The rest of the data stays. `modified_at`, the save and
    /// the listeners as [`ConfigEntries::update_data`]. `false` if nothing
    /// changed.
    pub fn replace_data_value(
        &self,
        entry_id: &str,
        key: &str,
        revision: u64,
        value: &str,
    ) -> bool {
        #[cfg(test)]
        tests::before_write();
        let mut listeners = self.listeners.lock();
        {
            let mut guard = self.entries.write();
            let Some(entry) = guard
                .iter_mut()
                .flatten()
                .find(|e| e.get("entry_id").and_then(Value::as_str) == Some(entry_id))
            else {
                return false;
            };
            let Some(Value::Object(data)) = entry.get_mut("data") else {
                return false;
            };
            if self.revision(entry_id) != revision
                || data.get(key).and_then(Value::as_str) == Some(value)
            {
                return false;
            }
            data.insert(key.to_owned(), Value::String(value.to_owned()));
            entry.insert(
                "modified_at".into(),
                Value::String(ha_core::isoformat(self.hass.clock().now())),
            );
            self.bump(entry_id);
        }
        self.schedule();
        if let Some(entry) = self.get(entry_id) {
            tell(&mut listeners, Change::Updated, &entry);
        }
        true
    }

    /// `async_update_entry(entry, data=…)`: when the data differs it is
    /// replaced, `modified_at` moves and the save is scheduled. `false`
    /// for no change (or no such entry).
    pub fn update_data(&self, entry_id: &str, data: Map<String, Value>) -> bool {
        #[cfg(test)]
        tests::before_write();
        let mut listeners = self.listeners.lock();
        {
            let mut guard = self.entries.write();
            let Some(entry) = guard
                .iter_mut()
                .flatten()
                .find(|e| e.get("entry_id").and_then(Value::as_str) == Some(entry_id))
            else {
                return false;
            };
            if entry.get("data").and_then(Value::as_object) == Some(&data) {
                return false;
            }
            entry.insert("data".into(), Value::Object(data));
            entry.insert(
                "modified_at".into(),
                Value::String(ha_core::isoformat(self.hass.clock().now())),
            );
            self.bump(entry_id);
        }
        self.schedule();
        if let Some(entry) = self.get(entry_id) {
            tell(&mut listeners, Change::Updated, &entry);
        }
        true
    }

    /// Write what's pending (at stop).
    pub async fn flush(&self) {
        if let Some(save) = &self.save {
            save.flush().await;
        }
    }

    /// HA's layout: each entry one compact line with sorted keys (a
    /// `serde_json::Map` is sorted), in a 2-space envelope.
    fn write(&self, dir: &Path) -> std::io::Result<()> {
        #[derive(Serialize)]
        struct Out {
            entries: Vec<Box<serde_json::value::RawValue>>,
        }
        let entries = self.list().unwrap_or_default();
        let rows = entries
            .iter()
            .map(|e| serde_json::value::to_raw_value(e).map_err(std::io::Error::other))
            .collect::<std::io::Result<_>>()?;
        store(dir).save(&Out { entries: rows })
    }

    #[cfg(test)]
    pub(crate) fn write_to(&self, dir: &Path) -> std::io::Result<()> {
        self.write(dir)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    thread_local! {
        /// What this thread does at the start of its next write of an
        /// entry's data, before it looks at the entry: a test makes another
        /// writer come first there.
        static BEFORE_WRITE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::RefCell::new(None) };
    }

    pub(crate) fn before_write() {
        let hook = BEFORE_WRITE.with(|hook| hook.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Run `hook` at the start of this thread's next write of entry data.
    pub(crate) fn set_before_write(hook: impl FnOnce() + 'static) {
        BEFORE_WRITE.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    /// HA's own files (the registry-write runs) are written back byte for
    /// byte.
    #[tokio::test]
    async fn files_round_trip_byte_for_byte() {
        for name in ["before", "after_update_registration"] {
            let text = match name {
                "before" => include_str!("config_entries/vectors/before.json"),
                _ => include_str!("config_entries/vectors/after_update_registration.json"),
            };
            let dir =
                std::env::temp_dir().join(format!("hars-entries-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join(".storage")).unwrap();
            std::fs::write(dir.join(".storage").join(KEY), text).unwrap();
            let hass = Hass::new();
            let entries = ConfigEntries::load(&hass, &dir);
            entries.write_to(&dir).unwrap();
            let written = std::fs::read_to_string(dir.join(".storage").join(KEY)).unwrap();
            assert!(written == text, "{name}:\n{written}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Review regression: a save rewrites every entry, so other entries'
    /// numbers must come back exactly (HA's orjson keeps them).
    #[tokio::test]
    async fn other_entries_keep_their_numbers() {
        let dir = std::env::temp_dir().join(format!("hars-entries-float-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".storage")).unwrap();
        let file = dir.join(".storage").join(KEY);
        let text = r#"{
  "version": 1,
  "minor_version": 5,
  "key": "core.config_entries",
  "data": {
    "entries": [
      {"data":{"a":1},"entry_id":"one","options":{"x":25.096823722419074,"y":1e-7,"z":12345678901234567890}},
      {"data":{"a":1},"entry_id":"two"}
    ]
  }
}"#;
        std::fs::write(&file, text).unwrap();
        let entries = ConfigEntries::load(&Hass::new(), &dir);
        let mut data = Map::new();
        data.insert("a".into(), Value::from(2));
        assert!(entries.update_data("two", data));
        entries.flush().await;
        let saved = std::fs::read_to_string(&file).unwrap();
        assert!(saved.contains(r#""x":25.096823722419074"#), "{saved}");
        assert!(saved.contains(r#""z":12345678901234567890"#), "{saved}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review regression: a removed or replaced entry's data revision goes
    /// with it, so adding, changing and removing entries leaves nothing.
    #[tokio::test]
    async fn a_removed_entry_leaves_no_revision() {
        let entry = |id: &str| {
            let mut e = Map::new();
            e.insert("entry_id".into(), Value::from(id));
            e.insert("data".into(), Value::Object(Map::new()));
            e
        };
        let entries = ConfigEntries::in_memory(&Hass::new(), vec![entry("one"), entry("two")]);
        for id in ["one", "two"] {
            let mut data = Map::new();
            data.insert("host".into(), Value::from("192.0.2.1"));
            assert!(entries.update_data(id, data));
        }
        assert_eq!(entries.revision_count(), 2);
        assert!(entries.remove("one").is_some());
        assert_eq!(entries.revision_count(), 1);
        assert!(entries.take("two").is_some());
        assert_eq!(entries.revision_count(), 0);
        assert_eq!(entries.revisions.lock().capacity(), 0);
    }

    /// A subscriber's first look and the changes after it never overlap,
    /// and a subscription that ended leaves nothing behind.
    #[tokio::test]
    async fn subscribers_hear_each_change_once_and_are_dropped() {
        let entries = ConfigEntries::in_memory(&Hass::new(), Vec::new());
        let entry = entries.new_entry("sun", "import", "Sun", None, Map::new());
        let id = entry["entry_id"].as_str().unwrap().to_owned();
        entries.add_tracked(entry);
        let heard = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let live: Live = Arc::new(parking_lot::Mutex::new(true));
        let listener: Listener = {
            let (heard, live) = (heard.clone(), live.clone());
            Box::new(move |change, entry| {
                heard.lock().push((change, entry["entry_id"].clone()));
                *live.lock()
            })
        };
        let mut first = None;
        entries.subscribe(live.clone(), listener, || {
            first = entries.list().map(|l| l.len());
        });
        assert_eq!(first, Some(1));
        assert!(heard.lock().is_empty(), "what was there isn't news");
        entries.set_state(&id, EntryState::Loaded, None);
        entries.set_state("gone", EntryState::Loaded, None);
        assert_eq!(
            *heard.lock(),
            [(Change::Updated, Value::String(id.clone()))]
        );
        assert_eq!(entries.runtime("gone"), None);
        // Unsubscribed: dropped at once, and the list gives its memory back.
        *live.lock() = false;
        entries.prune();
        assert_eq!(entries.listeners.lock().capacity(), 0);
        // Removed: its state is forgotten too.
        entries.remove(&id).unwrap();
        assert_eq!(entries.runtime.lock().capacity(), 0);
        assert_eq!(heard.lock().len(), 1);
        // A subscriber never hears `removed` for an entry its first list
        // didn't hold: the entry leaves the list as it is told.
        let entry = entries.new_entry("sun", "import", "Sun", None, Map::new());
        let id = entry["entry_id"].as_str().unwrap().to_owned();
        entries.add(entry);
        let live: Live = Arc::new(parking_lot::Mutex::new(true));
        let listed = Arc::new(parking_lot::Mutex::new(None));
        let listener: Listener = {
            let (heard, listed) = (heard.clone(), listed.clone());
            Box::new(move |change, entry| {
                assert_eq!(*listed.lock(), Some(1), "told before the first list");
                heard.lock().push((change, entry["entry_id"].clone()));
                true
            })
        };
        entries.subscribe(live, listener, || {
            *listed.lock() = entries.list().map(|l| l.len());
        });
        entries.remove(&id).unwrap();
        assert_eq!(
            heard.lock().last(),
            Some(&(Change::Removed, Value::String(id)))
        );
    }

    /// A file of another version is never written, so nothing should be
    /// added to it from the UI.
    /// `_async_abort_entries_match` and `_async_current_ids(False)`: a
    /// value is looked for in an entry's options before its data, and an
    /// ignored entry counts for neither. An entry learns its unique id,
    /// unless another entry is already the one for it: of two that learn
    /// the same id, the first to ask; of two that have it, the first
    /// listed. An ignored entry is in nobody's way and hides nobody.
    #[tokio::test]
    async fn entries_are_matched_as_ha_matches_them() {
        use serde_json::json;
        let hass = Hass::new();
        let entry = |id: &str, source: &str, unique: Value, data: Value, options: Value| {
            json!({"entry_id": id, "domain": "hue", "source": source, "unique_id": unique,
                   "data": data, "options": options, "title": id})
            .as_object()
            .unwrap()
            .clone()
        };
        let entries = ConfigEntries::in_memory(
            &hass,
            vec![
                entry(
                    "a",
                    "user",
                    json!("one"),
                    json!({"host": "10.0.0.1"}),
                    json!({}),
                ),
                entry(
                    "b",
                    "user",
                    json!(null),
                    json!({"host": "10.0.0.2"}),
                    json!({"host": "10.0.0.3"}),
                ),
                entry(
                    "c",
                    "ignore",
                    json!("three"),
                    json!({"host": "10.0.0.4"}),
                    json!({}),
                ),
            ],
        );
        assert!(entries.has_data("hue", "host", "10.0.0.1"));
        assert!(entries.has_data("hue", "host", "10.0.0.3"));
        assert!(
            !entries.has_data("hue", "host", "10.0.0.2"),
            "the options come first"
        );
        assert!(
            !entries.has_data("hue", "host", "10.0.0.4"),
            "an ignored entry matched"
        );
        assert!(!entries.has_data("lifx", "host", "10.0.0.1"));
        assert_eq!(entries.unique_ids("hue"), ["one"]);
        assert!(entries.claim_unique_id("hue", "b", "two"));
        assert!(entries.claim_unique_id("hue", "b", "two"), "it is b's");
        assert_eq!(entries.unique_ids("hue"), ["one", "two"]);
        // `d` and `e` find out together that they are for one thing.
        for id in ["d", "e"] {
            entries.add(entry(id, "user", json!(null), json!({}), json!({})));
        }
        assert!(entries.claim_unique_id("hue", "e", "four"));
        assert!(!entries.claim_unique_id("hue", "d", "four"));
        assert_eq!(entries.get("d").unwrap()["unique_id"], json!(null));
        // A stale id does not take what another entry is.
        assert!(!entries.claim_unique_id("hue", "a", "two"));
        assert_eq!(entries.get("a").unwrap()["unique_id"], "one");
        // Two entries that both came with the id: the first listed.
        entries.add(entry("f", "user", json!("one"), json!({}), json!({})));
        assert!(entries.claim_unique_id("hue", "a", "one"));
        assert!(!entries.claim_unique_id("hue", "f", "one"));
        // The ignored entry `c` is first with "three": `g` may have it,
        // and `h` then may not, though the first match is ignored.
        for id in ["g", "h"] {
            entries.add(entry(id, "user", json!(null), json!({}), json!({})));
        }
        assert!(entries.claim_unique_id("hue", "g", "three"));
        assert!(!entries.claim_unique_id("hue", "h", "three"));
        // A disabled entry that is first with an id is in nobody's way
        // either, whether the other came with the id or learns it.
        let mut off = entry("i", "user", json!("five"), json!({}), json!({}));
        off.insert("disabled_by".into(), json!("user"));
        entries.add(off);
        entries.add(entry("j", "user", json!("five"), json!({}), json!({})));
        entries.add(entry("k", "user", json!(null), json!({}), json!({})));
        assert!(entries.claim_unique_id("hue", "j", "five"));
        assert!(!entries.claim_unique_id("hue", "k", "five"), "j has it");
        // Nor is one of a newer version of its flow, which isn't set up.
        let mut newer = entry("l", "user", json!("six"), json!({}), json!({}));
        newer.insert("version".into(), json!(99));
        entries.add(newer);
        entries.add(entry("m", "user", json!(null), json!({}), json!({})));
        assert!(entries.claim_unique_id("hue", "m", "six"));
        assert_eq!(
            entries.find_unique("hue", "three").unwrap()["entry_id"],
            "g"
        );
    }

    #[tokio::test]
    async fn a_file_of_another_version_is_read_only() {
        let dir = std::env::temp_dir().join(format!("hars-entries-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".storage")).unwrap();
        let text = include_str!("config_entries/vectors/before.json");
        std::fs::write(dir.join(".storage").join(KEY), text).unwrap();
        assert!(!ConfigEntries::load(&Hass::new(), &dir).read_only());
        let older = text.replace("\"minor_version\": 5", "\"minor_version\": 4");
        assert_ne!(older, text);
        std::fs::write(dir.join(".storage").join(KEY), older).unwrap();
        assert!(ConfigEntries::load(&Hass::new(), &dir).read_only());
        assert!(!ConfigEntries::in_memory(&Hass::new(), Vec::new()).read_only());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn updates_save_a_second_later_and_no_ops_nothing() {
        let dir = std::env::temp_dir().join(format!("hars-entries-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".storage")).unwrap();
        let file = dir.join(".storage").join(KEY);
        std::fs::write(&file, include_str!("config_entries/vectors/before.json")).unwrap();
        let hass = Hass::new();
        let entries = ConfigEntries::load(&hass, &dir);
        let entry = entries.list().unwrap()[0].clone();
        let id = entry["entry_id"].as_str().unwrap().to_owned();
        let data = entry["data"].as_object().unwrap().clone();
        assert!(!entries.update_data(&id, data.clone()));
        assert!(!entries.update_data("nope", data.clone()));
        let mut changed = data;
        changed.insert("os_version".into(), Value::String("27.0.1".into()));
        assert!(entries.update_data(&id, changed));
        let advance = |ms: u64| async move {
            tokio::time::advance(Duration::from_millis(ms)).await;
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        advance(500).await;
        assert!(!std::fs::read_to_string(&file).unwrap().contains("27.0.1"));
        advance(600).await;
        let saved = std::fs::read_to_string(&file).unwrap();
        assert!(saved.contains("\"os_version\":\"27.0.1\""));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
