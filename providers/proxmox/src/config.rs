//! This provider's slice of site configuration.
//!
//! The core hands over the `[proxmox]` table uninterpreted; everything Proxmox
//! needs to be told is read here and nowhere else.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use reaper_core::duration as dur;
use reaper_core::paths::expand_tilde;
use serde::Deserialize;

use crate::ids::IdRange;

/// How the server's certificate is checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tls {
    /// The system's ordinary public trust roots.
    Webpki,
    /// A specific certificate authority -- the right answer for a node whose
    /// certificate is issued by an internal CA, which is most of them.
    CaFile(PathBuf),
    /// No verification at all. Honest, loud, and never a default.
    Insecure,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub api: String,
    pub node: String,
    pub pool: String,
    pub ids: IdRange,
    pub token_file: PathBuf,
    /// Storage for the blank disk attached to each session. `None` means this
    /// provider cannot attach one, which is only workable if the templates
    /// carry their own.
    pub data_storage: Option<String>,
    /// Where that disk hangs. The templates boot from `virtio0`, so the data
    /// disk goes on the next VirtIO slot unless a site says otherwise.
    pub data_bus: String,
    pub tls: Tls,
    /// How long to wait for an asynchronous operation before giving up on
    /// knowing its outcome. Generous by default: a full-copy clone on storage
    /// without snapshots takes minutes.
    pub task_timeout: Duration,
    pub request_timeout: Duration,
}

#[derive(Debug)]
pub struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    api: String,
    node: String,
    pool: String,
    id_range: [u32; 2],
    token_file: String,
    data_storage: Option<String>,
    data_bus: Option<String>,
    tls: String,
    ca_file: Option<String>,
    task_timeout: Option<String>,
    request_timeout: Option<String>,
}

pub fn from_table(table: &toml::Value) -> Result<Config, ConfigError> {
    let err = |m: String| ConfigError(m);

    let raw: Raw = table
        .clone()
        .try_into()
        .map_err(|e| err(format!("[proxmox]: {e}")))?;

    let api = raw.api.trim_end_matches('/').to_string();
    check_endpoint(&api).map_err(err)?;

    if raw.node.trim().is_empty() {
        return Err(err("[proxmox].node is empty".into()));
    }

    // The token can allocate nowhere but this pool, so a create without it
    // fails with a permission error that reads like a bug in reaper. Refusing
    // here turns that into a sentence someone can act on.
    if raw.pool.trim().is_empty() {
        return Err(err(
            "[proxmox].pool is empty; every machine reaper creates must be placed \
             in a pool, and the credential is unlikely to be allowed to allocate \
             outside one"
                .into(),
        ));
    }

    let ids = IdRange::new(raw.id_range[0], raw.id_range[1])
        .map_err(|e| err(format!("[proxmox].id_range: {e}")))?;

    let tls = match raw.tls.as_str() {
        "webpki" => Tls::Webpki,
        "ca-file" => {
            let path = raw.ca_file.as_deref().ok_or_else(|| {
                err("[proxmox].tls is \"ca-file\" but no [proxmox].ca_file is set".into())
            })?;
            let path = expand_tilde(path);
            if !path.is_file() {
                return Err(err(format!(
                    "[proxmox].ca_file {} does not exist",
                    path.display()
                )));
            }
            Tls::CaFile(path)
        }
        "insecure" => Tls::Insecure,
        other => {
            return Err(err(format!(
                "[proxmox].tls is {other:?}; expected \"webpki\", \"ca-file\" or \"insecure\""
            )))
        }
    };

    // A ca_file that nothing reads is a person believing their traffic is
    // verified when it is not, which is the most expensive way to be wrong here.
    if raw.ca_file.is_some() && !matches!(tls, Tls::CaFile(_)) {
        return Err(err(
            "[proxmox].ca_file is set but [proxmox].tls does not use it; \
             one of the two is not what you meant"
                .into(),
        ));
    }

    let parse_dur = |name: &str, v: Option<&String>, default: &str| {
        dur::parse(v.map(String::as_str).unwrap_or(default))
            .map_err(|e| ConfigError(format!("[proxmox].{name}: {e}")))
    };

    Ok(Config {
        api,
        node: raw.node,
        pool: raw.pool,
        ids,
        token_file: expand_tilde(&raw.token_file),
        data_storage: raw.data_storage,
        data_bus: raw.data_bus.unwrap_or_else(|| "virtio1".to_string()),
        tls,
        task_timeout: parse_dur("task_timeout", raw.task_timeout.as_ref(), "10m")?,
        request_timeout: parse_dur("request_timeout", raw.request_timeout.as_ref(), "30s")?,
    })
}

