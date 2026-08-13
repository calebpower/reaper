//! The Proxmox VE provider.
//!
//! Everything Proxmox-shaped lives behind this crate boundary: numeric machine
//! identifiers and the range reaper may touch, resource pools, asynchronous
//! task handles, API token authentication and TLS policy. A lint guard fails
//! the build if any of that vocabulary appears in the core or the CLI.
//!
//! The contract this satisfies is written out in `docs/providers.md`.

pub mod config;
pub mod http;
/// A stand-in API for tests. Behind a feature so that other crates -- the CLI,
/// principally -- can drive the whole stack against a fake hypervisor without
/// it ever being compiled into a shipped binary.
#[cfg(any(test, feature = "mock"))]
pub mod mock;
pub mod ids;
pub mod tags;

use std::net::IpAddr;
use std::time::{Duration, SystemTime};

use reaper_core::provider::{
    CreateRequest, MachineRef, MachineSummary, Provider, ProviderError, Result,
};
use serde_json::Value;

pub use config::Config;

pub struct Proxmox {
    config: Config,
    client: http::Client,
    poll_interval: Duration,
}

impl Proxmox {
    /// Build a provider from the `[proxmox]` table the core carried through.
    pub fn from_table(table: &toml::Value) -> Result<Proxmox> {
        let config = config::from_table(table).map_err(|e| ProviderError::Config(e.to_string()))?;
        let token = config::read_token(&config.token_file)
            .map_err(|e| ProviderError::Config(e.to_string()))?;
        Self::with_token(config, token)
    }

    pub fn with_token(config: Config, token: String) -> Result<Proxmox> {
        let client = http::Client::new(&config, token)?;
        Ok(Proxmox {
            config,
            client,
            poll_interval: Duration::from_secs(2),
        })
    }

    /// Poll faster than production would. For tests only, so that a suite
    /// exercising task handling does not spend its life asleep.
    pub fn set_poll_interval(&mut self, d: Duration) {
        self.poll_interval = d;
    }

    fn node(&self) -> &str {
        &self.config.node
    }

    fn wait(&self, task: &Value) -> Result<()> {
        let upid = task.as_str().ok_or_else(|| ProviderError::Api {
            status: 200,
            message: "expected a task handle, got something else".into(),
        })?;
        http::wait_for_task(
            &self.client,
            self.node(),
            upid,
            self.config.task_timeout,
            self.poll_interval,
        )
    }

    /// The machine's current tag string, or empty if it carries none.
    fn tags_of(&self, id: u32) -> Result<String> {
        let cfg = self
            .client
            .get(&format!("/nodes/{}/qemu/{id}/config", self.node()))?;
        Ok(cfg
            .get("tags")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    /// Choose an unused identifier inside the permitted range.
    ///
    /// Deliberately drawn from the range rather than from the API's
    /// next-free-id endpoint, which knows nothing about which identifiers are
    /// reaper's to use and would happily hand back one belonging to the wider
    /// cluster.
    fn free_id(&self) -> Result<u32> {
        // Every machine in range, whoever owns it. Identifiers are cluster-wide
        // in Proxmox, so an identifier held by another pool -- or by a template,
        // which `list` deliberately hides -- is still taken. Drawing from the
        // filtered view would hand back an identifier already in use, and the
        // clone would fail on a collision that reads like a race.
        let taken = self.occupied_in_range()?;
        self.config
            .ids
            .iter()
            .find(|id| !taken.contains(id))
            .ok_or_else(|| {
                ProviderError::Refused(format!(
                    "every identifier in {} is in use; nothing can be created until \
                     something is destroyed",
                    self.config.ids
                ))
            })
    }

    /// Every identifier in range that is spoken for, regardless of pool, and
    /// including templates.
    ///
    /// This is the allocation view, and it is deliberately wider than `list`:
    /// the question here is "what would collide", not "what is mine".
    fn occupied_in_range(&self) -> Result<Vec<u32>> {
        let data = self.client.get("/cluster/resources?type=vm")?;
        Ok(data
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|item| item.get("vmid").and_then(Value::as_u64))
            .map(|id| id as u32)
            .filter(|id| self.config.ids.contains(*id))
            .collect())
    }

    /// Machines in reaper's pool and inside its identifier range.
    ///
    /// Both conditions, always. The pool alone is not enough: this provider
    /// must never report, still less act on, a machine outside the range it is
    /// permitted to touch.
    fn all_in_pool(&self) -> Result<Vec<(u32, String, String, bool)>> {
        let data = self.client.get("/cluster/resources?type=vm")?;
        let items = data.as_array().cloned().unwrap_or_default();

        let mut out = Vec::new();
        for item in items {
            let Some(id) = item.get("vmid").and_then(Value::as_u64) else {
                continue;
            };
            let id = id as u32;
            if !self.config.ids.contains(id) {
                continue;
            }
            if item.get("pool").and_then(Value::as_str) != Some(self.config.pool.as_str()) {
                continue;
            }
            // A template is not a session; it is the thing sessions are made
            // from, and reporting it as one would invite something to destroy it.
            if item.get("template").and_then(Value::as_u64).unwrap_or(0) == 1 {
                continue;
            }

            out.push((
                id,
                item.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                item.get("tags")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                item.get("status").and_then(Value::as_str) == Some("running"),
            ));
        }
        Ok(out)
    }
}

impl Provider for Proxmox {
    fn name(&self) -> &'static str {
        "proxmox"
    }

