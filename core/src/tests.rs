//! Tests for the core.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config;
use crate::paths;
use crate::provider::MachineRef;
use crate::session::{Session, Store};

/// Environment variables are process-global, so the handful of tests that must
/// set them take turns. Without this they pass alone and fail together, which
/// is the worst failure mode a suite has.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn scratch_dir(label: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "reaper-core-test-{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst),
        label
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

// --- configuration ---------------------------------------------------------

const GOOD: &str = r#"
provider = "someprovider"

[session]
default_ttl        = "2h"
heartbeat_interval = "5m"
ready_grace        = "30m"
max_concurrent     = 3

[guests."some-os-1.0"]
template = "opaque-handle-1"

[guests."other-os-2.0"]
template = "opaque-handle-2"

[someprovider]
endpoint = "https://example.invalid"
anything = ["the core", "never reads this"]
"#;

fn parse(text: &str) -> Result<config::Config, config::ConfigError> {
    config::parse(text, Path::new("<test>"))
}

#[test]
fn a_good_configuration_parses() {
    let c = parse(GOOD).expect("should parse");
    assert_eq!(c.provider, "someprovider");
    assert_eq!(c.session.default_ttl, Duration::from_secs(7200));
    assert_eq!(c.session.heartbeat_interval, Duration::from_secs(300));
    assert_eq!(c.session.max_concurrent, 3);
    assert_eq!(c.guest_names(), vec!["other-os-2.0", "some-os-1.0"]);
    assert_eq!(c.template_for("some-os-1.0"), Some("opaque-handle-1"));
}

#[test]
fn an_unregistered_guest_has_no_template() {
    // Callers resolve before touching a provider, so a typo costs nothing.
    let c = parse(GOOD).unwrap();
    assert_eq!(c.template_for("never-registered"), None);
}

#[test]
fn the_provider_table_is_carried_through_uninterpreted() {
    // The core must not need to know what keys a provider wants. If this ever
    // becomes a typed struct, the hypervisor seam has moved into the core.
    let c = parse(GOOD).unwrap();
    let t = c.provider_table();
    assert!(t.get("endpoint").is_some());
    assert!(t.get("anything").is_some());
}

#[test]
fn session_defaults_apply_when_the_section_is_absent() {
    let text = r#"
provider = "p"
[guests.g]
template = "t"
[p]
"#;
    let c = parse(text).expect("should parse");
    assert_eq!(c.session.default_ttl, Duration::from_secs(2 * 3600));
    assert_eq!(c.session.heartbeat_interval, Duration::from_secs(300));
    assert_eq!(c.session.ready_grace, Duration::from_secs(1800));
    assert_eq!(c.session.max_concurrent, 2);
}

#[test]
fn a_provider_with_no_section_is_refused() {
    let text = r#"
provider = "absent"
[guests.g]
template = "t"
[present]
x = 1
"#;
    let e = parse(text).expect_err("should be refused");
    let msg = e.to_string();
    assert!(msg.contains("no [absent] section"), "unhelpful message: {msg}");
}

#[test]
fn a_registry_with_no_guests_is_refused() {
    // Not an empty-but-workable configuration: no tenant could run anything.
    let text = r#"
provider = "p"
[p]
"#;
    let e = parse(text).expect_err("should be refused");
    assert!(e.to_string().contains("no guests"), "{e}");
}

#[test]
fn a_guest_with_an_empty_template_is_refused() {
    let text = r#"
provider = "p"
[guests.g]
template = "   "
[p]
"#;
    assert!(parse(text).is_err());
}

#[test]
fn a_heartbeat_that_does_not_fit_inside_the_ttl_is_refused() {
    // The margin is the point: at least two renewals must be allowed to fail
    // before a machine is lost.
    for (hb, ttl) in [("1h", "2h"), ("30m", "1h"), ("5m", "10m")] {
        let text = format!(
            r#"
provider = "p"
[session]
default_ttl = "{ttl}"
heartbeat_interval = "{hb}"
[guests.g]
template = "t"
[p]
"#
        );
        let e = parse(&text).expect_err("{hb} into {ttl} should be refused");
        assert!(e.to_string().contains("at least three times"), "{e}");
    }
}

#[test]
fn a_heartbeat_with_comfortable_margin_is_accepted() {
    let text = r#"
provider = "p"
[session]
default_ttl = "2h"
heartbeat_interval = "10m"
[guests.g]
template = "t"
[p]
"#;
    assert!(parse(text).is_ok());
}

