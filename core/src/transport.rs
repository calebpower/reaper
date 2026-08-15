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
    ) -> Ssh {
        Ssh {
            program: program.into(),
            user: user.into(),
            address,
            key,
            known_hosts,
            connect_timeout,
        }
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

impl Transport for Ssh {
    fn run(&self, command: &str, what: &str) -> Result<String> {
        let mut cmd = Command::new(&self.program);
        cmd.args(self.options()).arg(command);
        let out = cmd.output().map_err(|e| TransportError::Spawn {
            program: self.program.clone(),
            source: e,
        })?;

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

        let out = child.wait_with_output().map_err(|e| TransportError::Spawn {
            program: self.program.clone(),
            source: e,
        })?;

        let wrote = writer.join().expect("stdin writer thread panicked");

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
