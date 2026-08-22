//! Getting commands and files into a session.
//!
//! SSH is the transport, and the runner is invoked rather than resident --
//! there is no daemon in a guest and nothing reaper wrote lives in a template.
//! That decision is why this module exists at all.

use std::fmt;
use std::io::Write;
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Debug)]
pub enum TransportError {
    Spawn { program: String, source: std::io::Error },
    Failed { what: String, status: i32, stderr: String },
    /// The command stopped making progress and was given up on.
    ///
    /// Deliberately distinct from `Failed`: only one of the two says anything
    /// about the guest. A command that failed was answered; a command that
    /// stalled was not, and the machine at the other end may be perfectly
    /// healthy -- both times this was seen, a second ssh to the same guest
    /// answered in under a second while the first sat in `select`.
    Stalled { what: String, after: Duration },
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Spawn { program, source } => {
                write!(f, "could not run {program}: {source}")
            }
            TransportError::Failed { what, status, stderr } => {
                write!(f, "{what} failed ({status})")?;
                let trimmed = stderr.trim();
                if !trimmed.is_empty() {
                    write!(f, ": {trimmed}")?;
                }
                Ok(())
            }
            TransportError::Stalled { what, after } => write!(
                f,
                "{what} stopped responding: nothing came back for {}s, so reaper gave up on it \
                 and closed the connection. This says nothing about whether the command \
                 succeeded -- only that this end stopped hearing about it. session.io_timeout \
                 is the patience",
                after.as_secs()
            ),
        }
    }
}

impl std::error::Error for TransportError {}

type Result<T> = std::result::Result<T, TransportError>;

pub struct Ssh {
    program: String,
    user: String,
    address: IpAddr,
    key: Option<PathBuf>,
    known_hosts: PathBuf,
    connect_timeout: Duration,
    io_timeout: Duration,
}

impl Ssh {
    /// `known_hosts` should be per-session and disposable: see [`Ssh::options`].
    pub fn new(
        program: impl Into<String>,
        user: impl Into<String>,
        address: IpAddr,
        key: Option<PathBuf>,
        known_hosts: PathBuf,
        connect_timeout: Duration,
        io_timeout: Duration,
    ) -> Ssh {
        Ssh {
            program: program.into(),
            user: user.into(),
            address,
            key,
            known_hosts,
            connect_timeout,
            io_timeout,
        }
    }

    /// How long anything on this transport may go without progress.
    ///
    /// Carried here rather than passed alongside, so that rsync -- which is
    /// handed this same `Ssh` to build its transport from -- cannot end up
    /// with a different patience than ssh has.
    pub fn io_timeout(&self) -> Duration {
        self.io_timeout
    }

    /// The options every invocation carries, and why.
    ///
    /// `StrictHostKeyChecking=accept-new` because a machine created seconds ago
    /// has a host key nothing has ever seen, so strict checking would reject
    /// every first connection. Paired with a **per-session** known-hosts file,
    /// so a session starts with no history and cannot inherit a stale key from
    /// an address that has been recycled -- which is the failure the usual
    /// shared known_hosts produces, loudly and at the worst moment.
    ///
    /// What that trusts is the provider's report of the address. It adds no
    /// party that is not already trusted to create the machine, but it is a
    /// real assumption; `docs/guests.md` states it.
    ///
    /// `BatchMode` because nothing here can answer a prompt, and a command that
    /// blocks forever waiting for one is worse than a command that fails.
    pub fn options(&self) -> Vec<String> {
        let mut o = self.transport_options();
        o.push(self.address.to_string());
        o
    }

