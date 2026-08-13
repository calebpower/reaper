//! Turning a configured provider name into an implementation.
//!
//! This crate exists for one reason, and it is a boundary reason rather than an
//! architectural flourish. Something has to map the name in site configuration
//! to a concrete implementation, and wherever that mapping lives will contain
//! the name of every supported hypervisor. Put it in the CLI and the provider
//! guard fires -- correctly, because the CLI would then know what a Proxmox is.
//!
//! Adding a provider means adding a dependency and one arm below. There is no
//! dynamic loading, no ABI and no registration protocol, and there should not
//! be: the door is meant to be ajar, not open.

use reaper_core::provider::{Provider, ProviderError};

/// A stand-in hypervisor, for tests that need to drive the whole stack.
///
/// Which provider supplies it is this crate's business, exactly as the real
/// implementations are.
#[cfg(feature = "mock")]
pub mod mock {
    // Re-exported under neutral names. A caller driving the whole stack needs
    // *a* hypervisor, not a particular one, and saying so here means the CLI's
    // tests need no exemption from the provider lint -- which is better than
    // having one, since an exemption is a place coupling could hide later.
    pub use reaper_provider_proxmox::mock::MockPve as StandIn;
    pub use reaper_provider_proxmox::mock::{State, Task, Vm};
}

/// Provider names this build supports, in the order they are offered to a
/// reader who got the name wrong.
pub const AVAILABLE: &[&str] = &["proxmox"];

/// Build the named provider from its own slice of site configuration.
pub fn build(name: &str, table: &toml::Value) -> Result<Box<dyn Provider>, ProviderError> {
    match name {
        "proxmox" => Ok(Box::new(reaper_provider_proxmox::Proxmox::from_table(table)?)),
        other => Err(ProviderError::Config(format!(
            "no provider named {other:?} in this build; available: {}",
            AVAILABLE.join(", ")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_provider_names_the_ones_that_exist() {
        // The reader has a typo or an expectation this build does not meet,
        // and either way the next thing they need is the list.
        let m = match build("cbsd", &toml::Value::Table(Default::default())) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a provider this build does not have should not be buildable"),
        };
        assert!(m.contains("cbsd"), "{m}");
        assert!(m.contains("proxmox"), "should list what is available: {m}");
    }

    #[test]
    fn every_advertised_provider_can_actually_be_selected() {
        // A name in AVAILABLE that no arm matches would be a promise the
        // build does not keep, and it would only surface as a confusing
        // "no provider named" for a name the error itself suggested.
        for name in AVAILABLE {
            // Reaching configuration validation means the arm matched; an
            // empty table cannot get further than that, and does not need to.
            if let Err(ProviderError::Config(m)) = build(name, &toml::Value::Table(Default::default())) {
                assert!(!m.contains("no provider named"), "{name}: {m}");
            }
        }
    }
}
