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

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::{Duration, SystemTime};

use reaper_core::provider::{
    CreateRequest, Finding, Health, MachineRef, MachineSummary, Provider, ProviderError,
    RegisteredGuest, Result,
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

    /// Does this machine still exist, as far as we are concerned?
    ///
    /// Not as simple as asking. For a machine that no longer exists the API
    /// answers 403, not 404 -- the ACL check on /vms/<id> runs before the
    /// existence check, and there is no ACL entry for a machine that is gone.
    /// A refusal is therefore ambiguous: gone, or a credential problem.
    ///
    /// It is disambiguated by consulting the listing, which uses a different
    /// permission. If the listing works, the credential is fine, and a machine
    /// absent from it is genuinely gone. If the listing itself fails, the
    /// original refusal is propagated rather than guessed at -- reporting a
    /// live machine as gone would drop the session that is the only convenient
    /// record of it.
    fn still_exists(&self, id: u32) -> Result<bool> {
        match self
            .client
            .get(&format!("/nodes/{}/qemu/{id}/status/current", self.node()))
        {
            Ok(_) => Ok(true),
            Err(e @ (ProviderError::NotFound(_) | ProviderError::Unauthorized(_))) => {
                let listed = self.occupied_in_range()?;
                if listed.contains(&id) {
                    Err(e)
                } else {
                    Ok(false)
                }
            }
            Err(e) => Err(e),
        }
    }

    /// The 403 trap, in one place: on a pool-scoped token every per-VM route
    /// answers a *gone* machine with "Permission check failed", because the
    /// ACL check precedes the existence check. Any refusal for a machine the
    /// cluster listing no longer shows means gone, not forbidden. Observed
    /// live during acceptance; address() and destroy() learned it first, and
    /// this is what holds their siblings to the same reading.
    fn disambiguate(&self, id: u32, e: ProviderError) -> ProviderError {
        if matches!(e, ProviderError::NotFound(_) | ProviderError::Unauthorized(_)) {
            match self.occupied_in_range() {
                Ok(listed) if !listed.contains(&id) => {
                    return ProviderError::NotFound(format!("machine {id} no longer exists"));
                }
                _ => {}
            }
        }
        e
    }

    fn is_running(&self, id: u32) -> Result<bool> {
        let s = self
            .client
            .get(&format!("/nodes/{}/qemu/{id}/status/current", self.node()))?;
        Ok(s.get("status").and_then(Value::as_str) == Some("running"))
    }

    /// Wait for a machine to actually be stopped.
    ///
    /// The stop task completing and the machine being stopped are not quite the
    /// same instant, and deleting in the gap fails.
    fn await_stopped(&self, id: u32) -> Result<()> {
        let deadline = std::time::Instant::now() + self.config.task_timeout;
        loop {
            if !self.is_running(id)? {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(ProviderError::Timeout(format!(
                    "machine {id} was still running {}s after being asked to stop",
                    self.config.task_timeout.as_secs()
                )));
            }
            std::thread::sleep(self.poll_interval);
        }
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

    /// Refuse a session that would leave a storage too full.
    ///
    /// What a session costs is the template's disks copied whole -- this
    /// storage has no snapshots, so a clone is a byte-for-byte copy -- plus the
    /// blank pool disk. Those can land on different storages, so the need is
    /// totalled per storage and each is checked against its own free space.
    fn check_room(&self, template: u32, data_disk_gb: Option<u32>) -> Result<()> {
        let mut need: BTreeMap<String, u64> = BTreeMap::new();

        let config = self
            .client
            .get(&format!("/nodes/{}/qemu/{template}/config", self.node()))?;
        for (key, value) in config.as_object().into_iter().flatten() {
            if !is_disk_key(key) {
                continue;
            }
            let Some(spec) = value.as_str() else { continue };
            match disk_storage_and_size(spec) {
                Some((storage, bytes)) => *need.entry(storage).or_default() += bytes,
                // A disk that cannot be priced must not silently cost zero:
                // the whole check approves sessions on these numbers.
                None if !spec.contains("media=cdrom") => eprintln!(
                    "reaper: could not price {key}={spec}; it is not counted toward the room this session needs"
                ),
                None => {}
            }
        }

        if let (Some(gb), Some(storage)) = (data_disk_gb, self.config.data_storage.as_ref()) {
            *need.entry(storage.clone()).or_default() += u64::from(gb) * GIB;
        }

        let floor = u64::from(self.config.min_free_gb) * GIB;
        for (storage, wanted) in need {
            // A storage that cannot be queried and one that answers without a
            // figure are the same thing here: we do not know. Both were not
            // handled at first -- only the second -- so an offline storage
            // refused the session while the comment below claimed it would not.
            let reported = self
                .client
                .get(&format!("/nodes/{}/storage/{storage}/status", self.node()))
                .ok()
                .and_then(|status| status.get("avail").and_then(Value::as_u64));

            let Some(avail) = reported else {
                // Not knowing is not the same as knowing there is room, but
                // refusing every session because one storage will not report
                // itself would be worse. Say so and carry on.
                eprintln!(
                    "reaper: {storage} did not report its free space; \
                     creating without checking there is room"
                );
                continue;
            };

            if avail < wanted + floor {
                return Err(ProviderError::Refused(format!(
                    "{storage} has {} free and this session needs {}, leaving less \
                     than the {} floor. Take a session down, or lower \
                     [proxmox].min_free_gb if you mean to run it close",
                    gib(avail),
                    gib(wanted),
                    gib(floor),
                )));
            }
        }
        Ok(())
    }

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

        // Pure-configuration refusals come before anything exists. This one
        // used to run after the clone, and the error return leaked an
        // untagged machine -- the one state nothing ever collects.
        if req.data_disk_gb.is_some() && self.config.data_storage.is_none() {
            return Err(ProviderError::Config(
                "a session disk was requested but [proxmox].data_storage is not set, so there is nowhere to put it"
                    .into(),
            ));
        }

        let id = self.free_id()?;

        // Before anything is created, not after. A clone that runs a shared
        // storage out of space takes down everything else living on it, and
        // the failure arrives minutes in, with a half-copied disk to clean up.
        self.check_room(template, req.data_disk_gb)?;

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
        let machine = MachineRef::new(id.to_string());
        if let Err(e) = self.wait(&task) {
            return Err(match e {
                // Unknown outcome: leave it alone, but do not repeat the
                // generic timeout's claim that the expiry tag covers this --
                // no tag has been applied yet, so if the clone does finish,
                // nothing will ever collect it.
                ProviderError::Timeout(t) => ProviderError::Timeout(format!(
                    "{t}. CAUTION: this was the clone making {id}, which has no expiry tag yet -- if it did finish, {id} exists and nothing will collect it. Check for {id} and destroy it by hand"
                )),
                // Known failure: the task itself said so, which licenses a
                // cleanup the way a timeout does not. PVE usually removes the
                // half-made target itself, in which case this is a NotFound
                // and there was nothing to do.
                failed => match self.destroy(&machine) {
                    Ok(()) | Err(ProviderError::NotFound(_)) => ProviderError::Api {
                        status: 0,
                        message: format!(
                            "cloning {template} into {id} failed ({failed}); nothing was left behind"
                        ),
                    },
                    Err(also) => ProviderError::Api {
                        status: 0,
                        message: format!(
                            "cloning {template} into {id} failed ({failed}), and the leftover could not be destroyed either ({also}). It carries no expiry, so nothing will collect it: destroy {id} by hand"
                        ),
                    },
                },
            });
        }

        // Expiry first, before anything else and before the machine is ever
        // started. Between the clone finishing and this succeeding the machine
        // is untagged, which is the one state nothing will ever clean up -- so
        // if this fails, destroy what we just made rather than leaving it.
        // (A *crash* in the same window leaves the untagged machine the sweeper
        // logs for a human. The two look identical afterwards and are not the
        // same thing.)
        // protection=0, explicitly and always.
        //
        // A clone inherits the template's protection flag, and templates are
        // rightly protected -- that is what stops a stray destroy taking the
        // thing every session is made from. But a protected session cannot be
        // destroyed by anything: not `down`, not the sweeper. Ephemeral
        // machines that cannot be removed defeat the entire design, so this is
        // cleared in the same call that sets the expiry, before the machine is
        // ever started.
        let mut settings = vec![
            ("tags", tags::encode(req.expires_at)),
            ("protection", "0".to_string()),
        ];
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
        let existing = self.tags_of(id).map_err(|e| self.disambiguate(id, e))?;
        let updated = tags::with_expiry(&existing, at);
        self.client
            .put_form(
                &format!("/nodes/{}/qemu/{id}/config", self.node()),
                &[("tags", updated)],
            )
            .map_err(|e| self.disambiguate(id, e))?;
        Ok(())
    }

    fn start(&self, machine: &MachineRef) -> Result<()> {
        let id = self.config.ids.check(machine)?;
        let task = self
            .client
            .post_form(
                &format!("/nodes/{}/qemu/{id}/status/start", self.node()),
                &[],
            )
            .map_err(|e| self.disambiguate(id, e))?;
        self.wait(&task)
    }

    fn stop(&self, machine: &MachineRef) -> Result<()> {
        let id = self.config.ids.check(machine)?;
        let task = self
            .client
            .post_form(&format!("/nodes/{}/qemu/{id}/status/stop", self.node()), &[])
            .map_err(|e| self.disambiguate(id, e))?;
        self.wait(&task)
    }

    fn destroy(&self, machine: &MachineRef) -> Result<()> {
        let id = self.config.ids.check(machine)?;

        // Already gone is a success, not a failure. The usual reason is the
        // happy one: the sweeper collected an expired machine, doing exactly
        // its job. Reporting that as an error leaves an operator holding a
        // session they cannot clear.
        if !self.still_exists(id)? {
            return Err(ProviderError::NotFound(format!(
                "machine {id} no longer exists"
            )));
        }

        // Stop first if it is running. The API refuses to delete a running
        // machine, so a destroy that did not do this failed every time it was
        // asked to clean up a live session -- which is every time it matters.
        // The sweeper has always worked this way; the two now agree.
        if self.is_running(id)? {
            self.stop(machine)?;
            self.await_stopped(id)?;
        }

        // Same parameters the sweeper uses, deliberately. Disks named in the
        // configuration go either way, so this is not a leak being fixed --
        // but destroy-unreferenced-disks is what clears a disk left behind by
        // a create that failed between attaching one and recording it, and the
        // backstop and the thing it backs up should not disagree about what
        // destroying means.
        let task = self.client.delete(&format!(
            "/nodes/{}/qemu/{id}?purge=1&destroy-unreferenced-disks=1",
            self.node()
        ))?;
        self.wait(&task)
    }

    fn address(&self, machine: &MachineRef) -> Result<Option<IpAddr>> {
        let id = self.config.ids.check(machine)?;
        let path = format!("/nodes/{}/qemu/{id}/agent/network-get-interfaces", self.node());

        let data = match self.client.get(&path) {
            Ok(d) => d,
            // Before the guest agent is up, asking is not an error -- it is
            // the ordinary state of a machine that has only just been started.
            // Observed live: "QEMU guest agent is not running" while booting,
            // and "VM <id> is not running" for a machine that is (perhaps
            // momentarily) stopped. Neither has an address to report yet.
            Err(ProviderError::Api { message, .. })
                if message.contains("guest agent") || message.contains("is not running") =>
            {
                return Ok(None);
            }
            // A machine that is *gone* answers differently depending on the
            // token: a pool-scoped one gets 403, because the ACL check
            // precedes the existence check (the same trap still_exists
            // documents); a root-ish one gets a 500 naming the missing
            // configuration file. Text alone cannot be trusted for the 403 --
            // it reads like a permission problem -- so the cluster listing
            // disambiguates, exactly as it does for destroy.
            Err(e @ (ProviderError::NotFound(_) | ProviderError::Unauthorized(_))) => {
                if self.still_exists(id)? {
                    return Err(e);
                }
                return Err(ProviderError::NotFound(format!(
                    "machine {id} no longer exists"
                )));
            }
            Err(ProviderError::Api { message, .. })
                if message.contains("does not exist") =>
            {
                return Err(ProviderError::NotFound(message));
            }
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

    /// Staged deliberately: on a pool-scoped token the ACL check precedes the
    /// existence check, so one bad credential yields refusals on everything
    /// downstream. The stages that depend on a failed one report "skipped"
    /// rather than piling on findings that all mean the same broken thing.
    ///
    /// Every probe here uses only what the live token has been proven to
    /// answer -- /version, the cluster listing, per-VM config/status -- and
    /// deliberately not /nodes/<n>/status or /pools/<p>, which need Sys.Audit
    /// and Pool.Audit grants no site is required to have given. The node's
    /// health is evidenced through the template checks, whose requests all
    /// cross it.
    fn diagnose(&self, guests: &[RegisteredGuest]) -> Vec<Finding> {
        let mut out = Vec::new();
        let mut push = |label: &str, health: Health, detail: String| {
            out.push(Finding { label: label.into(), health, detail })
        };
        let skipped = |what: &str| (Health::Warn, format!("skipped: {what}"));

        // Stage 1: is anyone there, and do they know us.
        match self.client.get("/version") {
            Ok(v) => {
                let version = v.get("version").and_then(Value::as_str).unwrap_or("?");
                push("api", Health::Ok, format!(
                    "{} answers (version {version}) and accepts the credential",
                    self.config.api
                ));
            }
            Err(ProviderError::Unauthorized(m)) => {
                push("api", Health::Fail, format!(
                    "{} answers but refuses the credential: {m}",
                    self.config.api
                ));
                let (h, d) = skipped("the credential is refused");
                push("pool and sweeper", h, d.clone());
                push("templates", h, d.clone());
                push("storage", h, d);
                return out;
            }
            Err(e) => {
                push("api", Health::Fail, format!(
                    "{} did not answer: {e}. Nothing past this point could be checked",
                    self.config.api
                ));
                let (h, d) = skipped("the API is unreachable");
                push("pool and sweeper", h, d.clone());
                push("templates", h, d.clone());
                push("storage", h, d);
                return out;
            }
        }

        // Stage 2: the cluster listing, read once; pool visibility and pool
        // hygiene both come from it -- the one view this token provably has.
        let listing = match self.client.get("/cluster/resources?type=vm") {
            Ok(d) => d.as_array().cloned().unwrap_or_default(),
            Err(e) => {
                push("pool", Health::Fail, format!("the cluster listing failed: {e}"));
                let (h, d) = skipped("the cluster listing failed");
                push("templates", h, d.clone());
                push("storage", h, d);
                self.diagnose_tls(&mut |l, h, d| push(l, h, d));
                return out;
            }
        };

        let in_pool: Vec<&Value> = listing
            .iter()
            .filter(|i| i.get("pool").and_then(Value::as_str) == Some(self.config.pool.as_str()))
            .collect();
        if in_pool.is_empty() {
            push("pool", Health::Fail, format!(
                "pool {} has no members visible to this token -- either it does not exist, it is empty of even templates, or the token cannot see it",
                self.config.pool
            ));
        } else {
            push("pool", Health::Ok, format!(
                "pool {} is visible with {} member(s)",
                self.config.pool,
                in_pool.len()
            ));
        }

        // Pool hygiene: the states that want a human.
        let now = SystemTime::now();
        let mut expired_seen = false;
        for item in &in_pool {
            let Some(id) = item.get("vmid").and_then(Value::as_u64).map(|v| v as u32) else {
                continue;
            };
            let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
            if item.get("template").and_then(Value::as_u64).unwrap_or(0) == 1 {
                continue;
            }
            if !self.config.ids.contains(id) {
                push("pool hygiene", Health::Warn, format!(
                    "{id} ({name}) is in pool {} but outside {} -- the sweeper refuses to touch it by design, so it is somebody's by hand",
                    self.config.pool, self.config.ids
                ));
                continue;
            }
            let tag_string = item.get("tags").and_then(Value::as_str).unwrap_or("");
            match tags::expiry_of(tag_string) {
                None => push("pool hygiene", Health::Warn, format!(
                    "{id} ({name}) carries no expiry tag: nothing will ever collect it, and it wants a human"
                )),
                Some(at) => {
                    if let Ok(past) = now.duration_since(at) {
                        expired_seen = true;
                        if past > self.config.sweep_within {
                            push("sweeper", Health::Fail, format!(
                                "{id} ({name}) expired {}s ago and is still here, which is longer than sweep_within allows: the sweeper does not appear to be collecting",
                                past.as_secs()
                            ));
                        } else {
                            push("sweeper", Health::Ok, format!(
                                "{id} ({name}) is expired and inside the {}s sweep_within window; the sweeper has time",
                                self.config.sweep_within.as_secs()
                            ));
                        }
                    }
                }
            }
        }
        if !expired_seen {
            push("sweeper", Health::Warn,
                "no expired machine is present, so there is no evidence the sweeper works -- and none that it does not. Only a canary can answer this on a clean pool".to_string());
        }

        // Stage 3: every registered template, deduplicated -- findings
        // attributed per template, guests named together. Storage needs are
        // collected for stage 4.
        let mut storages: std::collections::BTreeMap<String, u64> =
            std::collections::BTreeMap::new();
        let mut checked: std::collections::BTreeMap<&str, Vec<&str>> =
            std::collections::BTreeMap::new();
        for g in guests {
            checked.entry(g.template.as_str()).or_default().push(g.name.as_str());
        }
        for (template, guest_names) in checked {
            let whose = guest_names.join(", ");
            let id = match self.config.ids.check(&MachineRef::new(template)) {
                Ok(id) => id,
                Err(e) => {
                    push("template", Health::Fail, format!(
                        "template {template} (guest {whose}) is not usable: {e}"
                    ));
                    continue;
                }
            };
            let listed = listing.iter().find(|i| {
                i.get("vmid").and_then(Value::as_u64) == Some(u64::from(id))
            });
            match listed {
                None => {
                    push("template", Health::Fail, format!(
                        "template {id} (guest {whose}) does not exist on the cluster"
                    ));
                    continue;
                }
                Some(item)
                    if item.get("template").and_then(Value::as_u64).unwrap_or(0) != 1 =>
                {
                    push("template", Health::Fail, format!(
                        "{id} (guest {whose}) exists but is not a template -- a clone would copy a machine somebody may be using"
                    ));
                    continue;
                }
                Some(_) => {}
            }
            match self.client.get(&format!("/nodes/{}/qemu/{id}/config", self.node())) {
                Ok(cfg) => {
                    let mut disks = 0;
                    let mut unpriceable = Vec::new();
                    for (key, value) in cfg.as_object().into_iter().flatten() {
                        if !is_disk_key(key) {
                            continue;
                        }
                        let Some(spec) = value.as_str() else { continue };
                        match disk_storage_and_size(spec) {
                            Some((storage, bytes)) => {
                                disks += 1;
                                let e = storages.entry(storage).or_default();
                                *e = (*e).max(bytes);
                            }
                            None if !spec.contains("media=cdrom") => {
                                unpriceable.push(key.clone())
                            }
                            None => {}
                        }
                    }
                    if disks == 0 {
                        push("template", Health::Fail, format!(
                            "template {id} (guest {whose}) has no priceable disk: a clone of it would boot nothing"
                        ));
                    } else if !unpriceable.is_empty() {
                        push("template", Health::Warn, format!(
                            "template {id} (guest {whose}): {} disk(s) priceable, but {} could not be priced and will not count toward room checks",
                            disks,
                            unpriceable.join(", ")
                        ));
                    } else {
                        push("template", Health::Ok, format!(
                            "template {id} (guest {whose}) exists, is a template, and its {disks} disk(s) price cleanly"
                        ));
                    }
                }
                Err(e) => push("template", Health::Fail, format!(
                    "template {id} (guest {whose}): its configuration could not be read: {}",
                    self.disambiguate(id, e)
                )),
            }
        }

        // Stage 4: the storages those templates and the data disk live on.
        if let Some(ds) = self.config.data_storage.as_ref() {
            storages.entry(ds.clone()).or_default();
        } else {
            push("storage", Health::Fail,
                "data_storage is not set, and every session asks for a data disk -- up would refuse each one".to_string());
        }
        let floor = u64::from(self.config.min_free_gb) * GIB;
        for (storage, template_need) in storages {
            match self
                .client
                .get(&format!("/nodes/{}/storage/{storage}/status", self.node()))
            {
                Ok(status) => match status.get("avail").and_then(Value::as_u64) {
                    Some(avail) if avail < floor + template_need => {
                        push("storage", Health::Fail, format!(
                            "{storage} has {} free, under the {} floor plus the {} a clone would take: sessions will be refused",
                            gib(avail), gib(floor), gib(template_need)
                        ))
                    }
                    Some(avail) => push("storage", Health::Ok, format!(
                        "{storage} has {} free ({} floor)",
                        gib(avail), gib(floor)
                    )),
                    None => push("storage", Health::Warn, format!(
                        "{storage} answered without a free-space figure; room checks will proceed unchecked against it"
                    )),
                },
                Err(e) => push("storage", Health::Fail, format!(
                    "{storage} did not answer: {e}"
                )),
            }
        }

        self.diagnose_tls(&mut |l, h, d| push(l, h, d));
        out
    }
}

impl Proxmox {
    fn diagnose_tls(&self, push: &mut dyn FnMut(&str, Health, String)) {
        match &self.config.tls {
            crate::config::Tls::Insecure => push("tls", Health::Warn,
                "certificate verification is disabled: anyone between here and the node can read the token and rewrite replies. Export the node's CA and set tls = \"ca-file\"".to_string()),
            _ => push("tls", Health::Ok, "certificate verification is on".to_string()),
        }
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


const GIB: u64 = 1024 * 1024 * 1024;

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / GIB as f64)
}

/// Disk keys, and only disk keys. `virtio0` is one; `virtiofs0` is not, and
/// neither is anything else that merely starts with a bus name.
fn is_disk_key(key: &str) -> bool {
    // efidisk, tpmstate and unused volumes are copied by a full clone just
    // as the bus disks are; leaving them out made every template look
    // slightly cheaper than it is (and an unused0 can be arbitrarily large).
    for bus in ["virtio", "scsi", "sata", "ide", "efidisk", "tpmstate", "unused"] {
        if let Some(rest) = key.strip_prefix(bus) {
            return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
        }
    }
    false
}

/// The storage a disk lives on, and how much room it takes.
///
/// A spec reads `storage:vm-9001-disk-0,iothread=1,size=8G`. A `cdrom` is not a
/// disk that gets copied, and `none`/`cloudinit` entries have no size at all.
fn disk_storage_and_size(spec: &str) -> Option<(String, u64)> {
    if spec.contains("media=cdrom") {
        return None;
    }
    let storage = spec.split(':').next()?.to_string();
    if storage.is_empty() || storage == "none" {
        return None;
    }
    let size = spec
        .split(',')
        .find_map(|part| part.strip_prefix("size="))
        .and_then(parse_size)?;
    Some((storage, size))
}

/// `8G`, `512M`, `1T`, `4.5G`, or a bare byte count.
fn parse_size(text: &str) -> Option<u64> {
    let (digits, scale) = match text.chars().last()? {
        'K' | 'k' => (&text[..text.len() - 1], 1024u64),
        'M' | 'm' => (&text[..text.len() - 1], 1024 * 1024),
        'G' | 'g' => (&text[..text.len() - 1], GIB),
        'T' | 't' => (&text[..text.len() - 1], GIB * 1024),
        _ => (text, 1),
    };
    // Through f64, because PVE will happily write `4.5G`; disk sizes are far
    // inside f64's exact-integer range. Guarded, because `n * scale` on an
    // absurd digit string would wrap in release and price a disk at almost
    // nothing.
    let n: f64 = digits.trim().parse().ok()?;
    let bytes = n * scale as f64;
    if !(0.0..=(u64::MAX / 2) as f64).contains(&bytes) {
        return None;
    }
    Some(bytes.ceil() as u64)
}
