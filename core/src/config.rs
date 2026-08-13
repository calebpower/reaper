//! Site configuration: what the sysadmin owns.
//!
//! Three things live here, and keeping them apart is the point. Which guests
//! *exist* is the sysadmin's decision, recorded in the registry below. Which
//! guests a project *wants* is the developer's, recorded in a manifest. What a
//! provider needs in order to talk to a hypervisor is the provider's own
//! business, and the core deliberately does not know what those keys are -- it
//! carries the table through uninterpreted.
//!
//! See `docs/site-config.md`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::duration;
use crate::paths;

#[derive(Debug)]
pub enum ConfigError {
    /// No configuration file was found. The candidate paths are listed,
    /// because "not found" without saying where you looked is not a report.
    NotFound(Vec<PathBuf>),
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        message: String,
    },
    /// Parsed, but says something that cannot be acted on.
    Invalid {
        path: PathBuf,
        message: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::NotFound(tried) => {
                write!(f, "no configuration file; looked in")?;
                for p in tried {
                    write!(f, "\n  {}", p.display())?;
                }
                Ok(())
            }
            ConfigError::Read { path, source } => {
                write!(f, "{}: cannot read: {source}", path.display())
            }
            ConfigError::Parse { path, message } => {
                write!(f, "{}: cannot parse: {message}", path.display())
            }
            ConfigError::Invalid { path, message } => {
                write!(f, "{}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// One registered guest: a name a tenant can ask for, and whatever the provider
/// needs in order to produce it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestEntry {
    /// Opaque to the core.
    pub template: String,
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub default_ttl: Duration,
    pub heartbeat_interval: Duration,
    /// How long a machine's first expiry lasts, covering the gap between
    /// creation and readiness. Full-copy clones are slow, and a TTL that
    /// started at the create request would collect machines that were never
    /// used.
    pub ready_grace: Duration,
    pub max_concurrent: usize,
    /// Size of the blank disk attached to each session, in gibibytes, unless a
    /// tenant asks for something else.
    pub default_disk_gb: u32,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub path: PathBuf,
    pub provider: String,
    pub session: SessionConfig,
    pub guests: BTreeMap<String, GuestEntry>,
    /// The selected provider's own table, uninterpreted.
    provider_table: toml::Value,
}

impl Config {
    /// The provider's configuration, for the provider to deserialize into
    /// whatever shape it needs. The core never looks inside.
    pub fn provider_table(&self) -> &toml::Value {
        &self.provider_table
    }

    /// The template registered for a guest name, or `None` if the sysadmin has
    /// not registered one. Callers must resolve before touching a provider:
    /// an unknown guest is a typo, and a typo should not cost an API round
    /// trip to discover.
    pub fn template_for(&self, guest: &str) -> Option<&str> {
        self.guests.get(guest).map(|g| g.template.as_str())
    }

    pub fn guest_names(&self) -> Vec<&str> {
        self.guests.keys().map(String::as_str).collect()
    }
}

#[derive(Deserialize)]
struct RawSession {
    default_ttl: Option<String>,
    heartbeat_interval: Option<String>,
    ready_grace: Option<String>,
    max_concurrent: Option<usize>,
    default_disk_gb: Option<u32>,
}

impl Default for RawSession {
    fn default() -> Self {
        RawSession {
            default_ttl: None,
            heartbeat_interval: None,
            ready_grace: None,
            max_concurrent: None,
            default_disk_gb: None,
        }
    }
}

#[derive(Deserialize)]
struct RawConfig {
    provider: String,
    #[serde(default)]
    session: RawSession,
    #[serde(default)]
    guests: BTreeMap<String, GuestEntry>,
    /// Everything else: one table per provider, none of which the core reads.
    #[serde(flatten)]
    extra: BTreeMap<String, toml::Value>,
}

/// Load configuration from the first candidate path that exists.
pub fn load() -> Result<Config, ConfigError> {
    let candidates = paths::config_candidates();
    for path in &candidates {
        if path.is_file() {
            return load_from(path);
        }
    }
    Err(ConfigError::NotFound(candidates))
}

pub fn load_from(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Read {
        path: path.to_path_buf(),
        source: e,
    })?;
    parse(&text, path)
}

pub fn parse(text: &str, path: &Path) -> Result<Config, ConfigError> {
    let invalid = |message: String| ConfigError::Invalid {
        path: path.to_path_buf(),
        message,
    };

    let raw: RawConfig = toml::from_str(text).map_err(|e| ConfigError::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    if raw.provider.trim().is_empty() {
        return Err(invalid("provider is empty".into()));
    }

    let provider_table = raw.extra.get(&raw.provider).cloned().ok_or_else(|| {
        invalid(format!(
            "provider is {:?} but there is no [{}] section to configure it",
            raw.provider, raw.provider
        ))
    })?;

    if raw.guests.is_empty() {
        return Err(invalid(
            "no guests are registered, so no tenant can run anything here".into(),
        ));
    }
    for (name, g) in &raw.guests {
        if g.template.trim().is_empty() {
            return Err(invalid(format!("guest {name:?} has an empty template")));
        }
    }

    let dur = |field: &str, value: Option<&String>, default: &str| {
        duration::parse(value.map(String::as_str).unwrap_or(default))
            .map_err(|e| invalid(format!("session.{field}: {e}")))
    };

    let default_ttl = dur("default_ttl", raw.session.default_ttl.as_ref(), "2h")?;
    let heartbeat_interval = dur(
        "heartbeat_interval",
        raw.session.heartbeat_interval.as_ref(),
        "5m",
    )?;
    let ready_grace = dur("ready_grace", raw.session.ready_grace.as_ref(), "30m")?;

    // The heartbeat is a dead-man's switch, so the margin between "we missed a
    // renewal" and "the sweeper takes the machine" has to be wide enough to
    // absorb a slow API call or a laptop lid. A third of the TTL means at least
    // two renewals can fail before anything is lost; at exactly the TTL, one
    // slow call costs a session.
    if heartbeat_interval * 3 > default_ttl {
        return Err(invalid(format!(
            "session.heartbeat_interval is {} but session.default_ttl is only {}: \
             a heartbeat must fit at least three times into the TTL, or a single \
             missed renewal loses the machine",
            duration::format(heartbeat_interval),
            duration::format(default_ttl),
        )));
    }

    let max_concurrent = raw.session.max_concurrent.unwrap_or(2);
    if max_concurrent == 0 {
        return Err(invalid(
            "session.max_concurrent is 0, which forbids every session".into(),
        ));
    }

    let default_disk_gb = raw.session.default_disk_gb.unwrap_or(64);
    // An upper bound as well as a lower one. A typo that asks for sixty-four
    // thousand gibibytes should be refused here rather than by a storage
    // backend, halfway through creating a session.
    if !(1..=4096).contains(&default_disk_gb) {
        return Err(invalid(format!(
            "session.default_disk_gb is {default_disk_gb}; expected between 1 and 4096"
        )));
    }

    Ok(Config {
        path: path.to_path_buf(),
        provider: raw.provider,
        session: SessionConfig {
            default_ttl,
            heartbeat_interval,
            ready_grace,
            max_concurrent,
            default_disk_gb,
        },
        guests: raw.guests,
        provider_table,
    })
}