#[test]
fn zero_concurrency_is_refused_rather_than_silently_forbidding_everything() {
    let text = r#"
provider = "p"
[session]
max_concurrent = 0
[guests.g]
template = "t"
[p]
"#;
    assert!(parse(text).is_err());
}

#[test]
fn an_unknown_key_in_a_guest_entry_is_refused() {
    let text = r#"
provider = "p"
[guests.g]
template = "t"
tempalte = "typo"
[p]
"#;
    assert!(parse(text).is_err());
}

#[test]
fn a_missing_configuration_names_where_it_looked() {
    let _guard = env_lock().lock().unwrap();
    std::env::set_var("REAPER_CONFIG", "/nonexistent/reaper/config.toml");
    let e = config::load().expect_err("should not find one");
    std::env::remove_var("REAPER_CONFIG");
    assert!(e.to_string().contains("/nonexistent/reaper/config.toml"), "{e}");
}

// --- paths -----------------------------------------------------------------

#[test]
fn tilde_expands_only_where_it_means_a_home_directory() {
    let _guard = env_lock().lock().unwrap();
    let previous = std::env::var("HOME").ok();
    std::env::set_var("HOME", "/home/someone");

    assert_eq!(paths::expand_tilde("~"), PathBuf::from("/home/someone"));
    assert_eq!(paths::expand_tilde("~/x/y"), PathBuf::from("/home/someone/x/y"));
    // Another user's home is not something this project resolves, and a
    // directory that happens to be named ~ is just a directory.
    assert_eq!(paths::expand_tilde("~other/x"), PathBuf::from("~other/x"));
    assert_eq!(paths::expand_tilde("a/~/b"), PathBuf::from("a/~/b"));
    assert_eq!(paths::expand_tilde("/abs"), PathBuf::from("/abs"));

    match previous {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
}

#[test]
fn an_explicit_config_path_wins_outright() {
    let _guard = env_lock().lock().unwrap();
    std::env::set_var("REAPER_CONFIG", "/tmp/explicit.toml");
    let c = paths::config_candidates();
    std::env::remove_var("REAPER_CONFIG");
    assert_eq!(c, vec![PathBuf::from("/tmp/explicit.toml")]);
}

// --- session store ---------------------------------------------------------

fn session(name: &str, machine: &str) -> Session {
    let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    Session {
        name: name.to_string(),
        project: "a-project".to_string(),
        guest: "a-guest".to_string(),
        template: "opaque".to_string(),
        machine: MachineRef::new(machine),
        address: Some("192.0.2.10".parse().unwrap()),
        created_at: now,
        ready_at: None,
        expires_at: now + Duration::from_secs(7200),
        ttl: Duration::from_secs(7200),
        heartbeat_pid: Some(4242),
    }
}

#[test]
fn a_session_round_trips_through_the_store() {
    let store = Store::at(scratch_dir("roundtrip").join("sessions.json"));
    store.put(session("alpha", "m-1")).unwrap();

    let got = store.get("alpha").unwrap().expect("stored session");
    assert_eq!(got.machine, MachineRef::new("m-1"));
    assert_eq!(got.address, Some("192.0.2.10".parse().unwrap()));
    assert_eq!(got.ttl, Duration::from_secs(7200));
    assert_eq!(got.heartbeat_pid, Some(4242));
    assert_eq!(got.expires_at, got.created_at + Duration::from_secs(7200));
}

#[test]
fn a_store_that_has_never_been_written_is_empty_rather_than_broken() {
    let store = Store::at(scratch_dir("absent").join("sessions.json"));
    assert!(store.list().unwrap().is_empty());
    assert!(store.get("anything").unwrap().is_none());
}

#[test]
fn sessions_accumulate_and_can_be_removed() {
    let store = Store::at(scratch_dir("many").join("sessions.json"));
    store.put(session("alpha", "m-1")).unwrap();
    store.put(session("beta", "m-2")).unwrap();
    assert_eq!(store.list().unwrap().len(), 2);

    let removed = store.remove("alpha").unwrap().expect("was there");
    assert_eq!(removed.machine, MachineRef::new("m-1"));
    assert_eq!(store.list().unwrap().len(), 1);
    assert!(store.remove("alpha").unwrap().is_none());
}

#[test]
fn putting_the_same_name_twice_replaces_rather_than_duplicates() {
    let store = Store::at(scratch_dir("replace").join("sessions.json"));
    store.put(session("alpha", "m-1")).unwrap();
    store.put(session("alpha", "m-2")).unwrap();
    let all = store.list().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].machine, MachineRef::new("m-2"));
}

