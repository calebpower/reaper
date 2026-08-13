//! The session verbs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use reaper_core::config::Config;
use reaper_core::provider::{CreateRequest, MachineRef, Provider};
use reaper_core::session::{Session, Store};
use reaper_core::sync;
use reaper_core::transport::{Ssh, Transport};
use reaper_core::{config, duration, job};
use reaper_manifest::{Exec, Manifest};

use crate::proc;

pub type Error = Box<dyn std::error::Error>;
type Result<T> = std::result::Result<T, Error>;

fn load_config() -> Result<Config> {
    Ok(config::load()?)
}

fn provider_for(cfg: &Config) -> Result<Box<dyn Provider>> {
    Ok(reaper_providers::build(&cfg.provider, cfg.provider_table())?)
}

/// A manifest, and the directory it sits in -- which is the tree that gets
/// synced. Taken from the manifest rather than from the current directory, so
/// `--manifest` points at a project rather than just at a file.
fn load_manifest_at(explicit: Option<PathBuf>) -> Result<(Manifest, PathBuf)> {
    let path = explicit.clone().unwrap_or_else(|| PathBuf::from(".reaper.yaml"));
    let root = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    Ok((load_manifest(explicit)?, root))
}

fn load_manifest(explicit: Option<PathBuf>) -> Result<Manifest> {
    let path = explicit.unwrap_or_else(|| PathBuf::from(".reaper.yaml"));
    if !path.exists() {
        return Err(format!(
            "no manifest at {}. reaper is run from inside a project; \
             see docs/tenants.md for what one looks like",
            path.display()
        )
        .into());
    }
    Ok(reaper_manifest::load(&path)?)
}

/// One session per guest. The name is the project alone when there is only one,
/// because `my-project` reads better than `my-project-some-guest` and most
/// projects have exactly one.
fn session_name(project: &str, guest: &str, single: bool) -> String {
    if single {
        project.to_string()
    } else {
        format!("{project}-{guest}")
    }
}

fn ttl_for(cfg: &Config, manifest: &Manifest, profile: Option<&str>, override_: Option<&str>) -> Result<Duration> {
    if let Some(t) = override_ {
        return Ok(duration::parse(t)?);
    }
    if let Some(name) = profile {
        let p = manifest.profiles.get(name).ok_or_else(|| {
            let known: Vec<&str> = manifest.profiles.keys().map(String::as_str).collect();
            format!(
                "the manifest has no profile {name:?}; it defines: {}",
                if known.is_empty() { "none".to_string() } else { known.join(", ") }
            )
        })?;
        if let Some(t) = &p.ttl {
            return Ok(duration::parse(t)?);
        }
    }
    Ok(cfg.session.default_ttl)
}

