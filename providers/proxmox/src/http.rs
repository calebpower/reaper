//! The HTTP client: authentication, TLS policy, and the API envelope.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reaper_core::provider::ProviderError;
use serde_json::Value;
use ureq::tls::{Certificate, RootCerts, TlsConfig};
use ureq::Agent;

use crate::config::{Config, Tls};

type Result<T> = std::result::Result<T, ProviderError>;

pub struct Client {
    agent: Agent,
    base: String,
    token: String,
}

/// Warn once per process rather than once per request. A warning printed forty
/// times in a session is a warning nobody reads.
static INSECURE_WARNED: AtomicBool = AtomicBool::new(false);

impl Client {
    pub fn new(config: &Config, token: String) -> Result<Client> {
        let tls = match &config.tls {
            Tls::Webpki => TlsConfig::builder().root_certs(RootCerts::WebPki).build(),
            Tls::CaFile(path) => {
                let pem = std::fs::read(path).map_err(|e| {
                    ProviderError::Config(format!("cannot read ca_file {}: {e}", path.display()))
                })?;
                // Every certificate in the file, not just the first: a ca_file
                // is routinely a bundle (a rotation in progress, or an
                // intermediate stapled ahead of the root), and silently
                // trusting only block one fails the handshake with a message
                // that blames the wrong thing.
                let mut certs = Vec::new();
                for block in pem_blocks(&String::from_utf8_lossy(&pem)) {
                    let cert = Certificate::from_pem(block.as_bytes()).map_err(|e| {
                        ProviderError::Config(format!(
                            "ca_file {} holds a block that is not a certificate: {e}",
                            path.display()
                        ))
                    })?;
                    certs.push(cert);
                }
                if certs.is_empty() {
                    return Err(ProviderError::Config(format!(
                        "ca_file {} holds no certificate at all",
                        path.display()
                    )));
                }
                TlsConfig::builder()
                    .root_certs(RootCerts::new_with_certs(&certs))
                    .build()
            }
            Tls::Insecure => {
                // Loud, every process, on stderr. The whole reason this mode is
                // tolerable is that nobody can forget they are in it.
                if !INSECURE_WARNED.swap(true, Ordering::SeqCst) {
                    eprintln!(
                        "reaper: WARNING: TLS certificate verification is disabled for {}. \
                         Anyone between here and there can read the API token and rewrite \
                         the replies. Set tls = \"ca-file\" once you have the node's CA.",
                        config.api
                    );
                }
                TlsConfig::builder().disable_verification(true).build()
            }
        };

        let agent: Agent = Agent::config_builder()
            .tls_config(tls)
            .timeout_global(Some(config.request_timeout))
            // Read the body of a refusal rather than throwing it away: the API
            // puts the reason in there, and "403" on its own has never helped
            // anybody.
            .http_status_as_error(false)
            .build()
            .into();

        Ok(Client {
            agent,
            base: format!("{}/api2/json", config.api),
            token,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    pub fn get(&self, path: &str) -> Result<Value> {
        let resp = self
            .agent
            .get(self.url(path))
            .header("Authorization", self.auth())
            .call();
        self.envelope(resp, path)
    }

    pub fn post_form(&self, path: &str, form: &[(&str, String)]) -> Result<Value> {
        let resp = self
            .agent
            .post(self.url(path))
            .header("Authorization", self.auth())
            .send_form(form.iter().map(|(k, v)| (*k, v.as_str())));
        self.envelope(resp, path)
    }

    pub fn put_form(&self, path: &str, form: &[(&str, String)]) -> Result<Value> {
        let resp = self
            .agent
            .put(self.url(path))
            .header("Authorization", self.auth())
            .send_form(form.iter().map(|(k, v)| (*k, v.as_str())));
        self.envelope(resp, path)
    }

    pub fn delete(&self, path: &str) -> Result<Value> {
        let resp = self
            .agent
            .delete(self.url(path))
            .header("Authorization", self.auth())
            .call();
        self.envelope(resp, path)
    }

    fn auth(&self) -> String {
        // Case-sensitive, and the API says nothing useful when it is wrong.
        format!("PVEAPIToken={}", self.token)
    }

    /// Unwrap the `{"data": ...}` envelope, turning transport and status
    /// failures into errors the core knows how to reason about.
    fn envelope(
        &self,
        resp: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
        path: &str,
    ) -> Result<Value> {
        let mut resp = resp.map_err(|e| match e {
            ureq::Error::Timeout(_) => ProviderError::Timeout(format!("{path}: {e}")),
            other => ProviderError::Transport(format!("{path}: {other}")),
        })?;

        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .read_to_string()
            .unwrap_or_else(|e| format!("<unreadable body: {e}>"));

        if !(200..300).contains(&status) {
            let message = format!("{path}: {}", body.trim());
            return Err(match status {
                401 | 403 => ProviderError::Unauthorized(message),
                404 => ProviderError::NotFound(message),
                _ => ProviderError::Api { status, message },
            });
        }

        let parsed: Value = serde_json::from_str(&body).map_err(|e| ProviderError::Api {
            status,
            message: format!("{path}: reply was not JSON: {e}"),
        })?;

        parsed
            .get("data")
            .cloned()
            .ok_or_else(|| ProviderError::Api {
                status,
                message: format!("{path}: reply carried no data field"),
            })
    }
}

/// The PEM blocks of a bundle, each with its BEGIN/END lines intact.
pub(crate) fn pem_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if line.starts_with("-----BEGIN ") {
            current = Some(String::new());
        }
        if let Some(b) = current.as_mut() {
            b.push_str(line);
            b.push('\n');
        }
        if line.starts_with("-----END ") {
            if let Some(b) = current.take() {
                blocks.push(b);
            }
        }
    }
    blocks
}

/// Wait for an asynchronous operation to finish.
///
/// Proxmox answers most mutations with a task handle rather than a result, so
/// "the call returned" and "the thing happened" are different events, and only
/// the second one matters.
pub fn wait_for_task(
    client: &Client,
    node: &str,
    task: &str,
    timeout: Duration,
    poll: Duration,
) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    let path = format!("/nodes/{node}/tasks/{task}/status");

