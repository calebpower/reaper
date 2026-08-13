//! reaper: an ephemeral test-VM harness for the pre-push loop.
//!
//! This binary knows nothing about any hypervisor. It resolves what a tenant
//! asked for against what the sysadmin registered, and drives whatever provider
//! the site configuration selected. Lint guards fail the build if hypervisor or
//! operating-system vocabulary appears here.

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
        /// The manifest to read. Defaults to .reaper.yaml here.
        #[arg(long)]
        manifest: Option<PathBuf>,
    },

    /// Copy the working tree into a session, and results back out.
    Sync {
        /// Which session. Defaults to every session for this project.
        session: Option<String>,
        /// The manifest to read, and the tree to sync. Defaults to .reaper.yaml here.
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

    /// Show live sessions.
    List,

    /// Push a session's expiry further out.
    Renew {
        /// Which session. Defaults to every session for this project.
        session: Option<String>,
        #[arg(long)]
        ttl: Option<String>,
    },

    /// Destroy a session and its machine.
    Down {
        /// Which session. Defaults to every session for this project.
        session: Option<String>,
        /// Every session, not just this project's.
        #[arg(long)]
        all: bool,
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
        Command::List => commands::list(),
        Command::Renew { session, ttl } => commands::renew(session, ttl),
        Command::Down { session, all } => commands::down(session, all),
        Command::Heartbeat { session } => commands::heartbeat(&session),
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("reaper: {e}");
            ExitCode::FAILURE
        }
    }
}
