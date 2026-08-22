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
    let mut args = vec![
        "-a".to_string(),
        "--delete".to_string(),
        // Itemized so that [`deletions`] can count what went. A removal that
        // silently fails to propagate leaves the guest compiling a file the
        // operator deleted, and a baseline built that way is green for the
        // wrong reason -- the one failure this channel has that looks like
        // success. `-i` rather than `--info=DEL`, which is newer than some of
        // the rsync builds this runs against; the extra lines are captured and
        // counted here, never shown.
        "--itemize-changes".to_string(),
        // rsync's own inactivity timeout, and the only thing that bounds the
        // failure this channel actually produces. Twice now a transfer has
        // wedged with both ends idle in select and nothing moving, while the
        // connection stayed ESTABLISHED and a second ssh to the same guest
        // answered instantly -- so nothing at the connection layer, keepalives
        // included, was ever going to notice. This does: no data for this long
        // and rsync gives up and says so.
        format!("--timeout={}", ssh.io_timeout().as_secs().max(1)),
    ];
    args.extend(ownership());
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

/// How many paths a forward sync removed from the receiver.
///
/// Read from rsync's own itemized output rather than inferred here, because
/// the question this answers is precisely "did `--delete` reach that path?" --
/// and a count reaper computed from what it *believed* it had deleted would
/// answer a different question, the one that is never in doubt.
///
/// rsync writes one `*deleting   <path>` line per removal. Directories and
/// files both count: a removed directory is a removed path.
pub fn deletions(output: &str) -> usize {
    output
        .lines()
        .filter(|l| l.trim_start().starts_with("*deleting"))
        .count()
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
    // Bounded for the same reason as the forward direction, and more urgently:
    // this is the one that runs on a timer while a tenant's command is going,
    // so a pull that never returns holds the whole verb open after the run it
    // was reporting on has already passed.
    let mut args = vec![
        "-a".to_string(),
        format!("--timeout={}", ssh.io_timeout().as_secs().max(1)),
    ];
    args.extend(ownership());
    args.extend([
        "-e".to_string(),
        rsh.to_string_lossy().to_string(),
        format!("{}:{}", ssh.rsync_host(), with_slash(remote)),
        dir(local),
    ]);
    Plan { program: rsync.to_string(), args }
}

/// Empty the workstation's results directory, and say how many entries went.
///
/// The counterpart to [`pull`]'s refusal to delete, and the reason that refusal
/// is safe to keep. Because the backward sync merges, a run that fails *before
/// the guest produces anything* leaves the previous run's results sitting in
/// `out/` -- and a collector globbing `out/*.xml`, or a person, then reads a
/// complete green battery for a tier that never executed. The exit status is
/// right and the artifacts contradict it, which is the worst shape a result can
/// have.
///
/// So this runs once at the top of a whole loop, before anything is synced: the
/// operator asking for another run is what supersedes the last one's output.
/// Nothing is destroyed to mirror the guest, which is the trade `pull`
/// documents and declines; the failure trace from the run just finished
/// survives until the next run is deliberately started.
///
/// The directory itself is kept, because it is the rsync destination and
/// recreating it is one more thing to get wrong. A directory that is not there
/// is not an error: there is nothing to clear.
pub fn clear_results(tree: &Path) -> io::Result<usize> {
    let at = tree.join(RESULTS);
    let entries = match std::fs::read_dir(&at) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };

    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        // Never followed. A symlink in here is removed as a link; whatever it
        // points at is somebody else's file and is none of this function's
        // business.
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
        removed += 1;
    }
    Ok(removed)
}

/// Numeric owners are not carried across.
///
/// `-a` preserves them, and the two machines share no user database, so a tree
/// synced from a workstation lands owned by a uid the guest has never heard of.
/// Everything in a session runs as root, and the practical result of the
/// default is that tools which check who owns what -- git's own repository
/// ownership check is the one that bit -- refuse to work on the synced tree.
fn ownership() -> [String; 2] {
    ["--no-owner".to_string(), "--no-group".to_string()]
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