/// Plain HTTP is refused unless the host is loopback.
///
/// A credential travelling in a header over a plaintext link to another machine
/// is a credential you have given away. Loopback is exempt because nothing
/// leaves the machine -- which is also what lets the test suite drive the real
/// client against a local server rather than a stubbed-out one.
fn check_endpoint(api: &str) -> Result<(), String> {
    let Some((scheme, rest)) = api.split_once("://") else {
        return Err(format!("[proxmox].api {api:?} has no scheme"));
    };

    match scheme {
        "https" => Ok(()),
        "http" => {
            let host = host_of(rest);
            if is_loopback(host) {
                Ok(())
            } else {
                Err(format!(
                    "[proxmox].api {api:?} is plain HTTP to {host:?}; the API token \
                     would travel in clear text. Use https, or loopback for testing"
                ))
            }
        }
        other => Err(format!("[proxmox].api has scheme {other:?}; expected http or https")),
    }
}

/// The host out of an authority, IPv6 literals included.
///
/// Splitting on the first colon is the obvious approach and it is wrong: the
/// colons inside `[::1]` are part of the address, not a port separator.
fn host_of(rest: &str) -> &str {
    let authority = rest.split('/').next().unwrap_or("");
    match authority.strip_prefix('[') {
        Some(after) => after.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    }
}

fn is_loopback(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// Read the API token.
///
/// The file holds the whole credential -- `user@realm!name=secret` -- so that
/// there is exactly one secret in exactly one place, rather than an identifier
/// in configuration that must be kept in step with a secret beside it.
pub fn read_token(path: &std::path::Path) -> Result<String, ConfigError> {
    let meta = std::fs::metadata(path)
        .map_err(|e| ConfigError(format!("token file {}: {e}", path.display())))?;

    check_permissions(&meta, path)?;

    let text = std::fs::read_to_string(path)
        .map_err(|e| ConfigError(format!("token file {}: {e}", path.display())))?;
    let token = text.trim().to_string();

    if token.is_empty() {
        return Err(ConfigError(format!("token file {} is empty", path.display())));
    }
    // Shape check only. Whether the credential is *accepted* is the server's
    // business, but a token missing its identifier will produce a 401 that
    // sends the reader hunting for a permissions problem they do not have.
    if !token.contains('!') || !token.contains('=') {
        return Err(ConfigError(format!(
            "token file {} does not look like `user@realm!name=secret`",
            path.display()
        )));
    }
    if token.contains(char::is_whitespace) {
        return Err(ConfigError(format!(
            "token file {} contains whitespace inside the credential",
            path.display()
        )));
    }

    Ok(token)
}

#[cfg(unix)]
fn check_permissions(meta: &std::fs::Metadata, path: &std::path::Path) -> Result<(), ConfigError> {
    use std::os::unix::fs::MetadataExt;
    // The same rule ssh applies to a private key, for the same reason: a
    // credential others can read is a credential others have.
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(ConfigError(format!(
            "token file {} is mode {:03o}; it is readable by others. \
             chmod 600 it rather than letting reaper use it",
            path.display(),
            mode
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_: &std::fs::Metadata, _: &std::path::Path) -> Result<(), ConfigError> {
    Ok(())
}
