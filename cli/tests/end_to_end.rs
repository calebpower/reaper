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
            sess=''
            for b in "$@"; do
                case "${b}" in
                    UserKnownHostsFile=*known-hosts-*) sess=${b##*known-hosts-} ;;
                esac
            done
            if [ -n "${sess}" ] && [ -f "${here}/snapshots.${sess}" ]; then
                cat "${here}/snapshots.${sess}"
            else
                cat "${here}/snapshots" 2>/dev/null || true
            fi ;;
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
            &dir.join(".reaper.toml"),
            r#"
schema = 1
project = "a-project"
guests = ["a-guest"]
exec = "host"

[run]
cmd = "make check"
"#,
            None,
        );

        Harness { hypervisor, dir }
    }

    /// Like `run`, but standing somewhere that is not the project tree.
    fn run_from(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_reaper"))
            .args(args)
            .current_dir(cwd)
            .env("REAPER_CONFIG", self.dir.join("config.toml"))
            .env("REAPER_STATE", self.dir.join("sessions.json"))
            .output()
            .expect("run reaper")
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
        &h.dir.join(".reaper.toml"),
        r#"
schema = 1
project = "a-project"
guests = ["never-registered"]
exec = "host"

[run]
cmd = "make check"
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
            &h.dir.join(".reaper.toml"),
            &format!("schema = 1\nproject = \"{project}\"\nguests = [\"a-guest\"]\nexec = \"host\"\n[run]\ncmd = \"make check\"\n"),
            None,
        );
        h.ok(&["up"]);
    }
    write(
        &h.dir.join(".reaper.toml"),
        "schema = 1\nproject = \"three\"\nguests = [\"a-guest\"]\nexec = \"host\"\n[run]\ncmd = \"make check\"\n",
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
    std::fs::remove_file(h.dir.join(".reaper.toml")).unwrap();
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
        &h.dir.join(".reaper.toml"),
        "schema = 1\nproject = \"a-project\"\nguests = [\"a-guest\"]\nexec = \"host\"\n\
         [run]\ncmd = \"make check\"\n[resources]\ndisk_gb = 200\n",
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
schema = 1
project = "a-project"
guests = ["a-guest"]
exec = "container"

[build]
image = "docker.io/library/toolchain@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
cmd = "make build"
cache = ["deps", "build-dir"]

[build.env]
SHARED = "from-the-build"
ONLY_BUILD = "1"

[run]
exec = "host"
cmd = "make check"
images = ["docker.io/library/db@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]

[sync]
exclude = ["/scratch/"]

[reset]
datasets = ["state"]

[profiles]

[profiles.nightly]
warm_cache = false
env = { SHARED = "from-the-profile" }
"#;

#[test]
fn a_sync_pushes_the_tree_and_brings_results_back() {
    let h = Harness::new("sync");
    write(&h.dir.join(".reaper.toml"), FULL, None);
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
    write(&h.dir.join(".reaper.toml"), FULL, None);
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
    write(&h.dir.join(".reaper.toml"), FULL, None);
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
    write(&h.dir.join(".reaper.toml"), FULL, None);
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
    write(&h.dir.join(".reaper.toml"), FULL, None);
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
    write(&h.dir.join(".reaper.toml"), FULL, None);

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
    write(&h.dir.join(".reaper.toml"), FULL, None);
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
    write(&h.dir.join(".reaper.toml"), FULL, None);
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
    write(&h.dir.join(".reaper.toml"), FULL, None);
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
    write(&h.dir.join(".reaper.toml"), FULL, None);
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
/// session to run it on*, so `reaper sync --manifest other.toml` looked up
/// sessions for whichever project happened to be in `.reaper.toml` and failed
/// saying there were none -- naming a project it had not been asked about.
/// Found live, driving a second guest from a scratch manifest.
#[test]
fn a_manifest_elsewhere_decides_the_project_too() {
    let h = Harness::new("manifest-elsewhere");

    // The directory's own manifest names one project; the one we pass names
    // another. Only the second has a session.
    write(&h.dir.join(".reaper.toml"), FULL, None);
    write(
        &h.dir.join("other.toml"),
        r#"
schema = 1
project = "elsewhere"
guests = ["a-guest"]
exec = "host"

[run]
cmd = "make check"
"#,
        None,
    );

    h.ok(&["up", "--manifest", "other.toml"]);
    assert_eq!(h.machines().len(), 1);

    // Both must find the session the passed manifest describes, rather than
    // looking for one belonging to the directory's project.
    let synced = h.ok(&["sync", "--manifest", "other.toml"]);
    assert!(synced.contains("elsewhere"), "{synced}");
    let ran = h.ok(&["run", "--manifest", "other.toml"]);
    assert!(ran.contains("elsewhere"), "{ran}");

    // And the directory's own project still has no session, which is the
    // thing that made the old behaviour look plausible.
    let err = h.fails(&["sync"]);
    assert!(err.contains("a-project"), "{err}");

    // down and renew take it too. They did not, so `reaper down --manifest x`
    // was rejected outright -- and in a script that reads as a session that
    // was taken down when it was not.
    h.ok(&["renew", "--manifest", "other.toml"]);
    h.ok(&["down", "--manifest", "other.toml"]);
    assert!(h.machines().is_empty(), "down must have acted on the right project");
}

// ---------------------------------------------------------------------------
// Phase 4: rolling state back
// ---------------------------------------------------------------------------

#[test]
fn a_run_takes_the_pristine_snapshot_and_says_what_it_captured() {
    let h = Harness::new("pristine");
    write(&h.dir.join(".reaper.toml"), FULL, None);
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
    write(&h.dir.join(".reaper.toml"), FULL, None);
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
    write(&h.dir.join(".reaper.toml"), FULL, None);
    h.ok(&["up"]);
    h.forget_logs();

    let out = h.ok(&["reset"]);
    let ssh = h.log("ssh.log");
    assert!(
        ssh.contains("rollback --dataset state --name 'pristine'"),
        "{ssh}"
    );
    assert!(out.contains("rolled back to pristine"), "{out}");

    // And to a named point when asked.
    h.forget_logs();
    h.ok(&["reset", "--to", "before-the-bad-step"]);
    assert!(
        h.log("ssh.log").contains("--name 'before-the-bad-step'"),
        "{}",
        h.log("ssh.log")
    );

    h.ok(&["down"]);
}

#[test]
fn snapshot_names_a_point() {
    let h = Harness::new("snapshot");
    write(&h.dir.join(".reaper.toml"), FULL, None);
    h.ok(&["up"]);
    h.forget_logs();

    h.ok(&["snapshot", "mid"]);
    let ssh = h.log("ssh.log");
    assert!(ssh.contains("snapshot --dataset state --name 'mid'"), "{ssh}");
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
    write(&h.dir.join(".reaper.toml"), FULL, None);

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
    write(&h.dir.join(".reaper.toml"), FULL, None);
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
    write(&h.dir.join(".reaper.toml"), FULL, None);
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
    write(&h.dir.join(".reaper.toml"), FULL, None);
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
    write(&h.dir.join(".reaper.toml"), FULL, None);
    write(&h.dir.join("snapshots"), "pristine\nafter-stack-up\n", None);
    h.ok(&["up"]);
    h.forget_logs();

    let out = h.ok(&["test", "--to", "after-stack-up"]);
    assert!(out.contains("reset to after-stack-up"), "{out}");
    assert!(
        h.log("ssh.log").contains("rollback --dataset state --name 'after-stack-up'"),
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
    write(&h.dir.join(".reaper.toml"), FULL, None);
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

// ---------------------------------------------------------------------------
// Hardening: defects found by adversarial review. Every test here was watched
// failing against the code as first written.
// ---------------------------------------------------------------------------

impl Harness {
    /// Register a second guest at the site and hand back nothing: the point
    /// is that the config and the hypervisor both know it.
    fn add_guest(&self, name: &str) {
        let template = self.hypervisor.add_template(POOL);
        let mut cfg = std::fs::read_to_string(self.dir.join("config.toml")).unwrap();
        cfg.push_str(&format!("\n[guests.\"{name}\"]\ntemplate = \"{template}\"\n"));
        std::fs::write(self.dir.join("config.toml"), cfg).unwrap();
    }
}

const TWO_GUESTS: &str = r#"
schema = 1
project = "a-project"
guests = ["a-guest", "b-guest"]
exec = "host"

[run]
cmd = "make check"
"#;

#[test]
fn selecting_one_guest_of_two_names_the_session_the_same_way() {
    // `up --guest a` in a two-guest manifest used to name its session
    // "a-project" -- the single-guest spelling -- so a later plain `up` could
    // not see it and brought up a second machine for the same guest.
    let h = Harness::new("guestname");
    h.add_guest("b-guest");
    write(&h.dir.join(".reaper.toml"), TWO_GUESTS, None);

    let out = h.ok(&["up", "--guest", "a-guest"]);
    assert!(
        out.contains("a-project-a-guest: creating"),
        "the name must carry the guest suffix whenever the manifest has more \
         than one, whatever subset this invocation chose: {out}"
    );

    let out = h.ok(&["up"]);
    assert!(out.contains("a-project-a-guest: already up"), "{out}");
    assert!(out.contains("a-project-b-guest: creating"), "{out}");
    assert_eq!(
        h.machines().len(),
        2,
        "two guests means two machines; three means the spellings disagreed"
    );
}

#[test]
fn a_guest_without_a_build_is_skipped_not_fatal() {
    // Build is per-guest. A manifest mixing a compiled guest with an
    // interpreted one used to abort the whole `test` at the second session.
    let h = Harness::new("mixedbuild");
    h.add_guest("b-guest");
    write(
        &h.dir.join(".reaper.toml"),
        r#"
schema = 1
project = "a-project"
guests = [{ name = "a-guest", build = { cmd = "make prep" } }, "b-guest"]
exec = "host"

[run]
cmd = "make check"
"#,
        None,
    );

    h.ok(&["up"]);
    let out = h.ok(&["test"]);
    assert!(
        out.contains("b-guest declares no build; skipping"),
        "{out}"
    );
    assert!(
        out.contains("a-project-b-guest: run on b-guest"),
        "the skip must not cost the guest its run: {out}"
    );
    assert!(
        out.contains("a-project-a-guest: build on a-guest"),
        "and the guest that declares one still builds: {out}"
    );
}

#[test]
fn a_session_without_the_point_skips_only_itself() {
    // reset-before-run returned from inside the per-session loop, so one
    // fresh session cancelled the rollback its older sibling was owed, and
    // the older session ran on dirty state with nothing saying so.
    let h = Harness::new("resetpair");
    h.add_guest("b-guest");
    write(
        &h.dir.join(".reaper.toml"),
        r#"
schema = 1
project = "a-project"
guests = ["a-guest", "b-guest"]
exec = "host"

[run]
cmd = "make check"

[reset]
datasets = ["state"]
"#,
        None,
    );

    h.ok(&["up"]);
    // The b session has run before and holds a pristine; the a session --
    // which sorts first, which is what made the early return quiet -- has
    // nothing yet.
    write(&h.dir.join("snapshots.a-project-b-guest"), "pristine\n", None);

    let out = h.ok(&["test"]);
    assert!(
        out.contains("a-project-a-guest: nothing to reset to yet"),
        "{out}"
    );
    assert!(out.contains("a-project-b-guest: reset to pristine"), "{out}");
    let rolled: Vec<&str> = h
        .log("ssh.log")
        .lines()
        .filter(|l| l.contains("rollback --dataset state"))
        .map(|l| if l.contains("known-hosts-a-project-b-guest") { "b" } else { "a" })
        .collect::<Vec<_>>()
        .into_iter()
        .collect();
    assert_eq!(
        rolled,
        vec!["b"],
        "exactly the session that has the point rolls back"
    );
}

#[test]
fn a_snapshot_name_reaches_the_guest_as_data_not_shell() {
    // The name is the one free-text argument the snapshot verbs send, and the
    // runner's own validation happens only after the remote shell has parsed
    // the command line. Unquoted, `a;boom` runs boom.
    let h = Harness::new("snapquote");
    write(
        &h.dir.join(".reaper.toml"),
        r#"
schema = 1
project = "a-project"
guests = ["a-guest"]
exec = "host"

[run]
cmd = "make check"

[reset]
datasets = ["state"]
"#,
        None,
    );
    h.ok(&["up"]);
    h.ok(&["snapshot", "a;boom"]);
    assert!(
        h.log("ssh.log").contains("--name 'a;boom'"),
        "the whole name must arrive as one quoted word: {}",
        h.log("ssh.log")
    );
}

#[test]
fn down_refuses_a_session_and_all_together() {
    // `down staging --all` silently ignored "staging" and destroyed every
    // session of every project. Contradictory instructions are an error.
    let h = Harness::new("downall");
    let err = h.fails(&["down", "a-project", "--all"]);
    assert!(err.contains("--all"), "{err}");
}

#[test]
fn an_explicit_session_of_another_project_is_refused() {
    // `reaper sync <other-projects-session>` used to push this tree into that
    // project's machine -- with --delete -- and stamp its synced_at. A verb
    // acting for a project stays inside it; a one-character typo costs a
    // sentence, not a poisoned workspace.
    let h = Harness::new("crossproj");
    h.ok(&["up"]);
    write(
        &h.dir.join("other.toml"),
        r#"
schema = 1
project = "b-project"
guests = ["a-guest"]
exec = "host"

[run]
cmd = "make check"
"#,
        None,
    );
    let err = h.fails(&["sync", "a-project", "--manifest", "other.toml"]);
    assert!(err.contains("belongs to"), "{err}");
    assert!(
        !h.log("rsync.log").contains("--delete"),
        "nothing may have been pushed: {}",
        h.log("rsync.log")
    );
}

#[test]
fn reusing_a_session_that_never_became_ready_is_refused() {
    // The reuse branch used to print "already up on no address yet -- reusing
    // it" and exit 0, handing the operator a session no verb can use.
    let h = Harness::new("unready");
    write(
        &h.dir.join("sessions.json"),
        r#"{
  "version": 1,
  "sessions": {
    "a-project": {
      "name": "a-project",
      "project": "a-project",
      "guest": "a-guest",
      "template": "opaque",
      "machine": "an-opaque-handle",
      "address": null,
      "created_at": 1700000000,
      "ready_at": null,
      "expires_at": 1700003600,
      "ttl": 7200,
      "heartbeat_pid": null,
      "synced_at": null
    }
  }
}"#,
        None,
    );
    let err = h.fails(&["up"]);
    assert!(err.contains("never became ready"), "{err}");
    assert!(err.contains("reaper down"), "say what clears it: {err}");
}

#[test]
fn down_with_a_manifest_collects_results_from_anywhere() {
    // --manifest selected the sessions but the results tree was still read
    // from the current directory, so `down --manifest X` from elsewhere
    // destroyed the machine after printing that results had nowhere to land.
    let h = Harness::new("downmanifest");
    h.ok(&["up"]);
    h.ok(&["sync"]);
    h.forget_logs();

    let elsewhere = h.dir.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let manifest = h.dir.join(".reaper.toml");
    let out = h.run_from(&elsewhere, &["down", "--manifest", manifest.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(
        stdout.contains("results collected"),
        "the manifest names the tree, wherever the invocation stands: {stdout}"
    );
    assert!(
        !h.log("rsync.log").is_empty(),
        "the final pull must actually have run"
    );
}

// ---------------------------------------------------------------------------
// Fresh battery: paths the adversarial review found no test exercising.
// ---------------------------------------------------------------------------

#[test]
fn a_profile_ttl_reaches_the_machine() {
    let h = Harness::new("profilettl");
    write(
        &h.dir.join(".reaper.toml"),
        r#"
schema = 1
project = "a-project"
guests = ["a-guest"]
exec = "host"

[run]
cmd = "make check"

[profiles]

[profiles.quick]
ttl = "1h"
"#,
        None,
    );
    let out = h.ok(&["up", "--profile", "quick"]);
    assert!(
        out.contains("expires in 1h"),
        "the profile's TTL, not the site default: {out}"
    );
}

#[test]
fn renew_with_an_explicit_ttl_uses_it() {
    let h = Harness::new("renewttl");
    h.ok(&["up"]);
    let out = h.ok(&["renew", "--ttl", "30m"]);
    assert!(out.contains("expires in 30m"), "{out}");
}

#[test]
fn list_is_honest_about_expiry_and_dead_heartbeats() {
    // EXPIRED is not cosmetic -- the sweeper may take the machine at any
    // moment -- and a DEAD heartbeat means the countdown has stopped moving.
    // Both renderings existed with no test that would notice them lying.
    let h = Harness::new("listhonest");
    write(
        &h.dir.join("sessions.json"),
        r#"{
  "version": 1,
  "sessions": {
    "a-project": {
      "name": "a-project",
      "project": "a-project",
      "guest": "a-guest",
      "template": "opaque",
      "machine": "an-opaque-handle",
      "address": "192.0.2.42",
      "created_at": 1700000000,
      "ready_at": 1700000060,
      "expires_at": 1700007200,
      "ttl": 7200,
      "heartbeat_pid": 4194000,
      "synced_at": null
    }
  }
}"#,
        None,
    );
    let out = h.ok(&["list"]);
    assert!(out.contains("EXPIRED"), "{out}");
    assert!(out.contains("DEAD"), "{out}");
}

#[test]
fn a_heartbeat_for_a_forgotten_session_exits_quietly() {
    // `down` removed the session; the heartbeat's next tick finds nothing.
    // That is the intended end of its life, not an error worth a message.
    let h = Harness::new("hbgone");
    let out = h.run(&["heartbeat", "--session", "no-such-session"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // The insecure-TLS warning is loud on every invocation by design;
    // nothing else may be said.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let unexpected: Vec<&str> = stderr
        .lines()
        .filter(|l| !l.contains("TLS certificate verification") && !l.trim().is_empty())
        .collect();
    assert!(
        out.stdout.is_empty() && unexpected.is_empty(),
        "nothing to renew and nothing to report: {} / {unexpected:?}",
        String::from_utf8_lossy(&out.stdout),
    );
}

// ---------------------------------------------------------------------------
// Battery: the cluster changed underneath you. One dead or vanished session
// must cost exactly itself, never its siblings' verb; and every surprise must
// arrive as a sentence, not a raw refusal. Written before the fixes and
// watched failing.
// ---------------------------------------------------------------------------

impl Harness {
    /// Rewrite one `[session]` key in the site config.
    fn amend_config(&self, key: &str, value: &str) {
        let p = self.dir.join("config.toml");
        let cfg = std::fs::read_to_string(&p).unwrap();
        let mut hit = false;
        let out: Vec<String> = cfg
            .lines()
            .map(|l| {
                if l.trim_start().starts_with(key) {
                    hit = true;
                    format!("{key} = \"{value}\"")
                } else {
                    l.to_string()
                }
            })
            .collect();
        assert!(hit, "no {key} in the harness config");
        std::fs::write(&p, out.join("\n")).unwrap();
    }
}

#[test]
fn renew_survives_a_machine_the_sweeper_took() {
    // Two sessions; the sweeper (played by the stand-in) took a-guest's
    // machine. Renewing used to abort at the first refusal, leaving b-guest's
    // expiry unmoved -- the session most in need of renewal punished for its
    // sibling's death.
    let h = Harness::new("renewgone");
    h.add_guest("b-guest");
    write(&h.dir.join(".reaper.toml"), TWO_GUESTS, None);
    h.ok(&["up"]);
    let gone = h
        .hypervisor
        .machine_named("a-project-a-guest")
        .expect("a-guest's machine");
    h.hypervisor.collect(&gone);

    let out = h.run(&["renew", "--ttl", "1h"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a vanished machine is worth a non-zero exit"
    );
    assert!(
        stdout.contains("a-project-b-guest: expires in 1h"),
        "the surviving session still renews: {stdout}"
    );
    assert!(
        stderr.contains("a-project-a-guest") && stderr.contains("gone"),
        "and the dead one is named, as gone: {stderr}"
    );
    assert!(
        stderr.contains("reaper down"),
        "with the way out: {stderr}"
    );
}

#[test]
fn sync_survives_a_session_that_cannot_be_reached() {
    let h = Harness::new("syncgone");
    h.add_guest("b-guest");
    write(&h.dir.join(".reaper.toml"), TWO_GUESTS, None);
    h.ok(&["up"]);
    // Every connection to a-guest's session refuses; b-guest's works.
    h.make_ssh_fail("known-hosts-a-project-a-guest");

    let out = h.run(&["sync"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a failed push is a failure");
    assert!(
        stdout.contains("a-project-b-guest: synced"),
        "the reachable session still gets the tree: {stdout}"
    );
    assert!(
        stderr.contains("a-project-a-guest"),
        "the unreachable one is named: {stderr}"
    );
}

#[test]
fn run_survives_a_session_that_cannot_be_reached() {
    let h = Harness::new("rungone");
    h.add_guest("b-guest");
    write(&h.dir.join(".reaper.toml"), TWO_GUESTS, None);
    h.ok(&["up"]);
    h.ok(&["sync"]);
    h.make_ssh_fail("known-hosts-a-project-a-guest");

    let out = h.run(&["run"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stdout.contains("a-project-b-guest: run finished"),
        "the reachable session still runs: {stdout}"
    );
    assert!(
        stderr.contains("a-project-a-guest"),
        "the unreachable one is named: {stderr}"
    );
}

#[test]
fn a_heartbeat_whose_machine_is_gone_ends_itself() {
    // set_expiry now says NotFound for a machine the cluster no longer
    // lists. A heartbeat hearing that has nothing left to renew, ever --
    // looping forever warning every interval is a leaked process wearing a
    // log line. It must end, and say why.
    let h = Harness::new("hbmachinegone");
    h.amend_config("heartbeat_interval", "1s");
    h.amend_config("default_ttl", "10s");
    h.ok(&["up"]);
    let machine = h.hypervisor.machine_named("a-project").expect("machine");
    // Stop the up-started heartbeat before the scenario, so it does not race.
    let stored = std::fs::read_to_string(h.dir.join("sessions.json")).unwrap();
    if let Some(pid) = stored
        .split("\"heartbeat_pid\": ")
        .nth(1)
        .and_then(|r| r.split(&[',', '\n'][..]).next())
        .and_then(|p| p.trim().parse::<i32>().ok())
    {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    h.hypervisor.collect(&machine);

    let mut child = Command::new(env!("CARGO_BIN_EXE_reaper"))
        .args(["heartbeat", "--session", "a-project"])
        .current_dir(&h.dir)
        .env("REAPER_CONFIG", h.dir.join("config.toml"))
        .env("REAPER_STATE", h.dir.join("sessions.json"))
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn heartbeat");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(st) = child.try_wait().expect("wait") {
            break Some(st);
        }
        if std::time::Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let Some(status) = status else {
        child.kill().ok();
        panic!("the heartbeat is still running against a machine that is gone");
    };
    assert!(status.success(), "ending is the correct outcome, not a crash");
    let mut err = String::new();
    use std::io::Read as _;
    child.stderr.take().unwrap().read_to_string(&mut err).ok();
    assert!(err.contains("gone"), "say why it ended: {err}");
}

#[test]
fn the_cap_message_counts_strangers() {
    let h = Harness::new("capstrangers");
    h.hypervisor.add_foreign_session(POOL);
    h.hypervisor.add_foreign_session(POOL);
    let err = h.fails(&["up"]);
    assert!(err.contains("2 of them not yours"), "{err}");
}

#[test]
fn a_corrupt_store_is_a_sentence_not_a_panic() {
    let h = Harness::new("corruptstore");
    write(&h.dir.join("sessions.json"), "{ definitely not json", None);
    let err = h.fails(&["list"]);
    assert!(!err.contains("panicked"), "{err}");
    assert!(
        err.contains("sessions.json"),
        "name the file somebody must look at: {err}"
    );
}

#[test]
fn an_up_nobody_answers_leaves_an_inspectable_tagged_session() {
    let h = Harness::new("neveranswers");
    h.amend_config("ready_grace", "1s");
    // The agent never comes up, so no address is ever reported and the wait
    // can only time out.
    h.hypervisor.with_state(|s| s.agent_unavailable = true);
    let out = h.ok(&["up"]);
    assert!(
        out.contains("nothing answered"),
        "say what happened and what to do: {out}"
    );
    assert_eq!(h.machines().len(), 1, "the machine exists, tagged, for autopsy");
    let list = h.ok(&["list"]);
    assert!(list.contains("a-project"), "{list}");
    h.ok(&["down"]);
    assert!(h.machines().is_empty(), "and down clears it");
}

#[test]
fn a_destroy_the_hypervisor_refuses_keeps_the_session_visible() {
    let h = Harness::new("downrefused");
    h.ok(&["up"]);
    let m = h.hypervisor.machine_named("a-project").expect("machine");
    h.hypervisor.protect(&m);
    let err = h.fails(&["down"]);
    assert!(err.contains("could not destroy"), "{err}");
    assert!(
        h.ok(&["list"]).contains("a-project"),
        "forgetting a machine that still exists would hide it"
    );
}

// ---------------------------------------------------------------------------
// Battery: what `up` spends before checking, and what a session leaves
// behind on the workstation. Written before the fixes and watched failing.
// ---------------------------------------------------------------------------

#[test]
fn down_leaves_no_per_session_droppings_on_the_workstation() {
    // Fourteen files from long-destroyed sessions were sitting in the real
    // state directory when somebody finally looked: a heartbeat log, a
    // known-hosts file and an rsh wrapper per session, kept forever. The
    // project that counts leaked directories does not get to litter its own.
    let h = Harness::new("droppings");
    h.ok(&["up"]);
    h.ok(&["sync"]);
    h.ok(&["down"]);
    for leftover in ["known-hosts-a-project", "rsh-a-project", "heartbeat-a-project.log"] {
        assert!(
            !h.dir.join(leftover).exists(),
            "{leftover} outlived its session"
        );
    }
}

#[test]
fn a_failed_down_keeps_the_sessions_workstation_files() {
    // The cleanup must be tied to the forgetting: a session that could not
    // be destroyed keeps its record, and its files with it -- the rsh
    // wrapper is how the next attempt will reach the machine.
    let h = Harness::new("keepfiles");
    h.ok(&["up"]);
    h.ok(&["sync"]);
    let m = h.hypervisor.machine_named("a-project").expect("machine");
    h.hypervisor.protect(&m);
    h.fails(&["down"]);
    assert!(
        h.dir.join("rsh-a-project").exists(),
        "a kept session keeps its transport"
    );
}

#[test]
fn up_refuses_an_unwritable_store_before_spending_a_machine() {
    // The session record is the only thing that lets `down` find the machine
    // later. Discovering the store is unwritable AFTER the clone leaves a
    // machine running on its grace with no record -- the exact shape of loss
    // the record exists to prevent. Refuse first; a machine is the expensive
    // half.
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::new("rostore");
    let ro = h.dir.join("ro-state");
    std::fs::create_dir_all(&ro).unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Root ignores modes (the in-guest suite runs as root); assert only what
    // this world can enforce, and never a spent machine either way.
    let wedge_holds = std::fs::write(ro.join("probe"), "").is_err();

    let out = Command::new(env!("CARGO_BIN_EXE_reaper"))
        .args(["up"])
        .current_dir(&h.dir)
        .env("REAPER_CONFIG", h.dir.join("config.toml"))
        .env("REAPER_STATE", ro.join("sessions.json"))
        .output()
        .expect("run reaper");
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();

    if wedge_holds {
        assert!(!out.status.success(), "an unwritable store is a refusal");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("sessions.json"), "name the path: {err}");
        assert!(
            h.machines().is_empty(),
            "the refusal must come before the machine is spent"
        );
    } else {
        assert!(out.status.success(), "writable after all, so up proceeds");
    }
}

#[test]
fn up_refuses_a_missing_ssh_key_before_spending_a_machine() {
    // Without the key no session is ever reachable, so a machine created
    // first is a machine wasted -- it would sit for the whole readiness
    // grace failing every connection, then need a manual down.
    let h = Harness::new("nokey");
    // Short, so a regression fails in seconds; the bug this test was written
    // against polled the full grace with a key that could never work.
    h.amend_config("ready_grace", "2s");
    h.amend_config("ssh_command", "/nonexistent/reaper-test-ssh");
    let cfg = std::fs::read_to_string(h.dir.join("config.toml")).unwrap();
    let cfg = cfg.replace(
        "ssh_user = \"root\"",
        "ssh_user = \"root\"\nssh_key = \"/nonexistent/reaper-test-key\"",
    );
    std::fs::write(h.dir.join("config.toml"), cfg).unwrap();

    let err = h.fails(&["up"]);
    assert!(
        err.contains("/nonexistent/reaper-test-key"),
        "name the missing key: {err}"
    );
    assert!(
        h.machines().is_empty(),
        "the refusal must come before the machine is spent"
    );
}
