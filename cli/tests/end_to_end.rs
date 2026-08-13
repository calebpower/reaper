//! The CLI, end to end, against a stand-in hypervisor.
//!
//! This runs the real binary as a subprocess -- argument parsing, configuration
//! discovery, manifest loading, the provider, the session store and the
//! heartbeat all included. It is as close to Phase 1's live acceptance criteria
//! as can be reached without a hypervisor, and it exists so that the live run,
//! when it happens, is confirming rather than discovering.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use reaper_providers::mock::StandIn;

const POOL: &str = "a/pool";

struct Harness {
    hypervisor: StandIn,
    dir: PathBuf,
}

impl Harness {
    fn new(label: &str) -> Harness {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "reaper-cli-{}-{}-{label}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let hypervisor = StandIn::start();
        let template = hypervisor.add_template(POOL);
        // The machine answers as soon as it is asked, so `up` does not spend
        // the test's life waiting.
        hypervisor.reports_address("192.0.2.42");

        write(&dir.join("token"), hypervisor.credential(), Some(0o600));
        write(
            &dir.join("fake-ssh"),
            r#"#!/bin/sh
# Stands in for ssh. Records the invocation; captures an upload's stdin; fails
# on demand. It deliberately does not run the runner -- the runner has its own
# suite, and re-testing it through here would only make failures harder to read.
printf '%s
' "$*" >> "$(dirname "$0")/ssh.log"
for a in "$@"; do
    case "${a}" in
        *"cat > "*) cat > "$(dirname "$0")/uploaded" ;;
    esac
done
# Fails only for commands matching the pattern it was given, so a test can
# break one step without breaking the ones before it.
if [ -f "$(dirname "$0")/ssh.fail" ]; then
    pattern=$(cat "$(dirname "$0")/ssh.fail")
    for a in "$@"; do
        case "${a}" in
            *"${pattern}"*)
                echo "stand-in ssh: refusing ${pattern}" >&2
                exit 7 ;;
        esac
    done
fi
exit 0
"#,
            Some(0o755),
        );
        write(
            &dir.join("config.toml"),
            &format!(
                "{provider}\n\
                 [session]\n\
                 default_ttl = \"2h\"\n\
                 heartbeat_interval = \"10m\"\n\
                 ready_grace = \"30m\"\n\
                 max_concurrent = 2\n\
                 ssh_command = \"{ssh}\"\n\
                 ssh_user = \"root\"\n\n\
                 [guests.\"a-guest\"]\n\
                 template = \"{template}\"\n",
                provider = hypervisor.site_config(&dir.join("token"), POOL),
                ssh = dir.join("fake-ssh").display(),
            ),
            None,
        );
        write(
            &dir.join(".reaper.yaml"),
            r#"
schema: 1
project: a-project
guests: [a-guest]
exec: host
run:
  cmd: make check
"#,
            None,
        );

        Harness { hypervisor, dir }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_reaper"))
            .args(args)
            .current_dir(&self.dir)
            .env("REAPER_CONFIG", self.dir.join("config.toml"))
            .env("REAPER_STATE", self.dir.join("sessions.json"))
            .output()
            .expect("run reaper")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "reaper {args:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn fails(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "reaper {args:?} should have failed");
        String::from_utf8_lossy(&out.stderr).to_string()
    }

    /// The session machines the stand-in holds, templates excluded.
    fn machines(&self) -> Vec<String> {
        self.hypervisor.session_machines()
    }
}

fn write(path: &Path, body: &str, mode: Option<u32>) {
    std::fs::write(path, body).unwrap();
    if let Some(m) = mode {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(m)).unwrap();
    }
}

#[test]
fn a_session_goes_up_gets_listed_renewed_and_comes_back_down() {
    // Phase 1's acceptance criteria, in the one form available without a
    // hypervisor.
    let h = Harness::new("lifecycle");

    let up = h.ok(&["up"]);
    assert!(up.contains("192.0.2.42"), "up should report the address: {up}");
    let created = h.machines();
    assert_eq!(created.len(), 1, "exactly one machine: {created:?}");
    let id = created[0].clone();

    // Tagged with an expiry from the moment it existed.
    let tags = h.hypervisor.tags_of(&id);
    assert!(tags.contains("expires-"), "no expiry tag: {tags:?}");
    let first_expiry = expiry_in(&tags);

    let listed = h.ok(&["list"]);
    assert!(listed.contains("a-project"), "{listed}");
    assert!(listed.contains("192.0.2.42"), "{listed}");
    assert!(listed.contains("a-guest"), "{listed}");

    // Renewing moves the tag forward, which is the whole dead-man's switch.
    std::thread::sleep(Duration::from_millis(1100));
    h.ok(&["renew"]);
    let renewed = expiry_in(&h.hypervisor.tags_of(&id));
    assert!(
        renewed > first_expiry,
        "renew must move the expiry: {first_expiry} -> {renewed}"
    );

    h.ok(&["down"]);
    assert!(h.machines().is_empty(), "down must destroy the machine");
    assert!(h.ok(&["list"]).contains("no sessions"));
}

