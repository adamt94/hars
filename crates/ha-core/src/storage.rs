//! Home Assistant's `<config>/.storage/<key>` files (`helpers/storage.py`).
//!
//! Every file is the same envelope, so a config directory moved from HA to
//! `hars` (or back) keeps working:
//!
//! ```json
//! {"version": 1, "minor_version": 1, "key": "auth", "data": { ... }}
//! ```
//!
//! The API is blocking: call it at start-up or from `spawn_blocking`.

use serde::{Serialize, de::DeserializeOwned};
use std::borrow::Cow;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// One `.storage` file.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
    key: Cow<'static, str>,
    version: u32,
    minor_version: u32,
    private: bool,
}

/// Why a store could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("reading {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{path} is not a valid store file: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// HA refuses to load a file written by a newer major version, and so do we.
    #[error("{path} has version {found}, newer than the supported {supported}")]
    TooNew {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    /// Older major versions need a migration that `hars` doesn't have yet.
    #[error("{path} has version {found}; only version {supported} can be read")]
    TooOld {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
}

/// `minor_version` isn't checked (newer minor versions are backwards
/// compatible) but is returned for migrations; `key` isn't checked either,
/// as in HA.
#[derive(serde::Deserialize)]
struct Envelope<T> {
    version: u32,
    #[serde(default = "first_minor")]
    minor_version: u32,
    data: T,
}

fn first_minor() -> u32 {
    1
}

#[derive(Serialize)]
struct EnvelopeRef<'a, T> {
    version: u32,
    minor_version: u32,
    key: &'a str,
    data: &'a T,
}

