//! reaper: an ephemeral test-VM harness for the pre-push loop.
//!
//! This binary knows nothing about any hypervisor. It resolves what a tenant
//! asked for against what the sysadmin registered, and drives whatever provider
//! the site configuration selected. Lint guards fail the build if hypervisor or
//! operating-system vocabulary appears here.

// Before the modules that use its macros: a macro_rules! macro is visible
// only after its definition point, and #[macro_use] is what carries these
// into the rest of the crate.
#[macro_use]
mod out;

mod commands;
mod proc;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "reaper",
    version,
    about = "Disposable test machines for the pre-push loop",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Bring up a session for this project.
    Up {
        /// Only this guest, rather than every guest the manifest names.
        #[arg(long)]
        guest: Option<String>,
        /// Take the time-to-live and environment from this profile.
        #[arg(long)]
        profile: Option<String>,
        /// Override the time-to-live, e.g. 4h.
        #[arg(long)]
        ttl: Option<String>,
        /// The manifest to read. Defaults to .reaper.toml here.
        #[arg(long)]
        manifest: Option<PathBuf>,
    },

    /// Copy the working tree into a session, and results back out.
    Sync {
        /// Which session. Defaults to every session for this project.
        session: Option<String>,
        /// The manifest to read, and the tree to sync. Defaults to .reaper.toml here.
        #[arg(long)]
        manifest: Option<PathBuf>,
    },

    /// Run this project's build command in a session.
    Build {
        session: Option<String>,
        /// Take the environment and cache policy from this profile.
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        manifest: Option<PathBuf>,
    },

    /// Run this project's run command in a session.
    Run {
        session: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        manifest: Option<PathBuf>,
    },

    /// The whole loop: sync, build, reset, run.
    Test {
        session: Option<String>,
        /// Reset to a named snapshot rather than the session's pristine.
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        manifest: Option<PathBuf>,
    },

    /// Roll this project's state back.
    Reset {
        session: Option<String>,
        /// A named snapshot. Defaults to the session's pristine.
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        manifest: Option<PathBuf>,
    },

    /// Name a point this project's state can be rolled back to.
    Snapshot {
        name: String,
        session: Option<String>,
        #[arg(long)]
        manifest: Option<PathBuf>,
    },

    /// Show live sessions.
    List,

    /// Judge the site end to end: configuration, credentials, templates,
    /// storage, records -- and say what is wrong. Exit 0 healthy (warnings
    /// permitted), 1 if anything failed, 2 if doctor itself could not run.
    Doctor {
        /// A manifest to judge along with the site. Defaults to .reaper.toml
        /// here, when one exists.
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Prove the sweeper by making an already-expired machine and
        /// watching for it to be collected. Creates one real machine.
        #[arg(long)]
        canary: bool,
        /// How long the canary waits before calling the sweeper absent.
        #[arg(long)]
        within: Option<String>,
    },

    /// Push a session's expiry further out.
    Renew {
        /// Which session. Defaults to every session for this project.
        session: Option<String>,
        #[arg(long)]
        ttl: Option<String>,
        /// Which project's sessions. Defaults to .reaper.toml here.
        #[arg(long)]
        manifest: Option<PathBuf>,
    },

    /// Destroy a session and its machine.
    Down {
        /// Which session. Defaults to every session for this project.
        session: Option<String>,
        /// Every session, not just this project's.
        #[arg(long, conflicts_with = "session")]
        all: bool,
        /// Which project's sessions. Defaults to .reaper.toml here.
        #[arg(long)]
        manifest: Option<PathBuf>,
    },

    /// Renew a session's expiry until it goes away. Started by `up`.
    #[command(hide = true)]
    Heartbeat {
        #[arg(long)]
        session: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let outcome = match cli.command {
        // Doctor has its own exit contract -- 0 healthy / 1 failed findings /
        // 2 could-not-run -- which the shared Ok/Err mapping below cannot
        // express, and its report must not gain a "reaper:" prefix line.
        Command::Doctor { manifest, canary, within } => {
            return match commands::doctor(manifest, canary, within) {
                Ok(v) if v.fail == 0 => ExitCode::SUCCESS,
                Ok(_) => ExitCode::FAILURE,
                Err(e) => {
                    warn_line!("reaper: {e}");
                    ExitCode::from(2)
                }
            };
        }
        Command::Up {
            guest,
            profile,
            ttl,
            manifest,
        } => commands::up(guest, profile, ttl, manifest),
        Command::Sync { session, manifest } => commands::sync(session, manifest),
        Command::Build {
            session,
            profile,
            manifest,
        } => commands::exec(commands::Verb::Build, session, profile, manifest),
        Command::Run {
            session,
            profile,
            manifest,
        } => commands::exec(commands::Verb::Run, session, profile, manifest),
        Command::Test {
            session,
            to,
            profile,
            manifest,
        } => commands::test(to, session, profile, manifest),
        Command::Reset {
            session,
            to,
            manifest,
        } => commands::reset(to, session, manifest),
        Command::Snapshot {
            name,
            session,
            manifest,
        } => commands::snapshot(name, session, manifest),
        Command::List => commands::list(),
        Command::Renew {
            session,
            ttl,
            manifest,
        } => commands::renew(session, ttl, manifest),
        Command::Down {
            session,
            all,
            manifest,
        } => commands::down(session, all, manifest),
        Command::Heartbeat { session } => commands::heartbeat(&session),
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            warn_line!("reaper: {e}");
            ExitCode::FAILURE
        }
    }
}
