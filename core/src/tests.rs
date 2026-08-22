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

/// A temporary directory that removes itself, however the test ends.
///
/// A `remove_dir_all` at the end of a test body does not run when the test
/// panics, and tests panic -- that is what they are for. Three harnesses in
/// this project leaked scratch directories for exactly that reason, nearly a
/// thousand of them before anyone counted. Drop runs either way.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Scratch {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "reaper-core-test-{}-{}-{label}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Scratch(dir)
    }
}

impl AsRef<std::path::Path> for Scratch {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

impl std::ops::Deref for Scratch {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch_dir(label: &str) -> Scratch {
    Scratch::new(label)
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
        synced_at: None,
    }
}

#[test]
fn a_session_round_trips_through_the_store() {
    let dir = scratch_dir("roundtrip");
    let store = Store::at(dir.join("sessions.json"));
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
    let dir = scratch_dir("absent");
    let store = Store::at(dir.join("sessions.json"));
    assert!(store.list().unwrap().is_empty());
    assert!(store.get("anything").unwrap().is_none());
}

#[test]
fn sessions_accumulate_and_can_be_removed() {
    let dir = scratch_dir("many");
    let store = Store::at(dir.join("sessions.json"));
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
    let dir = scratch_dir("replace");
    let store = Store::at(dir.join("sessions.json"));
    store.put(session("alpha", "m-1")).unwrap();
    store.put(session("alpha", "m-2")).unwrap();
    let all = store.list().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].machine, MachineRef::new("m-2"));
}

#[test]
fn a_store_from_another_version_is_refused_rather_than_misread() {
    // The guard is bound, not chained off: `scratch_dir(..).join(..)` drops the
    // directory at the end of the statement and leaves the path dangling.
    let dir = scratch_dir("version");
    let path = dir.join("sessions.json");
    std::fs::write(&path, r#"{"version":99,"sessions":{}}"#).unwrap();
    let e = Store::at(&path).list().expect_err("should refuse");
    assert!(e.to_string().contains("different version"), "{e}");
}

#[test]
fn a_corrupt_store_is_reported_not_silently_emptied() {
    // Silently treating unreadable state as "no sessions" would strand live
    // machines, which is the one outcome the design must not produce quietly.
    let dir = scratch_dir("corrupt");
    let path = dir.join("sessions.json");
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

    // sessions.lock is expected to survive and is excluded by name, not by
    // pattern. The lock is now held *on* that file rather than being its
    // existence, and removing it on release is the classic route to two
    // holders -- so its presence is correct, and only its presence. Anything
    // else lock-shaped, and every temp file, is still a leak.
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n != "sessions.lock")
        .filter(|n| n.contains("tmp") || n.contains("lock"))
        .collect();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");

    // The half the exclusion above would otherwise cost: that the surviving
    // file is a *released* lock and not a leaked one. A guard that stopped
    // releasing would leave this file exactly as it is, and only this can tell
    // the difference.
    //
    // Retried rather than sampled once, and for a reason worth knowing: a lock
    // is inherited across fork, and released only when the last descriptor to
    // it closes. Another thread in this binary forking between our release and
    // our probe hands a copy to its child, which holds it until that child's
    // own exec succeeds. So a single instantaneous sample can see a lock that
    // *is* released, held by a process that is on its way out -- which made
    // this assertion itself flaky, at about one run in twenty, before it
    // learned to wait. The production locker rides out the same window through
    // its own retry loop.
    let lock = dir.join("sessions.lock");
    if lock.exists() {
        let probe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock)
            .unwrap();
        let mut released = false;
        for _ in 0..400 {
            if probe.try_lock().is_ok() {
                released = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            released,
            "the store never released its lock: the guard is leaking it"
        );
        probe.unlock().unwrap();
    }

    assert!(serde_json::from_str::<serde_json::Value>(
        &std::fs::read_to_string(&path).unwrap()
    )
    .is_ok());
}

#[test]
fn a_lock_whose_holder_died_does_not_wedge_the_next_run() {
    // What CI found and the workstation never did, because the window is
    // microseconds wide and a loaded runner is where microseconds live.
    //
    // `down` stops the heartbeat with SIGTERM and then SIGKILL. If that lands
    // while the heartbeat is inside a store mutation, the holder is gone --
    // and whatever it left behind must not outlive it. Under a scheme where
    // the lock *is* a file somebody created, it did: the next locker waited
    // out its entire timeout and then refused, so `down` failed on
    // `store.remove` having already destroyed the machine, leaving a session
    // record for a machine that no longer exists and a store nothing could
    // write for two minutes.
    let dir = scratch_dir("orphanlock");
    let path = dir.join("sessions.json");

    // Precisely what a killed holder leaves behind: the file, freshly stamped,
    // with nobody holding it.
    std::fs::write(dir.join("sessions.lock"), "").unwrap();

    let started = std::time::Instant::now();
    Store::at(&path)
        .put(session("alpha", "m-1"))
        .expect("a lock whose holder is gone is not a lock");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "took {:?}, so it is still reading a leftover file as a live lock",
        started.elapsed()
    );
}

#[test]
fn a_leftover_lock_file_of_any_age_is_not_a_lock() {
    // This used to assert that an old lock file was *aged out* after two
    // minutes, which was the best a scheme built on file existence could do.
    // The lock is now held on the file rather than being the file, so age
    // stops meaning anything: an hour-old leftover and a one-second-old one
    // are both simply files nobody is holding. Kept because the scenario is
    // still worth pinning -- what changed is that the answer is now "instant"
    // rather than "eventually".
    let dir = scratch_dir("stalelock");
    let path = dir.join("sessions.json");
    let lock = dir.join("sessions.lock");
    std::fs::write(&lock, "").unwrap();

    let old = SystemTime::now() - Duration::from_secs(3600);
    let f = std::fs::File::open(&lock).unwrap();
    f.set_modified(old).unwrap();
    drop(f);

    let started = std::time::Instant::now();
    Store::at(&path)
        .put(session("alpha", "m-1"))
        .expect("a leftover lock file should not block at all");
    assert!(started.elapsed() < Duration::from_secs(1), "it waited");
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
    let dir = scratch_dir("update");
    let store = Store::at(dir.join("sessions.json"));
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
    let dir = scratch_dir("update-absent");
    let store = Store::at(dir.join("sessions.json"));
    assert!(!store.update("ghost", |s| s.heartbeat_pid = None).unwrap());
}