    /// The same options, without the target.
    ///
    /// rsync appends the host itself, so it needs the options and not the
    /// destination. Splitting it out here rather than reconstructing the list
    /// somewhere else is what stops `ssh` and `rsync` drifting into connecting
    /// two different ways -- which would surface as a host-key prompt at the
    /// least convenient moment, in a command that cannot answer one.
    pub fn transport_options(&self) -> Vec<String> {
        let mut o = vec![
            "-o".into(), "BatchMode=yes".into(),
            "-o".into(), "StrictHostKeyChecking=accept-new".into(),
            "-o".into(), format!("UserKnownHostsFile={}", self.known_hosts.display()),
            "-o".into(), format!("ConnectTimeout={}", self.connect_timeout.as_secs()),
            // ConnectTimeout bounds *establishing* a connection and nothing
            // after it. These bound an established one: without them a
            // connection whose path has quietly gone away is indistinguishable
            // from one where the far end is simply thinking, and ssh waits on
            // it for ever. Four probes at a quarter of the patience each, so
            // the whole budget is io_timeout however it is set.
            //
            // They do not bound a command that is merely slow, which is the
            // point -- a build that runs for an hour without printing anything
            // keeps answering keepalives and is left alone.
            "-o".into(), format!("ServerAliveInterval={}", (self.io_timeout.as_secs() / 4).max(1)),
            "-o".into(), "ServerAliveCountMax=4".into(),
            "-l".into(), self.user.clone(),
        ];
        if let Some(k) = &self.key {
            o.push("-i".into());
            o.push(k.display().to_string());
            // Offer only the key we were given. Without this, a workstation
            // with several loaded keys can exhaust the server's auth attempts
            // before reaching the right one.
            o.push("-o".into());
            o.push("IdentitiesOnly=yes".into());
        }
        o
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    /// The host as rsync wants it written. An IPv6 literal needs brackets, or
    /// the colon introducing the path is ambiguous with the address's own.
    pub fn rsync_host(&self) -> String {
        match self.address {
            IpAddr::V4(a) => a.to_string(),
            IpAddr::V6(a) => format!("[{a}]"),
        }
    }
}

pub trait Transport {
    /// Run a command, returning its output. A non-zero exit is an error: every
    /// caller here treats one as fatal, so making them all check would only
    /// create places to forget.
    fn run(&self, command: &str, what: &str) -> Result<String>;

    /// Run a command with its output going straight to the terminal.
    ///
    /// A build takes minutes, and a tenant watching a blank screen until it
    /// finishes cannot tell a slow compile from a hung one. Output is not
    /// captured, so nothing here can inspect it -- which is the right trade for
    /// a command whose output belongs to the person who asked for it.
    fn run_live(&self, command: &str, what: &str) -> Result<()>;

    /// Write bytes to a path, and make it executable.
    fn put_executable(&self, bytes: &[u8], dest: &str) -> Result<()>;

    /// Something a person can read in an error message.
    fn describe(&self) -> String;
}

/// Wait for a child, but not for ever.
///
/// `Command::output()` waits until the child exits and its pipes close, with
/// no way to say "and if that never happens". Both halves of that have been
/// seen to never happen against a healthy guest: an ssh whose remote command
/// had already exited sat in `select` for thirty-two minutes, and an rsync
/// pair sat idle at both ends while `$REAPER_OUT` held the finished results.
///
/// The pipes are drained on their own threads because they must be: a child
/// that fills a pipe buffer blocks, and a parent that waits for exit before
/// reading would then wait for a child that cannot proceed. Read-then-wait is
/// the deadlock this shape exists to avoid.
fn wait_with_deadline(
    program: &str,
    mut child: std::process::Child,
    what: &str,
    within: Duration,
) -> Result<std::process::Output> {
    use std::io::Read;

    let mut so = child.stdout.take().expect("stdout was piped");
    let mut se = child.stderr.take().expect("stderr was piped");
    let out_reader = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = so.read_to_end(&mut b);
        b
    });
    let err_reader = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = se.read_to_end(&mut b);
        b
    });

    let deadline = std::time::Instant::now() + within;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                return Err(TransportError::Spawn {
                    program: program.to_string(),
                    source: e,
                })
            }
        }
        if std::time::Instant::now() >= deadline {
            // Killed rather than abandoned: leaving it would leak a process
            // per stall.
            let _ = child.kill();
            let _ = child.wait();
            // The readers are deliberately NOT joined here, and this cost a
            // hung test to learn. Killing a process does not close pipes that
            // something it spawned still holds open, so a `read_to_end` on
            // them can outlive the child by as long as that grandchild lives
            // -- which would make the deadline path block for ever, in the
            // exact shape the deadline exists to prevent. Dropping the handles
            // detaches them; each ends by itself when its pipe finally closes,
            // and neither is holding anything anyone is waiting for.
            drop(out_reader);
            drop(err_reader);
            return Err(TransportError::Stalled {
                what: what.to_string(),
                after: within,
            });
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    Ok(std::process::Output {
        status,
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
    })
}

