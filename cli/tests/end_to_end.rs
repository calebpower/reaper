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
here=$(dirname "$0")
printf '%s
' "$*" >> "${here}/ssh.log"
for a in "$@"; do
    case "${a}" in
        *"cat > '/tmp/reaper-job.sh'"*) cat > "${here}/job" ;;
        *"cat > "*) cat > "${here}/uploaded" ;;
        # The one runner reply the CLI reads rather than ignores: where the
        # workspace is. Answered here so the CLI's own use of it is exercised.
        *workspace*)
            printf 'work=/a-pool/work/a-project\nout=/a-pool/work/a-project/out\n' ;;
        # The other reply the CLI reads rather than ignores: whether a snapshot
        # was actually taken. Deciding that is the runner's job and is tested
        # there; what is tested here is what the CLI does with the answer.
        # Which points exist. A file the test controls, so both the
        # never-run-yet case and the ordinary one can be modelled.
        *"runner.sh snapshots"*)
            cat "${here}/snapshots" 2>/dev/null || true ;;
        *"runner.sh snapshot"*)
            printf 'snapshot=tank/state@pristine\n' ;;
    esac
done
# Refuses the first N invocations and then works, so a test can model a machine
# that is not reachable yet without making it permanently unreachable.
if [ -f "${here}/ssh.refuse-times" ]; then
    n=$(cat "${here}/ssh.refuse-times")
    if [ "${n}" -gt 0 ]; then
        printf '%s' "$((n - 1))" > "${here}/ssh.refuse-times"
        echo "stand-in ssh: no route to host" >&2
        exit 255
    fi
fi
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
            &dir.join("fake-rsync"),
            r#"#!/bin/sh
# Stands in for rsync. Records the invocation and copies nothing: what the real
# flags do is proved against real rsync in reaper-core, and repeating it here
# would only test rsync twice.
printf '%s
' "$*" >> "$(dirname "$0")/rsync.log"
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
                 ssh_user = \"root\"\n\
                 rsync_command = \"{rsync}\"\n\
                 results_interval = \"1s\"\n\n\
                 [guests.\"a-guest\"]\n\
                 template = \"{template}\"\n",
                provider = hypervisor.site_config(&dir.join("token"), POOL),
                ssh = dir.join("fake-ssh").display(),
                rsync = dir.join("fake-rsync").display(),
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

    fn log(&self, which: &str) -> String {
        std::fs::read_to_string(self.dir.join(which)).unwrap_or_default()
    }

    fn forget_logs(&self) {
        for f in ["ssh.log", "rsync.log"] {
            let _ = std::fs::remove_file(self.dir.join(f));
        }
    }

    /// The job script the CLI delivered, as the guest would have received it.
    fn job(&self) -> String {
        std::fs::read_to_string(self.dir.join("job")).expect("no job was delivered")
    }
}