    fn create(&self, req: &CreateRequest) -> Result<MachineRef> {
        // The template is an identifier too, and cloning *from* outside the
        // permitted range is just as wrong as cloning into it.
        let template = self
            .config
            .ids
            .check(&MachineRef::new(req.template.clone()))
            .map_err(|e| {
                ProviderError::Refused(format!(
                    "the registered template {:?} is not usable: {e}",
                    req.template
                ))
            })?;

        let id = self.free_id()?;

        // pool is always sent. The credential can allocate nowhere else, and
        // omitting it produces a permission error that reads like a bug.
        let form = vec![
            ("newid", id.to_string()),
            ("name", sanitize_name(&req.name)),
            ("pool", self.config.pool.clone()),
            ("full", "1".to_string()),
        ];
        let task = self.client.post_form(
            &format!("/nodes/{}/qemu/{template}/clone", self.node()),
            &form,
        )?;
        self.wait(&task)?;

        let machine = MachineRef::new(id.to_string());

        // Expiry first, before anything else and before the machine is ever
        // started. Between the clone finishing and this succeeding the machine
        // is untagged, which is the one state nothing will ever clean up -- so
        // if this fails, destroy what we just made rather than leaving it.
        // (A *crash* in the same window leaves the untagged machine the sweeper
        // logs for a human. The two look identical afterwards and are not the
        // same thing.)
        let mut settings = vec![("tags", tags::encode(req.expires_at))];
        if let Some(cores) = req.cores {
            settings.push(("cores", cores.to_string()));
        }
        if let Some(ram) = req.ram_gb {
            settings.push(("memory", (u64::from(ram) * 1024).to_string()));
        }
        // The blank disk rides along in the same call as the expiry, so it
        // costs nothing extra and does not widen the window in which the
        // machine exists without a tag.
        if let Some(gb) = req.data_disk_gb {
            let storage = self.config.data_storage.as_ref().ok_or_else(|| {
                ProviderError::Config(format!(
                    "a {gb} GiB session disk was requested but [proxmox].data_storage \
                     is not set, so there is nowhere to put it"
                ))
            })?;
            settings.push((
                self.config.data_bus.as_str(),
                format!("{storage}:{gb}"),
            ));
        }

        if let Err(tag_failure) = self
            .client
            .put_form(&format!("/nodes/{}/qemu/{id}/config", self.node()), &settings)
        {
            let cleanup = self.destroy(&machine);
            return Err(match cleanup {
                Ok(()) => ProviderError::Api {
                    status: 0,
                    message: format!(
                        "machine {id} was created but could not be given an expiry \
                         ({tag_failure}); it has been destroyed"
                    ),
                },
                Err(also) => ProviderError::Api {
                    status: 0,
                    message: format!(
                        "machine {id} was created, could not be given an expiry \
                         ({tag_failure}), and could not be destroyed either ({also}). \
                         It carries no expiry, so nothing will collect it: destroy {id} by hand"
                    ),
                },
            });
        }

        Ok(machine)
    }