#[test]
fn times_are_kept_to_whole_seconds_on_purpose() {
    // Sub-second precision is dropped deliberately: this file gets read by
    // people during incidents, and a number that can be pasted straight into
    // `date` is worth more than nanoseconds nobody will ever act on. Against a
    // TTL measured in hours the truncation is immaterial -- but it is a
    // guarantee, so it is asserted rather than left to be rediscovered.
    let dir = scratch_dir("granularity");
    let store = Store::at(dir.join("sessions.json"));
    let ragged = UNIX_EPOCH + Duration::from_nanos(1_700_000_000_987_654_321);
    let mut s = session("alpha", "m-1");
    s.expires_at = ragged;
    store.put(s).unwrap();

    let got = store.get("alpha").unwrap().unwrap();
    assert_eq!(got.expires_at, UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    assert!(got.expires_at <= ragged, "truncation must never round forward");
}

#[test]
fn the_default_pool_size_has_bounds_at_both_ends() {
    // A typo asking for tens of thousands of gibibytes should be refused here,
    // not by a storage backend part-way through creating a session.
    for bad in [0, 4097, 999_999] {
        let text = format!(
            "provider = \"p\"\n[session]\ndefault_disk_gb = {bad}\n[guests.g]\ntemplate = \"t\"\n[p]\n"
        );
        assert!(parse(&text).is_err(), "default_disk_gb = {bad} should be refused");
    }
    for good in [1, 64, 4096] {
        let text = format!(
            "provider = \"p\"\n[session]\ndefault_disk_gb = {good}\n[guests.g]\ntemplate = \"t\"\n[p]\n"
        );
        assert!(parse(&text).is_ok(), "default_disk_gb = {good} should be accepted");
    }
}

#[test]
fn the_default_pool_size_defaults() {
    assert_eq!(parse(GOOD).unwrap().session.default_disk_gb, 64);
}

// --- transport -------------------------------------------------------------

#[test]
fn ssh_never_prompts_and_never_shares_a_known_hosts_file() {
    // Both matter for the same reason: this runs unattended against machines
    // that did not exist a minute ago.
    let ssh = crate::transport::Ssh::new(
        "ssh",
        "root",
        "192.0.2.7".parse().unwrap(),
        None,
        PathBuf::from("/state/known-hosts-a-session"),
        Duration::from_secs(15),
        Duration::from_secs(300),
    );
    let opts = ssh.options().join(" ");

    assert!(opts.contains("BatchMode=yes"), "{opts}");
    assert!(opts.contains("StrictHostKeyChecking=accept-new"), "{opts}");
    assert!(
        opts.contains("UserKnownHostsFile=/state/known-hosts-a-session"),
        "the known-hosts file must be the per-session one: {opts}"
    );
    assert!(opts.contains("ConnectTimeout=15"), "{opts}");
    assert!(opts.ends_with("192.0.2.7"), "the host comes last: {opts}");
}

#[test]
fn a_configured_key_is_offered_and_it_is_the_only_one() {
    // A workstation with several keys loaded can exhaust the server's auth
    // attempts before reaching the right one, which presents as a permission
    // error that has nothing to do with permissions.
    let ssh = crate::transport::Ssh::new(
        "ssh",
        "root",
        "192.0.2.7".parse().unwrap(),
        Some(PathBuf::from("/keys/session")),
        PathBuf::from("/state/kh"),
        Duration::from_secs(15),
        Duration::from_secs(300),
    );
    let opts = ssh.options().join(" ");
    assert!(opts.contains("-i /keys/session"), "{opts}");
    assert!(opts.contains("IdentitiesOnly=yes"), "{opts}");
}

#[test]
fn with_no_key_configured_nothing_is_forced() {
    let ssh = crate::transport::Ssh::new(
        "ssh",
        "root",
        "192.0.2.7".parse().unwrap(),
        None,
        PathBuf::from("/state/kh"),
        Duration::from_secs(15),
        Duration::from_secs(300),
    );
    let opts = ssh.options().join(" ");
    assert!(!opts.contains("IdentitiesOnly"), "{opts}");
    assert!(!opts.contains("-i "), "{opts}");
}

#[test]
fn ssh_defaults_are_sane_and_overridable() {
    let c = parse(GOOD).unwrap();
    assert_eq!(c.session.ssh_user, "root");
    assert_eq!(c.session.ssh_command, "ssh");
    assert_eq!(c.session.ssh_connect_timeout, Duration::from_secs(15));
    assert!(c.session.ssh_key.is_none());

    let text = r#"
provider = "p"
[session]
ssh_user = "someone"
ssh_command = "/usr/local/bin/ssh-wrapper"
ssh_connect_timeout = "5s"
ssh_key = "/keys/k"
[guests.g]
template = "t"
[p]
"#;
    let c = parse(text).unwrap();
    assert_eq!(c.session.ssh_user, "someone");
    assert_eq!(c.session.ssh_command, "/usr/local/bin/ssh-wrapper");
    assert_eq!(c.session.ssh_connect_timeout, Duration::from_secs(5));
    assert_eq!(c.session.ssh_key, Some(PathBuf::from("/keys/k")));
}

// ---------------------------------------------------------------------------
// Job rendering
//
// The escaping is the part that matters. A command reaching a guest as
// anything other than what the tenant wrote is a bug that produces wrong test
// results rather than an error, and this project has already had one instance
// of a quote closing early -- on the workstation, where `rm -f` then ran
// locally. So the hostile cases are tested first and hardest.
// ---------------------------------------------------------------------------

use std::collections::BTreeMap;

use crate::job;

/// Run a rendered script through a real shell and report what one variable
/// actually held. Asserting on the script's text would only prove it matches
/// what this test expects; asserting on what a shell makes of it proves the
/// escaping works.
fn value_through_sh(value: &str) -> String {
    let env: BTreeMap<String, String> = [("V".to_string(), value.to_string())]
        .into_iter()
        .collect();
    // `printf %s` rather than echo: echo mangles backslashes on some shells,
    // and this test would then be measuring echo.
    let script = job::render("printf '%s' \"$V\" > \"$OUT\"", &env);

    let held = Scratch::new("job");
    let out = held.join("out");
    let status = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .env("OUT", &out)
        .status()
        .expect("run sh");
    assert!(status.success(), "script failed:\n{script}");

    std::fs::read_to_string(&out).expect("read output")
}

#[test]
fn hostile_environment_values_survive_a_real_shell() {
    for value in [
        "plain",
        "",
        "it's a quote",
        "\"double\" and 'single'",
        "$HOME and ${NOPE} and $(echo pwned)",
        "back`tick`s",
        "back\\slash\\",
        "semi; colon && ampersand | pipe",
        "new\nline",
        "trailing space ",
        "*",
    ] {
        assert_eq!(
            value_through_sh(value),
            value,
            "value did not survive rendering: {value:?}"
        );
    }
}

#[test]
fn a_command_is_not_quoted_because_it_is_a_shell_command() {
    let script = job::render("make test | tee $REAPER_OUT/log", &BTreeMap::new());
    assert!(
        script.contains("make test | tee $REAPER_OUT/log"),
        "the tenant's command must reach the shell as written, pipes and all:\n{script}"
    );
}

#[test]
fn a_job_knows_nothing_about_where_anything_lives() {
    // The runner owns the paths, because it is the component choosing the
    // execution mode and the two modes see different ones. A job that named
    // them would be a second place obliged to agree forever.
    let script = job::render("make", &BTreeMap::new());
    assert!(
        !script.contains("REAPER_WORK=") && !script.contains("cd "),
        "the job must not set the workspace up for itself:\n{script}"
    );
}

#[test]
fn a_profile_overlays_the_verbs_environment() {
    let verb: BTreeMap<String, String> = [
        ("KEEP".to_string(), "verb".to_string()),
        ("SHARED".to_string(), "verb".to_string()),
    ]
    .into_iter()
    .collect();
    let profile: BTreeMap<String, String> = [
        ("SHARED".to_string(), "profile".to_string()),
        ("ADDED".to_string(), "profile".to_string()),
    ]
    .into_iter()
    .collect();

    let merged = job::overlay(&verb, Some(&profile));
    assert_eq!(merged.get("KEEP").unwrap(), "verb");
    assert_eq!(merged.get("ADDED").unwrap(), "profile");
    // The profile wins. A profile changes how a session is run, and being
    // unable to change a variable the verb already set would make the nightly
    // profile in the shipped examples useless.
    assert_eq!(merged.get("SHARED").unwrap(), "profile");

    assert_eq!(job::overlay(&verb, None), verb);
}

// ---------------------------------------------------------------------------
// Sync
//
// Two layers, and both are needed. The argument assertions pin the decisions --
// what is excluded, what mirrors deletions, what does not. The round trip below
// runs real rsync over the same flags, because an assertion that reaper passes
// `--delete` proves reaper passes `--delete`, and says nothing about whether
// the results directory survives it.
// ---------------------------------------------------------------------------

use std::net::IpAddr;

use crate::sync;
use crate::transport::Ssh;

fn ssh_to(address: &str) -> Ssh {
    Ssh::new(
        "ssh",
        "root",
        address.parse::<IpAddr>().expect("address"),
        None,
        PathBuf::from("/tmp/kh"),
        Duration::from_secs(15),
        Duration::from_secs(300),
    )
}

#[test]
fn the_forward_sync_mirrors_deletions_and_protects_the_results_directory() {
    let plan = sync::push(
        "rsync",
        Path::new("/state/rsh"),
        &ssh_to("192.0.2.1"),
        Path::new("/home/tree"),
        "/pool/work/a-project",
        &["/target/".to_string()],
    );

    assert_eq!(plan.program, "rsync");
    assert!(plan.args.contains(&"--delete".to_string()));
    // Anchored. Without the leading slash this would also exclude any nested
    // `out` in the tree, and -- far worse -- the unanchored form is what people
    // write first, so the anchor is the thing worth asserting.
    assert!(plan.args.contains(&"--exclude=/out/".to_string()));
    assert!(plan.args.contains(&"--exclude=/target/".to_string()));
    assert!(
        !plan.args.iter().any(|a| a == "--delete-excluded"),
        "excluded means protected on the receiver, not destroyed"
    );
    assert!(!plan.args.iter().any(|a| a == "-z"), "no compression on a LAN");

    // Trailing slashes on both ends. Without one on the source, rsync nests the
    // tree one directory deeper on every single sync.
    assert_eq!(plan.args[plan.args.len() - 2], "/home/tree/");
    assert_eq!(plan.args[plan.args.len() - 1], "192.0.2.1:/pool/work/a-project/");
}

#[test]
fn the_forward_sync_reports_what_it_removed() {
    // Not decoration. A stash-based baseline answers "does this failure predate
    // my changes?" by removing files, and if that removal does not reach the
    // guest, the build there compiles code the operator deleted -- a baseline
    // that is green for the wrong reason, failing in the direction that looks
    // like success. rsync's own itemized output is the only witness to whether
    // `--delete` reached a given path, so it is asked rather than assumed.
    let plan = sync::push(
        "rsync",
        Path::new("/state/rsh"),
        &ssh_to("192.0.2.1"),
        Path::new("/home/tree"),
        "/pool/work/a-project",
        &[],
    );
    assert!(
        plan.args.contains(&"--itemize-changes".to_string()),
        "without it rsync says nothing at all about deletions"
    );

    let itemized = "\
*deleting   src/gone.rs
>f.st...... src/main.rs
*deleting   src/also-gone/
cd+++++++++ src/new/
";
    assert_eq!(sync::deletions(itemized), 2);
    assert_eq!(sync::deletions(""), 0, "a sync that removed nothing says nothing");
}

#[test]
fn clearing_the_results_directory_empties_it_and_keeps_it() {
    let dir = scratch("clear-results");
    let tree = dir.join("tree");
    let out = tree.join(sync::RESULTS);

    // A directory that is not there is not an error: nothing to clear.
    assert_eq!(sync::clear_results(&tree).expect("absent"), 0);

    std::fs::create_dir_all(out.join("stage-3")).unwrap();
    std::fs::write(out.join("results.xml"), b"<testsuite failures=\"0\"/>").unwrap();
    std::fs::write(out.join("stage-3/results.xml"), b"<testsuite failures=\"0\"/>").unwrap();
    // Outside the results directory, and none of its business.
    std::fs::write(tree.join("src.rs"), b"fn main() {}").unwrap();

    assert_eq!(sync::clear_results(&tree).expect("clear"), 2);
    assert!(out.exists(), "the directory itself is the rsync destination");
    assert_eq!(
        std::fs::read_dir(&out).unwrap().count(),
        0,
        "a run that produces nothing must not inherit the last one's results"
    );
    assert!(tree.join("src.rs").exists(), "and nothing outside it is touched");
}

#[test]
fn both_directions_bound_a_transfer_that_stops_moving() {
    // R-1, from the seance reports. Twice a results pull wedged with both ends
    // idle in select and nothing transferring, while the run it was reporting
    // on had already passed and the connection stayed ESTABLISHED -- a second
    // ssh to the same guest answered instantly both times. Nothing at the
    // connection layer was ever going to notice that, keepalives included,
    // because nothing about the connection was wrong. rsync's own inactivity
    // timeout is what notices, and without it `collector.join()` is an
    // unbounded wait on a transfer that has stopped having anything to say.
    let ssh = ssh_to("192.0.2.1");
    let push = sync::push(
        "rsync",
        Path::new("/state/rsh"),
        &ssh,
        Path::new("/home/tree"),
        "/pool/work/a-project",
        &[],
    );
    let pull = sync::pull(
        "rsync",
        Path::new("/state/rsh"),
        &ssh,
        "/pool/work/a-project/out",
        Path::new("/home/tree/out"),
    );
    for (which, plan) in [("push", &push), ("pull", &pull)] {
        assert!(
            plan.args.iter().any(|a| a.starts_with("--timeout=")),
            "the {which} direction can stall for ever: {:?}",
            plan.args
        );
    }
    // Taken from the transport rather than passed alongside it, so ssh and
    // rsync cannot end up with different patience for the same guest.
    assert!(
        push.args.contains(&"--timeout=300".to_string()),
        "the timeout must come from the transport: {:?}",
        push.args
    );
}

#[test]
fn the_results_channel_never_deletes() {
    let plan = sync::pull(
        "rsync",
        Path::new("/state/rsh"),
        &ssh_to("192.0.2.1"),
        "/pool/work/a-project/out",
        Path::new("/home/tree/out"),
    );

    assert!(
        !plan.args.iter().any(|a| a.starts_with("--delete")),
        "the guest is authoritative for what it produced, not for what was in \
         the operator's results directory beforehand"
    );
    assert_eq!(plan.args[plan.args.len() - 2], "192.0.2.1:/pool/work/a-project/out/");
    assert_eq!(plan.args[plan.args.len() - 1], "/home/tree/out/");
}

#[test]
fn an_ipv6_session_is_written_the_way_rsync_reads_it() {
    let plan = sync::pull(
        "rsync",
        Path::new("/state/rsh"),
        &ssh_to("2001:db8::1"),
        "/pool/work/p/out",
        Path::new("/tree/out"),
    );
    // Unbracketed, the colon before the path is ambiguous with the address's
    // own and rsync reads the wrong thing as the host.
    assert!(
        plan.args[plan.args.len() - 2].starts_with("[2001:db8::1]:"),
        "got {:?}",
        plan.args[plan.args.len() - 2]
    );
}

#[test]
fn the_transport_wrapper_carries_the_same_options_ssh_uses() {
    let dir = scratch("rsh-wrapper");
    let ssh = Ssh::new(
        "/usr/bin/ssh",
        "root",
        "192.0.2.9".parse().unwrap(),
        Some(PathBuf::from("/keys/session")),
        dir.join("known-hosts"),
        Duration::from_secs(15),
        Duration::from_secs(300),
    );

    let path = sync::rsh_wrapper(&ssh, &dir.join("rsh")).expect("wrapper");
    let text = std::fs::read_to_string(&path).unwrap();

    for expected in [
        "BatchMode=yes",
        "StrictHostKeyChecking=accept-new",
        "IdentitiesOnly=yes",
        "known-hosts",
        "/keys/session",
    ] {
        assert!(text.contains(expected), "wrapper is missing {expected}:\n{text}");
    }
    // The address belongs to rsync, which appends it itself. Carrying it here
    // as well would hand ssh two hosts.
    assert!(
        !text.contains("192.0.2.9"),
        "the wrapper must not name the host:\n{text}"
    );
    assert!(text.trim_end().ends_with("\"$@\""));

    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700, "the wrapper names a key file");
}