impl Transport for Ssh {
    fn run(&self, command: &str, what: &str) -> Result<String> {
        // Bounded, unlike `run_live`. Everything that comes through here is
        // reaper's own control chatter -- making a workspace, firstboot,
        // listing snapshots -- and none of it has any business taking minutes.
        // A tenant's command, which legitimately might, goes through
        // `run_live` and is deliberately left unbounded.
        let child = Command::new(&self.program)
            .args(self.options())
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| TransportError::Spawn {
                program: self.program.clone(),
                source: e,
            })?;
        let out = wait_with_deadline(&self.program, child, what, self.io_timeout)?;

        if !out.status.success() {
            return Err(TransportError::Failed {
                what: what.to_string(),
                status: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    fn run_live(&self, command: &str, what: &str) -> Result<()> {
        let status = Command::new(&self.program)
            .args(self.options())
            .arg(command)
            .status()
            .map_err(|e| TransportError::Spawn {
                program: self.program.clone(),
                source: e,
            })?;

        if !status.success() {
            return Err(TransportError::Failed {
                what: what.to_string(),
                status: status.code().unwrap_or(-1),
                // Nothing was captured, so there is nothing to quote back. The
                // output already went where it was wanted.
                stderr: String::new(),
            });
        }
        Ok(())
    }

    fn put_executable(&self, bytes: &[u8], dest: &str) -> Result<()> {
        // Piped over the same connection rather than via scp or sftp: one tool
        // to depend on instead of three, and no assumption about which of them
        // a given guest ships.
        // Quoted for the same reason job.rs quotes every value: this string
        // is parsed by the remote shell, and a path with a space or a
        // metacharacter must stay a path.
        let q = crate::job::quote(dest);
        let script = format!("cat > {q} && chmod 0755 {q}");
        let mut child = Command::new(&self.program)
            .args(self.options())
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| TransportError::Spawn {
                program: self.program.clone(),
                source: e,
            })?;

        // The write happens on its own thread while wait_with_output drains
        // stdout and stderr: writing first and draining after deadlocks the
        // moment the child fills a pipe buffer while stdin is still mid-write
        // (a verbose ssh wrapper is enough).
        let mut stdin = child.stdin.take().expect("stdin was piped");
        let bytes_owned = bytes.to_vec();
        let writer = std::thread::spawn(move || {
            let r = stdin.write_all(&bytes_owned);
            drop(stdin);
            r
        });

        // Bounded like `run`, and joined before the deadline is reported
        // either way: a stall that returned early would leave the writer
        // thread behind, and the whole point of the deadline is that nothing
        // is left waiting on a conversation that has stopped.
        let waited = wait_with_deadline(
            &self.program,
            child,
            &format!("writing {dest}"),
            self.io_timeout,
        );
        let wrote = writer.join().expect("stdin writer thread panicked");
        let out = waited?;

        // Status first: a write that broke mid-stream usually broke because
        // the remote command died, and the status plus stderr says why.
        if !out.status.success() {
            return Err(TransportError::Failed {
                what: format!("writing {dest}"),
                status: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            });
        }
        wrote.map_err(|e| TransportError::Spawn {
            program: self.program.clone(),
            source: e,
        })?;
        Ok(())
    }

    fn describe(&self) -> String {
        format!("{}@{}", self.user, self.address)
    }
}
