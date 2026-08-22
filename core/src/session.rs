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
    /// Another *live* process holds the lock and did not let go in time.
    Locked { path: PathBuf, waited: Duration },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            StoreError::Corrupt { path, message } => {
                write!(f, "{}: unreadable session store: {message}", path.display())
            }
            StoreError::Locked { path, waited } => write!(
                f,
                "{}: another reaper is holding the session lock and did not let go \
                 within {}s. It is a live process, not a leftover file -- the lock \
                 is released by the kernel when its holder exits, so there is \
                 nothing here to clear by hand. Wait for the other command, or \
                 find it with `ps`",
                path.display(),
                waited.as_secs()
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
        from_secs_checked(secs).ok_or_else(|| serde::de::Error::custom("epoch seconds overflow SystemTime"))
    }
}

/// Checked, because this arrives from a file: a value that overflows
/// SystemTime must surface as the store's Corrupt error, not a panic.
fn from_secs_checked(secs: u64) -> Option<SystemTime> {
    UNIX_EPOCH.checked_add(Duration::from_secs(secs))
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
        secs.map(|s| {
            super::from_secs_checked(s)
                .ok_or_else(|| serde::de::Error::custom("epoch seconds overflow SystemTime"))
        })
        .transpose()
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
}

impl Store {
    /// The store at the conventional location.
    pub fn open() -> Store {
        Store::at(crate::paths::state_file())
    }

    /// Test-only: the production timeout makes a contended-lock test take ten
    /// seconds, and a slow test is a test that stops being run.
    #[cfg(test)]
    pub(crate) fn with_timeouts(path: impl Into<PathBuf>, lock_timeout: Duration) -> Store {
        Store {
            path: path.into(),
            lock_timeout,
        }
    }

    pub fn at(path: impl Into<PathBuf>) -> Store {
        Store {
            path: path.into(),
            lock_timeout: Duration::from_secs(10),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Refuse early if this store could not be written to.
    ///
    /// The record is the only thing that lets `down` find a machine later, so
    /// a caller about to spend one wants this answered first. The probe never
    /// creates the store file itself: an empty sessions.json would read as
    /// corrupt, so an absent file is probed through its parent directory.
    pub fn probe_writable(&self) -> Result<()> {
        let io_err = |path: &Path, e: std::io::Error| StoreError::Io {
            path: path.to_path_buf(),
            source: e,
        };
        if self.path.exists() {
            fs::OpenOptions::new()
                .write(true)
                .open(&self.path)
                .map_err(|e| io_err(&self.path, e))?;
            return Ok(());
        }
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
        let probe = dir.join(format!(".reaper-probe.{}", std::process::id()));
        fs::write(&probe, b"").map_err(|e| io_err(&self.path, e))?;
        let _ = fs::remove_file(&probe);
        Ok(())
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

    /// Hold the store against every other process, for as long as the guard
    /// lives.
    ///
    /// The lock is an advisory lock **on** a file, not the existence of one,
    /// and that distinction is the whole of this function.
    ///
    /// A lock that *is* a file somebody created outlives whoever created it.
    /// `down` stops the heartbeat with SIGTERM and then SIGKILL, and a
    /// heartbeat killed inside a mutation left a file every later run read as
    /// "somebody is working": the next locker waited out its entire timeout
    /// and refused, so `down` failed on its own `remove` having already
    /// destroyed the machine -- a session record for a machine that no longer
    /// exists, and a store nothing could write until a staleness fallback
    /// finally aged the file out two minutes later. Found by CI on a loaded
    /// runner; the window is microseconds wide and a workstation never hit it.
    ///
    /// This lock is released by the kernel when the holder's descriptor
    /// closes, which includes the holder dying by any means. There is no
    /// orphaned state to recover from, so there is no staleness heuristic here
    /// to get wrong -- and a lock that is still held is now proof that a live
    /// process holds it, which is what the error says.
    ///
    /// The file is never removed. Unlinking a lock file is the classic way to
    /// end up with two holders: one process unlinks while a second still holds
    /// the old inode, a third creates a fresh file and locks that, and now two
    /// processes each believe they are alone.
    fn lock(&self) -> Result<LockGuard> {
        let path = self.lock_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| StoreError::Io {
                path: dir.to_path_buf(),
                source: e,
            })?;
        }

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| StoreError::Io {
                path: path.clone(),
                source: e,
            })?;

        let deadline = SystemTime::now() + self.lock_timeout;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(LockGuard { _file: file }),
                // Somebody live has it. Wait, within reason.
                Err(fs::TryLockError::WouldBlock) => {}
                // The lock could not be attempted at all -- an unwritable
                // directory, a filesystem that does not carry locks. That is
                // an I/O fault and must not be reported as contention, which
                // would send somebody hunting a process that does not exist.
                Err(fs::TryLockError::Error(e)) => {
                    return Err(StoreError::Io { path, source: e })
                }
            }

            if SystemTime::now() >= deadline {
                return Err(StoreError::Locked {
                    path,
                    waited: self.lock_timeout,
                });
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// The open descriptor is the lock. Dropping it closes the descriptor, and
/// closing it is what releases the lock -- so there is deliberately nothing to
/// do here, and deliberately no file to remove.
struct LockGuard {
    _file: fs::File,
}