#[test]
fn a_wrapper_path_with_whitespace_is_refused_rather_than_mangled() {
    let dir = scratch("rsh space");
    let ssh = ssh_to("192.0.2.1");
    let e = sync::rsh_wrapper(&ssh, &dir.join("rsh")).expect_err("should refuse");

    let message = e.to_string();
    assert!(
        message.contains("whitespace") || message.contains("spaces"),
        "the refusal should say why: {message}"
    );
    // And it must say what to do about it, since the cause is somewhere the
    // operator has never had to think about.
    assert!(message.contains("XDG_STATE_HOME"), "no remedy offered: {message}");
}

fn scratch(label: &str) -> Scratch {
    Scratch::new(label)
}

/// A stand-in for ssh that drops the host and runs the command here.
///
/// This is what lets real rsync exercise the real flag set with no network and
/// no guest. rsync invokes `<rsh> <host> rsync --server …`, so dropping the
/// first argument turns a remote transfer into a local one without changing a
/// single flag under test.
fn local_rsh(dir: &Path) -> PathBuf {
    let path = dir.join("rsh-local");
    // Through write_executable like every other generated stub: real rsync
    // execs this one, so it is exposed to the same ETXTBSY race.
    write_executable(
        &path,
        "#!/bin/sh\n# Stand-in for ssh: drop the host, run the rest here.\nshift\nexec \"$@\"\n",
    );
    path
}

