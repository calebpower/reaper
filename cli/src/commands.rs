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
    let path = explicit.clone().unwrap_or_else(|| PathBuf::from(".reaper.toml"));
    let root = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    Ok((load_manifest(explicit)?, root))
}

fn load_manifest(explicit: Option<PathBuf>) -> Result<Manifest> {
    let path = explicit.unwrap_or_else(|| PathBuf::from(".reaper.toml"));
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
        if let Some(complaint) = unregistered_guest(&cfg, &g.name) {
            return Err(complaint.into());
        }
    }

    // Cheap checks before expensive spending: a machine created ahead of
    // these failing is a machine wasted -- unreachable forever without the
    // key, or unfindable later without a writable record.
    store.probe_writable()?;
    if let Some(complaint) = missing_ssh_key(&cfg) {
        return Err(complaint.into());
    }

    let ttl = ttl_for(&cfg, &manifest, profile.as_deref(), ttl.as_deref())?;
    // From the manifest, not the selection: `up --guest a` in a two-guest
    // manifest must produce the same name a plain `up` would, or the two
    // spellings create sessions that cannot see each other.
    let single = manifest.guests.len() == 1;
    let provider = provider_for(&cfg)?;

    for g in wanted {
        let name = session_name(&manifest.project, &g.name, single);

        if let Some(existing) = store.get(&name)? {
            // Whose record is this? Session names are {project}-{guest} (or
            // the bare project), so distinct projects can mint the same name
            // -- "a" with guest "b-guest" and a project named "a-b-guest".
            // Reusing across that boundary would hand this project another
            // one's machine, and every verb after would push trees and take
            // snapshots across it.
            if existing.project != manifest.project {
                return Err(format!(
                    "{name}: that session name is taken by project {:?}, whose \
                     naming collides with {:?} here. Rename one of the two \
                     projects, or take the other session down first",
                    existing.project, manifest.project
                )
                .into());
            }
            // A record with no address is an `up` that never finished; there
            // is nothing here to reuse and no way to resume it. Judged before
            // the cluster is consulted: it is about the record's own shape.
            let Some(address) = existing.address else {
                return Err(format!(
                    "{name}: a session by this name exists but never became ready, so \
                     there is nothing to reuse. `reaper down {name}` clears it"
                )
                .into());
            };
            // The record says up; the cluster may know better. Reusing a
            // machine the sweeper has taken hands back a session every verb
            // fails against, with a heartbeat that ends seconds later.
            let live_machines = provider.list()?;
            if !live_machines.iter().any(|m| m.machine == existing.machine) {
                return Err(format!(
                    "{name}: its machine is gone (the sweeper collects anything past its expiry). `reaper down {name}` clears the record, and the next `reaper up` starts fresh"
                )
                .into());
            }
            // Reusing a session whose heartbeat died (a reboot, a killed
            // terminal) would hand the operator a machine on a fixed
            // countdown; restart the renewal before calling it up.
            if !existing.heartbeat_pid.map(proc::is_alive).unwrap_or(false) {
                let pid = start_heartbeat(&name)?;
                store.update(&name, |st| st.heartbeat_pid = pid)?;
                match pid {
                    Some(pid) => println!(
                        "{name}: its heartbeat was dead; restarted as pid {pid}, which runs \
                         until `reaper down`"
                    ),
                    None => println!("{name}: its heartbeat was dead; restarted"),
                }
            }
            println!(
                "{name}: already up on {address} since {} ago -- reusing it",
                duration::format_rough(existing.age(SystemTime::now()))
            );
            continue;
        }

        // Counted from the provider, not from this workstation's session file.
        // The resources a cap protects -- identifiers and storage -- are the
        // cluster's, and a limit that only sees your own sessions is no limit
        // at all the moment a second person shares the hardware.
        let live = provider.list()?;
        if live.len() >= cfg.session.max_concurrent {
            let mine: Vec<String> = store.list()?.into_iter().map(|s| s.name).collect();
            let others = live.len().saturating_sub(mine.len());
            return Err(format!(
                "{} session(s) are already up on this provider and it allows {}{}. \
                 Take one down, or raise session.max_concurrent in {}",
                live.len(),
                cfg.session.max_concurrent,
                if others > 0 {
                    format!(" -- {others} of them not yours")
                } else {
                    String::new()
                },
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

        match wait_until_reachable(provider.as_ref(), &machine, &cfg, &name, created_at)? {
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

                // The ready session is recorded before the heartbeat spawns:
                // a failed spawn used to abort `up` after the machine had its
                // full TTL, leaving a store record that still said "no
                // address, expiring on the grace". The machine is fine either
                // way -- it just will not renew -- so say that instead.
                session.address = Some(address);
                session.ready_at = Some(ready_at);
                session.expires_at = ready_at + ttl;
                store.put(session)?;
                let mut renewer = None;
                match start_heartbeat(&name) {
                    Ok(pid) => {
                        renewer = pid;
                        store.update(&name, |st| st.heartbeat_pid = pid)?;
                    }
                    Err(e) => eprintln!(
                        "{name}: could not start the renewal heartbeat: {e}. The session works but its expiry will not move; `reaper renew` extends it by hand"
                    ),
                }

                println!(
                    "{name}: up at {address}, expires in {}",
                    duration::format(ttl)
                );
                // Named, because `up` leaves a second reaper process behind
                // and nothing used to say so. It is detached into its own
                // session so it survives the terminal, which means `ps` after
                // `up` returns shows a live `reaper` -- and reading that as an
                // `up` that never exited costs an afternoon, then costs the
                // session too when the wrong process is killed.
                if let Some(pid) = renewer {
                    println!(
                        "{name}: renewing in the background as pid {pid}. That process runs \
                         until `reaper down`; `up` itself is finished here"
                    );
                }
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
    created_at: SystemTime,
) -> Result<Option<(std::net::IpAddr, Ssh)>> {
    // Cleared once, here. A session starts with no history, so it cannot
    // inherit a stale key from an address that has been recycled -- and the
    // loop below may legitimately try more than one address for one machine.
    let _ = std::fs::remove_file(state_file(session, "known-hosts")?);

    // From creation, not from now: the machine's grace expiry was stamped at
    // creation, and a clone that took six minutes must not leave this loop
    // polling a machine the sweeper is already entitled to collect.
    let deadline = created_at + cfg.session.ready_grace;
    let mut said: Option<String> = None;

    loop {
        let address = match provider.address(machine) {
            Ok(a) => a,
            // Gone mid-wait means collected: the grace expired while we were
            // still polling. Saying so beats surfacing a raw NotFound.
            Err(reaper_core::ProviderError::NotFound(_)) => {
                return Err(format!(
                    "{session}: the machine was destroyed while waiting for it \
                     to answer. The usual reason is the readiness grace ({}) \
                     running out and the sweeper collecting it -- a slow clone \
                     eats most of the grace, and session.ready_grace in the \
                     site config is the knob -- but anything with access may \
                     have destroyed it",
                    duration::format(cfg.session.ready_grace)
                )
                .into());
            }
            Err(e) => return Err(e.into()),
        };
        if let Some(address) = address {
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
/// The sentence for a guest the site does not register, or None if it does.
/// Shared by `up` (a refusal) and `doctor` (a finding) so the two can never
/// drift into disagreeing about what registered means.
fn unregistered_guest(cfg: &Config, guest: &str) -> Option<String> {
    if cfg.template_for(guest).is_some() {
        return None;
    }
    Some(format!(
        "no guest named {guest:?} is registered here; this site offers: {}. \
         Registering one is a template build and an entry in {} -- see docs/guests.md",
        cfg.guest_names().join(", "),
        cfg.path.display()
    ))
}

/// The sentence for a configured ssh key that does not exist, or None.
/// Shared by `up` and `doctor` for the same no-drift reason.
fn missing_ssh_key(cfg: &Config) -> Option<String> {
    let key = cfg.session.ssh_key.as_ref()?;
    if key.exists() {
        return None;
    }
    Some(format!(
        "session.ssh_key {} does not exist, so no session could ever be reached. \
         Fix the path in {} before creating machines",
        key.display(),
        cfg.path.display()
    ))
}

/// Remove what a session accumulated on the workstation, when the session
/// itself is forgotten: its known-hosts file, its rsh wrapper, its heartbeat
/// log. Tied to the forgetting, not the destroying -- a kept session keeps
/// its files, because the rsh wrapper is how the next attempt reaches it.
/// Best-effort: a file that will not delete is not worth failing a down over.
fn forget_workstation_files(session: &str) {
    let store = Store::open();
    for name in [
        format!("known-hosts-{session}"),
        format!("rsh-{session}"),
        format!("heartbeat-{session}.log"),
    ] {
        let _ = std::fs::remove_file(store.path().with_file_name(name));
    }
}

fn state_file(session: &str, prefix: &str) -> Result<PathBuf> {
    let path = Store::open()
        .path()
        .with_file_name(format!("{prefix}-{session}"));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    Ok(path)
}

/// Why a verb against this session probably failed, when the session is past
/// its expiry.
///
/// An expired session keeps its record and its address, and the machine behind
/// it may already have been collected. Every verb then fails the same way --
/// `ssh: connect to host <addr> port 22: Operation timed out`, minutes later,
/// naming an address that stopped meaning anything hours ago. The record knows
/// better than the transport does, so it says so.
fn expiry_note(s: &Session) -> Option<String> {
    if s.remaining(SystemTime::now()).is_some() {
        return None;
    }
    Some(format!(
        "{}: this session expired {} ago, so the machine has very likely been collected \
         and its address means nothing now. `reaper down {}` clears the record and \
         `reaper up` starts fresh",
        s.name,
        duration::format_rough(
            SystemTime::now()
                .duration_since(s.expires_at)
                .unwrap_or_default()
        ),
        s.name
    ))
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
            // The same judgement `down` uses before signalling: alive AND
            // verifiably ours. An alive pid the OS has recycled for a
            // stranger is a dead heartbeat wearing a number, and this column
            // is the dead-man's-switch indicator -- it must not vouch for it.
            Some(pid) if proc::looks_like_heartbeat(pid) == Some(true) => format!("{pid}"),
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
        // A verb that knows which project it is acting for must not cross to
        // another project's machine on the strength of a typo: pushing tree A
        // into session B's workspace, or running A's command there, poisons B.
        // Verbs with no project in hand (`down foo` from anywhere) still work.
        if let Some(p) = project {
            if s.project != p {
                return Err(format!(
                    "session {name:?} belongs to {:?}, but this command is acting for {p:?}. Run it from that project (or with its --manifest), or name one of this project's sessions",
                    s.project
                )
                .into());
            }
        }
        return Ok(vec![s]);
    }

    // The project is passed in by any verb that has already read a manifest, so
    // that `--manifest` means the same thing everywhere. It used to be read
    // from `.reaper.toml` here regardless, which made `--manifest` decide what
    // to run while the current directory decided where to run it -- and pointed
    // at a project with no sessions, `reaper sync --manifest other.toml` failed
    // saying there were no sessions for a project it had not been asked about.
    let project = match project {
        Some(p) => p.to_string(),
        None => {
            let here = Path::new(".reaper.toml");
            if !here.exists() {
                return Err(
                    "not inside a project, so there is nothing implied. Name a session, or run \
                     this where a .reaper.toml is"
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
        // Naming `up` rather than only `list`. "The whole loop: sync, build,
        // reset, run" reads as though it includes bringing a machine up, and
        // `reaper test` is the first thing a new tenant runs -- so the answer
        // to "there are none" should be the command that makes one, not just
        // the one that confirms there are none.
        return Err(format!(
            "no sessions for {project:?}. `reaper up` creates one (it is not part of              `reaper test`, which needs a session to already be there); `reaper list`              shows what is running"
        )
        .into());
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

    // Per session, to the end: one machine the sweeper took must not leave
    // its siblings unrenewed -- the surviving sessions are exactly the ones
    // that still need their expiry moved.
    let mut failures = 0;
    for s in implied_sessions(&store, session, project.as_deref())? {
        let ttl = match &ttl {
            Some(t) => duration::parse(t)?,
            None => s.ttl,
        };
        let expires_at = SystemTime::now() + ttl;
        match provider.set_expiry(&s.machine, expires_at) {
            Ok(()) => {
                store.update(&s.name, |st| {
                    st.expires_at = expires_at;
                    st.ttl = ttl;
                })?;
                println!("{}: expires in {}", s.name, duration::format(ttl));
            }
            Err(reaper_core::ProviderError::NotFound(_)) => {
                failures += 1;
                eprintln!(
                    "{}: its machine is gone (the sweeper collects anything past its expiry); there is nothing left to renew. `reaper down {}` clears the record",
                    s.name, s.name
                );
            }
            Err(e) => {
                failures += 1;
                eprintln!("{}: could not renew: {e}", s.name);
            }
        }
    }
    if failures > 0 {
        return Err(format!("{failures} session(s) could not be renewed").into());
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
        let project = project_of(manifest_path.clone())?;
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
        collect_last_results(&cfg, &s, manifest_path.as_deref());

        // Heartbeat next. A renewal landing between the destroy and the
        // forget would be harmless but confusing in the logs, and there is no
        // reason to leave the process running once its session is going.
        if let Some(pid) = s.heartbeat_pid {
            // The pid is verified before anything is signalled: it was
            // recorded by a different process, possibly days ago, and the OS
            // reuses identifiers. An unrecognized pid is left alone -- the
            // heartbeat exits by itself once the session leaves the store.
            match proc::looks_like_heartbeat(pid) {
                Some(true) => {
                    if !proc::stop(pid) {
                        eprintln!("{}: heartbeat {pid} would not stop", s.name);
                    }
                }
                Some(false) => eprintln!(
                    "{}: pid {pid} is no longer the heartbeat (reused after a reboot?); leaving it alone",
                    s.name
                ),
                None => {} // already gone
            }
        }

        match provider.destroy(&s.machine) {
            Ok(()) => {
                store.remove(&s.name)?;
                forget_workstation_files(&s.name);
                println!("{}: destroyed", s.name);
            }
            // Already gone. The usual reason is the happy one: the session
            // outlived its expiry and the sweeper did exactly its job. Treating
            // that as a failure would leave the operator with a session they
            // cannot get rid of, so destroy is idempotent.
            Err(reaper_core::ProviderError::NotFound(_)) => {
                store.remove(&s.name)?;
                forget_workstation_files(&s.name);
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
        // A transient store failure (somebody else holding the lock) must not
        // kill the renewal loop: nothing restarts it, so dying here converts
        // a blip into a machine collected while in use. Only the session
        // genuinely being gone ends the loop.
        let session = match store.get(name) {
            Ok(Some(s)) => s,
            // `down` removed it. Nothing to renew and nothing to report.
            Ok(None) => return Ok(()),
            Err(e) => {
                eprintln!("{name}: could not read the session store: {e}");
                std::thread::sleep(interval);
                continue;
            }
        };

        let expires_at = SystemTime::now() + session.ttl;
        match provider.set_expiry(&session.machine, expires_at) {
            Ok(()) => {
                // The machine is renewed even when the record cannot say so;
                // the record catches up on the next tick.
                if let Err(e) = store.update(name, |s| s.expires_at = expires_at) {
                    eprintln!("{name}: renewed, but could not record it: {e}");
                }
            }
            // Gone is not a blip: the cluster listing no longer shows the
            // machine, and nothing this loop does will bring it back. Looping
            // on warning every interval would be a leaked process wearing a
            // log line.
            Err(reaper_core::ProviderError::NotFound(_)) => {
                eprintln!(
                    "{name}: its machine is gone; nothing left to renew, so this heartbeat is ending. `reaper down {name}` clears the record"
                );
                return Ok(());
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
fn tree_for(s: &Session, manifest_path: Option<&Path>) -> Option<PathBuf> {
    // The explicit --manifest wins, exactly as it does for selecting the
    // sessions: a `down --manifest X` that selected X's sessions and then
    // "could not find" X's tree was collecting from half a contract.
    if let Some(p) = manifest_path {
        let m = reaper_manifest::load(p).ok()?;
        if m.project == s.project {
            let root = p
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            return Some(root);
        }
        return None;
    }
    let here = Path::new(".reaper.toml");
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

    // Per session, to the end: an unreachable machine costs itself, never
    // its siblings their copy of the tree.
    let mut failures = 0;
    for s in implied_sessions(&store, session, Some(&manifest.project))? {
        let attempt = || -> Result<()> {
            let ssh = ssh_for(&cfg, &s)?;
            deliver_runner(&ssh)?;
            let (work, results) = workspace(&ssh, &manifest.project)?;
            let rsh = sync::rsh_wrapper(&ssh, &state_file(&s.name, "rsh")?)?;

            println!("{}: {} -> {}", s.name, tree.display(), ssh.describe());
            let pushed = sync::push(
                &cfg.session.rsync_command,
                &rsh,
                &ssh,
                &tree,
                &work,
                &manifest.sync_exclude,
            )
            .run()?;
            // Said out loud, because the alternative is inferring it. A
            // baseline taken by stashing removes files, and if that removal
            // does not reach the guest the build there compiles code the
            // operator deleted -- a green baseline for the wrong reason, and
            // the one failure in this channel that looks like success.
            let removed = sync::deletions(&pushed);
            if removed > 0 {
                println!(
                    "{}: {removed} path(s) removed there to match the tree here",
                    s.name
                );
            }
            store.update(&s.name, |st| st.synced_at = Some(SystemTime::now()))?;

            // And straight back, so a session that already holds results hands
            // them over on the first sync rather than waiting for a run.
            results_plan(&cfg, &ssh, &rsh, &results, &tree)?.run()?;
            println!("{}: synced", s.name);
            Ok(())
        };
        if let Err(e) = attempt() {
            failures += 1;
            eprintln!("{}: could not sync: {e}", s.name);
            if let Some(note) = expiry_note(&s) {
                eprintln!("{note}");
            }
        }
    }
    if failures > 0 {
        return Err(format!("{failures} session(s) could not be synced").into());
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


/// The points a session's state can be rolled back to.
fn existing_snapshots(ssh: &Ssh, dataset: &str) -> Result<Vec<String>> {
    let out = ssh.run(
        &format!("{RUNNER_PATH} snapshots --dataset {dataset}"),
        "listing snapshots",
    )?;
    Ok(out.split_whitespace().map(str::to_string).collect())
}

/// The loop, as one verb: sync, build, reset, run.
///
/// Composition rather than new machinery -- each step is the same code path the
/// individual verb uses, so nothing behaves differently for having been called
/// from here. What this adds is the order, and the judgement about which steps
/// have anything to do.
pub fn test(
    to: Option<String>,
    session: Option<String>,
    profile: Option<String>,
    manifest_path: Option<PathBuf>,
) -> Result<()> {
    let (manifest, tree) = load_manifest_at(manifest_path.clone())?;

    // Which sessions this loop will act on, asked before anything is touched.
    // `sync` asks the same question a moment later and would refuse just as
    // clearly -- but by then the results directory has been emptied, and a
    // loop that could not start must not cost the last one its output.
    implied_sessions(&Store::open(), session.clone(), Some(&manifest.project))?;

    // Before anything else, and only here. `test` is the whole loop, so this
    // is exactly once per run -- whereas clearing inside `build` or `run`
    // would have the second step delete the first step's output. What it
    // prevents is the shape found while wiring a full-stack tier: `test`
    // failed in the build verb, never reached the run, pulled nothing back,
    // and left eight result files from a green run four hours earlier sitting
    // in `out/`. The exit status said failed and the artifacts said passed.
    match sync::clear_results(&tree) {
        Ok(0) => {}
        Ok(n) => println!(
            "{}: cleared {n} entr{} from {}/ -- this run's results, and only this run's, land there",
            manifest.project,
            if n == 1 { "y" } else { "ies" },
            sync::RESULTS
        ),
        // Not fatal. A results directory that will not clear is a permissions
        // problem worth saying out loud, but refusing to run the tests over it
        // helps nobody -- and the operator now knows what `out/` holds.
        Err(e) => eprintln!(
            "{}: could not clear {}/ before this run ({e}), so it may still hold results from an earlier one",
            manifest.project,
            sync::RESULTS
        ),
    }

    println!("{}: sync", manifest.project);
    sync(session.clone(), manifest_path.clone())?;

    // A project with no build step is ordinary -- the smallest legal manifest
    // has none -- so this is a skip rather than a failure.
    let builds = manifest.guests.iter().any(|g| g.build.is_some());
    if builds {
        println!("{}: build", manifest.project);
        exec(
            Verb::Build,
            session.clone(),
            profile.clone(),
            manifest_path.clone(),
        )?;
    } else {
        println!("{}: no build declared; skipping", manifest.project);
    }

    reset_before_run(&manifest, to, session.clone(), manifest_path.clone())?;

    println!("{}: run", manifest.project);
    exec(Verb::Run, session, profile, manifest_path)?;
    Ok(())
}

/// Roll state back, but only when there is somewhere to roll back to.
///
/// On a session that has never had a successful run there is no `@pristine`,
/// and resetting would fail on the first pass of `test` for a reason that has
/// nothing to do with the project. `run` takes the snapshot at the end of that
/// first pass, so every later `test` gets the full four steps.
fn reset_before_run(
    manifest: &Manifest,
    to: Option<String>,
    session: Option<String>,
    manifest_path: Option<PathBuf>,
) -> Result<()> {
    let name = to.clone().unwrap_or_else(|| PRISTINE.to_string());
    if manifest.reset.is_empty() {
        println!("{}: no reset datasets declared; skipping", manifest.project);
        return Ok(());
    }

    let cfg = load_config()?;
    let store = Store::open();
    let sessions = implied_sessions(&store, session.clone(), Some(&manifest.project))?;

    // Asked of the session rather than assumed, because "has this project ever
    // completed a run here" is a fact about the machine and not about the
    // manifest. Per session, too: a fresh session skipping its first reset
    // must not cancel the rollback its older sibling was owed.
    for s in &sessions {
        let ssh = ssh_for(&cfg, s)?;
        deliver_runner(&ssh)?;
        let mut missing = false;
        for d in &manifest.reset {
            if !existing_snapshots(&ssh, d)?.iter().any(|n| n == &name) {
                // A point the tenant asked for by name and that does not exist
                // is a different thing from having no pristine yet: the first
                // is very likely a typo, and skipping it silently would run the
                // command against whatever state happened to be there.
                if to.is_some() {
                    return Err(format!(
                        "{}: there is no {name:?} to reset to. `reaper snapshot {name}` names one, or a run can name it for itself through $REAPER_CONTROL/snapshot",
                        s.name
                    )
                    .into());
                }
                println!(
                    "{}: nothing to reset to yet; this run will take {PRISTINE}",
                    s.name
                );
                missing = true;
                break;
            }
        }
        if missing {
            continue;
        }
        println!("{}: reset to {name}", s.name);
        reset(to.clone(), Some(s.name.clone()), manifest_path.clone())?;
    }
    Ok(())
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
             Add `[reset]` with `datasets = [\"state\"]` to the manifest",
            manifest.project
        )
        .into());
    }

    for s in implied_sessions(&store, session, Some(&manifest.project))? {
        let ssh = ssh_for(&cfg, &s)?;
        deliver_runner(&ssh)?;
        for d in datasets {
            // Quoted: the name is the one free-text argument on this line,
            // and the runner's own validation runs only after the remote
            // shell has already parsed the string.
            ssh.run(
                &format!("{RUNNER_PATH} snapshot --dataset {d} --name {}", job::quote(&name)),
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
             Add `[reset]` with `datasets = [\"state\"]` to the manifest",
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
                &format!("{RUNNER_PATH} rollback --dataset {d} --name {}", job::quote(&name)),
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

    // Refused up front, so the per-guest skip below can never add up to a
    // silent no-op: `reaper build` on a project with no build step anywhere
    // is a misunderstanding worth a sentence.
    if which == Verb::Build && manifest.guests.iter().all(|g| g.build.is_none()) {
        return Err(format!(
            "{} declares no build for any guest, so there is nothing to build. A project whose test command needs no build step is ordinary; run it instead",
            manifest.project
        )
        .into());
    }

    let mut failures = 0;
    for s in implied_sessions(&store, session, Some(&manifest.project))? {
        // Nothing has ever been pushed, so the workspace is empty; a build
        // has nothing to compile and a run's "success" would be meaningless
        // -- and on a project with reset datasets, that success would take
        // @pristine of unseeded state, poisoning every later reset.
        if s.synced_at.is_none() {
            return Err(format!(
                "{}: nothing has been synced into it yet, so there is nothing to {}. `reaper sync` pushes the tree (and `reaper test` does all of this in order)",
                s.name,
                which.label()
            )
            .into());
        }
        let g = manifest.guest(&s.guest).ok_or_else(|| {
            format!(
                "this session runs on {:?}, and the manifest no longer names that \
                 guest. Take the session down, or put the guest back",
                s.guest
            )
        })?;

        let (cmd, verb_env, image, mode) = match which {
            Verb::Build => match g.build.as_ref() {
                Some(b) => (&b.cmd, &b.env, b.image.as_ref(), b.exec),
                // A skip, not a failure: build is per-guest, and a manifest
                // mixing a compiled guest with an interpreted one is legal.
                // The nothing-to-build-anywhere case was refused before the
                // loop, so silence here never means "did nothing at all".
                None => {
                    println!(
                        "{}: {} declares no build; skipping",
                        s.name, g.name
                    );
                    continue;
                }
            },
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

        // Everything from here on is one session's business. A failure --
        // unreachable machine, failed command, broken results channel --
        // costs this session and is reported; its siblings still run, and
        // the verb exits non-zero at the end. One dead machine silently
        // cancelling every other session's run was the multi-session bug
        // this file kept re-growing.
        let attempt = || -> Result<()> {
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
        Ok(())
        };
        if let Err(e) = attempt() {
            failures += 1;
            eprintln!("{}: {} failed: {e}", s.name, which.label());
            if let Some(note) = expiry_note(&s) {
                eprintln!("{note}");
            }
        }
    }
    if failures > 0 {
        return Err(format!("{failures} session(s) failed to {}", which.label()).into());
    }
    Ok(())
}

/// One last reverse-sync before a machine is destroyed.
///
/// Best-effort throughout, and never allowed to stop a destroy: a machine that
/// cannot be reached is exactly the machine most in need of being taken down,
/// and refusing to remove it because its results could not be fetched would
/// leave the operator with a session they cannot get rid of.
fn collect_last_results(cfg: &Config, s: &Session, manifest_path: Option<&Path>) {
    // Nothing was ever pushed, so there is no workspace to read and nothing
    // could have been written. Attempting it would fail on a directory that was
    // never made, and read as though results had been lost.
    if s.synced_at.is_none() {
        return;
    }

    let Some(tree) = tree_for(s, manifest_path) else {
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
        Err(e) => {
            eprintln!(
                "{}: could not collect results before destroying it: {e}",
                s.name
            );
            if let Some(note) = expiry_note(s) {
                eprintln!("{note}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// doctor: the site, judged
// ---------------------------------------------------------------------------

/// What the report added up to. `main` maps this to the exit contract
/// (0 healthy-or-warned / 1 any failure / 2 doctor itself could not run),
/// which the generic Ok/Err mapping cannot express. Only the failure count
/// crosses this boundary: the ok/warn tallies live in the printed report,
/// and a field nothing reads is a warning the strict build rightly refuses.
pub struct DoctorVerdict {
    pub fail: usize,
}

struct Report {
    ok: usize,
    warn: usize,
    fail: usize,
}

impl Report {
    fn new() -> Report {
        Report { ok: 0, warn: 0, fail: 0 }
    }

    fn section(&mut self, title: &str) {
        println!("\n--- {title} ---\n");
    }

    fn line(&mut self, label: &str, health: reaper_core::Health, detail: &str) {
        let word = match health {
            reaper_core::Health::Ok => {
                self.ok += 1;
                "ok  "
            }
            reaper_core::Health::Warn => {
                self.warn += 1;
                "WARN"
            }
            reaper_core::Health::Fail => {
                self.fail += 1;
                "FAIL"
            }
        };
        println!("{word}  {label}");
        for l in detail.lines() {
            // Six spaces, and the padding-collapse sweep must not "fix" it:
            // this indent is the report's structure, not an accident.
            println!("{}{l}", "      ");
        }
    }
}

/// Judge the site end to end and say what is wrong, in one pass. Every
/// problem is a finding rather than an abort -- knowing that three things
/// broke is worth more than knowing that one did -- and nothing here creates
/// anything unless `--canary` asks for the one deliberate exception.
pub fn doctor(
    manifest_path: Option<PathBuf>,
    canary: bool,
    within: Option<String>,
) -> Result<DoctorVerdict> {
    use reaper_core::Health;
    // A malformed flag is doctor failing to run, not a site finding.
    let within = match within.as_deref() {
        Some(t) => duration::parse(t)?,
        None => Duration::from_secs(15 * 60),
    };

    let mut r = Report::new();
    r.section("workstation");

    // The config is the root of everything else; unparseable means one Fail
    // and the shortest honest report there is.
    let cfg = match reaper_core::config::load() {
        Ok(c) => c,
        Err(e) => {
            r.line("site configuration", Health::Fail, &e.to_string());
            println!("\n{} ok, {} warnings, {} failed", r.ok, r.warn, r.fail);
            return Ok(DoctorVerdict { fail: r.fail });
        }
    };
    r.line(
        "site configuration",
        Health::Ok,
        &format!("{} parses, {} guest(s) registered", cfg.path.display(), cfg.guests.len()),
    );

    match missing_ssh_key(&cfg) {
        Some(complaint) => r.line("session key", Health::Fail, &complaint),
        None => match &cfg.session.ssh_key {
            Some(k) => r.line("session key", Health::Ok, &format!("{} exists", k.display())),
            None => r.line(
                "session key",
                Health::Warn,
                "no session.ssh_key is configured; whatever identities ssh \
                 offers will be tried, which works until it quietly does not",
            ),
        },
    }

    for (what, cmd) in [
        ("ssh command", &cfg.session.ssh_command),
        ("rsync command", &cfg.session.rsync_command),
    ] {
        match resolvable(cmd) {
            Some(path) => r.line(what, Health::Ok, &format!("{cmd} is {}", path.display())),
            None => r.line(
                what,
                Health::Fail,
                &format!("{cmd} is not on PATH and is not an executable path; \
                          every session operation needs it"),
            ),
        }
    }

    let store = Store::open();
    match store.probe_writable() {
        Ok(()) => r.line(
            "session store",
            Health::Ok,
            &format!("{} is writable", store.path().display()),
        ),
        Err(e) => r.line("session store", Health::Fail, &e.to_string()),
    }

    // Registry coherence: two guests naming one template is legal and worth a
    // person's eyes -- their sessions clone the same machine.
    {
        let mut by_template: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for name in cfg.guest_names() {
            if let Some(t) = cfg.template_for(name) {
                by_template.entry(t).or_default().push(name);
            }
        }
        let shared: Vec<String> = by_template
            .iter()
            .filter(|(_, gs)| gs.len() > 1)
            .map(|(t, gs)| format!("{t} is registered for {}", gs.join(" and ")))
            .collect();
        if shared.is_empty() {
            r.line("guest registry", Health::Ok, "every guest has its own template");
        } else {
            r.line("guest registry", Health::Warn, &shared.join("\n"));
        }
    }

    // The manifest, when one is in reach. Its absence is a normal state for a
    // doctor run from anywhere, not an unchecked hazard.
    let manifest_at = manifest_path.unwrap_or_else(|| PathBuf::from(".reaper.toml"));
    if manifest_at.exists() {
        match reaper_manifest::load(&manifest_at) {
            Ok(m) => {
                let complaints: Vec<String> = m
                    .guests
                    .iter()
                    .filter_map(|g| unregistered_guest(&cfg, &g.name))
                    .collect();
                if complaints.is_empty() {
                    r.line(
                        "manifest",
                        Health::Ok,
                        &format!("{} is valid and every guest it names is registered",
                                 manifest_at.display()),
                    );
                } else {
                    r.line("manifest", Health::Fail, &complaints.join("\n"));
                }
            }
            Err(e) => r.line("manifest", Health::Fail, &e.to_string()),
        }
    } else {
        r.line(
            "manifest",
            Health::Ok,
            &format!("no manifest at {}; nothing to judge", manifest_at.display()),
        );
    }

    // The provider. Construction failure IS the credential/config finding --
    // the token is read and permission-checked on this path.
    r.section("provider");
    let provider = match provider_for(&cfg) {
        Ok(p) => p,
        Err(e) => {
            r.line("provider", Health::Fail, &e.to_string());
            r.line(
                "sessions",
                Health::Warn,
                "skipped: without the provider, records cannot be checked \
                 against the machines they claim",
            );
            println!("\n{} ok, {} warnings, {} failed", r.ok, r.warn, r.fail);
            return Ok(DoctorVerdict { fail: r.fail });
        }
    };
    let guests: Vec<reaper_core::RegisteredGuest> = cfg
        .guest_names()
        .iter()
        .filter_map(|n| {
            cfg.template_for(n).map(|t| reaper_core::RegisteredGuest {
                name: (*n).to_string(),
                template: t.to_string(),
            })
        })
        .collect();
    for f in provider.diagnose(&guests) {
        r.line(&f.label, f.health, &f.detail);
    }

    // The records, judged against the machines they claim.
    r.section("sessions");
    let live = provider.list().ok();
    match store.list() {
        Err(e) => r.line("session store", Health::Fail, &e.to_string()),
        Ok(sessions) if sessions.is_empty() => {
            r.line("sessions", Health::Ok, "no sessions recorded");
        }
        Ok(sessions) => {
            let now = SystemTime::now();
            for s in sessions {
                let mut notes = Vec::new();
                let mut worst = Health::Ok;
                match &live {
                    Some(l) if !l.iter().any(|m| m.machine == s.machine) => {
                        worst = Health::Fail;
                        notes.push(format!(
                            "its machine is gone; `reaper down {}` clears the record",
                            s.name
                        ));
                    }
                    Some(_) => {}
                    None => notes.push("machine check skipped: the provider \
                                        could not list".to_string()),
                }
                if s.remaining(now).is_none() {
                    if worst == Health::Ok {
                        worst = Health::Warn;
                    }
                    notes.push("its record is expired: the sweeper may take \
                                the machine at any moment".to_string());
                }
                match s.heartbeat_pid {
                    Some(pid) if proc::looks_like_heartbeat(pid) == Some(true) => {
                        // Ask the expiry, not the log. A heartbeat writes to
                        // its log only when it has something to SAY -- a
                        // successful renewal says nothing -- so the log's mtime
                        // freezes at session creation and reports every healthy
                        // session as stalled once it is older than two
                        // intervals. Measured: over 341s the expiry moved 301s
                        // and the log moved 0s.
                        //
                        // Renewal pushes the expiry to now + ttl every
                        // interval, so remaining should stay within an interval
                        // or so of the full ttl. Letting it decay past two is
                        // the symptom of a heartbeat that is running but no
                        // longer renewing -- which is exactly what the old
                        // check meant to catch, and could not.
                        if let Some(remaining) = s.remaining(now) {
                            let floor = s
                                .ttl
                                .checked_sub(cfg.session.heartbeat_interval * 2);
                            if let Some(floor) = floor {
                                if remaining < floor {
                                    if worst == Health::Ok {
                                        worst = Health::Warn;
                                    }
                                    notes.push(format!(
                                        "its heartbeat is alive but the expiry \
                                         has decayed to {}s of a {}s ttl -- \
                                         renewal may not be happening",
                                        remaining.as_secs(),
                                        s.ttl.as_secs()
                                    ));
                                }
                            }
                        }
                    }
                    Some(_) => {
                        if worst == Health::Ok {
                            worst = Health::Warn;
                        }
                        notes.push("its heartbeat is dead: the expiry has \
                                    stopped moving".to_string());
                    }
                    None => {
                        if worst == Health::Ok {
                            worst = Health::Warn;
                        }
                        notes.push("it has no heartbeat: the expiry never \
                                    moves".to_string());
                    }
                }
                if notes.is_empty() {
                    notes.push("record, machine and heartbeat agree".to_string());
                }
                r.line(&format!("session {}", s.name), worst, &notes.join("\n"));
            }
        }
    }

    if canary {
        r.section("canary");
        run_canary(&mut r, &cfg, provider.as_ref(), within);
    }

    println!("\n{} ok, {} warnings, {} failed", r.ok, r.warn, r.fail);
    Ok(DoctorVerdict { fail: r.fail })
}

/// The active sweeper check: make a machine that is expired from birth, and
/// watch for the sweeper to take it. Inherently safe -- even a crashed doctor
/// leaves nothing the sweeper will not collect -- and the strongest evidence
/// there is: a canary disappearing IS the sweeper working.
fn run_canary(
    r: &mut Report,
    cfg: &Config,
    provider: &dyn Provider,
    within: Duration,
) {
    use reaper_core::Health;
    let Some((guest, template)) = cfg
        .guest_names()
        .first()
        .and_then(|n| cfg.template_for(n).map(|t| ((*n).to_string(), t.to_string())))
    else {
        r.line("canary", Health::Fail, "no guest is registered to clone from");
        return;
    };

    let born_expired = SystemTime::now() - Duration::from_secs(1);
    let machine = match provider.create(&CreateRequest {
        name: "reaper-doctor-canary".into(),
        template,
        cores: None,
        ram_gb: None,
        data_disk_gb: None,
        expires_at: born_expired,
    }) {
        Ok(m) => m,
        Err(e) => {
            r.line(
                "canary",
                Health::Fail,
                &format!("could not create the canary (from {guest}'s template): {e}"),
            );
            return;
        }
    };
    println!(
        "      canary {machine} created, already expired; waiting up to {} \
         for the sweeper",
        duration::format(within)
    );

    let started = std::time::Instant::now();
    let deadline = started + within;
    loop {
        match provider.list() {
            Ok(l) if !l.iter().any(|m| m.machine == machine) => {
                r.line(
                    "canary",
                    Health::Ok,
                    &format!(
                        "the sweeper collected it after {}s -- the dead-man's \
                         switch is live",
                        started.elapsed().as_secs()
                    ),
                );
                return;
            }
            Ok(_) => {}
            Err(e) => {
                // A blink mid-wait is absorbed; the deadline bounds it.
                eprintln!("reaper: canary poll failed ({e}); retrying");
            }
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_secs(5));
    }

    let cleanup = match provider.destroy(&machine) {
        Ok(()) => "the canary has been destroyed by hand".to_string(),
        Err(reaper_core::ProviderError::NotFound(_)) => {
            // Collected between the last poll and now: late, but alive.
            r.line(
                "canary",
                Health::Warn,
                &format!(
                    "the sweeper took it only as the wait expired (~{}s) -- \
                     alive, but slower than sweep_within expects",
                    started.elapsed().as_secs()
                ),
            );
            return;
        }
        Err(e) => format!("and destroying it failed too ({e}); it is expired, so \
                           a working sweeper would still collect it"),
    };
    r.line(
        "canary",
        Health::Fail,
        &format!(
            "nothing collected an expired machine in {}: the sweeper is absent \
             or stopped; {cleanup}",
            duration::format(within)
        ),
    );
}

/// A command as configured: an explicit path must exist and be executable, a
/// bare name must be findable on PATH. Checked nowhere else, and a missing
/// rsync surfaces today only mid-verb with rsync's own error.
fn resolvable(cmd: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    if cmd.contains('/') {
        let p = PathBuf::from(cmd);
        return executable(&p).then_some(p);
    }
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| Path::new(d).join(cmd))
        .find(|p| executable(p))
}