    // A single failed status poll used to abort the whole wait -- reporting
    // an operation as failed whose task was still running, and in create()'s
    // case leaking the untagged clone. The deadline loop exists to absorb
    // time, so it absorbs a few bad polls too; only a *persistently*
    // unreachable API is worth giving up on early.
    let mut consecutive_failures = 0u32;

    loop {
        let data = match client.get(&path) {
            Ok(d) => {
                consecutive_failures = 0;
                d
            }
            Err(e) => {
                consecutive_failures += 1;
                if consecutive_failures >= 3 || std::time::Instant::now() >= deadline {
                    return Err(e);
                }
                std::thread::sleep(poll);
                continue;
            }
        };
        let status = data.get("status").and_then(Value::as_str).unwrap_or("");

        if status == "stopped" {
            let exit = data.get("exitstatus").and_then(Value::as_str).unwrap_or("");
            // Proxmox reports success as the literal "OK"; everything else is
            // the reason it failed, and is worth passing along verbatim.
            return if exit == "OK" {
                Ok(())
            } else {
                Err(ProviderError::Api {
                    status: 200,
                    message: format!("task {task} finished with: {exit}"),
                })
            };
        }

        if std::time::Instant::now() >= deadline {
            // Deliberately *not* a cue to clean up. A timeout means the outcome
            // is unknown -- the operation may well still be running -- and
            // destroying things on a guess is how you delete somebody else's
            // machine. The expiry tag and the sweeper cover this case.
            return Err(ProviderError::Timeout(format!(
                "task {task} was still {status:?} after {}s; leaving it alone, since \
                 its outcome is unknown. The expiry tag means nothing is leaked",
                timeout.as_secs()
            )));
        }

        std::thread::sleep(poll);
    }
}
