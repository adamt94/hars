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

/// `minor_version` and `key` are not checked: newer minor versions are
/// backwards compatible, and HA doesn't check the key either.
#[derive(serde::Deserialize)]
struct Envelope<T> {
    version: u32,
    data: T,
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
            std::cmp::Ordering::Equal => Ok(Some(env.data)),
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
