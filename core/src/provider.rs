//! The provider seam.
//!
//! A provider creates, tags, inspects and destroys machines. Everything below
//! is deliberately free of hypervisor vocabulary -- no numeric identifiers, no
//! identifier ranges, no resource pools, no task handles, no API tokens. Those
//! are a provider's business, and a lint guard fails the build if they appear
//! outside one.
//!
//! The contract is written out in `docs/providers.md`; this is its executable
//! half.

use std::fmt;
use std::net::IpAddr;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// A machine, as the provider that made it refers to it.
///
/// Opaque on purpose. The core stores it, hands it back, and never parses it:
/// the moment anything outside a provider tries to read structure out of this
/// string, the seam has stopped meaning anything.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MachineRef(String);

impl MachineRef {
    pub fn new(s: impl Into<String>) -> Self {
        MachineRef(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MachineRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What to make.
#[derive(Debug, Clone)]
pub struct CreateRequest {
    /// A human-facing name for the machine. Providers may have to mangle it.
    pub name: String,
    /// Which template to make it from. Opaque to the core: this came out of
    /// the site registry and means whatever the provider says it means.
    pub template: String,
    pub cores: Option<u16>,
    pub ram_gb: Option<u16>,
    /// A blank disk to attach, in gibibytes, for the session's storage pool.
    ///
    /// Attached rather than carried by the template on purpose. Where cloning
    /// is a byte-for-byte copy, a data disk in the template is copied in full
    /// on every session whether or not anything is on it -- so this is created,
    /// not copied, and its size is a per-session decision rather than one
    /// frozen when the template was built.
    ///
    /// `None` means the template already provides one.
    pub data_disk_gb: Option<u32>,
    /// When this machine stops being anybody's responsibility.
    ///
    /// Part of creation rather than a follow-up call, so that the window
    /// between "machine exists" and "machine has an expiry" is as small as the
    /// provider's API allows. That window is the design's one unrecoverable
    /// state: a machine nothing will ever collect.
    pub expires_at: SystemTime,
}

/// A machine the provider is responsible for.
#[derive(Debug, Clone)]
pub struct MachineSummary {
    pub machine: MachineRef,
    pub name: String,
    /// `None` means the machine carries no expiry at all. That is not a
    /// machine with plenty of time left -- it is a machine no sweeper will
    /// ever collect, and it wants a human.
    pub expires_at: Option<SystemTime>,
    pub running: bool,
}

#[derive(Debug)]
pub enum ProviderError {
    /// An invariant refused the operation before any call was made. This is
    /// the provider protecting the cluster from the caller, and it is never a
    /// transient condition to retry.
    Refused(String),
    /// The credential was rejected, or does not carry the needed right.
    Unauthorized(String),
    NotFound(String),
    /// An operation was still running when we stopped waiting.
    ///
    /// Distinct from every other failure on purpose. A timeout means the state
    /// of the machine is *unknown*, so the caller must not respond by
    /// destroying things; the expiry tag and the sweeper exist for exactly
    /// this case.
    Timeout(String),
    /// The request never got an answer.
    Transport(String),
    /// The provider answered, and the answer was a refusal or a surprise.
    Api { status: u16, message: String },
    /// The provider's own configuration is wrong or missing.
    Config(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::Refused(m) => write!(f, "refused: {m}"),
            ProviderError::Unauthorized(m) => write!(f, "not authorized: {m}"),
            ProviderError::NotFound(m) => write!(f, "not found: {m}"),
            ProviderError::Timeout(m) => write!(f, "timed out: {m}"),
            ProviderError::Transport(m) => write!(f, "transport failure: {m}"),
            ProviderError::Api { status, message } => write!(f, "api error {status}: {message}"),
            ProviderError::Config(m) => write!(f, "provider configuration: {m}"),
        }
    }
}

impl std::error::Error for ProviderError {}

pub type Result<T> = std::result::Result<T, ProviderError>;

pub trait Provider {
    /// The name this provider is selected by in site configuration.
    fn name(&self) -> &'static str;

    /// Make a machine from a template, carrying an expiry from the moment it
    /// exists. Implementations that cannot set both atomically should set the
    /// expiry as their very next call, and destroy what they made if that
    /// fails -- they know the identifier, so leaking it would be carelessness.
    fn create(&self, req: &CreateRequest) -> Result<MachineRef>;

    /// Move the expiry. Implementations must preserve any other metadata the
    /// machine carries: this is a shared cluster and reaper is not the only
    /// thing that writes tags.
    fn set_expiry(&self, machine: &MachineRef, at: SystemTime) -> Result<()>;

    fn start(&self, machine: &MachineRef) -> Result<()>;
    fn stop(&self, machine: &MachineRef) -> Result<()>;
    fn destroy(&self, machine: &MachineRef) -> Result<()>;

    /// The machine's address, or `None` if it does not have one yet. Must not
    /// depend on DNS or mDNS.
    fn address(&self, machine: &MachineRef) -> Result<Option<IpAddr>>;

    /// Every machine this provider is responsible for -- which is narrower
    /// than every machine that exists. A provider scoped to part of a shared
    /// cluster reports only its own part.
    fn list(&self) -> Result<Vec<MachineSummary>>;
}
