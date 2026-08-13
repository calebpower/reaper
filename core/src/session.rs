//! Session bookkeeping.
//!
//! A session is the pairing of a project with a machine. This store is what
//! lets `list` show sessions rather than raw machines, and what lets `down`
//! find the heartbeat it has to stop.
//!
//! It is a cache of things the provider also knows, not a source of truth. If
//! it is lost, sessions become hard to find; they do not become immortal. That
//! is the whole reason the expiry lives on the machine itself, where a sweeper
//! can read it without this file existing at all.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::provider::MachineRef;

#[derive(Debug)]
pub enum StoreError {
    Io { path: PathBuf, source: io::Error },
    Corrupt { path: PathBuf, message: String },
    /// Another process holds the lock and did not let go in time.
    Locked { path: PathBuf, held_for: Duration },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            StoreError::Corrupt { path, message } => {
                write!(f, "{}: unreadable session store: {message}", path.display())
            }
            StoreError::Locked { path, held_for } => write!(
                f,
                "{}: another reaper has held the session lock for {}s; \
                 if nothing else is running, remove it",
                path.display(),
                held_for.as_secs()
            ),
        }
    }
}

impl std::error::Error for StoreError {}

type Result<T> = std::result::Result<T, StoreError>;

/// Times are stored as whole seconds since the epoch. Not because precision is
/// unwelcome, but because this file is read by people during incidents, and
/// a number they can paste into `date` is worth more than nanoseconds.
mod epoch {
    use super::*;

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_u64(
            t.duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        )
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<SystemTime, D::Error> {
        let secs = u64::deserialize(d)?;
        Ok(UNIX_EPOCH + Duration::from_secs(secs))
    }
}

mod epoch_opt {
    use super::*;