    fn set_expiry(&self, machine: &MachineRef, at: SystemTime) -> Result<()> {
        let id = self.config.ids.check(machine)?;
        // Read-modify-write. This is a shared cluster and reaper is not the
        // only thing that writes tags; replacing the whole string would discard
        // somebody else's, invisibly.
        let existing = self.tags_of(id)?;
        let updated = tags::with_expiry(&existing, at);
        self.client.put_form(
            &format!("/nodes/{}/qemu/{id}/config", self.node()),
            &[("tags", updated)],
        )?;
        Ok(())
    }

    fn start(&self, machine: &MachineRef) -> Result<()> {
        let id = self.config.ids.check(machine)?;
        let task = self.client.post_form(
            &format!("/nodes/{}/qemu/{id}/status/start", self.node()),
            &[],
        )?;
        self.wait(&task)
    }

    fn stop(&self, machine: &MachineRef) -> Result<()> {
        let id = self.config.ids.check(machine)?;
        let task = self
            .client
            .post_form(&format!("/nodes/{}/qemu/{id}/status/stop", self.node()), &[])?;
        self.wait(&task)
    }

    fn destroy(&self, machine: &MachineRef) -> Result<()> {
        let id = self.config.ids.check(machine)?;
        let task = self
            .client
            .delete(&format!("/nodes/{}/qemu/{id}", self.node()))?;
        self.wait(&task)
    }

    fn address(&self, machine: &MachineRef) -> Result<Option<IpAddr>> {
        let id = self.config.ids.check(machine)?;
        let path = format!("/nodes/{}/qemu/{id}/agent/network-get-interfaces", self.node());

        let data = match self.client.get(&path) {
            Ok(d) => d,
            // Before the guest agent is up, asking is not an error -- it is the
            // ordinary state of a machine that has only just been started.
            Err(ProviderError::NotFound(_)) | Err(ProviderError::Api { .. }) => return Ok(None),
            Err(e) => return Err(e),
        };

        Ok(first_usable_address(&data))
    }

    fn list(&self) -> Result<Vec<MachineSummary>> {
        Ok(self
            .all_in_pool()?
            .into_iter()
            .map(|(id, name, tag_string, running)| MachineSummary {
                machine: MachineRef::new(id.to_string()),
                name,
                expires_at: tags::expiry_of(&tag_string),
                running,
            })
            .collect())
    }
}

/// Proxmox names must look like hostnames. Rather than reject a session name a
/// person chose, bend it into something acceptable and get on with it.
fn sanitize_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    out = out.trim_matches('-').to_string();
    if out.is_empty() {
        out.push_str("reaper");
    }
    out.truncate(63);
    out.trim_end_matches('-').to_string()
}

/// The first address a guest agent reports that is worth talking to.
///
/// Loopback and link-local are skipped: both are things every machine has and
/// neither is reachable from here, so returning one would look like success and
/// fail at the first connection.
fn first_usable_address(data: &Value) -> Option<IpAddr> {
    let interfaces = data
        .get("result")
        .and_then(Value::as_array)
        .or_else(|| data.as_array())?;

    let mut candidates: Vec<IpAddr> = Vec::new();
    for iface in interfaces {
        let Some(addrs) = iface.get("ip-addresses").and_then(Value::as_array) else {
            continue;
        };
        for a in addrs {
            let Some(ip) = a.get("ip-address").and_then(Value::as_str) else {
                continue;
            };
            let Ok(ip) = ip.parse::<IpAddr>() else {
                continue;
            };
            if ip.is_loopback() {
                continue;
            }
            if let IpAddr::V4(v4) = ip {
                if v4.is_link_local() {
                    continue;
                }
            }
            if let IpAddr::V6(v6) = ip {
                // No is_unicast_link_local on stable; the fe80::/10 test is the
                // same thing written out.
                if v6.segments()[0] & 0xffc0 == 0xfe80 {
                    continue;
                }
            }
            candidates.push(ip);
        }
    }

    // IPv4 first: every path this project uses today is v4, and preferring a
    // v6 address the tunnel cannot carry would be a confusing way to fail.
    candidates
        .iter()
        .find(|ip| ip.is_ipv4())
        .or_else(|| candidates.first())
        .copied()
}

#[cfg(test)]
mod tests;