fn expiry_in(tags: &str) -> u64 {
    tags.split([';', ','])
        .find_map(|t| t.trim().strip_prefix("expires-"))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no expiry in {tags:?}"))
}

#[test]
fn a_second_up_reuses_the_live_session_rather_than_making_another() {
    let h = Harness::new("reuse");
    h.ok(&["up"]);
    let after_first = h.machines();

    let again = h.ok(&["up"]);
    assert!(again.contains("reusing"), "{again}");
    assert_eq!(h.machines(), after_first, "a second up must not create more");

    h.ok(&["down"]);
}

#[test]
fn an_unregistered_guest_is_refused_before_anything_is_created() {
    // A typo should cost nothing, and certainly not a machine.
    let h = Harness::new("unregistered");
    write(
        &h.dir.join(".reaper.yaml"),
        r#"
schema: 1
project: a-project
guests: [never-registered]
exec: host
run:
  cmd: make check
"#,
        None,
    );

    let err = h.fails(&["up"]);
    assert!(err.contains("never-registered"), "{err}");
    assert!(err.contains("a-guest"), "should list what is registered: {err}");
    assert!(h.machines().is_empty(), "nothing may have been created");
}

#[test]
fn the_concurrency_cap_is_enforced() {
    let h = Harness::new("cap");
    // The cap is two; three projects cannot all be up.
    for project in ["one", "two"] {
        write(
            &h.dir.join(".reaper.yaml"),
            &format!("schema: 1\nproject: {project}\nguests: [a-guest]\nexec: host\nrun:\n  cmd: make check\n"),
            None,
        );
        h.ok(&["up"]);
    }
    write(
        &h.dir.join(".reaper.yaml"),
        "schema: 1\nproject: three\nguests: [a-guest]\nexec: host\nrun:\n  cmd: make check\n",
        None,
    );

    let err = h.fails(&["up"]);
    assert!(err.contains("max_concurrent"), "{err}");
    assert_eq!(h.machines().len(), 2, "the third must not have been created");

    h.ok(&["down", "--all"]);
}

#[test]
fn down_keeps_a_session_it_could_not_destroy() {
    // Forgetting it would hide a machine that still exists. The expiry means
    // the sweeper collects it regardless, but the operator should see it.
    let h = Harness::new("down-fails");
    h.ok(&["up"]);
    h.hypervisor.stall_operations(true);

    let err = h.fails(&["down"]);
    assert!(err.contains("could not destroy"), "{err}");
    assert!(
        h.ok(&["list"]).contains("a-project"),
        "the session must remain visible"
    );

    // Retrying succeeds: the stand-in did carry out the deletion before its
    // task stalled, so the machine is genuinely gone. That is the same shape as
    // the case that matters in production -- the sweeper collected an expired
    // machine first -- and destroy is idempotent so the session can be cleared.
    h.hypervisor.stall_operations(false);
    let recovered = h.ok(&["down"]);
    assert!(recovered.contains("already gone"), "{recovered}");
    assert!(h.ok(&["list"]).contains("no sessions"));
}

#[test]
fn down_forgets_a_session_whose_machine_something_else_already_collected() {
    // The sweeper exists precisely to destroy machines whose expiry passed.
    // When it has, `down` must still let the operator clear the session.
    let h = Harness::new("already-collected");
    h.ok(&["up"]);
    let id = h.machines()[0].clone();
    h.hypervisor.collect(&id);

    let out = h.ok(&["down"]);
    assert!(out.contains("already gone"), "{out}");
    assert!(h.ok(&["list"]).contains("no sessions"));
}

#[test]
fn a_missing_manifest_says_what_to_do_about_it() {
    let h = Harness::new("no-manifest");
    std::fs::remove_file(h.dir.join(".reaper.yaml")).unwrap();
    let err = h.fails(&["up"]);
    assert!(err.contains("docs/tenants.md"), "{err}");
}