    pub fn serialize<S: Serializer>(
        t: &Option<SystemTime>,
        s: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match t {
            Some(t) => super::epoch::serialize(t, s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> std::result::Result<Option<SystemTime>, D::Error> {
        let secs = Option::<u64>::deserialize(d)?;
        Ok(secs.map(|s| UNIX_EPOCH + Duration::from_secs(s)))
    }
}

mod secs {
    use super::*;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Duration, D::Error> {
        Ok(Duration::from_secs(u64::deserialize(d)?))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub project: String,
    pub guest: String,
    pub template: String,
    pub machine: MachineRef,
    #[serde(default)]
    pub address: Option<IpAddr>,

    #[serde(with = "epoch")]
    pub created_at: SystemTime,
    /// When the machine first reported an address. `None` means it never has,
    /// and the session is still living on its creation grace.
    #[serde(with = "epoch_opt", default)]
    pub ready_at: Option<SystemTime>,
    #[serde(with = "epoch")]
    pub expires_at: SystemTime,
    #[serde(with = "secs")]
    pub ttl: Duration,

    /// The heartbeat renewing this session's expiry, if one was started.
    #[serde(default)]
    pub heartbeat_pid: Option<u32>,

    /// When a working tree was last pushed into this session. `None` means one
    /// never has been, which is how `down` knows there is nothing to collect on
    /// the way past -- pulling from a directory that was never created would
    /// fail, and a failure there would read as though results had been lost.
    #[serde(with = "epoch_opt", default)]
    pub synced_at: Option<SystemTime>,
}

impl Session {
    /// Time left before a sweeper may collect the machine, or `None` if that
    /// moment has passed.
    pub fn remaining(&self, now: SystemTime) -> Option<Duration> {
        self.expires_at.duration_since(now).ok()
    }

    pub fn age(&self, now: SystemTime) -> Duration {
        now.duration_since(self.created_at).unwrap_or_default()
    }
}

/// The on-disk shape. Versioned from the first commit, because the alternative
/// is guessing later what an unversioned file meant.
#[derive(Debug, Serialize, Deserialize)]
struct Document {
    version: u32,
    sessions: BTreeMap<String, Session>,
}

const VERSION: u32 = 1;

impl Default for Document {
    fn default() -> Self {
        Document {
            version: VERSION,
            sessions: BTreeMap::new(),
        }
    }
}

pub struct Store {
    path: PathBuf,
    lock_timeout: Duration,
    stale_lock_after: Duration,
}

impl Store {
    /// The store at the conventional location.
    pub fn open() -> Store {
        Store::at(crate::paths::state_file())
    }

    pub fn at(path: impl Into<PathBuf>) -> Store {
        Store {
            path: path.into(),
            lock_timeout: Duration::from_secs(10),
            stale_lock_after: Duration::from_secs(120),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn list(&self) -> Result<Vec<Session>> {
        Ok(self.read()?.sessions.into_values().collect())
    }

    pub fn get(&self, name: &str) -> Result<Option<Session>> {
        Ok(self.read()?.sessions.remove(name))
    }

    /// Insert or replace a session.
    pub fn put(&self, session: Session) -> Result<()> {
        self.mutate(|doc| {
            doc.sessions.insert(session.name.clone(), session.clone());
        })
    }

    /// Forget a session, returning it if it was there.
    pub fn remove(&self, name: &str) -> Result<Option<Session>> {
        let mut taken = None;
        self.mutate(|doc| {
            taken = doc.sessions.remove(name);
        })?;
        Ok(taken)
    }

    /// Change one session in place, returning false if it is not there.
    ///
    /// The heartbeat's whole job is this: move an expiry without disturbing
    /// anything else in the store.
    pub fn update<F: FnOnce(&mut Session)>(&self, name: &str, f: F) -> Result<bool> {
        let mut found = false;
        let mut f = Some(f);
        self.mutate(|doc| {
            if let Some(s) = doc.sessions.get_mut(name) {
                if let Some(f) = f.take() {
                    f(s);
                }
                found = true;
            }
        })?;
        Ok(found)
    }

    /// Read-modify-write under the lock.
    ///
    /// Every mutation goes through here. Two `up`s racing would otherwise each
    /// read, each add their own session, and each write -- and the second would
    /// erase the first, leaving a machine running that nothing knows about.
    fn mutate<F: FnMut(&mut Document)>(&self, mut f: F) -> Result<()> {
        let _guard = self.lock()?;
        let mut doc = self.read()?;
        f(&mut doc);
        self.write(&doc)
    }

    fn read(&self) -> Result<Document> {
        let text = match fs::read_to_string(&self.path) {
            Ok(t) => t,
            // A store that has never been written is an empty store, not an
            // error. Every other I/O failure is real and is reported.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Document::default()),
            Err(e) => {
                return Err(StoreError::Io {
                    path: self.path.clone(),
                    source: e,
                })
            }
        };

        let doc: Document = serde_json::from_str(&text).map_err(|e| StoreError::Corrupt {
            path: self.path.clone(),
            message: e.to_string(),
        })?;

        if doc.version != VERSION {
            return Err(StoreError::Corrupt {
                path: self.path.clone(),
                message: format!(
                    "written by a different version of reaper (file says {}, this is {VERSION})",
                    doc.version
                ),
            });
        }

        Ok(doc)
    }

    /// Write by replacement, never in place.
    ///
    /// A crash midway through rewriting this file in place would leave
    /// truncated JSON, and the next run would refuse to read it -- losing track
    /// of live machines at exactly the moment something is already going wrong.
    fn write(&self, doc: &Document) -> Result<()> {
        let io_err = |path: &Path, source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        };

        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
        }

        let text = serde_json::to_string_pretty(doc).map_err(|e| StoreError::Corrupt {
            path: self.path.clone(),
            message: e.to_string(),
        })?;

        let tmp = self.path.with_extension(format!("tmp.{}", std::process::id()));
        fs::write(&tmp, text.as_bytes()).map_err(|e| io_err(&tmp, e))?;
        fs::rename(&tmp, &self.path).map_err(|e| io_err(&self.path, e))
    }

    fn lock_path(&self) -> PathBuf {
        self.path.with_extension("lock")
    }

    fn lock(&self) -> Result<LockGuard> {
        let path = self.lock_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| StoreError::Io {
                path: dir.to_path_buf(),
                source: e,
            })?;
        }

        let deadline = SystemTime::now() + self.lock_timeout;
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(LockGuard { path }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(StoreError::Io {
                        path: path.clone(),
                        source: e,
                    })
                }
            }

            // A lock nobody released is worse than no lock: it wedges every
            // future run. Age it out rather than requiring a person to know
            // this file exists.
            let held_for = fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .unwrap_or_default();
            if held_for > self.stale_lock_after {
                let _ = fs::remove_file(&path);
                continue;
            }

            if SystemTime::now() >= deadline {
                return Err(StoreError::Locked { path, held_for });
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
