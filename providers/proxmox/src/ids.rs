//! Machine identifiers, and the range guard.
//!
//! Proxmox names virtual machines with small integers, shared across the whole
//! cluster -- so the identifiers reaper is allowed to touch are a *range*, and
//! everything outside it belongs to somebody else. The token's permissions
//! already stop most mistakes, but a permission error arrives after the request
//! and reads like a bug. This refuses first, and says why.
//!
//! The sweeper enforces the same rule independently. That duplication is
//! deliberate: the sweeper is the backstop for this code being wrong.

use std::fmt;
use std::ops::RangeInclusive;

use reaper_core::provider::{MachineRef, ProviderError};

/// The identifiers this provider may touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdRange {
    low: u32,
    high: u32,
}

impl IdRange {
    pub fn new(low: u32, high: u32) -> Result<IdRange, String> {
        // Proxmox itself will not accept an identifier below 100.
        if low < 100 {
            return Err(format!("{low} is below the lowest identifier Proxmox allows (100)"));
        }
        if low > high {
            return Err(format!("range {low}-{high} is inverted"));
        }
        Ok(IdRange { low, high })
    }

    pub fn contains(&self, id: u32) -> bool {
        (self.low..=self.high).contains(&id)
    }

    pub fn iter(&self) -> RangeInclusive<u32> {
        self.low..=self.high
    }

    /// Read a machine reference as an identifier this provider may act on.
    ///
    /// Every operation calls this before touching the network, which is the
    /// point: an out-of-range identifier must never reach the API, whether it
    /// came from a stale session file, a typo, or a bug here.
    pub fn check(&self, machine: &MachineRef) -> Result<u32, ProviderError> {
        let raw = machine.as_str();
        let id: u32 = raw.parse().map_err(|_| {
            ProviderError::Refused(format!(
                "{raw:?} is not a machine identifier this provider issued"
            ))
        })?;

        if !self.contains(id) {
            return Err(ProviderError::Refused(format!(
                "machine {id} is outside the range this provider may touch ({self}); \
                 refusing before contacting the API"
            )));
        }

        Ok(id)
    }
}

impl fmt::Display for IdRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.low, self.high)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range() -> IdRange {
        IdRange::new(9000, 9099).unwrap()
    }

    #[test]
    fn identifiers_inside_the_range_are_allowed() {
        for id in [9000, 9050, 9099] {
            assert_eq!(range().check(&MachineRef::new(id.to_string())).unwrap(), id);
        }
    }

    #[test]
    fn identifiers_outside_the_range_are_refused() {
        // 8100 is the sweeper's own machine. If this ever stops being refused,
        // reaper can destroy the thing that cleans up after it.
        for id in [99, 100, 8100, 8999, 9100, 100000] {
            let e = range()
                .check(&MachineRef::new(id.to_string()))
                .expect_err("{id} should be refused");
            assert!(matches!(e, ProviderError::Refused(_)), "{id}: {e}");
        }
    }

    #[test]
    fn a_reference_that_is_not_an_identifier_is_refused() {
        for raw in ["", "abc", "9000x", "-9000", "9000 ", " 9000", "0x2328", "9000.0"] {
            assert!(
                range().check(&MachineRef::new(raw)).is_err(),
                "{raw:?} should be refused"
            );
        }
    }

    #[test]
    fn the_refusal_says_what_was_wrong() {
        let e = range().check(&MachineRef::new("8100")).unwrap_err();
        let m = e.to_string();
        assert!(m.contains("8100") && m.contains("9000-9099"), "unhelpful: {m}");
        assert!(m.contains("before contacting the API"), "unhelpful: {m}");
    }

    #[test]
    fn nonsense_ranges_are_refused_at_construction() {
        assert!(IdRange::new(9099, 9000).is_err(), "inverted");
        assert!(IdRange::new(0, 9099).is_err(), "below the Proxmox minimum");
        assert!(IdRange::new(99, 100).is_err(), "below the Proxmox minimum");
        assert!(IdRange::new(9000, 9000).is_ok(), "a single identifier is a range");
    }
}
