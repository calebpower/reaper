//! reaper's core: session lifecycle, site configuration, and the provider seam.
//!
//! This crate knows nothing about any hypervisor, any operating system, or any
//! tenant. That is not an aspiration -- lint guards in `tools/guards.sh` fail
//! the build when hypervisor vocabulary, OS-specific identifiers or tenant
//! names appear here.

pub mod config;
pub mod duration;
pub mod job;
pub mod paths;
pub mod provider;
pub mod session;
pub mod sync;
pub mod transport;

pub use config::{Config, ConfigError};
pub use provider::{CreateRequest, MachineRef, MachineSummary, Provider, ProviderError};
pub use session::{Session, Store};
pub use transport::{Ssh, Transport};

#[cfg(test)]
mod tests;