fn rsync_binary() -> String {
    // Not skipped when absent. A suite that quietly passes because its tool is
    // missing is worse than one that fails, and the same rule already governs
    // the shell linter.
    let found = std::process::Command::new("sh")
        .args(["-c", "command -v rsync"])
        .output()
        .expect("look for rsync");
    assert!(
        found.status.success(),
        "rsync is not installed, so the flags in sync.rs have never been run. \
         Install it rather than letting this suite pass without checking them."
    );
    String::from_utf8_lossy(&found.stdout).trim().to_string()
}

/// The line every generated stub carries, so that [`write_executable`] can
/// prove a file is executable without the script *doing* anything. Several of
/// these stubs record what they were invoked with, and a probe run that landed
/// in the record would be a test failing for a reason the harness invented.
const EXEC_PROBE_GUARD: &str = "if [ -n \"${REAPER_EXEC_PROBE:-}\" ]; then exit 0; fi\n";

/// Write a shell stub, and do not return until it can actually be executed.
///
/// ETXTBSY, and it is not hypothetical in a threaded test binary. The kernel
/// refuses to exec a file that any process holds open for writing. While this
/// thread has the stub open, another thread's fork duplicates that descriptor
/// into its child -- and the copy is closed only when the child's own exec
/// *succeeds*. For the width of that window a file this thread has already
/// closed is still open somewhere else, and exec'ing it fails with "Text file
/// busy".
///
/// Seen once in roughly six runs of this suite, as
/// `could not run .../fake-rsync: Text file busy`. That was survivable while
/// the suite was something a person ran; it stopped being survivable when a
/// release gate started running the whole suite, because a flake there is a
/// release that fails for no reason and a person who learns to re-run it.
///
/// So executability is confirmed rather than assumed. Retrying the exec is the
/// only fix available: no amount of care on this side closes a descriptor that
/// belongs to somebody else's child.
fn write_executable(path: &Path, body: &str) {
    let (shebang, rest) = body.split_once('\n').expect("a stub needs a shebang");
    assert!(shebang.starts_with("#!"), "a stub needs a shebang: {shebang:?}");
    write(path, format!("{shebang}\n{EXEC_PROBE_GUARD}{rest}").as_bytes());

    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");

    for _ in 0..400 {
        match std::process::Command::new(path)
            .env("REAPER_EXEC_PROBE", "1")
            .output()
        {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(Duration::from_millis(5))
            }
            _ => return,
        }
    }
    panic!(
        "{} was still not executable after two seconds",
        path.display()
    );
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// Real rsync, real flags, no network. This is the test that would catch a
/// wrong flag; the argument assertions above would not.
#[test]
fn a_real_round_trip_keeps_results_and_mirrors_deletions() {
    let rsync = rsync_binary();
    let dir = scratch("round-trip");
    let rsh = local_rsh(&dir);
    let ssh = ssh_to("127.0.0.1");

    let tree = dir.join("tree");
    let remote = dir.join("remote");
    std::fs::create_dir_all(&remote).unwrap();

    write(&tree.join("src/main.rs"), b"fn main() {}");
    write(&tree.join("doomed.txt"), b"here for now");
    write(&tree.join("target/huge.o"), b"expensive and useless over there");
    // A nested results directory, which the anchored exclude must not catch.
    write(&tree.join("fixtures/out/case.json"), b"{}");
    // Uncommitted work, and git metadata: neither may be excluded by default.
    write(&tree.join(".git/HEAD"), b"ref: refs/heads/main\n");

    // Something the guest produced on an earlier run, which a forward sync must
    // not destroy on its way past.
    write(&remote.join("out/trace.json"), b"{\"kept\":true}");

    let excludes = vec!["/target/".to_string()];
    let remote_str = remote.to_string_lossy().to_string();

    sync::push(&rsync, &rsh, &ssh, &tree, &remote_str, &excludes)
        .run()
        .expect("first push");

    assert!(remote.join("src/main.rs").exists());
    assert!(remote.join(".git/HEAD").exists(), ".git must go by default");
    assert!(
        remote.join("fixtures/out/case.json").exists(),
        "the exclude is anchored at the top of the tree, not every out anywhere"
    );
    assert!(!remote.join("target").exists(), "the tenant excluded this");
    assert!(
        remote.join("out/trace.json").exists(),
        "--delete must not reach into the results directory"
    );

    // Now delete something locally and sync again: the guest must forget it.
    std::fs::remove_file(tree.join("doomed.txt")).unwrap();
    let second = sync::push(&rsync, &rsh, &ssh, &tree, &remote_str, &excludes)
        .run()
        .expect("second push");
    assert!(
        !remote.join("doomed.txt").exists(),
        "a tree still holding a file the operator removed is not the tree under test"
    );
    // And real rsync says so in the shape reaper counts. An assertion against
    // a hand-written sample proves the parser; this proves the parser against
    // the program whose output it parses.
    assert_eq!(
        sync::deletions(&second),
        1,
        "the removal must be visible, not merely done: {second}"
    );
    assert!(remote.join("out/trace.json").exists(), "still protected");

    // Results, coming back. A multi-megabyte binary, because the evidence tier
    // that matters most emits screenshots rather than JSON.
    let blob: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    write(&remote.join("out/screenshot.png"), &blob);

    let local_out = tree.join("out");
    write(&local_out.join("from-an-earlier-local-run.txt"), b"mine");

    sync::pull(
        &rsync,
        &rsh,
        &ssh,
        &remote.join("out").to_string_lossy(),
        &local_out,
    )
    .run()
    .expect("pull");

    assert_eq!(
        std::fs::read(local_out.join("screenshot.png")).unwrap(),
        blob,
        "a bulky binary must come back byte for byte"
    );
    assert!(local_out.join("trace.json").exists());
    assert!(
        local_out.join("from-an-earlier-local-run.txt").exists(),
        "the results channel adds; it does not mirror"
    );

}