impl Store {
    /// `private` stores (credentials, tokens) are written with mode `0600`,
    /// the rest `0644`, as HA does.
    pub fn new(
        config_dir: &Path,
        key: impl Into<Cow<'static, str>>,
        version: u32,
        minor_version: u32,
        private: bool,
    ) -> Self {
        let key = key.into();
        Self {
            path: config_dir.join(".storage").join(key.as_ref()),
            key,
            version,
            minor_version,
            private,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The stored data, or `None` if the file doesn't exist. A newer minor
    /// version is read as-is: HA keeps minor versions backwards compatible.
    pub fn load<T: DeserializeOwned>(&self) -> Result<Option<T>, StoreError> {
        Ok(self.load_with_minor()?.map(|(data, _)| data))
    }

    /// [`Store::load`], with the file's `minor_version` (1 if missing), for
    /// stores whose older minor versions HA migrates on load.
    pub fn load_with_minor<T: DeserializeOwned>(&self) -> Result<Option<(T, u32)>, StoreError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(StoreError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let env: Envelope<T> =
            serde_json::from_slice(&bytes).map_err(|source| StoreError::Json {
                path: self.path.clone(),
                source,
            })?;
        let path = || self.path.clone();
        match env.version.cmp(&self.version) {
            std::cmp::Ordering::Greater => Err(StoreError::TooNew {
                path: path(),
                found: env.version,
                supported: self.version,
            }),
            std::cmp::Ordering::Less => Err(StoreError::TooOld {
                path: path(),
                found: env.version,
                supported: self.version,
            }),
            std::cmp::Ordering::Equal => Ok(Some((env.data, env.minor_version))),
        }
    }

    /// Write atomically: a temporary file in the same directory, flushed to
    /// disk, then renamed over the old one. A crash leaves the old or the new
    /// file, never a torn one.
    pub fn save<T: Serialize>(&self, data: &T) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(&EnvelopeRef {
            version: self.version,
            minor_version: self.minor_version,
            key: &self.key,
            data,
        })?;
        self.save_raw(&json)
    }

    /// Write an already-serialized store file as is (the importer copies
    /// HA's files byte for byte), with the same atomic, durable write.
    pub fn save_raw(&self, json: &[u8]) -> io::Result<()> {
        let dir = self.path.parent().expect("store path has a parent");
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".{}.tmp{}", self.key, std::process::id()));
        let result = (|| {
            let mut file = open_new(&tmp, self.private)?;
            file.write_all(json)?;
            file.sync_all()?;
            fs::rename(&tmp, &self.path)?;
            // Make the rename itself durable: without this, power loss can
            // bring back the old file (a revoked token, say).
            #[cfg(unix)]
            fs::File::open(dir)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }
}

fn open_new(path: &Path, private: bool) -> io::Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(if private { 0o600 } else { 0o644 });
    }
    #[cfg(not(unix))]
    let _ = private;
    let file = opts.open(path)?;
    // `mode` only applies to new files and is masked by the umask.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(if private {
            0o600
        } else {
            0o644
        }))?;
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hars-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn round_trips_in_has_envelope() {
        let dir = temp_dir("roundtrip");
        let store = Store::new(&dir, "onboarding", 4, 1, true);
        assert!(store.load::<Value>().unwrap().is_none());
        store.save(&json!({"done": ["user"]})).unwrap();

        let raw: Value =
            serde_json::from_slice(&fs::read(dir.join(".storage/onboarding")).unwrap()).unwrap();
        assert_eq!(
            raw,
            json!({"version": 4, "minor_version": 1, "key": "onboarding", "data": {"done": ["user"]}})
        );
        assert_eq!(
            store.load::<Value>().unwrap(),
            Some(json!({"done": ["user"]}))
        );
        // No temporary file left behind.
        assert_eq!(fs::read_dir(dir.join(".storage")).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn private_stores_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("private");
        Store::new(&dir, "auth", 1, 1, true)
            .save(&json!({}))
            .unwrap();
        Store::new(&dir, "core.config", 1, 4, false)
            .save(&json!({}))
            .unwrap();
        let mode = |key: &str| {
            fs::metadata(dir.join(".storage").join(key))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("auth"), 0o600);
        assert_eq!(mode("core.config"), 0o644);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn version_mismatches_are_errors_minor_is_not() {
        let dir = temp_dir("versions");
        fs::create_dir_all(dir.join(".storage")).unwrap();
        let write = |v: Value| fs::write(dir.join(".storage/auth"), v.to_string()).unwrap();
        let store = Store::new(&dir, "auth", 1, 1, true);

        write(json!({"version": 1, "minor_version": 3, "key": "auth", "data": 7}));
        assert_eq!(store.load::<u32>().unwrap(), Some(7));
        write(json!({"version": 1, "key": "auth", "data": 8})); // minor defaults to 1
        assert_eq!(store.load::<u32>().unwrap(), Some(8));
        write(json!({"version": 2, "minor_version": 1, "key": "auth", "data": 7}));
        assert!(matches!(
            store.load::<u32>(),
            Err(StoreError::TooNew { .. })
        ));
        write(json!({"version": 0, "minor_version": 1, "key": "auth", "data": 7}));
        assert!(matches!(
            store.load::<u32>(),
            Err(StoreError::TooOld { .. })
        ));
        fs::write(dir.join(".storage/auth"), "{").unwrap();
        assert!(matches!(store.load::<u32>(), Err(StoreError::Json { .. })));
        fs::remove_dir_all(dir).unwrap();
    }
}

/// HA's `Store.async_delay_save`: a write `delay` after the **last**
/// change (each change pushes it back), of the data as it is at write
/// time. One task per store sleeps until the deadline, and waits on a
/// `Notify` (no timer) while nothing is pending.
pub struct DelayedSave {
    due: parking_lot::Mutex<Option<tokio::time::Instant>>,
    wake: tokio::sync::Notify,
    /// One write at a time (the task's and a flush's).
    writing: tokio::sync::Mutex<()>,
    write: Box<dyn Fn() -> io::Result<()> + Send + Sync>,
    name: String,
}

impl DelayedSave {
    /// `write` serialises the current data and saves it (blocking; it runs
    /// on the blocking pool). Spawns the task.
    pub fn spawn(
        name: impl Into<String>,
        write: impl Fn() -> io::Result<()> + Send + Sync + 'static,
    ) -> std::sync::Arc<Self> {
        let this = std::sync::Arc::new(Self {
            due: parking_lot::Mutex::new(None),
            wake: tokio::sync::Notify::new(),
            writing: tokio::sync::Mutex::new(()),
            write: Box::new(write),
            name: name.into(),
        });
        let task = std::sync::Arc::downgrade(&this);
        tokio::spawn(async move {
            loop {
                let Some(this) = task.upgrade() else { return };
                let due = *this.due.lock();
                match due {
                    None => {
                        // Holds the `Arc` while idle; the task ends with the
                        // runtime.
                        this.wake.notified().await;
                    }
                    Some(at) => {
                        drop(this);
                        tokio::time::sleep_until(at).await;
                        let Some(this) = task.upgrade() else { return };
                        // Pushed back meanwhile: sleep again.
                        let ready = {
                            let mut due = this.due.lock();
                            if *due == Some(at) {
                                *due = None;
                                true
                            } else {
                                false
                            }
                        };
                        if ready {
                            this.write_now().await;
                        }
                    }
                }
            }
        });
        this
    }

    /// Write `delay` from now (replacing any earlier deadline).
    pub fn schedule(&self, delay: std::time::Duration) {
        *self.due.lock() = Some(tokio::time::Instant::now() + delay);
        self.wake.notify_one();
    }

    /// Whether a write is pending.
    pub fn pending(&self) -> bool {
        self.due.lock().is_some()
    }

    /// Write now if anything is pending (`homeassistant_final_write`).
    pub async fn flush(self: &std::sync::Arc<Self>) {
        if self.due.lock().take().is_some() {
            self.write_now().await;
        }
    }

    async fn write_now(self: &std::sync::Arc<Self>) {
        let _turn = self.writing.lock().await;
        let this = self.clone();
        let res = tokio::task::spawn_blocking(move || (this.write)()).await;
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!("saving {}: {e}", self.name),
            Err(e) => tracing::error!("saving {}: {e}", self.name),
        }
    }
}

#[cfg(test)]
mod delayed_save_tests {
    use super::DelayedSave;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn counter() -> (Arc<AtomicUsize>, Arc<DelayedSave>) {
        let n = Arc::new(AtomicUsize::new(0));
        let m = n.clone();
        let save = DelayedSave::spawn("test", move || {
            m.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        (n, save)
    }

    async fn advance(d: Duration) {
        tokio::time::advance(d).await;
        // Let the task and the blocking write run.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    /// The write lands `delay` after the last change, once.
    #[tokio::test(start_paused = true)]
    async fn trailing_debounce() {
        let (n, save) = counter();
        save.schedule(Duration::from_secs(10));
        advance(Duration::from_secs(5)).await;
        save.schedule(Duration::from_secs(10));
        advance(Duration::from_secs(9)).await;
        assert_eq!(n.load(Ordering::SeqCst), 0);
        assert!(save.pending());
        advance(Duration::from_secs(2)).await;
        assert_eq!(n.load(Ordering::SeqCst), 1);
        assert!(!save.pending());
        advance(Duration::from_secs(60)).await;
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn flush_writes_only_what_is_pending() {
        let (n, save) = counter();
        save.flush().await;
        assert_eq!(n.load(Ordering::SeqCst), 0);
        save.schedule(Duration::from_secs(180));
        save.flush().await;
        assert_eq!(n.load(Ordering::SeqCst), 1);
        advance(Duration::from_secs(200)).await;
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }
}