pub fn up(
    guest: Option<String>,
    profile: Option<String>,
    ttl: Option<String>,
    manifest_path: Option<PathBuf>,
) -> Result<()> {
    let cfg = load_config()?;
    let manifest = load_manifest(manifest_path)?;
    let store = Store::open();

    let wanted: Vec<&reaper_manifest::Guest> = match &guest {
        Some(name) => vec![manifest.guest(name).ok_or_else(|| {
            format!(
                "the manifest does not name a guest {name:?}; it names: {}",
                manifest
                    .guests
                    .iter()
                    .map(|g| g.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?],
        None => manifest.guests.iter().collect(),
    };

    // Resolve every guest against the registry before creating anything. A typo
    // should cost nothing, and it certainly should not leave one machine
    // running while the second name turns out to be wrong.
    for g in &wanted {
        if cfg.template_for(&g.name).is_none() {
            return Err(format!(
                "no guest named {:?} is registered here; this site offers: {}. \
                 Registering one is a template build and an entry in {} -- see docs/guests.md",
                g.name,
                cfg.guest_names().join(", "),
                cfg.path.display()
            )
            .into());
        }
    }

    let ttl = ttl_for(&cfg, &manifest, profile.as_deref(), ttl.as_deref())?;
    let single = wanted.len() == 1;
    let provider = provider_for(&cfg)?;

    for g in wanted {
        let name = session_name(&manifest.project, &g.name, single);

        if let Some(existing) = store.get(&name)? {
            println!(
                "{name}: already up on {} since {} ago -- reusing it",
                existing
                    .address
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "no address yet".into()),
                duration::format_rough(existing.age(SystemTime::now()))
            );
            continue;
        }

        let live = store.list()?.len();
        if live >= cfg.session.max_concurrent {
            return Err(format!(
                "{live} sessions are already up and this site allows {}. \
                 Take one down, or raise session.max_concurrent in {}",
                cfg.session.max_concurrent,
                cfg.path.display()
            )
            .into());
        }

        let template = cfg.template_for(&g.name).expect("resolved above").to_string();
        println!("{name}: creating on {}", g.name);

        // The first expiry is the readiness grace, not the session's TTL: a
        // full-copy clone can take minutes, and a TTL counted from the create
        // request would collect machines that were never used. The heartbeat
        // switches to the real TTL once the machine answers.
        let created_at = SystemTime::now();
        let machine = provider.create(&CreateRequest {
            name: name.clone(),
            template: template.clone(),
            cores: g.resources.cores,
            ram_gb: g.resources.ram_gb,
            // The tenant's size if it named one, otherwise the site's. A
            // project with a large build cache needs a bigger pool, and that
            // is the tenant's knowledge, not the sysadmin's.
            data_disk_gb: Some(g.resources.disk_gb.unwrap_or(cfg.session.default_disk_gb)),
            expires_at: created_at + cfg.session.ready_grace,
        })?;

        // Recorded before anything slow happens. An interrupted `up` must still
        // leave a session `down` can find; the alternative is a machine running
        // that nothing knows about.
        let mut session = Session {
            name: name.clone(),
            project: manifest.project.clone(),
            guest: g.name.clone(),
            template,
            machine: machine.clone(),
            address: None,
            created_at,
            ready_at: None,
            expires_at: created_at + cfg.session.ready_grace,
            ttl,
            heartbeat_pid: None,
            synced_at: None,
        };
        store.put(session.clone())?;

        provider.start(&machine)?;

        match wait_until_reachable(provider.as_ref(), &machine, &cfg, &name)? {
            Some((address, ssh)) => {
                // A machine we can reach is not yet a machine anyone can use:
                // it has no pool. Firstboot is what makes it a session, so it
                // happens before the session is called ready.
                prepare(&name, &ssh)?;
                prepull(&ssh, &name, &declared_images(g));
                if !manifest.reset.is_empty() {
                    start_control(&ssh, &manifest.project)?;
                }

                let ready_at = SystemTime::now();
                provider.set_expiry(&machine, ready_at + ttl)?;

                session.address = Some(address);
                session.ready_at = Some(ready_at);
                session.expires_at = ready_at + ttl;
                session.heartbeat_pid = start_heartbeat(&name)?;
                store.put(session)?;

                println!(
                    "{name}: up at {address}, expires in {}",
                    duration::format(ttl)
                );
            }
            None => {
                // The machine exists and carries its grace expiry, so nothing
                // is leaked whatever happens next.
                println!(
                    "{name}: created, but nothing answered on it within {}. \
                     It is tagged to expire; `reaper list` will show it, and \
                     `reaper down {name}` will remove it.",
                    duration::format(cfg.session.ready_grace)
                );
            }
        }
    }

    Ok(())
}

/// Wait until the machine has an address that actually answers.
///
/// Having an address and being reachable at it are different claims, and this
/// used to make only the first. A dual-stacked guest configures IPv6 by
/// autoconfiguration in a second or two and takes several more to get a DHCP
/// lease, so the first address it reports is often a v6 one -- and if the path
/// from here carries no v6, every later step fails with `No route to host` on a
/// machine that was working perfectly.
///
/// So the address is re-asked for on every attempt rather than fixed on the
/// first sighting, and the test is a real connection over the real transport.
/// Not a TCP probe of port 22: the transport is configurable, nothing else here
/// assumes a port, and "the daemon is listening" is a weaker claim than "a
/// command ran" in any case.
fn wait_until_reachable(
    provider: &dyn Provider,
    machine: &MachineRef,
    cfg: &Config,
    session: &str,
) -> Result<Option<(std::net::IpAddr, Ssh)>> {
    // Cleared once, here. A session starts with no history, so it cannot
    // inherit a stale key from an address that has been recycled -- and the
    // loop below may legitimately try more than one address for one machine.
    let _ = std::fs::remove_file(state_file(session, "known-hosts")?);

    let deadline = SystemTime::now() + cfg.session.ready_grace;
    let mut said: Option<String> = None;

    loop {
        if let Some(address) = provider.address(machine)? {
            let ssh = ssh_to(cfg, session, address)?;
            match ssh.run("true", "reaching the machine") {
                Ok(_) => return Ok(Some((address, ssh))),
                Err(e) => {
                    // Said once per distinct reason. A machine that is still
                    // booting produces the same refusal every three seconds,
                    // and repeating it would bury anything else.
                    let note = format!("{address}: {e}");
                    if said.as_deref() != Some(&note) {
                        println!("{session}: waiting -- {note}");
                        said = Some(note);
                    }
                }
            }
        }
        if SystemTime::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// The runner, as shipped. Compiled in so the CLI and the runner can never be
/// separate versions of themselves -- the reason nothing reaper wrote lives in
/// a template in the first place.
const RUNNER: &str = include_str!("../../runner/runner.sh");

/// Where the runner lands, and where a rendered job lands beside it.
const RUNNER_PATH: &str = "/tmp/reaper-runner.sh";

/// The snapshot a session takes for itself, and the one `reset` returns to
/// unless told otherwise.
const PRISTINE: &str = "pristine";
const JOB_PATH: &str = "/tmp/reaper-job.sh";

/// A file this session owns, beside the session store.
fn state_file(session: &str, prefix: &str) -> Result<PathBuf> {
    let path = Store::open()
        .path()
        .with_file_name(format!("{prefix}-{session}"));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    Ok(path)
}

fn ssh_to(cfg: &Config, session: &str, address: std::net::IpAddr) -> Result<Ssh> {
    // Per-session, and thrown away with the session. A shared known-hosts file
    // would accumulate keys for addresses that get recycled, and then refuse to
    // connect at the least convenient moment.
    Ok(Ssh::new(
        cfg.session.ssh_command.clone(),
        cfg.session.ssh_user.clone(),
        address,
        cfg.session.ssh_key.clone(),
        state_file(session, "known-hosts")?,
        cfg.session.ssh_connect_timeout,
    ))
}

fn ssh_for(cfg: &Config, s: &Session) -> Result<Ssh> {
    let address = s.address.ok_or_else(|| {
        format!(
            "{}: no address, so there is nothing to connect to. It may still be \
             starting; `reaper list` will show one when it has",
            s.name
        )
    })?;
    ssh_to(cfg, &s.name, address)
}

/// Delivered before every remote operation, not once at creation.
///
/// It is a few kilobytes over an already-open connection, and it means the
/// runner in a session can never be an older version of itself than the CLI
/// driving it -- including in a session created before the CLI was upgraded.
fn deliver_runner(ssh: &Ssh) -> Result<()> {
    ssh.put_executable(RUNNER.as_bytes(), RUNNER_PATH)?;
    Ok(())
}

/// Ask the runner where this project's tree and results live.
///
/// The CLI deliberately does not compute these. Pool layout belongs to the
/// runner, and a second opinion here would be a second thing to keep in step.
fn workspace(ssh: &Ssh, project: &str) -> Result<(String, String)> {
    let reply = ssh.run(
        &format!("{RUNNER_PATH} workspace --project {project}"),
        "making the workspace",
    )?;

    let mut work = None;
    let mut results = None;
    for line in reply.lines() {
        if let Some(v) = line.strip_prefix("work=") {
            work = Some(v.trim().to_string());
        }
        if let Some(v) = line.strip_prefix("out=") {
            results = Some(v.trim().to_string());
        }
    }
    match (work, results) {
        (Some(w), Some(o)) => Ok((w, o)),
        _ => Err(format!(
            "the runner did not say where the workspace is; it replied: {reply:?}"
        )
        .into()),
    }
}

/// Deliver the runner and build the session's storage.
fn prepare(session: &str, ssh: &Ssh) -> Result<()> {
    println!("{session}: preparing storage on {}", ssh.describe());
    deliver_runner(ssh)?;
    ssh.run(&format!("{RUNNER_PATH} firstboot"), "firstboot")?;
    Ok(())
}

fn start_heartbeat(session: &str) -> Result<Option<u32>> {
    let exe = std::env::current_exe()?;
    let log_path = Store::open()
        .path()
        .with_file_name(format!("heartbeat-{session}.log"));
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    let mut cmd = std::process::Command::new(exe);
    cmd.args(["heartbeat", "--session", session])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);

    // Detach into its own session, so it outlives the terminal that started
    // it. Without this the heartbeat stays in the parent's process group and
    // dies when that group is signalled -- closing the shell after `up` would
    // silently stop renewing, and the machine would be collected hours later
    // while someone was still working on it. The dead-man's switch is meant to
    // fire when the operator vanishes, not when their terminal does.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            // Safety: setsid in the child between fork and exec is
            // async-signal-safe. It fails only if we are already a group
            // leader, which a freshly forked child is not.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn()?;

    Ok(Some(child.id()))
}

pub fn list() -> Result<()> {
    let store = Store::open();
    let sessions = store.list()?;

    if sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }

    let now = SystemTime::now();
    println!(
        "{:<24} {:<16} {:<8} {:<10} {:<16} {}",
        "SESSION", "GUEST", "AGE", "EXPIRES", "ADDRESS", "HEARTBEAT"
    );
    for s in sessions {
        let expires = match s.remaining(now) {
            Some(d) => duration::format_rough(d),
            // Not a cosmetic state: the sweeper may take this machine at any
            // moment, and saying "expired" is the only honest thing to show.
            None => "EXPIRED".to_string(),
        };
        let heartbeat = match s.heartbeat_pid {
            Some(pid) if proc::is_alive(pid) => format!("{pid}"),
            // A dead heartbeat means the expiry has stopped moving. Nothing is
            // leaked -- that is what the tag is for -- but the session is now
            // on a countdown nobody is winding.
            Some(pid) => format!("{pid} DEAD"),
            None => "none".to_string(),
        };
        println!(
            "{:<24} {:<16} {:<8} {:<10} {:<16} {}",
            s.name,
            s.guest,
            duration::format_rough(s.age(now)),
            expires,
            s.address.map(|a| a.to_string()).unwrap_or_else(|| "-".into()),
            heartbeat
        );
    }
    Ok(())
}

/// The sessions a bare `renew`/`down` should act on: this project's, if we are
/// standing in one, and otherwise nothing without an explicit name.
/// The project a verb was pointed at, if it was pointed at one.
///
/// Every verb that acts on a project's sessions takes `--manifest`, and they
/// all have to agree about what it means -- otherwise it selects the work for
/// one verb and is silently rejected by the next, which is how a `down` came to
/// do nothing at all while reporting no error worth noticing.
fn project_of(manifest_path: Option<PathBuf>) -> Result<Option<String>> {
    match manifest_path {
        Some(p) => Ok(Some(load_manifest(Some(p))?.project)),
        None => Ok(None),
    }
}

fn implied_sessions(
    store: &Store,
    explicit: Option<String>,
    project: Option<&str>,
) -> Result<Vec<Session>> {
    if let Some(name) = explicit {
        let s = store
            .get(&name)?
            .ok_or_else(|| format!("no session named {name:?}; try `reaper list`"))?;
        return Ok(vec![s]);
    }

    // The project is passed in by any verb that has already read a manifest, so
    // that `--manifest` means the same thing everywhere. It used to be read
    // from `.reaper.yaml` here regardless, which made `--manifest` decide what
    // to run while the current directory decided where to run it -- and pointed
    // at a project with no sessions, `reaper sync --manifest other.yaml` failed
    // saying there were no sessions for a project it had not been asked about.
    let project = match project {
        Some(p) => p.to_string(),
        None => {
            let here = Path::new(".reaper.yaml");
            if !here.exists() {
                return Err(
                    "not inside a project, so there is nothing implied. Name a session, or run \
                     this where a .reaper.yaml is"
                        .into(),
                );
            }
            reaper_manifest::load(here)?.project
        }
    };
    let mine: Vec<Session> = store
        .list()?
        .into_iter()
        .filter(|s| s.project == project)
        .collect();

    if mine.is_empty() {
        return Err(format!("no sessions for {project:?}; try `reaper list`").into());
    }
    Ok(mine)
}

pub fn renew(
    session: Option<String>,
    ttl: Option<String>,
    manifest_path: Option<PathBuf>,
) -> Result<()> {
    let cfg = load_config()?;
    let store = Store::open();
    let provider = provider_for(&cfg)?;
    let project = project_of(manifest_path)?;

    for s in implied_sessions(&store, session, project.as_deref())? {
        let ttl = match &ttl {
            Some(t) => duration::parse(t)?,
            None => s.ttl,
        };
        let expires_at = SystemTime::now() + ttl;
        provider.set_expiry(&s.machine, expires_at)?;
        store.update(&s.name, |st| {
            st.expires_at = expires_at;
            st.ttl = ttl;
        })?;
        println!("{}: expires in {}", s.name, duration::format(ttl));
    }
    Ok(())
}

pub fn down(session: Option<String>, all: bool, manifest_path: Option<PathBuf>) -> Result<()> {
    let cfg = load_config()?;
    let store = Store::open();
    let provider = provider_for(&cfg)?;

    let targets = if all {
        store.list()?
    } else {
        let project = project_of(manifest_path)?;
        implied_sessions(&store, session, project.as_deref())?
    };

    if targets.is_empty() {
        println!("no sessions");
        return Ok(());
    }

    let mut failures = 0;
    for s in targets {
        // Results before anything else, and before the heartbeat stops: a
        // collection of a large artifact takes time, and the expiry should keep
        // moving while it does.
        collect_last_results(&cfg, &s);

        // Heartbeat next. A renewal landing between the destroy and the
        // forget would be harmless but confusing in the logs, and there is no
        // reason to leave the process running once its session is going.
        if let Some(pid) = s.heartbeat_pid {
            if !proc::stop(pid) {
                eprintln!("{}: heartbeat {pid} would not stop", s.name);
            }
        }

        match provider.destroy(&s.machine) {
            Ok(()) => {
                store.remove(&s.name)?;
                println!("{}: destroyed", s.name);
            }
            // Already gone. The usual reason is the happy one: the session
            // outlived its expiry and the sweeper did exactly its job. Treating
            // that as a failure would leave the operator with a session they
            // cannot get rid of, so destroy is idempotent.
            Err(reaper_core::ProviderError::NotFound(_)) => {
                store.remove(&s.name)?;
                println!("{}: already gone; forgotten", s.name);
            }
            Err(e) => {
                failures += 1;
                // The session stays in the store deliberately: forgetting it
                // here would hide a machine that still exists. It carries an
                // expiry, so the sweeper is the backstop either way.
                eprintln!(
                    "{}: could not destroy {}: {e}. The session is kept so it \
                     stays visible; its expiry means it will be collected regardless",
                    s.name, s.machine
                );
            }
        }
    }

    if failures > 0 {
        return Err(format!("{failures} session(s) could not be destroyed").into());
    }
    Ok(())
}

/// Renew one session's expiry until the session goes away.
///
/// This is the live half of the dead-man's switch. If it stops -- crash, closed
/// laptop, killed terminal -- the expiry stops moving and the sweeper collects
/// the machine. That is the intended behaviour, not a failure mode.
pub fn heartbeat(name: &str) -> Result<()> {
    let cfg = load_config()?;
    let store = Store::open();
    let provider = provider_for(&cfg)?;
    let interval = cfg.session.heartbeat_interval;

    loop {
        let Some(session) = store.get(name)? else {
            // `down` removed it. Nothing to renew and nothing to report.
            return Ok(());
        };

        let expires_at = SystemTime::now() + session.ttl;
        match provider.set_expiry(&session.machine, expires_at) {
            Ok(()) => {
                store.update(name, |s| s.expires_at = expires_at)?;
            }
            Err(e) => {
                // Keep going. A single failed renewal is survivable precisely
                // because the interval fits several times into the TTL -- that
                // margin is why the configuration insists on it. Giving up here
                // would convert a blip into a destroyed session.
                eprintln!("{name}: renewal failed: {e}");
            }
        }

        std::thread::sleep(interval);
    }
}

// ---------------------------------------------------------------------------
// Getting work in, and results back out
// ---------------------------------------------------------------------------

/// Every image this guest declared, in one list and without duplicates.
///
/// Pre-pulling covers the toolchain as well as the tenant's own stack: both are
/// declared, both are needed, and a digest already in the store costs nothing
/// to ask for again.
fn declared_images(g: &reaper_manifest::Guest) -> Vec<String> {
    let mut all: Vec<String> = g
        .build
        .as_ref()
        .and_then(|b| b.image.clone())
        .into_iter()
        .chain(g.run.image.clone())
        .chain(g.run.images.iter().cloned())
        .collect();
    all.sort();
    all.dedup();
    all
}

/// Fetch what the manifest declared, and do not let a failure cost a session.
///
/// A pre-pull is an optimisation -- the engine would fetch on demand anyway --
/// so a registry outage should cost a slow first build and never a machine that
/// took nine minutes to clone.
fn prepull(ssh: &Ssh, session: &str, images: &[String]) {
    if images.is_empty() {
        return;
    }
    println!("{session}: fetching {} declared image(s)", images.len());
    let command = format!("{RUNNER_PATH} pull {}", images.join(" "));
    if let Err(e) = ssh.run_live(&command, "pre-pulling images") {
        eprintln!(
            "{session}: could not pre-fetch images: {e}. The session is up and \
             usable; the first build will fetch them itself."
        );
    }
}

/// The tree that belongs to a session, if we are standing in it.
///
/// `down` may be run from anywhere, and results have nowhere to land unless
/// the project they belong to is here. Saying so beats writing them somewhere
/// arbitrary.
fn tree_for(s: &Session) -> Option<PathBuf> {
    let here = Path::new(".reaper.yaml");
    let project = reaper_manifest::load(here).ok()?.project;
    (project == s.project).then(|| PathBuf::from("."))
}

/// One reverse-sync, built ready to run repeatedly.
fn results_plan(
    cfg: &Config,
    ssh: &Ssh,
    rsh: &Path,
    remote_results: &str,
    tree: &Path,
) -> Result<sync::Plan> {
    let local = tree.join(sync::RESULTS);
    std::fs::create_dir_all(&local)?;
    Ok(sync::pull(
        &cfg.session.rsync_command,
        rsh,
        ssh,
        remote_results,
        &local,
    ))
}

pub fn sync(session: Option<String>, manifest_path: Option<PathBuf>) -> Result<()> {
    let cfg = load_config()?;
    let (manifest, tree) = load_manifest_at(manifest_path)?;
    let store = Store::open();

    for s in implied_sessions(&store, session, Some(&manifest.project))? {
        let ssh = ssh_for(&cfg, &s)?;
        deliver_runner(&ssh)?;
        let (work, results) = workspace(&ssh, &manifest.project)?;
        let rsh = sync::rsh_wrapper(&ssh, &state_file(&s.name, "rsh")?)?;

        println!("{}: {} -> {}", s.name, tree.display(), ssh.describe());
        sync::push(
            &cfg.session.rsync_command,
            &rsh,
            &ssh,
            &tree,
            &work,
            &manifest.sync_exclude,
        )
        .run()?;
        store.update(&s.name, |st| st.synced_at = Some(SystemTime::now()))?;

        // And straight back, so a session that already holds results hands them
        // over on the first sync rather than waiting for a run.
        results_plan(&cfg, &ssh, &rsh, &results, &tree)?.run()?;
        println!("{}: synced", s.name);
    }
    Ok(())
}


/// Take `@pristine` after a run, if the tenant wants resets and has none yet.
///
/// Be exact about what this captures: the state *after* the whole run, test
/// residue included -- not the state right after the stack came up, which is
/// what a runner would take if it could tell the difference. It cannot: a
/// tenant's command is opaque, and "the stack is up now" is that tenant's
/// vocabulary. A project that wants a tighter point calls `reaper snapshot` at
/// the moment it chooses.
///
/// Reset is deterministic either way. Every reset returns to the same place; it
/// is just a slightly later place than the plan first imagined.
fn take_pristine(ssh: &Ssh, session: &str, manifest: &Manifest) {
    if manifest.reset.is_empty() {
        return;
    }
    for d in &manifest.reset {
        match ssh.run(
            &format!("{RUNNER_PATH} snapshot --dataset {d} --name {PRISTINE} --if-absent"),
            "taking the pristine snapshot",
        ) {
            // The runner says so only when it actually took one, so a second
            // run does not repeat the explanation.
            Ok(out) if out.contains("snapshot=") => println!(
                "{session}: {d} snapshotted as {PRISTINE} -- as it stands now, after this run. \
                 `reaper snapshot` names an earlier point if you want one"
            ),
            Ok(_) => {}
            // Not fatal. The run succeeded, and that is what was asked for.
            Err(e) => eprintln!("{session}: could not take {PRISTINE}: {e}"),
        }
    }
}

/// Start the in-guest reset trigger, idempotently.
///
/// Started with the session and again before anything that might need it, for
/// the same reason the runner is re-delivered: it is one cheap call over an
/// open connection, and a session whose loop died for any reason repairs
/// itself rather than failing a tenant's reset much later.
fn start_control(ssh: &Ssh, project: &str) -> Result<()> {
    ssh.run(
        &format!("{RUNNER_PATH} control --project {project} start"),
        "starting the reset trigger",
    )?;
    Ok(())
}

/// The datasets a manifest asks to be able to roll back.
fn reset_datasets(manifest: &Manifest) -> &[String] {
    &manifest.reset
}

pub fn snapshot(
    name: String,
    session: Option<String>,
    manifest_path: Option<PathBuf>,
) -> Result<()> {
    let cfg = load_config()?;
    let (manifest, _) = load_manifest_at(manifest_path)?;
    let store = Store::open();

    let datasets = reset_datasets(&manifest);
    if datasets.is_empty() {
        return Err(format!(
            "{} declares no reset datasets, so there is no state to name a point in. \
             Add `reset: {{ datasets: [state] }}` to the manifest",
            manifest.project
        )
        .into());
    }

    for s in implied_sessions(&store, session, Some(&manifest.project))? {
        let ssh = ssh_for(&cfg, &s)?;
        deliver_runner(&ssh)?;
        for d in datasets {
            ssh.run(
                &format!("{RUNNER_PATH} snapshot --dataset {d} --name {name}"),
                "taking a snapshot",
            )?;
        }
        println!("{}: {} snapshotted as {name}", s.name, datasets.join(", "));
    }
    Ok(())
}

pub fn reset(
    to: Option<String>,
    session: Option<String>,
    manifest_path: Option<PathBuf>,
) -> Result<()> {
    let cfg = load_config()?;
    let (manifest, _) = load_manifest_at(manifest_path)?;
    let store = Store::open();

    let datasets = reset_datasets(&manifest);
    if datasets.is_empty() {
        return Err(format!(
            "{} declares no reset datasets, so there is nothing to roll back. \
             Add `reset: {{ datasets: [state] }}` to the manifest",
            manifest.project
        )
        .into());
    }
    let name = to.unwrap_or_else(|| PRISTINE.to_string());

    for s in implied_sessions(&store, session, Some(&manifest.project))? {
        let ssh = ssh_for(&cfg, &s)?;
        deliver_runner(&ssh)?;

        let started = SystemTime::now();
        for d in datasets {
            // Output goes straight through: the runner announces what it stops
            // and what it rolls back, and that is exactly what somebody
            // watching a reset wants to see.
            ssh.run_live(
                &format!("{RUNNER_PATH} rollback --dataset {d} --name {name}"),
                "rolling back",
            )?;
        }
        println!(
            "{}: rolled back to {name} in {}",
            s.name,
            duration::format_rough(started.elapsed().unwrap_or_default())
        );
    }
    Ok(())
}

/// Which of a guest's two commands to run. They differ in what they execute
/// and in nothing else, which is why they share a path.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Build,
    Run,
}

impl Verb {
    fn label(self) -> &'static str {
        match self {
            Verb::Build => "build",
            Verb::Run => "run",
        }
    }
}

pub fn exec(
    which: Verb,
    session: Option<String>,
    profile: Option<String>,
    manifest_path: Option<PathBuf>,
) -> Result<()> {
    let cfg = load_config()?;
    let (manifest, tree) = load_manifest_at(manifest_path)?;
    let store = Store::open();

    let profile = match &profile {
        Some(name) => Some(manifest.profiles.get(name).ok_or_else(|| {
            let known: Vec<&str> = manifest.profiles.keys().map(String::as_str).collect();
            format!(
                "the manifest has no profile {name:?}; it defines: {}",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            )
        })?),
        None => None,
    };

    for s in implied_sessions(&store, session, Some(&manifest.project))? {
        let g = manifest.guest(&s.guest).ok_or_else(|| {
            format!(
                "this session runs on {:?}, and the manifest no longer names that \
                 guest. Take the session down, or put the guest back",
                s.guest
            )
        })?;

        let (cmd, verb_env, image, mode) = match which {
            Verb::Build => {
                let b = g.build.as_ref().ok_or_else(|| {
                    format!(
                        "{} declares no build for {}, so there is nothing to build. \
                         A project whose test command needs no build step is ordinary; \
                         run it instead",
                        manifest.project, g.name
                    )
                })?;
                (&b.cmd, &b.env, b.image.as_ref(), b.exec)
            }
            Verb::Run => (&g.run.cmd, &g.run.env, g.run.image.as_ref(), g.run.exec),
        };

        // The schema guarantees these agree, having checked the resolved form.
        // Asserting it here costs a line and turns an impossible state into a
        // sentence rather than into an argument the runner would refuse.
        let image = match mode {
            Exec::Container => Some(image.ok_or_else(|| {
                format!("{}: container execution with no image", g.name)
            })?),
            Exec::Host => None,
        };

        // A cold profile still names every cache; what changes is that the
        // runner gives it an empty one. That is the whole of determinism mode:
        // if a run passes here, a warm cache was not the reason. Dropping the
        // names instead -- which this did first -- broke every command that
        // referred to a cache path, which is the documented way to use one.
        let warm = profile.and_then(|p| p.warm_cache) != Some(false);
        let caches: Vec<String> =
            g.build.as_ref().map(|b| b.cache.clone()).unwrap_or_default();

        let env: BTreeMap<String, String> = job::overlay(verb_env, profile.map(|p| &p.env));
        let script = job::render(cmd, &env);

        let ssh = ssh_for(&cfg, &s)?;
        deliver_runner(&ssh)?;
        let (_, results) = workspace(&ssh, &manifest.project)?;
        // Over stdin, as a file. Never as a quoted argument: a command
        // containing an apostrophe passed inline closes the quote, and the rest
        // of it then runs somewhere nobody intended.
        ssh.put_executable(script.as_bytes(), JOB_PATH)?;

        let mut command = format!(
            "{RUNNER_PATH} exec --project {} --job {JOB_PATH}",
            manifest.project
        );
        if let Some(i) = image {
            command.push_str(&format!(" --image {i}"));
        }
        for c in &caches {
            command.push_str(&format!(" --cache {c}"));
        }
        if !warm {
            command.push_str(" --cold");
        }

        let rsh = sync::rsh_wrapper(&ssh, &state_file(&s.name, "rsh")?)?;
        let plan = results_plan(&cfg, &ssh, &rsh, &results, &tree)?;

        println!(
            "{}: {} on {}{}",
            s.name,
            which.label(),
            g.name,
            if warm { "" } else { " (cold)" }
        );

        // Results flow while the command runs, not after it. A failure trace
        // must never exist only on a machine scheduled for destruction, and the
        // interesting failures are the ones that end with the operator giving
        // up and running `down`.
        let stop = Arc::new(AtomicBool::new(false));
        let collector = {
            let plan = plan.clone();
            let stop = Arc::clone(&stop);
            let interval = cfg.session.results_interval;
            let name = s.name.clone();
            std::thread::spawn(move || {
                let mut complained = false;
                while !stop.load(Ordering::Relaxed) {
                    // Slept in slices so that stopping is prompt: the interval
                    // is minutes-scale in some configurations, and a command
                    // that finished should not wait one out.
                    let slice = Duration::from_millis(200);
                    let mut waited = Duration::ZERO;
                    while waited < interval && !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(slice);
                        waited += slice;
                    }
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Err(e) = plan.run() {
                        // Once. A broken channel would otherwise print on every
                        // tick and bury the output of the command itself.
                        if !complained {
                            eprintln!("{name}: results are not coming back: {e}");
                            complained = true;
                        }
                    }
                }
            })
        };

        let outcome = ssh.run_live(&command, which.label());
        stop.store(true, Ordering::Relaxed);
        let _ = collector.join();

        // The last pass happens whatever the command did, and before its
        // failure is reported: a failed run is exactly when its trace matters.
        let collected = plan.run();

        match (outcome, collected) {
            (Err(e), pulled) => {
                if let Err(p) = pulled {
                    eprintln!("{}: results could not be collected: {p}", s.name);
                }
                return Err(e.into());
            }
            (Ok(()), Err(p)) => return Err(p.into()),
            (Ok(()), Ok(_)) => {
                println!("{}: {} finished", s.name, which.label());
                if which == Verb::Run {
                    take_pristine(&ssh, &s.name, &manifest);
                }
            }
        }
    }
    Ok(())
}