// ---------------------------------------------------------------------------
// Hardening: inputs that used to panic, hang, or be silently accepted. Every
// test here failed against the code as first written.
// ---------------------------------------------------------------------------

#[test]
fn a_multibyte_unit_is_a_parse_error_not_a_panic() {
    // split_at on a byte index used to land inside the µ and panic; a typo in
    // a config file must never take the process down.
    for input in ["5µ", "µ", "10µs", "5\u{00e9}"] {
        let e = crate::duration::parse(input).expect_err(input);
        assert!(e.to_string().contains(input), "{e}");
    }
}

#[test]
fn an_astronomical_duration_is_refused_not_a_panic() {
    // Two layers used to be able to panic on a huge-but-parseable duration:
    // the heartbeat margin check (Duration * 3 overflow) and every
    // `SystemTime + ttl` in the CLI. The parser now refuses anything past
    // ten years, which is the single fence all of them stand behind -- and
    // the margin check multiplies checked anyway, in case the fence moves.
    let e = crate::duration::parse("6148914691236517206s").expect_err("should refuse");
    assert!(e.to_string().contains("ten years"), "{e}");
    // The largest sane spellings still pass.
    assert!(crate::duration::parse("3650d").is_ok());
    assert!(crate::duration::parse("3651d").is_err());

    let text = r#"
provider = "p"
[guests.g]
template = "t"
[session]
heartbeat_interval = "6148914691236517206s"
default_ttl = "6148914691236517206s"
[p]
"#;
    let e = parse(text).expect_err("should refuse");
    assert!(e.to_string().contains("ten years"), "{e}");
}

