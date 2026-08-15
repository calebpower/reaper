//! A stand-in Proxmox API, over loopback.
//!
//! Deliberately a real HTTP server rather than a stubbed-out client. The whole
//! request path -- URL building, the authorization header, form encoding, the
//! `{"data": ...}` envelope, status handling, task polling -- is the part most
//! likely to be subtly wrong, and a fake that replaced it would test nothing
//! but the fake.
//!
//! It is also why plain HTTP to loopback is permitted by the provider's own
//! endpoint check: that rule exists so this can work without a certificate,
//! and it is defensible on its own terms.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

#[derive(Debug, Clone, Default)]
pub struct Vm {
    pub name: String,
    pub tags: String,
    pub running: bool,
    pub template: bool,
    pub pool: String,
    /// Proxmox copies this to clones. A protected machine cannot be destroyed.
    pub protection: bool,
    pub cores: Option<u32>,
    pub memory: Option<u64>,
    /// Disks attached after creation, as bus -> "<storage>:<gib>".
    pub extra_disks: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Task {
    pub status: String,
    pub exitstatus: String,
}

/// One name for the stand-in storage, so the configuration handed out and the
/// disk specs reported cannot drift apart.
const STORAGE: &str = "stand-in-storage";

#[derive(Debug, Default)]
pub struct State {
    pub vms: BTreeMap<u32, Vm>,
    pub tasks: BTreeMap<String, Task>,
    /// Every request the provider made, in order: method and path. Tests assert
    /// on this to prove that a refusal happened *before* the network, which is
    /// the whole claim the range guard makes.
    pub requests: Vec<(String, String)>,
    pub task_seq: u32,

    // Behaviours a test can ask for.
    pub tasks_never_finish: bool,
    pub next_task_fails: Option<String>,
    pub reject_config_writes: bool,
    pub unauthorized: bool,
    pub agent_interfaces: Option<Value>,
    pub agent_unavailable: bool,
    /// Free space every storage reports. Generous by default, so only the tests
    /// that care about the floor have to arrange anything; zero means the
    /// storage refuses to answer at all.
    pub storage_avail_bytes: u64,
    /// Per-storage overrides, so a test can starve one storage while its
    /// neighbour stays roomy -- without this, an accounting bug that summed
    /// everything against one figure would pass every test.
    pub storage_avail_named: BTreeMap<String, u64>,
    /// Answer the next N task-status polls with a 502. How a test says "the
    /// API blinked mid-wait" without breaking the operation itself.
    pub flake_next: u32,
}

pub struct MockPve {
    pub addr: SocketAddr,
    pub state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
}

impl MockPve {
    pub fn start() -> MockPve {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        let state = Arc::new(Mutex::new(State {
            storage_avail_bytes: 4 * 1024 * 1024 * 1024 * 1024,
            storage_avail_named: BTreeMap::new(),
            flake_next: 0,
            task_seq: 1,
            ..State::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(stream) = stream else { continue };
                let _ = serve(stream, &thread_state);
            }
        });

        MockPve { addr, state, stop }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Add a template and return the handle a guest registry would record.
    ///
    /// Callers get an opaque string back, so a test can register a guest
    /// without knowing what this provider's handles look like.
    pub fn add_template(&self, pool: &str) -> String {
        let id = self.with_state(|s| {
            let id = (9000..=9099)
                .find(|id| !s.vms.contains_key(id))
                .expect("a free identifier");
            s.vms.insert(
                id,
                Vm {
                    name: "stand-in-template".into(),
                    template: true,
                    pool: pool.to_string(),
                    ..Vm::default()
                },
            );
            id
        });
        id.to_string()
    }

    /// A session this workstation did not create.
    ///
    /// The cluster is shared, so a cap that only counts what is in the local
    /// session file is no cap at all. This is how a test says "somebody else
    /// already has one up".
    pub fn add_foreign_session(&self, pool: &str) -> String {
        let id = self.with_state(|s| {
            let id = (9000..=9099)
                .find(|id| !s.vms.contains_key(id))
                .expect("a free identifier");
            s.vms.insert(
                id,
                Vm {
                    name: "somebody-elses-session".into(),
                    template: false,
                    running: true,
                    tags: "expires-9999999999".into(),
                    pool: pool.to_string(),
                    ..Vm::default()
                },
            );
            id
        });
        id.to_string()
    }

    /// A site-configuration fragment pointing reaper at this stand-in.
    ///
    /// Knowing how to be configured is the provider's business, not the
    /// caller's -- which is what lets a caller drive the whole stack without
    /// naming a hypervisor anywhere in its own source.
    pub fn site_config(&self, token_file: &std::path::Path, pool: &str) -> String {
        format!(
            r#"provider = "proxmox"

[proxmox]
api        = "{}"
node       = "somenode"
pool       = "{pool}"
id_range   = [9000, 9099]
token_file = "{}"
data_storage = "stand-in-storage"
tls        = "insecure"
# Short, so a test exercising the timeout path finishes in seconds rather than
# waiting out a duration chosen for full-copy clones.
task_timeout = "5s"
"#,
            self.url(),
            token_file.display()
        )
    }

    /// The machines that are not templates: the sessions, in other words.
    pub fn session_machines(&self) -> Vec<String> {
        self.with_state(|s| {
            s.vms
                .iter()
                .filter(|(_, vm)| !vm.template)
                .map(|(id, _)| id.to_string())
                .collect()
        })
    }

    /// What was attached to a machine after it was made, by slot.
    pub fn attached_disks(&self, machine: &str) -> BTreeMap<String, String> {
        let id: u32 = machine.parse().expect("handle this provider issued");
        self.with_state(|s| {
            s.vms.get(&id).map(|v| v.extra_disks.clone()).unwrap_or_default()
        })
    }

    /// The tag string a machine carries.
    pub fn tags_of(&self, machine: &str) -> String {
        let id: u32 = machine.parse().expect("handle this provider issued");
        self.with_state(|s| s.vms.get(&id).map(|v| v.tags.clone()).unwrap_or_default())
    }

    /// Mark a template protected, as a real one should be.
    pub fn protect(&self, machine: &str) {
        let id: u32 = machine.parse().expect("handle this provider issued");
        self.with_state(|s| {
            if let Some(vm) = s.vms.get_mut(&id) { vm.protection = true; }
        });
    }

    /// Is this machine protected?
    pub fn is_protected(&self, machine: &str) -> bool {
        let id: u32 = machine.parse().expect("handle this provider issued");
        self.with_state(|s| s.vms.get(&id).map(|v| v.protection).unwrap_or(false))
    }

    /// A credential this stand-in accepts.
    ///
    /// What a valid credential looks like is the provider's business too, so a
    /// caller can write one to disk without knowing the shape.
    pub fn credential(&self) -> &'static str {
        "someone@realm!test=secret"
    }

