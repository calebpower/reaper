//! Getting a working tree in, and results back out.
//!
//! Both directions are rsync over the same SSH transport the rest of reaper
//! uses. The two are deliberately not symmetrical, and the asymmetry is the
//! only interesting thing in this module.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::transport::Ssh;

/// The directory results come back on, relative to the synced tree.
///
/// Anchored with a leading slash in the exclude below, so it means the one at
/// the top of the tree and not every `out` anywhere in it. Without that anchor
/// a forward sync would carry `--delete` into the guest's results directory and
/// destroy the run's output on the way past -- the sharpest edge in this
/// design, and the reason this is a constant rather than a literal.
pub const RESULTS: &str = "out";

#[derive(Debug)]
pub enum SyncError {
    Spawn { program: String, source: io::Error },
    Failed { what: String, status: i32, stderr: String },
    /// The transport wrapper's path is unusable. See [`rsh_wrapper`].
    UnusablePath { path: PathBuf },
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::Spawn { program, source } => write!(f, "could not run {program}: {source}"),
            SyncError::Failed { what, status, stderr } => {
                write!(f, "{what} failed ({status})")?;
                let trimmed = stderr.trim();
                if !trimmed.is_empty() {
                    write!(f, ": {trimmed}")?;
                }
                Ok(())
            }
            SyncError::UnusablePath { path } => write!(
                f,
                "the transport wrapper would live at {}, and rsync splits its \
                 --rsh option on whitespace with no quoting, so a path containing \
                 any would be torn into pieces. Point XDG_STATE_HOME at a \
                 directory whose path has no spaces in it",
                path.display()
            ),
        }
    }
}

impl std::error::Error for SyncError {}

type Result<T> = std::result::Result<T, SyncError>;

/// An rsync invocation, built but not yet run.
///
/// Separating construction from execution is what lets the suite assert on the
/// arguments -- the flags are the whole substance of this module, and a test
/// that ran rsync to find out what it did would be testing rsync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub program: String,
    pub args: Vec<String>,
}

impl Plan {
    pub fn run(&self) -> Result<String> {
        let out = Command::new(&self.program)
            .args(&self.args)
            .output()
            .map_err(|e| SyncError::Spawn {
                program: self.program.clone(),
                source: e,
            })?;

        if !out.status.success() {
            return Err(SyncError::Failed {
                what: format!("{} {}", self.program, self.args.join(" ")),
                status: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }
}

/// Write the script rsync will use in place of `ssh`, and return its path.
///
/// rsync's `--rsh` takes a command *string*, which it splits on whitespace with
/// no quote handling of its own. Anything with a path in it -- a key, a
/// per-session known-hosts file -- is therefore one space away from being torn
/// into pieces. A script sidesteps the whole question: the options live inside
/// it, quoted by us, and rsync sees a single word.
///
/// It is built from [`Ssh::transport_options`], so ssh and rsync cannot drift
/// into connecting two different ways.
pub fn rsh_wrapper(ssh: &Ssh, at: &Path) -> Result<PathBuf> {
    let text = at.to_string_lossy();
    if text.chars().any(char::is_whitespace) {
        return Err(SyncError::UnusablePath { path: at.to_path_buf() });
    }

    let mut script = String::from("#!/bin/sh\n");
    script.push_str("# Written by reaper. rsync splits --rsh on whitespace and does no\n");
    script.push_str("# quoting, so the transport's options live in a script instead.\n");
    script.push_str("exec ");
    script.push_str(&crate::job::quote(ssh.program()));
    for opt in ssh.transport_options() {
        script.push(' ');
        script.push_str(&crate::job::quote(&opt));
    }
    script.push_str(" \"$@\"\n");

    if let Some(dir) = at.parent() {
        std::fs::create_dir_all(dir).map_err(|e| SyncError::Spawn {
            program: text.to_string(),
            source: e,
        })?;
    }
    std::fs::write(at, script).map_err(|e| SyncError::Spawn {
        program: text.to_string(),
        source: e,
    })?;
    make_executable(at)?;
    Ok(at.to_path_buf())
}

fn make_executable(at: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(at)
        .map_err(|e| SyncError::Spawn {
            program: at.to_string_lossy().to_string(),
            source: e,
        })?
        .permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(at, perms).map_err(|e| SyncError::Spawn {
        program: at.to_string_lossy().to_string(),
        source: e,
    })
}

/// The working tree, into the session.
///
/// `--delete` mirrors deletions, because a tree that still holds a file the
/// operator removed is not the tree they are testing. The results directory is
/// excluded, and excluded rather than `--delete-excluded`, so the receiver's
/// copy is protected rather than destroyed.
///
/// `.git` is not excluded by default, deliberately: never needing a commit is
/// the point of this whole channel, version-stamping builds read git metadata,
/// and rsync's deltas make every sync after the first one cheap. A tenant who
/// disagrees says so in `sync.exclude`.
///
/// No `-z`. This is a local network, and compressing an already-compressed
/// artifact costs more than it saves.
pub fn push(rsync: &str, rsh: &Path, ssh: &Ssh, local: &Path, remote: &str, exclude: &[String]) -> Plan {
    let mut args = vec!["-a".to_string(), "--delete".to_string()];
    args.push(format!("--exclude=/{RESULTS}/"));
    for pattern in exclude {
        args.push(format!("--exclude={pattern}"));
    }
    args.push("-e".into());
    args.push(rsh.to_string_lossy().to_string());
    args.push(dir(local));
    args.push(format!("{}:{}", ssh.rsync_host(), with_slash(remote)));
    Plan { program: rsync.to_string(), args }
}

/// Results, back out.
///
/// **No `--delete`**, and this is a decision rather than an omission. The guest
/// is authoritative for what it produced; it is not authoritative for what was
/// in the operator's results directory beforehand. Destroying a local artifact
/// to mirror a guest that never had it is the wrong trade for a channel whose
/// entire purpose is that a failure trace must never exist only on a machine
/// scheduled for destruction.
pub fn pull(rsync: &str, rsh: &Path, ssh: &Ssh, remote: &str, local: &Path) -> Plan {
    let args = vec![
        "-a".to_string(),
        "-e".to_string(),
        rsh.to_string_lossy().to_string(),
        format!("{}:{}", ssh.rsync_host(), with_slash(remote)),
        dir(local),
    ];
    Plan { program: rsync.to_string(), args }
}

/// A trailing slash on a source means "the contents of", and on a destination
/// it costs nothing. Getting it wrong on the source nests the tree inside
/// itself one directory deeper on every sync.
fn dir(p: &Path) -> String {
    with_slash(&p.to_string_lossy())
}

fn with_slash(s: &str) -> String {
    if s.ends_with('/') {
        s.to_string()
    } else {
        format!("{s}/")
    }
}