/// One last reverse-sync before a machine is destroyed.
///
/// Best-effort throughout, and never allowed to stop a destroy: a machine that
/// cannot be reached is exactly the machine most in need of being taken down,
/// and refusing to remove it because its results could not be fetched would
/// leave the operator with a session they cannot get rid of.
fn collect_last_results(cfg: &Config, s: &Session) {
    // Nothing was ever pushed, so there is no workspace to read and nothing
    // could have been written. Attempting it would fail on a directory that was
    // never made, and read as though results had been lost.
    if s.synced_at.is_none() {
        return;
    }

    let Some(tree) = tree_for(s) else {
        eprintln!(
            "{}: not standing in {}, so its results have nowhere to land. \
             Run `reaper sync` from the project before taking it down if you want them",
            s.name, s.project
        );
        return;
    };

    let attempt = || -> Result<()> {
        let ssh = ssh_for(cfg, s)?;
        deliver_runner(&ssh)?;
        let (_, results) = workspace(&ssh, &s.project)?;
        let rsh = sync::rsh_wrapper(&ssh, &state_file(&s.name, "rsh")?)?;
        results_plan(cfg, &ssh, &rsh, &results, &tree)?.run()?;
        Ok(())
    };

    match attempt() {
        Ok(()) => println!("{}: results collected", s.name),
        Err(e) => eprintln!(
            "{}: could not collect results before destroying it: {e}",
            s.name
        ),
    }
}