    /// Report this address from every machine's guest agent.
    /// How much room every storage claims to have.
    pub fn storage_has(&self, bytes: u64) {
        self.state.lock().expect("mock state").storage_avail_bytes = bytes;
    }

    /// One storage's free space, leaving every other storage on the default.
    pub fn storage_named_has(&self, name: &str, bytes: u64) {
        self.state
            .lock()
            .expect("mock state")
            .storage_avail_named
            .insert(name.to_string(), bytes);
    }

    /// Answer the next N requests with a 502, whoever asks.
    pub fn flake_next(&self, n: u32) {
        self.state.lock().expect("mock state").flake_next = n;
    }

    pub fn reports_address(&self, addr: &str) {
        self.with_state(|s| {
            s.agent_interfaces = Some(serde_json::json!([
                {"name": "net0", "ip-addresses": [
                    {"ip-address": addr, "ip-address-type": "ipv4"}
                ]}
            ]));
        });
    }

    /// Leave asynchronous operations unfinished, so callers must time out.
    pub fn stall_operations(&self, stalled: bool) {
        self.with_state(|s| s.tasks_never_finish = stalled);
    }

    /// Forget a machine, as an external sweeper would.
    pub fn collect(&self, machine: &str) {
        let id: u32 = machine.parse().expect("handle this provider issued");
        self.with_state(|s| {
            s.vms.remove(&id);
        });
    }

    pub fn with_state<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self.state.lock().expect("state lock"))
    }

    pub fn vm(&self, id: u32) -> Option<Vm> {
        self.with_state(|s| s.vms.get(&id).cloned())
    }

    /// Paths the provider actually requested, in order.
    pub fn paths(&self) -> Vec<String> {
        self.with_state(|s| s.requests.iter().map(|(_, p)| p.clone()).collect())
    }

    /// Requests as method and path.
    ///
    /// The path alone is not enough to say what happened: reading a template's
    /// configuration and writing a session's tags are both `/config`, and a
    /// test that means the second must be able to say so.
    pub fn calls(&self) -> Vec<(String, String)> {
        self.with_state(|s| s.requests.clone())
    }

    pub fn request_count(&self) -> usize {
        self.with_state(|s| s.requests.len())
    }
}

impl Drop for MockPve {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Unblock the accept loop so the thread can notice and exit.
        let _ = TcpStream::connect(self.addr);
    }
}