#[test]
fn a_typoed_session_key_is_refused_not_silently_defaulted() {
    // A misspelt key that quietly gets the default is the worst outcome a
    // config file has: the person believes they set it.
    let text = r#"
provider = "p"
[guests.g]
template = "t"
[session]
default_ttll = "8h"
[p]
"#;
    let e = parse(text).expect_err("should refuse");
    assert!(e.to_string().contains("default_ttll"), "{e}");
}

#[test]
fn an_epoch_that_overflows_systemtime_is_corrupt_not_a_panic() {
    // The store's whole design converts an unreadable file into Corrupt; a
    // number too large for SystemTime arrives from the same file and must get
    // the same treatment.
    let dir = scratch_dir("epochoverflow");
    let path = dir.join("sessions.json");
    let store = Store::at(&path);
    store.put(session("alpha", "m-1")).unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    let sabotaged = text.replace("1700007200", "18446744073709551615");
    assert_ne!(text, sabotaged, "fixture no longer contains the expiry");
    std::fs::write(&path, sabotaged).unwrap();

    let e = store.list().expect_err("should refuse");
    assert!(
        matches!(e, crate::session::StoreError::Corrupt { .. }),
        "wanted Corrupt, got {e}"
    );
}

#[test]
fn a_lock_a_live_process_holds_ends_in_locked_not_a_spin() {
    // The claim this has always made is the one that matters: a lock somebody
    // else genuinely holds must end in Locked, within the deadline, without
    // spinning at 100% CPU on the way there.
    //
    // What changed is that it can now be tested honestly. It used to simulate
    // contention with a backdated file in an unwritable directory, which bound
    // only where DAC binds -- run as root, which is how this suite runs inside
    // a session, the "unstealable" lock was simply stolen and the test had to
    // branch on which world it was in. A real advisory lock contends the same
    // way for everyone, so the branch is gone and so is the ambiguity.
    let dir = scratch_dir("heldlock");
    let path = dir.join("sessions.json");

    // A second open file description on the same file, held for the duration.
    // flock contends across descriptions, including within one process, so
    // this is real contention and not a stand-in for it.
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("sessions.lock"))
        .unwrap();
    holder.try_lock().expect("the test takes the lock first");

    let store = Store::with_timeouts(&path, Duration::from_millis(300));
    let started = std::time::Instant::now();
    let result = store.put(session("alpha", "m-1"));
    let waited = started.elapsed();
    holder.unlock().unwrap();

    let e = result.expect_err("a lock somebody holds must refuse, not proceed");
    assert!(
        matches!(e, crate::session::StoreError::Locked { .. }),
        "wanted Locked, got {e}"
    );
    // Long enough to have actually waited its deadline, short enough not to
    // have spun past it. Both halves matter: returning instantly would mean
    // it never retried, and running long would mean the deadline is not
    // being checked.
    assert!(
        waited >= Duration::from_millis(250),
        "gave up after {waited:?} without waiting out its deadline"
    );
    assert!(
        waited < Duration::from_secs(5),
        "took {waited:?}, which looks like the spin"
    );

    // And once the holder lets go, the very next attempt works -- the lock is
    // released, not merely aged.
    store
        .put(session("alpha", "m-1"))
        .expect("released means released");
}

