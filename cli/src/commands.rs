//! The session verbs.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use reaper_core::config::Config;
use reaper_core::provider::{CreateRequest, MachineRef, Provider};
use reaper_core::session::{Session, Store};
use reaper_core::transport::{Ssh, Transport};
use reaper_core::{config, duration};
use reaper_manifest::Manifest;

use crate::proc;

pub type Error = Box<dyn std::error::Error>;
type Result<T> = std::result::Result<T, Error>;

fn load_config() -> Result<Config> {
    Ok(config::load()?)
}

fn provider_for(cfg: &Config) -> Result<Box<dyn Provider>> {
    Ok(reaper_providers::build(&cfg.provider, cfg.provider_table())?)
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
        };
        store.put(session.clone())?;

        provider.start(&machine)?;

        match wait_for_address(provider.as_ref(), &machine, cfg.session.ready_grace)? {
            Some(address) => {
                // A machine with an address is not yet a machine anyone can
                // use: it has no pool. Firstboot is what makes it a session,
                // so it happens before the session is called ready.
                prepare(&cfg, &name, address)?;

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
                    "{name}: created, but it has not reported an address within {}. \
                     It is tagged to expire; `reaper list` will show it, and \
                     `reaper down {name}` will remove it.",
                    duration::format(cfg.session.ready_grace)
                );
            }
        }
    }

    Ok(())
}

fn wait_for_address(
    provider: &dyn Provider,
    machine: &MachineRef,
    limit: Duration,
) -> Result<Option<std::net::IpAddr>> {
    let deadline = SystemTime::now() + limit;
    loop {
        if let Some(addr) = provider.address(machine)? {
            return Ok(Some(addr));
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

/// Deliver the runner and build the session's storage.
fn prepare(cfg: &Config, session: &str, address: std::net::IpAddr) -> Result<()> {
    // Per-session, and thrown away with the session. A shared known-hosts file
    // would accumulate keys for addresses that get recycled, and then refuse to
    // connect at the least convenient moment.
    let known_hosts = Store::open()
        .path()
        .with_file_name(format!("known-hosts-{session}"));
    if let Some(dir) = known_hosts.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(&known_hosts);

    let ssh = Ssh::new(
        cfg.session.ssh_command.clone(),
        cfg.session.ssh_user.clone(),
        address,
        cfg.session.ssh_key.clone(),
        known_hosts,
        cfg.session.ssh_connect_timeout,
    );

    let dest = "/tmp/reaper-runner.sh";
    println!("{session}: preparing storage on {}", ssh.describe());
    ssh.put_executable(RUNNER.as_bytes(), dest)?;
    ssh.run(&format!("{dest} firstboot"), "firstboot")?;
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

    let child = std::process::Command::new(exe)
        .args(["heartbeat", "--session", session])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()?;

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
fn implied_sessions(store: &Store, explicit: Option<String>) -> Result<Vec<Session>> {
    if let Some(name) = explicit {
        let s = store
            .get(&name)?
            .ok_or_else(|| format!("no session named {name:?}; try `reaper list`"))?;
        return Ok(vec![s]);
    }

    let here = Path::new(".reaper.yaml");
    if !here.exists() {
        return Err(
            "not inside a project, so there is nothing implied. Name a session, or run \
             this where a .reaper.yaml is"
                .into(),
        );
    }
    let project = reaper_manifest::load(here)?.project;
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

pub fn renew(session: Option<String>, ttl: Option<String>) -> Result<()> {
    let cfg = load_config()?;
    let store = Store::open();
    let provider = provider_for(&cfg)?;

    for s in implied_sessions(&store, session)? {
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

pub fn down(session: Option<String>, all: bool) -> Result<()> {
    let cfg = load_config()?;
    let store = Store::open();
    let provider = provider_for(&cfg)?;

    let targets = if all {
        store.list()?
    } else {
        implied_sessions(&store, session)?
    };

    if targets.is_empty() {
        println!("no sessions");
        return Ok(());
    }

    let mut failures = 0;
    for s in targets {
        // Heartbeat first. A renewal landing between the destroy and the
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