fn serve(mut stream: TcpStream, state: &Arc<Mutex<State>>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    let mut length = 0usize;
    let mut authorized = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            length = v.trim().parse().unwrap_or(0);
        }
        // The value, not just the scheme: a provider change that corrupted
        // the token while keeping the prefix must fail here the way the real
        // API would fail it.
        if lower.starts_with("authorization:")
            && line.contains("PVEAPIToken=someone@realm!test=secret")
        {
            authorized = true;
        }
    }

    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body)?;
    }
    let body = String::from_utf8_lossy(&body).to_string();

    let (status, payload) = {
        let mut s = state.lock().expect("state lock");
        s.requests.push((method.clone(), path.clone()));
        if s.flake_next > 0 && path.contains("/tasks/") {
            s.flake_next -= 1;
            (502u16, json!({"errors": "bad gateway (mock flake)"}))
        } else if !authorized {
            (401u16, json!({"errors": "no ticket"}))
        } else if s.unauthorized {
            (403, json!({"errors": "permission denied"}))
        } else {
            route(&mut s, &method, &path, &body)
        }
    };

    let body = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
    let reason = match status {
        200 => "OK",
        401 => "authentication failure",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

fn route(s: &mut State, method: &str, path: &str, body: &str) -> (u16, Value) {
    let (path_only, query) = match path.split_once('?') {
        Some((p, q)) => (p, q),
        None => (path, ""),
    };
    let segments: Vec<&str> = path_only.trim_matches('/').split('/').collect();

    // Everything lives under /api2/json.
    let seg = match segments.as_slice() {
        ["api2", "json", rest @ ..] => rest,
        _ => return (404, json!({"errors": "not an api path"})),
    };

    match (method, seg) {
        ("GET", ["cluster", "resources"]) => {
            let _ = query;
            let items: Vec<Value> = s
                .vms
                .iter()
                .map(|(id, vm)| {
                    json!({
                        "vmid": id,
                        "name": vm.name,
                        "tags": vm.tags,
                        "pool": vm.pool,
                        "status": if vm.running { "running" } else { "stopped" },
                        "template": if vm.template { 1 } else { 0 },
                        "type": "qemu",
                    })
                })
                .collect();
            (200, json!({ "data": items }))
        }

        ("POST", ["nodes", _node, "qemu", id, "clone"]) => {
            let Some(source) = id.parse::<u32>().ok().and_then(|i| s.vms.get(&i).cloned()) else {
                return (404, json!({"errors": "no such template"}));
            };
            let form = parse_form(body);
            let newid: u32 = form.get("newid").and_then(|v| v.parse().ok()).unwrap_or(0);
            if s.vms.contains_key(&newid) {
                return (500, json!({"errors": "identifier already in use"}));
            }
            let vm = Vm {
                name: form.get("name").cloned().unwrap_or_default(),
                tags: String::new(),
                running: false,
                template: false,
                pool: form.get("pool").cloned().unwrap_or_default(),
                // Inherited from the source, exactly as Proxmox does it.
                protection: source.protection,
                cores: source.cores,
                memory: source.memory,
                extra_disks: BTreeMap::new(),
            };
            s.vms.insert(newid, vm);
            let t = new_task(s);
            (200, json!({ "data": t }))
        }

        ("GET", ["nodes", _node, "qemu", id, "config"]) => match lookup(s, id) {
            Some(vm) => {
                let mut cfg = json!({
                    "tags": vm.tags,
                    "cores": vm.cores,
                    "memory": vm.memory,
                    // A boot disk, because a machine without one weighs nothing
                    // and a free-space check would have nothing to weigh.
                    "virtio0": format!("{STORAGE}:vm-{id}-disk-0,iothread=1,size=8G"),
                    // Not a disk, and named to catch a prefix match that thinks
                    // it is.
                    "virtiofs0": "some-share",
                });
                for (bus, spec) in &vm.extra_disks {
                    cfg[bus] = json!(spec);
                }
                (200, json!({ "data": cfg }))
            }
            None => (404, json!({"errors": "no such machine"})),
        },

        // How much room a storage has. The stand-in is generous unless a test
        // says otherwise, so only the tests that care have to arrange it.
        ("GET", ["nodes", _node, "storage", storage, "status"]) => {
            let avail = s
                .storage_avail_named
                .get(*storage)
                .copied()
                .unwrap_or(s.storage_avail_bytes);
            if avail == 0 {
                return (500, json!({"errors": "storage is not online"}));
            }
            (200, json!({"data": {"avail": avail, "total": avail}}))
        }

        ("PUT", ["nodes", _node, "qemu", id, "config"]) => {
            if s.reject_config_writes {
                return (500, json!({"errors": "configuration write refused"}));
            }
            let Some(key) = id.parse::<u32>().ok() else {
                return (404, json!({"errors": "no such machine"}));
            };
            let form = parse_form(body);
            let Some(vm) = s.vms.get_mut(&key) else {
                return (404, json!({"errors": "no such machine"}));
            };
            if let Some(t) = form.get("tags") {
                vm.tags = t.clone();
            }
            if let Some(p) = form.get("protection") {
                vm.protection = p != "0";
            }
            if let Some(c) = form.get("cores").and_then(|v| v.parse().ok()) {
                vm.cores = Some(c);
            }
            if let Some(m) = form.get("memory").and_then(|v| v.parse().ok()) {
                vm.memory = Some(m);
            }
            // Anything shaped like a disk slot is a disk being attached.
            for (k, v) in &form {
                if k.starts_with("virtio") || k.starts_with("scsi") || k.starts_with("sata") {
                    vm.extra_disks.insert(k.clone(), v.clone());
                }
            }
            (200, json!({ "data": null }))
        }

        ("GET", ["nodes", _node, "qemu", id, "status", "current"]) => match lookup(s, id) {
            Some(vm) => (
                200,
                json!({"data": {
                    "status": if vm.running { "running" } else { "stopped" },
                    "vmid": id,
                }}),
            ),
            // 403, not 404, exactly as the real API answers: the ACL check on
            // /vms/<id> precedes the existence check, and a machine that is
            // gone has no ACL entry. This distinction cost a bug.
            None => (403, json!({"message": "Permission check failed (/vms/x, VM.Audit)"})),
        },

        ("POST", ["nodes", _node, "qemu", id, "status", action]) => {
            let Some(key) = id.parse::<u32>().ok() else {
                return (404, json!({"errors": "no such machine"}));
            };
            let running = match *action {
                "start" => true,
                "stop" => false,
                _ => return (404, json!({"errors": "no such action"})),
            };
            let Some(vm) = s.vms.get_mut(&key) else {
                return (404, json!({"errors": "no such machine"}));
            };
            vm.running = running;
            let t = new_task(s);
            (200, json!({ "data": t }))
        }

        ("DELETE", ["nodes", _node, "qemu", id]) => {
            let Some(key) = id.parse::<u32>().ok() else {
                return (404, json!({"errors": "no such machine"}));
            };
            // Refuses what the real thing refuses. A stand-in that is kinder
            // than the API it stands in for tests only itself -- this one has
            // already hidden two bugs that live use found immediately.
            if s.vms.get(&key).map(|v| v.protection).unwrap_or(false) {
                return (500, json!({"errors": "protection mode enabled"}));
            }
            if s.vms.get(&key).map(|v| v.running).unwrap_or(false) {
                return (500, json!({"message": "VM is running - destroy failed"}));
            }
            if s.vms.remove(&key).is_none() {
                return (404, json!({"errors": "no such machine"}));
            }
            let t = new_task(s);
            (200, json!({ "data": t }))
        }

        ("GET", ["nodes", _node, "qemu", id, "agent", "network-get-interfaces"]) => {
            // Real PVE answers a missing machine with a 500 naming its
            // configuration file, not a 404 -- the text is all a caller gets.
            if lookup(s, id).is_none() {
                return (
                    500,
                    json!({"errors": format!("Configuration file 'nodes/x/qemu-server/{id}.conf' does not exist")}),
                );
            }
            if s.agent_unavailable {
                return (500, json!({"errors": "QEMU guest agent is not running"}));
            }
            let result = s.agent_interfaces.clone().unwrap_or_else(|| json!([]));
            (200, json!({ "data": { "result": result } }))
        }

        ("GET", ["nodes", _node, "tasks", upid, "status"]) => match s.tasks.get(*upid) {
            Some(t) => (
                200,
                json!({"data": {"status": t.status, "exitstatus": t.exitstatus, "upid": upid}}),
            ),
            None => (404, json!({"errors": "no such task"})),
        },

        _ => (404, json!({ "errors": format!("unrouted: {method} {path_only}") })),
    }
}

fn lookup(s: &State, id: &str) -> Option<Vm> {
    id.parse::<u32>().ok().and_then(|i| s.vms.get(&i).cloned())
}

fn new_task(s: &mut State) -> String {
    let upid = format!("UPID:node:0000:0000:0000:task:{}:cal@pve:", s.task_seq);
    s.task_seq += 1;

    let task = if s.tasks_never_finish {
        Task {
            status: "running".into(),
            exitstatus: String::new(),
        }
    } else if let Some(reason) = s.next_task_fails.take() {
        Task {
            status: "stopped".into(),
            exitstatus: reason,
        }
    } else {
        Task {
            status: "stopped".into(),
            exitstatus: "OK".into(),
        }
    };

    s.tasks.insert(upid.clone(), task);
    upid
}

fn parse_form(body: &str) -> BTreeMap<String, String> {
    body.split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (percent_decode(k), percent_decode(v)))
        .collect()
}

/// Enough percent-decoding for form bodies. Tags contain semicolons, which are
/// encoded, so this is load-bearing rather than decorative.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}