#[test]
fn put_executable_quotes_the_destination() {
    // The script goes to a remote shell; a destination with a space or a
    // metacharacter must stay a path. The stub stands in for ssh and records
    // the script it was handed.
    let dir = scratch_dir("putquote");
    let stub = dir.join("fake-ssh");
    let record = dir.join("argv");
    write_executable(&stub,
        &format!(
            "#!/bin/sh\nshift $(($# - 1))\nprintf '%s' \"$1\" > {} ; cat > /dev/null\n",
            crate::job::quote(record.to_str().unwrap())
        ),
    );

    let ssh = crate::transport::Ssh::new(
        stub.to_str().unwrap(),
        "root",
        "192.0.2.7".parse().unwrap(),
        None,
        dir.join("kh"),
        Duration::from_secs(15),
        Duration::from_secs(300),
    );
    use crate::transport::Transport;
    ssh.put_executable(b"#!/bin/sh\n", "/tmp/a b;touch pwned")
        .expect("stub accepts");

    let recorded = std::fs::read_to_string(&record).unwrap();
    // The last argument -- the remote script -- must carry the destination
    // inside single quotes, so the remote shell sees one word and no command.
    assert_eq!(
        recorded,
        "cat > '/tmp/a b;touch pwned' && chmod 0755 '/tmp/a b;touch pwned'",
    );
    assert!(!dir.join("pwned").exists(), "the metacharacter executed");
}

#[test]
fn put_executable_survives_a_chatty_child() {
    // A child that fills its stderr pipe while the parent is still writing
    // stdin used to deadlock both sides forever. The stub shouts a megabyte
    // of stderr first, then drains stdin, which is the pathological order.
    let dir = scratch_dir("putchatty");
    let stub = dir.join("fake-ssh");
    write_executable(&stub,
        "#!/bin/sh\ni=0; while [ $i -lt 16384 ]; do printf '%064d\\n' $i >&2; i=$((i+1)); done; cat > /dev/null\n",
    );

    let ssh = crate::transport::Ssh::new(
        stub.to_str().unwrap(),
        "root",
        "192.0.2.7".parse().unwrap(),
        None,
        dir.join("kh"),
        Duration::from_secs(15),
        Duration::from_secs(300),
    );
    use crate::transport::Transport;
    let payload = vec![b'x'; 1 << 20];
    ssh.put_executable(&payload, "/tmp/big")
        .expect("a chatty child is normal ssh -vvv behavior, not an error");
}

// ---------------------------------------------------------------------------
// Fresh battery: branches the adversarial review found no test exercising.
// ---------------------------------------------------------------------------

#[test]
fn state_file_precedence_is_explicit_then_xdg_then_home() {
    let _guard = env_lock().lock().unwrap();
    let home = std::env::var("HOME").expect("these tests need a HOME");

    std::env::set_var("REAPER_STATE", "/explicit/sessions.json");
    std::env::set_var("XDG_STATE_HOME", "/xdg-state");
    assert_eq!(
        crate::paths::state_file(),
        PathBuf::from("/explicit/sessions.json"),
        "the explicit override outranks everything"
    );

    std::env::remove_var("REAPER_STATE");
    assert_eq!(
        crate::paths::state_file(),
        PathBuf::from("/xdg-state/reaper/sessions.json"),
        "then the XDG spelling"
    );

    // An EMPTY XDG variable is set-but-meaningless, and must not put state at
    // the filesystem root.
    std::env::set_var("XDG_STATE_HOME", "");
    assert_eq!(
        crate::paths::state_file(),
        PathBuf::from(&home).join(".local/state/reaper/sessions.json"),
        "and finally home"
    );
    std::env::remove_var("XDG_STATE_HOME");
}