#[test]
fn a_heartbeat_is_started_and_stopped_with_the_session() {
    let h = Harness::new("heartbeat");
    h.ok(&["up"]);

    let listed = h.ok(&["list"]);
    let pid: u32 = listed
        .lines()
        .find(|l| l.starts_with("a-project"))
        .and_then(|l| l.split_whitespace().last())
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("no heartbeat pid in:\n{listed}"));
    assert!(
        unsafe { libc::kill(pid as i32, 0) } == 0,
        "the heartbeat should be running"
    );

    h.ok(&["down"]);
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        unsafe { libc::kill(pid as i32, 0) } != 0,
        "down should have stopped the heartbeat"
    );
}

#[test]
fn a_session_gets_a_blank_disk_sized_by_the_site_default() {
    let h = Harness::new("disk-default");
    h.ok(&["up"]);
    let id = h.machines()[0].clone();

    let disks = h.hypervisor.attached_disks(&id);
    assert!(
        disks.values().any(|d| d.ends_with(":64")),
        "expected a 64 GiB disk, attached: {disks:?}"
    );
    h.ok(&["down"]);
}

#[test]
fn a_tenant_can_ask_for_a_bigger_pool_than_the_site_default() {
    // The size is the tenant's knowledge -- a project with a large build cache
    // needs more -- so the manifest wins over the site's default.
    let h = Harness::new("disk-override");
    write(
        &h.dir.join(".reaper.yaml"),
        "schema: 1\nproject: a-project\nguests: [a-guest]\nexec: host\n\
         run:\n  cmd: make check\nresources:\n  disk_gb: 200\n",
        None,
    );
    h.ok(&["up"]);
    let id = h.machines()[0].clone();

    let disks = h.hypervisor.attached_disks(&id);
    assert!(
        disks.values().any(|d| d.ends_with(":200")),
        "expected a 200 GiB disk, attached: {disks:?}"
    );
    h.ok(&["down"]);
}

// --- bringing a machine to readiness ---------------------------------------

impl Harness {
    fn ssh_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("ssh.log")).unwrap_or_default()
    }
    fn uploaded(&self) -> String {
        std::fs::read_to_string(self.dir.join("uploaded")).unwrap_or_default()
    }
    /// Make the stand-in refuse only commands containing `pattern`.
    fn make_ssh_fail(&self, pattern: &str) {
        std::fs::write(self.dir.join("ssh.fail"), pattern).unwrap();
    }
}

#[test]
fn up_delivers_the_runner_and_builds_the_storage_before_declaring_ready() {
    let h = Harness::new("prepare");
    h.ok(&["up"]);

    // The runner is shipped, not installed into a template.
    let shipped = h.uploaded();
    assert!(shipped.contains("firstboot"), "runner not uploaded: {shipped:.80}");
    assert_eq!(
        shipped,
        include_str!("../../runner/runner.sh"),
        "what was uploaded must be exactly the runner this build carries"
    );

    let log = h.ssh_log();
    assert!(log.contains("chmod 0755 /tmp/reaper-runner.sh"), "{log}");
    assert!(log.contains("/tmp/reaper-runner.sh firstboot"), "{log}");

    // The connection is unattended and the host is brand new.
    assert!(log.contains("BatchMode=yes"), "{log}");
    assert!(log.contains("StrictHostKeyChecking=accept-new"), "{log}");
    assert!(
        log.contains("known-hosts-a-project"),
        "the known-hosts file must be per-session: {log}"
    );

    h.ok(&["down"]);
}

#[test]
fn a_session_whose_storage_cannot_be_built_is_not_reported_as_ready() {
    // A machine with an address but no pool is not a session. Reporting it as
    // up would send someone to a machine that cannot hold their work.
    let h = Harness::new("prepare-fails");
    h.make_ssh_fail("firstboot");

    let err = h.fails(&["up"]);
    assert!(err.contains("firstboot"), "{err}");
    // The upload succeeded, so the failure really is the storage step and not
    // an earlier one wearing its name.
    assert!(!h.uploaded().is_empty(), "the runner should still have been delivered");

    // The machine still exists and still carries its expiry, so nothing is
    // leaked -- and the session is recorded, so `down` can clear it.
    assert_eq!(h.machines().len(), 1);
    assert!(h.ok(&["list"]).contains("a-project"));

    std::fs::remove_file(h.dir.join("ssh.fail")).unwrap();
    h.ok(&["down"]);
}