/// Every harness takes its sessions down and takes its directory with it.
///
/// Not tidiness. `up` detaches a heartbeat into its own process group so it
/// survives the terminal that started it, which is exactly right in production
/// and means a test that leaves a session behind leaves a process behind too --
/// renewing an expiry against a stand-in that stopped listening, for as long as
/// the machine is up. They were found by looking, three deep, after an
/// afternoon of running this suite.
///
/// Drop rather than an explicit call at the end of each test, because the tests
/// that most need it are the ones that fail partway through.
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.run(&["down", "--all"]);
        let _ = std::fs::remove_dir_all(&self.dir);
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

    // Retrying succeeds once operations complete again. Asserted on the
    // outcome rather than the wording: what matters is that the machine is
    // gone and the session is cleared, not which of the two paths got there.
    h.hypervisor.stall_operations(false);
    h.ok(&["down"]);
    assert!(h.machines().is_empty(), "the machine should be gone");
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

    // It must be its own session leader, or it dies with the terminal that
    // started it -- which offline testing cannot notice, because nothing here
    // signals the process group. Found live: the heartbeat was gone the moment
    // `up` returned.
    let sid = std::process::Command::new("ps")
        .args(["-o", "sid=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    let sid: u32 = String::from_utf8_lossy(&sid.stdout).trim().parse().unwrap_or(0);
    assert_eq!(
        sid, pid,
        "the heartbeat must be its own session leader (sid {sid} != pid {pid})"
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
    assert!(log.contains("chmod 0755 '/tmp/reaper-runner.sh'"), "{log}");
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

// ---------------------------------------------------------------------------
// Phase 3: getting work in, and results back out
// ---------------------------------------------------------------------------

/// A manifest with both verbs, two caches, a sync exclusion and a cold profile.
const FULL: &str = r#"
schema: 1
project: a-project
guests: [a-guest]
exec: container
build:
  image: docker.io/library/toolchain@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  cmd: make build
  cache: [deps, build-dir]
  env:
    SHARED: from-the-build
    ONLY_BUILD: "1"
run:
  exec: host
  cmd: make check
  images:
    - docker.io/library/db@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
sync:
  exclude: [/scratch/]
reset:
  datasets: [state]
profiles:
  nightly:
    warm_cache: false
    env:
      SHARED: from-the-profile
"#;

#[test]
fn a_sync_pushes_the_tree_and_brings_results_back() {
    let h = Harness::new("sync");
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    h.ok(&["up"]);
    h.forget_logs();

    let out = h.ok(&["sync"]);
    assert!(out.contains("synced"), "{out}");

    let rsync = h.log("rsync.log");
    let lines: Vec<&str> = rsync.lines().collect();
    assert_eq!(lines.len(), 2, "one push and one pull: {rsync}");

    // Forward. The destination is the path the *runner* reported, not one the
    // CLI worked out for itself -- pool layout is the runner's business.
    let push = lines[0];
    assert!(push.contains("--delete"), "{push}");
    assert!(push.contains("--exclude=/out/"), "{push}");
    assert!(push.contains("--exclude=/scratch/"), "the tenant's own: {push}");
    assert!(
        push.contains("192.0.2.42:/a-pool/work/a-project/"),
        "the runner said where the workspace is: {push}"
    );

    // And back, with no --delete: the guest is not authoritative for what was
    // in the operator's results directory beforehand.
    let pull = lines[1];
    assert!(
        !pull.contains("--delete"),
        "the results channel must never delete: {pull}"
    );
    assert!(pull.contains("192.0.2.42:/a-pool/work/a-project/out/"), "{pull}");

    h.ok(&["down"]);
}

#[test]
fn a_build_runs_in_the_declared_image_with_its_caches() {
    let h = Harness::new("build");
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    h.ok(&["up"]);
    h.ok(&["sync"]);
    h.forget_logs();

    h.ok(&["build"]);

    let ssh = h.log("ssh.log");
    assert!(
        ssh.contains("exec --project a-project --job /tmp/reaper-job.sh"),
        "{ssh}"
    );
    assert!(
        ssh.contains("--image docker.io/library/toolchain@sha256:aaaa"),
        "{ssh}"
    );
    assert!(ssh.contains("--cache deps"), "{ssh}");
    assert!(ssh.contains("--cache build-dir"), "{ssh}");

    // The command reached the guest as a delivered file, not as an argument.
    let job = h.job();
    assert!(job.contains("make build"), "{job}");
    assert!(job.contains("ONLY_BUILD='1'"), "{job}");
    assert!(
        !ssh.contains("make build"),
        "a tenant's command must never appear in an argument list: {ssh}"
    );

    h.ok(&["down"]);
}

#[test]
fn a_run_may_execute_on_the_host_of_a_container_guest() {
    let h = Harness::new("run-host");
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    h.ok(&["up"]);
    h.ok(&["sync"]);
    h.forget_logs();

    h.ok(&["run"]);

    let ssh = h.log("ssh.log");
    assert!(ssh.contains("exec --project a-project"), "{ssh}");
    // The guest defaults to container execution and this verb overrode it. The
    // absence of an image is the whole assertion.
    assert!(
        !ssh.contains("--image"),
        "a host-execution run must not be given an image: {ssh}"
    );
    assert!(h.job().contains("make check"));

    h.ok(&["down"]);
}

#[test]
fn a_cold_profile_mounts_no_cache_and_wins_on_environment() {
    let h = Harness::new("cold");
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    h.ok(&["up"]);
    h.ok(&["sync"]);
    h.forget_logs();

    let out = h.ok(&["build", "--profile", "nightly"]);
    assert!(out.contains("cold"), "the operator should be told: {out}");

    let ssh = h.log("ssh.log");
    // The caches are still named -- the runner is what makes them empty. A
    // tenant's command that refers to a cache path is the documented way to
    // use one, and dropping the names broke exactly that.
    assert!(ssh.contains("--cache deps"), "{ssh}");
    assert!(
        ssh.contains("--cold"),
        "the runner has to be told this is determinism mode: {ssh}"
    );

    // A profile changes how a session is run, so it wins where both name the
    // same variable -- otherwise the nightly profile could not change anything.
    let job = h.job();
    assert!(job.contains("SHARED='from-the-profile'"), "{job}");
    assert!(!job.contains("from-the-build"), "{job}");

    h.ok(&["down"]);
}

#[test]
fn an_unknown_profile_is_refused_by_name() {
    let h = Harness::new("bad-profile");
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    h.ok(&["up"]);

    let err = h.fails(&["build", "--profile", "does-not-exist"]);
    assert!(err.contains("does-not-exist"), "{err}");
    assert!(err.contains("nightly"), "should say what does exist: {err}");

    h.ok(&["down"]);
}

#[test]
fn a_project_with_no_build_is_told_so_rather_than_failing_in_the_guest() {
    let h = Harness::new("no-build");
    h.ok(&["up"]);

    let err = h.fails(&["build"]);
    assert!(err.contains("no build"), "{err}");

    h.ok(&["down"]);
}

#[test]
fn every_declared_image_is_fetched_when_a_session_first_comes_up() {
    let h = Harness::new("prepull");
    write(&h.dir.join(".reaper.yaml"), FULL, None);

    h.ok(&["up"]);
    let ssh = h.log("ssh.log");
    // The toolchain as well as the tenant's own stack: both were declared, both
    // are needed, and a digest already in the store costs nothing to ask for.
    assert!(ssh.contains("pull docker.io/library/db@sha256:bbbb"), "{ssh}");
    assert!(ssh.contains("docker.io/library/toolchain@sha256:aaaa"), "{ssh}");

    h.ok(&["down"]);
}

#[test]
fn a_session_that_cannot_pre_fetch_is_still_a_usable_session() {
    // A registry outage must cost a slow first build, never a machine that
    // took nine minutes to clone.
    let h = Harness::new("prefetch-refused");
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    // Narrow on purpose: the stand-in fails any invocation whose arguments
    // contain this, and every invocation carries the session directory in a
    // path, so a loose pattern breaks steps it was never meant to touch.
    write(&h.dir.join("ssh.fail"), "reaper-runner.sh pull", None);

    let out = h.ok(&["up"]);
    assert!(out.contains("192.0.2.42"), "the session must still come up: {out}");
    assert_eq!(h.machines().len(), 1);

    let _ = std::fs::remove_file(h.dir.join("ssh.fail"));
    h.ok(&["down"]);
}

#[test]
fn results_are_collected_before_a_machine_is_destroyed() {
    let h = Harness::new("down-collects");
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    h.ok(&["up"]);
    h.ok(&["sync"]);
    h.forget_logs();

    let out = h.ok(&["down"]);
    assert!(out.contains("results collected"), "{out}");

    let rsync = h.log("rsync.log");
    assert_eq!(rsync.lines().count(), 1, "one last pull: {rsync}");
    assert!(
        !rsync.contains("--delete"),
        "and still not a mirror: {rsync}"
    );
    assert!(h.machines().is_empty());
}

#[test]
fn a_session_that_was_never_synced_is_not_asked_for_results() {
    // The workspace was never made, so a pull would fail on a directory that
    // never existed -- and read as though results had been lost.
    let h = Harness::new("down-never-synced");
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    h.ok(&["up"]);
    h.forget_logs();

    h.ok(&["down"]);
    assert_eq!(h.log("rsync.log"), "", "nothing to collect, so nothing tried");
    assert!(h.machines().is_empty());
}

#[test]
fn a_failed_command_still_gets_its_results_out() {
    // The interesting failures are the ones that end with somebody giving up
    // and running `down`, so the trace has to leave before the failure is
    // reported rather than after.
    let h = Harness::new("failure-collects");
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    h.ok(&["up"]);
    h.ok(&["sync"]);
    h.forget_logs();
    write(&h.dir.join("ssh.fail"), "exec --project", None);

    h.fails(&["build"]);

    let rsync = h.log("rsync.log");
    assert!(
        rsync.contains("/a-pool/work/a-project/out/"),
        "a failing build must still hand its results over: {rsync}"
    );

    let _ = std::fs::remove_file(h.dir.join("ssh.fail"));
    h.ok(&["down"]);
}

/// Having an address and being reachable at it are different claims.
///
/// A dual-stacked guest autoconfigures IPv6 in a second or two and takes
/// several more to get a DHCP lease, so the first address it reports is often
/// one nothing here can route to. Accepting it made every later step fail with
/// `No route to host` against a machine that was working perfectly -- which is
/// exactly what happened on the cluster, and what this reproduces.
#[test]
fn up_waits_for_a_machine_that_answers_not_merely_one_with_an_address() {
    let h = Harness::new("unreachable-at-first");
    // Three refusals, then the machine starts answering.
    write(&h.dir.join("ssh.refuse-times"), "3", None);

    let out = h.ok(&["up"]);
    assert!(out.contains("192.0.2.42"), "it should get there in the end: {out}");
    assert!(
        out.contains("waiting"),
        "and should say what it is waiting on rather than sitting silent: {out}"
    );
    assert_eq!(h.machines().len(), 1, "and must not create a second machine");

    // The refusals were consumed rather than ignored.
    let left = std::fs::read_to_string(h.dir.join("ssh.refuse-times")).unwrap();
    assert_eq!(left.trim(), "0");

    h.ok(&["down"]);
}

/// And a machine that never answers is reported, not destroyed.
#[test]
fn a_machine_that_never_answers_is_left_tagged_rather_than_torn_down() {
    let h = Harness::new("never-answers");
    // More refusals than the grace period allows attempts.
    write(&h.dir.join("ssh.refuse-times"), "100000", None);
    write(
        &h.dir.join("config.toml"),
        &std::fs::read_to_string(h.dir.join("config.toml"))
            .unwrap()
            .replace("ready_grace = \"30m\"", "ready_grace = \"4s\""),
        None,
    );

    let out = h.ok(&["up"]);
    assert!(out.contains("nothing answered"), "{out}");
    // Nothing is leaked: it exists, it carries an expiry, and `down` can find
    // it. Destroying it here would throw away the one machine somebody might
    // want to look at.
    assert_eq!(h.machines().len(), 1);
    let tags = h.hypervisor.tags_of(&h.machines()[0]);
    assert!(tags.contains("expires-"), "must still be collectable: {tags:?}");

    let _ = std::fs::remove_file(h.dir.join("ssh.refuse-times"));
    h.ok(&["down"]);
    assert!(h.machines().is_empty());
}

/// `--manifest` has to mean the same thing to every verb.
///
/// It used to decide *what* to run while the current directory decided *which
/// session to run it on*, so `reaper sync --manifest other.yaml` looked up
/// sessions for whichever project happened to be in `.reaper.yaml` and failed
/// saying there were none -- naming a project it had not been asked about.
/// Found live, driving a second guest from a scratch manifest.
#[test]
fn a_manifest_elsewhere_decides_the_project_too() {
    let h = Harness::new("manifest-elsewhere");

    // The directory's own manifest names one project; the one we pass names
    // another. Only the second has a session.
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    write(
        &h.dir.join("other.yaml"),
        r#"
schema: 1
project: elsewhere
guests: [a-guest]
exec: host
run:
  cmd: make check
"#,
        None,
    );

    h.ok(&["up", "--manifest", "other.yaml"]);
    assert_eq!(h.machines().len(), 1);

    // Both must find the session the passed manifest describes, rather than
    // looking for one belonging to the directory's project.
    let synced = h.ok(&["sync", "--manifest", "other.yaml"]);
    assert!(synced.contains("elsewhere"), "{synced}");
    let ran = h.ok(&["run", "--manifest", "other.yaml"]);
    assert!(ran.contains("elsewhere"), "{ran}");

    // And the directory's own project still has no session, which is the
    // thing that made the old behaviour look plausible.
    let err = h.fails(&["sync"]);
    assert!(err.contains("a-project"), "{err}");

    // down and renew take it too. They did not, so `reaper down --manifest x`
    // was rejected outright -- and in a script that reads as a session that
    // was taken down when it was not.
    h.ok(&["renew", "--manifest", "other.yaml"]);
    h.ok(&["down", "--manifest", "other.yaml"]);
    assert!(h.machines().is_empty(), "down must have acted on the right project");
}

// ---------------------------------------------------------------------------
// Phase 4: rolling state back
// ---------------------------------------------------------------------------

#[test]
fn a_run_takes_the_pristine_snapshot_and_says_what_it_captured() {
    let h = Harness::new("pristine");
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    h.ok(&["up"]);
    h.ok(&["sync"]);
    h.forget_logs();

    let out = h.ok(&["run"]);

    // --if-absent, because whether a snapshot already exists is the runner's
    // decision and not one to duplicate here.
    let ssh = h.log("ssh.log");
    assert!(
        ssh.contains("snapshot --dataset state --name pristine --if-absent"),
        "{ssh}"
    );

    // And when the runner reports it took one, say what it captured --
    // "pristine" reads like the state before anything happened, and this is
    // the state after a whole run.
    assert!(out.contains("after this run"), "{out}");

    h.ok(&["down"]);
}

#[test]
fn a_failed_run_takes_no_snapshot() {
    // Pristine has to be a point the tenant's own stack reached successfully.
    // Snapshotting a failed run would make every later reset return to a
    // broken state, which is far worse than having no pristine at all.
    let h = Harness::new("pristine-not-after-failure");
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    h.ok(&["up"]);
    h.ok(&["sync"]);
    h.forget_logs();
    write(&h.dir.join("ssh.fail"), "exec --project", None);

    h.fails(&["run"]);
    assert!(
        !h.log("ssh.log").contains("snapshot"),
        "no snapshot may be taken after a failed run: {}",
        h.log("ssh.log")
    );

    let _ = std::fs::remove_file(h.dir.join("ssh.fail"));
    h.ok(&["down"]);
}

#[test]
fn reset_rolls_back_every_declared_dataset() {
    let h = Harness::new("reset");
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    h.ok(&["up"]);
    h.forget_logs();

    let out = h.ok(&["reset"]);
    let ssh = h.log("ssh.log");
    assert!(
        ssh.contains("rollback --dataset state --name pristine"),
        "{ssh}"
    );
    assert!(out.contains("rolled back to pristine"), "{out}");

    // And to a named point when asked.
    h.forget_logs();
    h.ok(&["reset", "--to", "before-the-bad-step"]);
    assert!(
        h.log("ssh.log").contains("--name before-the-bad-step"),
        "{}",
        h.log("ssh.log")
    );

    h.ok(&["down"]);
}

#[test]
fn snapshot_names_a_point() {
    let h = Harness::new("snapshot");
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    h.ok(&["up"]);
    h.forget_logs();

    h.ok(&["snapshot", "mid"]);
    let ssh = h.log("ssh.log");
    assert!(ssh.contains("snapshot --dataset state --name mid"), "{ssh}");
    // Not --if-absent: an explicit name is a deliberate act, and silently
    // keeping an older point of the same name would be the wrong answer.
    assert!(!ssh.contains("--if-absent"), "{ssh}");

    h.ok(&["down"]);
}

#[test]
fn a_project_with_no_reset_datasets_is_told_so() {
    // The minimal manifest declares none. Reset would otherwise be a no-op
    // that looked like it had done something.
    let h = Harness::new("no-reset");
    h.ok(&["up"]);

    let err = h.fails(&["reset"]);
    assert!(err.contains("nothing to roll back"), "{err}");
    let err = h.fails(&["snapshot", "mid"]);
    assert!(err.contains("no state"), "{err}");

    h.ok(&["down"]);
}

#[test]
fn the_reset_trigger_is_started_with_a_session_that_wants_one() {
    let h = Harness::new("trigger");
    write(&h.dir.join(".reaper.yaml"), FULL, None);

    h.ok(&["up"]);
    assert!(
        h.log("ssh.log").contains("control --project a-project start"),
        "{}",
        h.log("ssh.log")
    );

    h.ok(&["down"]);
}

#[test]
fn a_session_that_cannot_roll_anything_back_starts_no_trigger() {
    // The minimal manifest declares no reset datasets, so there is nothing for
    // a trigger to do and no reason to leave a process running in the guest.
    let h = Harness::new("no-trigger");
    h.ok(&["up"]);
    assert!(
        !h.log("ssh.log").contains("control --project"),
        "{}",
        h.log("ssh.log")
    );
    h.ok(&["down"]);
}


// ---------------------------------------------------------------------------
// Phase 5: the loop as one verb
// ---------------------------------------------------------------------------

/// The order is the substance of this verb, so the order is what is asserted.
fn step_order(log: &str) -> Vec<&'static str> {
    let mut seen = Vec::new();
    for line in log.lines() {
        // rsync and ssh share one log here only in spirit; each step is
        // recognisable by the command it runs.
        if line.contains("--delete") && !seen.contains(&"sync") {
            seen.push("sync");
        }
        if line.contains("--image") && line.contains("exec --project") && !seen.contains(&"build") {
            seen.push("build");
        }
        if line.contains("rollback --dataset") && !seen.contains(&"reset") {
            seen.push("reset");
        }
        if line.contains("exec --project") && !line.contains("--image") && !seen.contains(&"run") {
            seen.push("run");
        }
    }
    seen
}

#[test]
fn test_runs_the_four_steps_in_order() {
    let h = Harness::new("loop-order");
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    // A session that has already had a successful run, so there is a pristine
    // and the reset step has somewhere to go.
    write(&h.dir.join("snapshots"), "pristine\n", None);
    h.ok(&["up"]);
    h.forget_logs();

    h.ok(&["test"]);

    // One combined view: rsync's log carries the sync, ssh's the rest.
    let combined = format!("{}\n{}", h.log("rsync.log"), h.log("ssh.log"));
    let order = step_order(&combined);
    assert_eq!(
        order,
        vec!["sync", "build", "reset", "run"],
        "steps ran in the wrong order:\n{combined}"
    );

    h.ok(&["down"]);
}

#[test]
fn the_first_test_on_a_session_does_not_reset() {
    // There is no pristine yet, so a reset would fail for a reason that has
    // nothing to do with the project. `run` takes the snapshot at the end of
    // this pass, and every later `test` gets all four steps.
    let h = Harness::new("loop-first");
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    write(&h.dir.join("snapshots"), "", None);
    h.ok(&["up"]);
    h.forget_logs();

    let out = h.ok(&["test"]);
    assert!(out.contains("nothing to reset to yet"), "{out}");
    assert!(
        !h.log("ssh.log").contains("rollback --dataset"),
        "no rollback may be attempted: {}",
        h.log("ssh.log")
    );
    // But the run still happened, and still took the snapshot.
    assert!(h.log("ssh.log").contains("snapshot --dataset state"), "{}", h.log("ssh.log"));

    h.ok(&["down"]);
}

#[test]
fn test_skips_the_steps_a_project_does_not_have() {
    // The minimal manifest has no build and no reset datasets. Both are
    // ordinary, so both are skips rather than failures.
    let h = Harness::new("loop-minimal");
    h.ok(&["up"]);
    h.forget_logs();

    let out = h.ok(&["test"]);
    assert!(out.contains("no build declared"), "{out}");
    assert!(out.contains("no reset datasets declared"), "{out}");
    assert!(
        !h.log("ssh.log").contains("--image"),
        "nothing may be built: {}",
        h.log("ssh.log")
    );
    // And the run still ran, which is the whole point of the verb.
    assert!(h.log("ssh.log").contains("exec --project"), "{}", h.log("ssh.log"));

    h.ok(&["down"]);
}

#[test]
fn a_failing_step_stops_the_ones_after_it() {
    let h = Harness::new("loop-stops");
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    write(&h.dir.join("snapshots"), "pristine\n", None);
    h.ok(&["up"]);
    h.forget_logs();
    // Break the build. Nothing after it should be attempted.
    write(&h.dir.join("ssh.fail"), "--image docker.io/library/toolchain", None);

    h.fails(&["test"]);
    let ssh = h.log("ssh.log");
    assert!(
        !ssh.contains("rollback --dataset"),
        "a failed build must not be followed by a reset: {ssh}"
    );

    let _ = std::fs::remove_file(h.dir.join("ssh.fail"));
    h.ok(&["down"]);
}


#[test]
fn the_cap_counts_the_cluster_and_not_just_this_workstation() {
    // The resources a cap protects -- identifiers and storage -- belong to the
    // cluster. Counting only the local session file meant two people each got
    // the whole allowance, which is not a cap.
    let h = Harness::new("cap-shared");

    // Somebody else already has two up. The cap here is two.
    h.hypervisor.add_foreign_session(POOL);
    h.hypervisor.add_foreign_session(POOL);

    let err = h.fails(&["up"]);
    assert!(err.contains("already up on this provider"), "{err}");
    assert!(
        err.contains("not yours"),
        "it should say whose they are, or the message is baffling: {err}"
    );
    assert!(
        h.machines().len() == 2,
        "nothing of ours may have been created: {:?}",
        h.machines()
    );
}

#[test]
fn test_can_reset_to_a_named_point() {
    let h = Harness::new("loop-named");
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    write(&h.dir.join("snapshots"), "pristine\nafter-stack-up\n", None);
    h.ok(&["up"]);
    h.forget_logs();

    let out = h.ok(&["test", "--to", "after-stack-up"]);
    assert!(out.contains("reset to after-stack-up"), "{out}");
    assert!(
        h.log("ssh.log").contains("rollback --dataset state --name after-stack-up"),
        "{}",
        h.log("ssh.log")
    );

    h.ok(&["down"]);
}

#[test]
fn a_named_point_that_does_not_exist_is_an_error_not_a_skip() {
    // Absent pristine is "this session has not run yet" and is skipped. A name
    // the tenant typed and that is not there is very likely a typo, and
    // skipping it would run the command against whatever state was lying about.
    let h = Harness::new("loop-named-missing");
    write(&h.dir.join(".reaper.yaml"), FULL, None);
    write(&h.dir.join("snapshots"), "pristine\n", None);
    h.ok(&["up"]);
    h.forget_logs();

    let err = h.fails(&["test", "--to", "no-such-point"]);
    assert!(err.contains("no-such-point"), "{err}");
    assert!(err.contains("REAPER_CONTROL/snapshot"), "should name both ways to make one: {err}");
    // The build legitimately ran -- it comes before the reset step. What must
    // not have happened is the *run*, which is the one that would have gone
    // against whatever state was lying about.
    let combined = format!("{}\n{}", h.log("rsync.log"), h.log("ssh.log"));
    assert!(
        !step_order(&combined).contains(&"run"),
        "nothing may run against unreset state: {combined}"
    );

    h.ok(&["down"]);
}