#[test]
fn a_blank_provider_name_is_refused() {
    // With a table whose name matches the blank, so this refusal -- and not
    // the missing-table one -- is the only thing standing.
    let text = "
provider = \"  \"
[guests.g]
template = \"t\"
[\"  \"]
";
    let e = parse(text).expect_err("should refuse");
    assert!(e.to_string().contains("provider is empty"), "{e}");
}

#[test]
fn the_ssh_key_expands_its_tilde() {
    let _guard = env_lock().lock().unwrap();
    let home = std::env::var("HOME").expect("these tests need a HOME");
    let text = r#"
provider = "p"
[guests.g]
template = "t"
[session]
ssh_key = "~/keys/session"
[p]
"#;
    let c = parse(text).expect("should parse");
    assert_eq!(
        c.session.ssh_key.as_deref(),
        Some(Path::new(&home).join("keys/session")).as_deref(),
        "a key the ssh binary would look for literally under ./~ is a config bug"
    );
}

#[test]
fn a_failed_sync_surfaces_the_tools_own_stderr() {
    // The reverse channel is how failure traces escape a machine about to be
    // destroyed; when it breaks, the person needs rsync's words, not ours.
    let dir = scratch_dir("syncfail");
    let stub = dir.join("fake-rsync");
    write_executable(&stub, "#!/bin/sh\necho 'connection unexpectedly closed' >&2\nexit 23\n");
    let ssh = crate::transport::Ssh::new(
        "ssh", "root", "192.0.2.7".parse().unwrap(), None, dir.join("kh"),
        Duration::from_secs(15),
        Duration::from_secs(300),
    );
    let plan = crate::sync::pull(
        stub.to_str().unwrap(),
        Path::new("/some/rsh"),
        &ssh,
        "/remote/results",
        &dir.join("local"),
    );
    let e = plan.run().expect_err("the stub fails");
    let msg = e.to_string();
    assert!(msg.contains("connection unexpectedly closed"), "{msg}");
    assert!(msg.contains("23"), "{msg}");
}

#[test]
fn format_rough_reads_right_at_the_unit_boundaries() {
    for (secs, want) in [
        (59u64, "59s"),
        (60, "1m00s"),
        (3599, "59m59s"),
        (3600, "1h00m"),
        (3661, "1h01m"),
    ] {
        assert_eq!(
            crate::duration::format_rough(Duration::from_secs(secs)),
            want
        );
    }
}

#[test]
fn a_pre_epoch_time_is_clamped_to_zero_not_a_panic() {
    // A machine with a badly wrong clock can hand out timestamps before 1970;
    // recording one must not take the store down, now or on the next read.
    let Some(before) = UNIX_EPOCH.checked_sub(Duration::from_secs(10)) else {
        return; // platform cannot represent it; nothing to defend against
    };
    let dir = scratch_dir("preepoch");
    let store = Store::at(dir.join("sessions.json"));
    let mut s = session("alpha", "m-1");
    s.created_at = before;
    store.put(s).expect("stored");
    let got = store.get("alpha").unwrap().expect("still there");
    assert_eq!(got.created_at, UNIX_EPOCH, "clamped, honestly, to the epoch");
}

#[test]
fn a_control_command_that_never_answers_is_given_up_on_and_killed() {
    // R-3, from the seance reports: an ssh whose remote command had already
    // exited sat in select for thirty-two minutes and held `reaper sync` open
    // behind it. The guest was healthy throughout. ConnectTimeout does not
    // reach this -- the connection was long since established -- so nothing
    // bounded it at all.
    let dir = scratch_dir("stalledcmd");
    let stub = dir.join("fake-ssh");
    // Answers nothing. `exec` so the stub *becomes* the sleep rather than
    // spawning one: a real ssh is a single process, and a stub that left a
    // grandchild behind would be testing a shape reaper never meets while
    // leaking a process for the length of the sleep.
    write_executable(&stub, "#!/bin/sh\nexec sleep 30\n");

    let ssh = crate::transport::Ssh::new(
        stub.to_str().unwrap(),
        "root",
        "192.0.2.7".parse::<IpAddr>().unwrap(),
        None,
        dir.join("kh"),
        Duration::from_secs(15),
        // The patience under test. Production is minutes; a test that waited
        // those out is a test that stops being run.
        Duration::from_millis(400),
    );

    let started = std::time::Instant::now();
    let e = crate::transport::Transport::run(&ssh, "true", "making the workspace")
        .expect_err("a command that never answers must not be waited on for ever");
    let waited = started.elapsed();

    assert!(
        matches!(e, crate::transport::TransportError::Stalled { .. }),
        "wanted Stalled, got {e}"
    );
    // It must not read as the guest having refused something. Only one of
    // those two says anything about the guest, and saying the wrong one sends
    // somebody to inspect a machine that is fine.
    let said = e.to_string();
    assert!(said.contains("stopped responding"), "{said}");
    assert!(said.contains("io_timeout"), "say which knob owns it: {said}");
    assert!(
        waited < Duration::from_secs(5),
        "took {waited:?}, so the deadline is not being enforced"
    );

    // And the child is gone rather than left running. A stall that leaked a
    // process per occurrence would be a slower version of the same bug.
    std::thread::sleep(Duration::from_millis(200));
    let leftover = std::process::Command::new("pgrep")
        .args(["-f", &format!("{}", stub.display())])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    assert!(
        leftover.is_empty(),
        "the stalled command was abandoned rather than killed: pids {leftover}"
    );
}

#[test]
fn a_failed_remote_command_reports_what_it_was_doing() {
    let dir = scratch_dir("runfail");
    let stub = dir.join("fake-ssh");
    write_executable(&stub, "#!/bin/sh\necho 'the guest said no' >&2\nexit 9\n");
    let ssh = crate::transport::Ssh::new(
        stub.to_str().unwrap(), "root", "192.0.2.7".parse().unwrap(), None,
        dir.join("kh"), Duration::from_secs(15),
        Duration::from_secs(300),
    );
    use crate::transport::Transport;
    let e = ssh.run("true", "checking the machine answers").expect_err("stub fails");
    let msg = e.to_string();
    assert!(msg.contains("checking the machine answers"), "{msg}");
    assert!(msg.contains("the guest said no"), "{msg}");
}

#[test]
fn a_provider_without_diagnostics_says_so_rather_than_answering_ok() {
    // The default impl exists so a new provider compiles without growing a
    // doctor -- but "nothing was checked" must never present as health.
    struct Bare;
    impl crate::provider::Provider for Bare {
        fn name(&self) -> &'static str { "bare" }
        fn create(&self, _: &crate::provider::CreateRequest) -> crate::provider::Result<crate::MachineRef> {
            unimplemented!()
        }
        fn set_expiry(&self, _: &crate::MachineRef, _: SystemTime) -> crate::provider::Result<()> {
            unimplemented!()
        }
        fn start(&self, _: &crate::MachineRef) -> crate::provider::Result<()> { unimplemented!() }
        fn stop(&self, _: &crate::MachineRef) -> crate::provider::Result<()> { unimplemented!() }
        fn destroy(&self, _: &crate::MachineRef) -> crate::provider::Result<()> { unimplemented!() }
        fn address(&self, _: &crate::MachineRef) -> crate::provider::Result<Option<std::net::IpAddr>> {
            unimplemented!()
        }
        fn list(&self) -> crate::provider::Result<Vec<crate::provider::MachineSummary>> {
            unimplemented!()
        }
    }
    let f = crate::provider::Provider::diagnose(&Bare, &[]);
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].health, crate::provider::Health::Warn, "{f:?}");
    assert!(f[0].detail.contains("no diagnostics"), "{}", f[0].detail);
}