#[test]
fn a_store_from_another_version_is_refused_rather_than_misread() {
    let path = scratch_dir("version").join("sessions.json");
    std::fs::write(&path, r#"{"version":99,"sessions":{}}"#).unwrap();
    let e = Store::at(&path).list().expect_err("should refuse");
    assert!(e.to_string().contains("different version"), "{e}");
}

#[test]
fn a_corrupt_store_is_reported_not_silently_emptied() {
    // Silently treating unreadable state as "no sessions" would strand live
    // machines, which is the one outcome the design must not produce quietly.
    let path = scratch_dir("corrupt").join("sessions.json");
    std::fs::write(&path, "{ this is not json").unwrap();
    let e = Store::at(&path).list().expect_err("should refuse");
    assert!(e.to_string().contains("unreadable"), "{e}");
}

#[test]
fn writes_replace_the_file_rather_than_editing_it_in_place() {
    // The temp file must be gone and the real file whole. A half-written store
    // is worse than an absent one.
    let dir = scratch_dir("atomic");
    let path = dir.join("sessions.json");
    let store = Store::at(&path);
    store.put(session("alpha", "m-1")).unwrap();

    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains("tmp") || n.contains("lock"))
        .collect();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    assert!(serde_json::from_str::<serde_json::Value>(
        &std::fs::read_to_string(&path).unwrap()
    )
    .is_ok());
}

#[test]
fn a_stale_lock_is_aged_out_rather_than_wedging_every_future_run() {
    let dir = scratch_dir("stalelock");
    let path = dir.join("sessions.json");
    let lock = dir.join("sessions.lock");
    std::fs::write(&lock, "").unwrap();

    // Backdate it well past the staleness threshold.
    let old = SystemTime::now() - Duration::from_secs(3600);
    let f = std::fs::File::open(&lock).unwrap();
    f.set_modified(old).unwrap();
    drop(f);

    Store::at(&path)
        .put(session("alpha", "m-1"))
        .expect("a stale lock should not block forever");
}

#[test]
fn remaining_time_runs_out_rather_than_going_negative() {
    let s = session("alpha", "m-1");
    assert_eq!(
        s.remaining(s.created_at),
        Some(Duration::from_secs(7200))
    );
    // Past the expiry there is no "negative time left"; there is no time left.
    assert_eq!(s.remaining(s.expires_at + Duration::from_secs(1)), None);
    assert_eq!(s.age(s.created_at + Duration::from_secs(60)), Duration::from_secs(60));
}

#[test]
fn updating_a_session_moves_only_that_session() {
    let store = Store::at(scratch_dir("update").join("sessions.json"));
    store.put(session("alpha", "m-1")).unwrap();
    store.put(session("beta", "m-2")).unwrap();

    // Whole seconds, because that is the granularity the store keeps -- see
    // the round-trip test below.
    let later = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    assert!(store.update("alpha", |s| s.expires_at = later).unwrap());

    let a = store.get("alpha").unwrap().unwrap();
    let b = store.get("beta").unwrap().unwrap();
    assert_eq!(a.expires_at, later);
    // The heartbeat renewing one session must not disturb another.
    assert_eq!(b.expires_at, session("beta", "m-2").expires_at);
}

#[test]
fn updating_a_session_that_is_not_there_says_so() {
    let store = Store::at(scratch_dir("update-absent").join("sessions.json"));
    assert!(!store.update("ghost", |s| s.heartbeat_pid = None).unwrap());
}

#[test]
fn times_are_kept_to_whole_seconds_on_purpose() {
    // Sub-second precision is dropped deliberately: this file gets read by
    // people during incidents, and a number that can be pasted straight into
    // `date` is worth more than nanoseconds nobody will ever act on. Against a
    // TTL measured in hours the truncation is immaterial -- but it is a
    // guarantee, so it is asserted rather than left to be rediscovered.
    let store = Store::at(scratch_dir("granularity").join("sessions.json"));
    let ragged = UNIX_EPOCH + Duration::from_nanos(1_700_000_000_987_654_321);
    let mut s = session("alpha", "m-1");
    s.expires_at = ragged;
    store.put(s).unwrap();

    let got = store.get("alpha").unwrap().unwrap();
    assert_eq!(got.expires_at, UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    assert!(got.expires_at <= ragged, "truncation must never round forward");
}
