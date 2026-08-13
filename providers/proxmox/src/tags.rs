//! The expiry tag.
//!
//! An ephemeral machine carries `expires-<unix-epoch>`. That tag is the whole
//! dead-man's switch: the CLI moves it forward while a session is alive, and an
//! independent sweeper destroys machines whose tag has passed. It lives on the
//! machine rather than in reaper's own state precisely so that the sweeper can
//! read it without reaper existing at all.
//!
//! Proxmox stores tags as one string, lowercases them, and accepts a limited
//! character set. Semicolons separate them; older versions and some tooling use
//! commas, so both are read and semicolons are written.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const PREFIX: &str = "expires-";

/// Split a tag string as Proxmox would, tolerating either separator and any
/// amount of incidental whitespace.
pub fn split(tags: &str) -> Vec<&str> {
    tags.split([';', ','])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect()
}

pub fn encode(at: SystemTime) -> String {
    let secs = at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{PREFIX}{secs}")
}

/// The expiry a tag string carries, if it carries one.
///
/// `None` is not "plenty of time left": it is a machine no sweeper will ever
/// collect, which means a create that half-failed and wants a human.
pub fn expiry_of(tags: &str) -> Option<SystemTime> {
    split(tags).into_iter().find_map(|t| {
        t.strip_prefix(PREFIX)
            .and_then(|s| s.parse::<u64>().ok())
            .map(|secs| UNIX_EPOCH + Duration::from_secs(secs))
    })
}

/// The tag string to write in order to set this expiry, preserving everything
/// else the machine carries.
///
/// Read-modify-write rather than blind assignment. This is a shared cluster and
/// reaper is not the only thing that writes tags; clobbering somebody else's
/// would be both rude and invisible.
pub fn with_expiry(existing: &str, at: SystemTime) -> String {
    let mut out: Vec<String> = split(existing)
        .into_iter()
        .filter(|t| !t.starts_with(PREFIX))
        .map(str::to_string)
        .collect();
    out.push(encode(at));
    out.join(";")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn an_expiry_round_trips() {
        let t = at(1_700_000_000);
        assert_eq!(encode(t), "expires-1700000000");
        assert_eq!(expiry_of(&encode(t)), Some(t));
    }

    #[test]
    fn both_separators_are_understood() {
        // Semicolons are what modern Proxmox writes; commas turn up from older
        // versions and from people typing into the UI.
        for tags in [
            "ephemeral;expires-1700000000;owner-cal",
            "ephemeral,expires-1700000000,owner-cal",
            " ephemeral ; expires-1700000000 ; owner-cal ",
        ] {
            assert_eq!(expiry_of(tags), Some(at(1_700_000_000)), "{tags:?}");
        }
    }

    #[test]
    fn a_machine_with_no_expiry_reports_none() {
        for tags in ["", "ephemeral", "ephemeral;owner-cal", "expires-", "expires-soon"] {
            assert_eq!(expiry_of(tags), None, "{tags:?}");
        }
    }

    #[test]
    fn setting_an_expiry_preserves_every_other_tag() {
        // The assertion that matters most in this file. This is a shared
        // cluster: silently dropping somebody else's tag would be invisible
        // until it mattered.
        let got = with_expiry("ephemeral;owner-cal;expires-1", at(1_700_000_000));
        let tags = split(&got);
        assert!(tags.contains(&"ephemeral"));
        assert!(tags.contains(&"owner-cal"));
        assert!(tags.contains(&"expires-1700000000"));
    }

    #[test]
    fn setting_an_expiry_replaces_any_previous_one() {
        // Two expiry tags would leave which one governs up to whoever reads
        // them first, and the sweeper and reaper could disagree.
        let got = with_expiry("expires-1;expires-2;keep", at(1_700_000_000));
        let expiries: Vec<_> = split(&got)
            .into_iter()
            .filter(|t| t.starts_with(PREFIX))
            .collect();
        assert_eq!(expiries, vec!["expires-1700000000"]);
        assert!(split(&got).contains(&"keep"));
    }

    #[test]
    fn setting_an_expiry_on_an_untagged_machine_works() {
        assert_eq!(with_expiry("", at(1_700_000_000)), "expires-1700000000");
    }

    #[test]
    fn the_written_form_uses_semicolons() {
        let got = with_expiry("a,b", at(1));
        assert_eq!(got, "a;b;expires-1");
    }

    #[test]
    fn a_time_before_the_epoch_does_not_wrap_into_the_far_future() {
        // Nothing should ever produce one, but an expiry that wrapped to a
        // huge number would make a machine effectively immortal.
        let ancient = UNIX_EPOCH - Duration::from_secs(1000);
        assert_eq!(encode(ancient), "expires-0");
    }
}
