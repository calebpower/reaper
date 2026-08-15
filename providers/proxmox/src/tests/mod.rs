//! Provider tests, driven against a stand-in API over loopback.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reaper_core::provider::{CreateRequest, MachineRef, Provider, ProviderError};
use serde_json::json;

use super::*;
use crate::config::{Config, Tls};
use crate::ids::IdRange;
use crate::mock::{MockPve, Vm};

const NODE: &str = "somenode";
const POOL: &str = "a/pool";

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

fn provider_for(pve: &MockPve) -> Proxmox {
    let config = Config {
        api: pve.url(),
        node: NODE.to_string(),
        pool: POOL.to_string(),
        ids: IdRange::new(9000, 9099).unwrap(),
        token_file: "/dev/null".into(),
        data_storage: Some("some-storage".to_string()),
        data_bus: "virtio1".to_string(),
        min_free_gb: 10,
        tls: Tls::Insecure,
        task_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(5),
    };
    let mut p = Proxmox::with_token(config, "someone@realm!test=secret".into()).expect("provider");
    p.set_poll_interval(Duration::from_millis(5));
    p
}

/// A mock holding one template, which is what a session is cloned from.
fn pve_with_template() -> MockPve {
    let pve = MockPve::start();
    pve.with_state(|s| {
        s.vms.insert(
            9000,
            Vm {
                name: "stub-template".into(),
                template: true,
                pool: POOL.into(),
                cores: Some(1),
                memory: Some(512),
                ..Vm::default()
            },
        );
    });
    pve
}

fn request(name: &str, expires: u64) -> CreateRequest {
    CreateRequest {
        name: name.to_string(),
        template: "9000".to_string(),
        cores: Some(4),
        ram_gb: Some(8),
        data_disk_gb: Some(64),
        expires_at: at(expires),
    }
}

// --- creation --------------------------------------------------------------

#[test]
fn creating_clones_the_template_and_tags_it_with_an_expiry() {
    let pve = pve_with_template();
    let p = provider_for(&pve);

    let m = p.create(&request("a-session", 1_700_000_000)).expect("create");
    let id: u32 = m.as_str().parse().expect("numeric identifier");
    assert!((9000..=9099).contains(&id));
    assert_ne!(id, 9000, "must not reuse the template's own identifier");

    let vm = pve.vm(id).expect("the machine exists");
    assert_eq!(tags::expiry_of(&vm.tags), Some(at(1_700_000_000)));
    assert_eq!(vm.pool, POOL, "must be placed in the pool");
    assert_eq!(vm.cores, Some(4));
    assert_eq!(vm.memory, Some(8 * 1024), "memory is set in megabytes");
}

#[test]
fn creating_tags_before_starting_and_does_not_start_at_all() {
    // The order is the invariant: a machine must never exist un-expiring, and
    // starting is somebody else's decision.
    let pve = pve_with_template();
    let p = provider_for(&pve);
    let m = p.create(&request("a-session", 1_700_000_000)).unwrap();

    let calls = pve.calls();
    let clone_at = calls.iter().position(|(_, p)| p.contains("/clone")).expect("cloned");
    // The *write*, specifically. Reading the template's configuration to see
    // what a session will cost is also a /config request, and it happens first.
    let tagged_at = calls
        .iter()
        .position(|(m, p)| m == "PUT" && p.ends_with("/config"))
        .expect("tagged");
    assert!(clone_at < tagged_at, "tagged before cloning? {calls:?}");
    assert!(
        !calls.iter().any(|(_, p)| p.contains("/status/start")),
        "create must not start the machine: {calls:?}"
    );
    assert!(!pve.vm(m.as_str().parse().unwrap()).unwrap().running);
}

#[test]
fn a_machine_that_cannot_be_given_an_expiry_is_destroyed_rather_than_leaked() {
    // An untagged machine is the one state nothing will ever collect. We know
    // its identifier, so leaving it would be carelessness rather than caution.
    let pve = pve_with_template();
    let p = provider_for(&pve);
    pve.with_state(|s| s.reject_config_writes = true);

    let e = p.create(&request("a-session", 1_700_000_000)).unwrap_err();
    assert!(e.to_string().contains("has been destroyed"), "{e}");

    let leftovers: Vec<u32> = pve.with_state(|s| {
        s.vms.keys().copied().filter(|id| *id != 9000).collect()
    });
    assert!(leftovers.is_empty(), "leaked machines: {leftovers:?}");
}

#[test]
fn a_template_outside_the_permitted_range_is_refused_before_any_request() {
    let pve = MockPve::start();
    let p = provider_for(&pve);
    let mut req = request("a-session", 1);
    // The sweeper's own machine. Cloning from it would be as wrong as
    // destroying it.
    req.template = "8100".into();

    let e = p.create(&req).unwrap_err();
    assert!(matches!(e, ProviderError::Refused(_)), "{e}");
    assert_eq!(pve.request_count(), 0, "refusal must precede the network");
}

#[test]
fn creating_when_every_identifier_is_taken_is_refused_clearly() {
    let pve = pve_with_template();
    pve.with_state(|s| {
        for id in 9001..=9099 {
            s.vms.insert(
                id,
                Vm {
                    name: format!("busy-{id}"),
                    pool: POOL.into(),
                    ..Vm::default()
                },
            );
        }
    });
    let p = provider_for(&pve);

    let e = p.create(&request("a-session", 1)).unwrap_err();
    assert!(e.to_string().contains("in use"), "{e}");
}

// --- the range guard -------------------------------------------------------

#[test]
fn every_operation_refuses_an_out_of_range_machine_before_the_network() {
    // The claim the guard makes is not "the API will say no" but "we never
    // ask". Asserting the request count is what proves it.
    let pve = MockPve::start();
    let p = provider_for(&pve);
    let outsider = MachineRef::new("8100");

    let outcomes: Vec<(&str, std::result::Result<(), ProviderError>)> = vec![
        ("set_expiry", p.set_expiry(&outsider, at(1))),
        ("start", p.start(&outsider)),
        ("stop", p.stop(&outsider)),
        ("destroy", p.destroy(&outsider)),
        ("address", p.address(&outsider).map(|_| ())),
    ];

    for (verb, outcome) in outcomes {
        let e = outcome.expect_err("{verb} should refuse");
        assert!(matches!(e, ProviderError::Refused(_)), "{verb}: {e}");
    }
    assert_eq!(pve.request_count(), 0, "no verb may contact the API first");
}

// --- expiry ----------------------------------------------------------------

#[test]
fn moving_an_expiry_preserves_tags_this_project_did_not_write() {
    // This is a shared cluster. Replacing the whole tag string would discard
    // somebody else's tags invisibly.
    let pve = MockPve::start();
    pve.with_state(|s| {
        s.vms.insert(
            9001,
            Vm {
                name: "a-session".into(),
                tags: "somebody-elses;expires-1;ephemeral".into(),
                pool: POOL.into(),
                ..Vm::default()
            },
        );
    });
    let p = provider_for(&pve);

    p.set_expiry(&MachineRef::new("9001"), at(1_700_000_000)).unwrap();

    let tags = tags::split(&pve.vm(9001).unwrap().tags)
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert!(tags.contains(&"somebody-elses".to_string()), "{tags:?}");
    assert!(tags.contains(&"ephemeral".to_string()), "{tags:?}");
    assert!(tags.contains(&"expires-1700000000".to_string()), "{tags:?}");
    assert!(!tags.contains(&"expires-1".to_string()), "stale expiry kept: {tags:?}");
}

// --- listing ---------------------------------------------------------------

#[test]
fn listing_reports_only_this_providers_machines() {
    let pve = pve_with_template();
    pve.with_state(|s| {
        s.vms.insert(9001, Vm { name: "mine".into(), tags: "expires-1700000000".into(), running: true, pool: POOL.into(), ..Vm::default() });
        // In range but another pool: not ours.
        s.vms.insert(9002, Vm { name: "another-tenant".into(), pool: "someone/else".into(), ..Vm::default() });
        // In our pool but outside the range: not ours either.
        s.vms.insert(8100, Vm { name: "the-sweeper".into(), pool: POOL.into(), ..Vm::default() });
    });
    let p = provider_for(&pve);

    let listed = p.list().unwrap();
    let names: Vec<&str> = listed.iter().map(|m| m.name.as_str()).collect();

    assert_eq!(names, vec!["mine"], "listed: {names:?}");
    assert_eq!(listed[0].expires_at, Some(at(1_700_000_000)));
    assert!(listed[0].running);
    // The template is what sessions are made from, not a session.
    assert!(!names.contains(&"stub-template"));
}

#[test]
fn a_machine_with_no_expiry_is_listed_with_none_rather_than_skipped() {
    // It is not a machine with plenty of time left; it is one nothing will
    // collect, and hiding it would hide the problem.
    let pve = MockPve::start();
    pve.with_state(|s| {
        s.vms.insert(9001, Vm { name: "untagged".into(), pool: POOL.into(), ..Vm::default() });
    });
    let p = provider_for(&pve);

    let listed = p.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].expires_at, None);
}

// --- lifecycle -------------------------------------------------------------

#[test]
fn starting_stopping_and_destroying_do_what_they_say() {
    let pve = MockPve::start();
    pve.with_state(|s| {
        s.vms.insert(9001, Vm { name: "a-session".into(), pool: POOL.into(), ..Vm::default() });
    });
    let p = provider_for(&pve);
    let m = MachineRef::new("9001");

    p.start(&m).unwrap();
    assert!(pve.vm(9001).unwrap().running);
    p.stop(&m).unwrap();
    assert!(!pve.vm(9001).unwrap().running);
    p.destroy(&m).unwrap();
    assert!(pve.vm(9001).is_none());
}

// --- tasks -----------------------------------------------------------------

#[test]
fn a_failed_task_is_reported_with_the_reason_the_api_gave() {
    let pve = pve_with_template();
    pve.with_state(|s| s.next_task_fails = Some("storage is full".into()));
    let p = provider_for(&pve);

    let e = p.create(&request("a-session", 1)).unwrap_err();
    assert!(e.to_string().contains("storage is full"), "{e}");
}

#[test]
fn a_task_that_never_finishes_times_out_and_destroys_nothing() {
    // A timeout means the outcome is unknown. Cleaning up on a guess is how
    // you destroy somebody else's machine; the expiry tag covers this instead.
    let pve = MockPve::start();
    pve.with_state(|s| {
        s.tasks_never_finish = true;
        s.vms.insert(9001, Vm { name: "a-session".into(), pool: POOL.into(), ..Vm::default() });
    });
    let p = provider_for(&pve);

    let e = p.start(&MachineRef::new("9001")).unwrap_err();
    assert!(matches!(e, ProviderError::Timeout(_)), "{e}");
    assert!(pve.vm(9001).is_some(), "a timeout must not destroy anything");
    assert!(
        !pve.paths().iter().any(|p| p.starts_with("DELETE")),
        "nothing may be deleted on a timeout"
    );
}

// --- addresses -------------------------------------------------------------

#[test]
fn an_address_is_none_until_the_guest_agent_answers() {
    let pve = MockPve::start();
    pve.with_state(|s| {
        s.agent_unavailable = true;
        s.vms.insert(9001, Vm { name: "a-session".into(), pool: POOL.into(), ..Vm::default() });
    });
    let p = provider_for(&pve);

    // Not an error: this is the ordinary state of a machine that has only just
    // been started.
    assert_eq!(p.address(&MachineRef::new("9001")).unwrap(), None);
}

#[test]
fn loopback_and_link_local_addresses_are_skipped() {
    let pve = MockPve::start();
    pve.with_state(|s| {
        s.vms.insert(9001, Vm { name: "a-session".into(), pool: POOL.into(), running: true, ..Vm::default() });
        s.agent_interfaces = Some(json!([
            {"name": "lo", "ip-addresses": [
                {"ip-address": "127.0.0.1", "ip-address-type": "ipv4"},
                {"ip-address": "::1", "ip-address-type": "ipv6"}
            ]},
            {"name": "eth0", "ip-addresses": [
                {"ip-address": "169.254.3.4", "ip-address-type": "ipv4"},
                {"ip-address": "fe80::1", "ip-address-type": "ipv6"},
                {"ip-address": "192.0.2.25", "ip-address-type": "ipv4"}
            ]}
        ]));
    });
    let p = provider_for(&pve);

    // Every address here is one a machine really has; only one is reachable.
    assert_eq!(
        p.address(&MachineRef::new("9001")).unwrap(),
        Some("192.0.2.25".parse().unwrap())
    );
}

#[test]
fn an_ipv4_address_is_preferred_over_ipv6() {
    let pve = MockPve::start();
    pve.with_state(|s| {
        s.vms.insert(9001, Vm { name: "a-session".into(), pool: POOL.into(), running: true, ..Vm::default() });
        s.agent_interfaces = Some(json!([
            {"name": "eth0", "ip-addresses": [
                {"ip-address": "2001:db8::5", "ip-address-type": "ipv6"},
                {"ip-address": "192.0.2.25", "ip-address-type": "ipv4"}
            ]}
        ]));
    });
    let p = provider_for(&pve);
    assert_eq!(
        p.address(&MachineRef::new("9001")).unwrap(),
        Some("192.0.2.25".parse().unwrap())
    );
}

// --- authentication --------------------------------------------------------

#[test]
fn a_rejected_credential_says_so_rather_than_looking_like_a_bug() {
    let pve = MockPve::start();
    pve.with_state(|s| s.unauthorized = true);
    let p = provider_for(&pve);

    let e = p.list().unwrap_err();
    assert!(matches!(e, ProviderError::Unauthorized(_)), "{e}");
}

#[test]
fn the_token_is_sent_on_every_request() {
    // The mock answers 401 when the header is missing, so a green suite is
    // itself the assertion -- but say it out loud, because a change that
    // dropped the header would otherwise show up as a confusing 401 much later.
    let pve = pve_with_template();
    let p = provider_for(&pve);
    p.list().expect("authorized");
    assert!(pve.request_count() > 0);
}

// --- names -----------------------------------------------------------------

#[test]
fn session_names_are_bent_into_something_the_api_accepts() {
    // Rather than reject a name a person chose.
    assert_eq!(sanitize_name("my-project/some-os 26.04"), "my-project-some-os-26-04");
    assert_eq!(sanitize_name("UPPER_case"), "upper-case");
    assert_eq!(sanitize_name("---"), "reaper");
    assert_eq!(sanitize_name(""), "reaper");
    assert!(sanitize_name(&"x".repeat(200)).len() <= 63);
    assert!(!sanitize_name(&format!("{}-", "x".repeat(62))).ends_with('-'));
}

#[test]
fn identifier_allocation_avoids_machines_this_provider_cannot_see() {
    // Identifiers are cluster-wide. A template, and a machine belonging to
    // another pool, both occupy one -- and `list` hides both. Allocating from
    // the filtered view would collide with something already there.
    let pve = pve_with_template();
    pve.with_state(|s| {
        s.vms.insert(9001, Vm { name: "another-tenant".into(), pool: "someone/else".into(), ..Vm::default() });
        s.vms.insert(9002, Vm { name: "another-template".into(), template: true, pool: "someone/else".into(), ..Vm::default() });
    });
    let p = provider_for(&pve);

    let m = p.create(&request("a-session", 1)).expect("create");
    let id: u32 = m.as_str().parse().unwrap();
    assert_eq!(id, 9003, "should skip 9000 (template), 9001 and 9002; got {id}");
}

// --- configuration ---------------------------------------------------------

fn table(body: &str) -> toml::Value {
    toml::from_str(body).expect("test table parses")
}

const BASE: &str = r#"
node = "somenode"
pool = "a/pool"
id_range = [9000, 9099]
token_file = "/dev/null"
data_storage = "some-storage"
"#;

fn with(extra: &str) -> std::result::Result<Config, crate::config::ConfigError> {
    crate::config::from_table(&table(&format!("{BASE}{extra}")))
}

#[test]
fn https_endpoints_are_accepted() {
    assert!(with("api = \"https://node.example:8006\"\ntls = \"webpki\"\n").is_ok());
}

#[test]
fn plain_http_to_another_machine_is_refused() {
    // The token travels in a header. Over plaintext to another host that is
    // simply giving it away.
    for host in ["node.example", "192.0.2.10", "[2001:db8::1]"] {
        let e = with(&format!("api = \"http://{host}:8006\"\ntls = \"webpki\"\n"))
            .expect_err("{host} should be refused");
        assert!(e.to_string().contains("clear text"), "{host}: {e}");
    }
}

#[test]
fn plain_http_to_loopback_is_allowed() {
    // Nothing leaves the machine. This is also what lets the suite drive the
    // real client against a local server rather than a stub.
    for host in ["127.0.0.1", "localhost", "[::1]"] {
        assert!(
            with(&format!("api = \"http://{host}:8006\"\ntls = \"webpki\"\n")).is_ok(),
            "{host} should be allowed"
        );
    }
}

#[test]
fn a_missing_or_unknown_scheme_is_refused() {
    assert!(with("api = \"node.example:8006\"\ntls = \"webpki\"\n").is_err());
    assert!(with("api = \"ftp://node.example\"\ntls = \"webpki\"\n").is_err());
}

#[test]
fn ca_file_verification_needs_a_ca_file() {
    let e = with("api = \"https://n\"\ntls = \"ca-file\"\n").expect_err("should refuse");
    assert!(e.to_string().contains("no [proxmox].ca_file"), "{e}");
}

#[test]
fn a_ca_file_nothing_reads_is_refused() {
    // Otherwise a person believes their traffic is verified when it is not,
    // which is the most expensive way to be wrong here.
    let e = with("api = \"https://n\"\ntls = \"insecure\"\nca_file = \"/tmp/ca.pem\"\n")
        .expect_err("should refuse");
    assert!(e.to_string().contains("does not use it"), "{e}");
}

#[test]
fn an_unknown_tls_mode_is_refused_rather_than_defaulted() {
    // Silently falling back to either extreme would be wrong: to verification,
    // and nothing works; to none, and nothing is safe.
    let e = with("api = \"https://n\"\ntls = \"maybe\"\n").expect_err("should refuse");
    assert!(e.to_string().contains("webpki"), "{e}");
}

#[test]
fn an_empty_pool_is_refused_with_a_reason() {
    let t = table("api = \"https://n\"\nnode = \"n\"\npool = \"\"\nid_range = [9000, 9099]\ntoken_file = \"/dev/null\"\ndata_storage = \"s\"\ntls = \"webpki\"\n");
    let e = crate::config::from_table(&t).expect_err("should refuse");
    assert!(e.to_string().contains("must be placed"), "{e}");
}

#[test]
fn an_unknown_key_in_the_provider_table_is_refused() {
    let e = with("api = \"https://n\"\ntls = \"webpki\"\nnodee = \"typo\"\n")
        .expect_err("should refuse");
    assert!(e.to_string().contains("proxmox"), "{e}");
}

// --- the token file --------------------------------------------------------

/// A credential file that removes itself, however the test ends.
///
/// Deref so it stands in for a path at every call site. Drop rather than a
/// tidy-up at the end of a test, because a panicking test never reaches the
/// tidy-up -- which is how this suite came to leave hundreds of directories
/// in /tmp.
struct TokenFile(std::path::PathBuf);

impl std::ops::Deref for TokenFile {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl AsRef<std::path::Path> for TokenFile {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TokenFile {
    fn drop(&mut self) {
        if let Some(dir) = self.0.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn token_file(name: &str, contents: &str, mode: u32) -> TokenFile {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("reaper-token-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("token");
    std::fs::write(&path, contents).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    TokenFile(path)
}

#[test]
fn a_token_others_can_read_is_refused() {
    // The same rule ssh applies to a private key, for the same reason.
    for mode in [0o644, 0o640, 0o604, 0o666] {
        let p = token_file(&format!("mode{mode:o}"), "cal@pve!x=secret", mode);
        let e = crate::config::read_token(&p).expect_err("mode {mode:o} should be refused");
        assert!(e.to_string().contains("readable by others"), "{mode:o}: {e}");
    }
}

#[test]
fn a_token_only_its_owner_can_read_is_accepted() {
    let p = token_file("ok", "cal@pve!harness=s3cret\n", 0o600);
    assert_eq!(crate::config::read_token(&p).unwrap(), "cal@pve!harness=s3cret");
}

#[test]
fn a_token_that_is_not_shaped_like_a_credential_is_refused() {
    // A token missing its identifier produces a 401 that sends the reader
    // hunting for a permissions problem they do not have.
    for (label, body) in [
        ("empty", ""),
        ("blank", " \n"),
        ("no bang", "cal@pve=secret"),
        ("no equals", "cal@pve!harness"),
        ("internal space", "cal@pve!harness= secret"),
    ] {
        let p = token_file(label, body, 0o600);
        assert!(crate::config::read_token(&p).is_err(), "{label} should be refused");
    }
}

// --- the session's disk ----------------------------------------------------

#[test]
fn a_blank_disk_is_attached_at_the_requested_size() {
    // Attached rather than carried by the template: where cloning is a
    // byte-for-byte copy, a template disk is copied in full every session.
    let pve = pve_with_template();
    let p = provider_for(&pve);

    let m = p.create(&request("a-session", 1)).expect("create");
    let disks = pve.attached_disks(m.as_str());

    assert_eq!(
        disks.get("virtio1").map(String::as_str),
        Some("some-storage:64"),
        "attached: {disks:?}"
    );
}

#[test]
fn the_disk_is_attached_in_the_same_call_as_the_expiry() {
    // Not thrift for its own sake: a separate call would widen the window in
    // which the machine exists without a tag, and that window is the one
    // unrecoverable state in the design.
    let pve = pve_with_template();
    let p = provider_for(&pve);
    p.create(&request("a-session", 1_700_000_000)).expect("create");

    // Writes only. Reading the template's configuration to price the session
    // is a /config request too, and it is not what this is counting.
    let config_writes = pve
        .calls()
        .iter()
        .filter(|(m, p)| m == "PUT" && p.ends_with("/config"))
        .count();
    assert_eq!(config_writes, 1, "expected one config write, saw {config_writes}");
}

#[test]
fn a_template_that_carries_its_own_disk_gets_no_second_one() {
    let pve = pve_with_template();
    let p = provider_for(&pve);
    let mut req = request("a-session", 1);
    req.data_disk_gb = None;

    let m = p.create(&req).expect("create");
    assert!(
        pve.attached_disks(m.as_str()).is_empty(),
        "nothing should have been attached"
    );
}

#[test]
fn asking_for_a_disk_with_nowhere_to_put_it_is_refused_clearly() {
    let pve = pve_with_template();
    let mut config = Config {
        api: pve.url(),
        node: NODE.to_string(),
        pool: POOL.to_string(),
        ids: IdRange::new(9000, 9099).unwrap(),
        token_file: "/dev/null".into(),
        data_storage: None,
        data_bus: "virtio1".to_string(),
        min_free_gb: 10,
        tls: Tls::Insecure,
        task_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(5),
    };
    config.data_storage = None;
    let mut p = Proxmox::with_token(config, "someone@realm!test=secret".into()).unwrap();
    p.set_poll_interval(Duration::from_millis(5));

    let e = p.create(&request("a-session", 1)).unwrap_err();
    assert!(e.to_string().contains("data_storage"), "{e}");
}

#[test]
fn the_disk_slot_is_configurable() {
    // Templates boot from virtio0 here, but a site whose templates differ
    // should not have to patch the provider.
    let pve = pve_with_template();
    let mut config = crate::config::from_table(&table(&format!(
        "{}\nnode = \"{NODE}\"\npool = \"{POOL}\"\nid_range = [9000, 9099]\n\
         token_file = \"/dev/null\"\ndata_storage = \"s\"\ndata_bus = \"scsi3\"\ntls = \"insecure\"\n",
        format_args!("api = \"{}\"", pve.url())
    )))
    .expect("config");
    config.task_timeout = Duration::from_secs(2);
    let mut p = Proxmox::with_token(config, "someone@realm!test=secret".into()).unwrap();
    p.set_poll_interval(Duration::from_millis(5));

    let m = p.create(&request("a-session", 1)).expect("create");
    let disks = pve.attached_disks(m.as_str());
    assert_eq!(disks.get("scsi3").map(String::as_str), Some("s:64"), "{disks:?}");
}

#[test]
fn destroying_also_clears_disks_a_failed_create_could_have_left() {
    // Parity with the sweeper, which has always destroyed this way. The two
    // should not disagree about what destroying a machine means.
    let pve = MockPve::start();
    pve.with_state(|s| {
        s.vms.insert(9001, Vm { name: "a-session".into(), pool: POOL.into(), ..Vm::default() });
    });
    let p = provider_for(&pve);

    p.destroy(&MachineRef::new("9001")).unwrap();

    let paths = pve.paths();
    let deletes: Vec<&String> = paths.iter().filter(|p| p.contains("/qemu/9001")).collect();
    assert!(
        deletes.iter().any(|p| p.contains("destroy-unreferenced-disks=1")),
        "unreferenced disks must be cleared too: {deletes:?}"
    );
    assert!(pve.vm(9001).is_none(), "the machine should be gone");
}

// --- protection ------------------------------------------------------------

#[test]
fn a_session_is_destroyable_even_though_its_template_is_protected() {
    // Proxmox copies the protection flag to clones, and templates are rightly
    // protected. Left inherited, a session could never be destroyed -- not by
    // down, not by the sweeper. This is the whole design failing quietly.
    let pve = pve_with_template();
    pve.protect("9000");
    let p = provider_for(&pve);

    let m = p.create(&request("a-session", 1)).expect("create");
    assert!(
        !pve.is_protected(m.as_str()),
        "the session inherited protection and could never be destroyed"
    );

    p.destroy(&m).expect("a session must always be destroyable");
    assert!(pve.vm(m.as_str().parse().unwrap()).is_none());
}

#[test]
fn protection_is_cleared_before_the_machine_is_ever_started() {
    // In the same call as the expiry: a window in which a session is both
    // running and undeletable is a window in which the sweeper is powerless.
    let pve = pve_with_template();
    pve.protect("9000");
    let p = provider_for(&pve);
    p.create(&request("a-session", 1)).expect("create");

    let paths = pve.paths();
    let config_at = pve
        .calls()
        .iter()
        .position(|(m, p)| m == "PUT" && p.ends_with("/config"))
        .expect("configured");
    let started = paths.iter().position(|x| x.contains("/status/start"));
    assert!(started.is_none(), "create must not start the machine: {paths:?}");
    assert_eq!(
        pve.calls()
            .iter()
            .filter(|(m, p)| m == "PUT" && p.ends_with("/config"))
            .count(),
        1,
        "expiry, resources, disk and protection belong in one call: {paths:?}"
    );
    assert!(config_at > 0);
}

#[test]
fn the_template_itself_is_never_unprotected() {
    // Clearing protection on the session must not reach back to the template.
    let pve = pve_with_template();
    pve.protect("9000");
    let p = provider_for(&pve);
    p.create(&request("a-session", 1)).expect("create");
    assert!(pve.is_protected("9000"), "the template must stay protected");
}

#[test]
fn destroying_a_running_machine_stops_it_first() {
    // The API refuses to delete a running machine. Without this, `down` failed
    // on every live session -- which is the only kind that matters.
    let pve = MockPve::start();
    pve.with_state(|s| {
        s.vms.insert(9001, Vm { name: "a-session".into(), running: true, pool: POOL.into(), ..Vm::default() });
    });
    let p = provider_for(&pve);

    p.destroy(&MachineRef::new("9001")).expect("destroy must handle a running machine");
    assert!(pve.vm(9001).is_none(), "the machine should be gone");

    let paths = pve.paths();
    let stopped = paths.iter().position(|x| x.contains("/status/stop")).expect("asked it to stop");
    let deleted = paths.iter().rposition(|x| x.contains("/qemu/9001?")).expect("deleted");
    assert!(stopped < deleted, "stop must precede delete: {paths:?}");
}

#[test]
fn destroying_a_stopped_machine_does_not_bother_stopping_it() {
    let pve = MockPve::start();
    pve.with_state(|s| {
        s.vms.insert(9001, Vm { name: "a-session".into(), running: false, pool: POOL.into(), ..Vm::default() });
    });
    let p = provider_for(&pve);
    p.destroy(&MachineRef::new("9001")).unwrap();
    assert!(
        !pve.paths().iter().any(|x| x.contains("/status/stop")),
        "no need to stop something already stopped"
    );
}

#[test]
fn destroying_a_machine_the_sweeper_already_took_reports_it_gone() {
    // The API answers 403 rather than 404 for a machine that no longer exists,
    // so a naive read of the refusal looks like a credential problem and the
    // session can never be cleared.
    let pve = pve_with_template();
    let p = provider_for(&pve);
    let m = p.create(&request("a-session", 1)).expect("create");
    pve.collect(m.as_str()); // as the sweeper would

    let e = p.destroy(&m).unwrap_err();
    assert!(
        matches!(e, ProviderError::NotFound(_)),
        "should be reported as gone, got: {e}"
    );
}

#[test]
fn a_genuine_credential_failure_is_not_mistaken_for_a_missing_machine() {
    // The dangerous inverse. Reporting a live machine as gone would drop the
    // session record that is the only convenient trace of it.
    let pve = pve_with_template();
    let p = provider_for(&pve);
    let m = p.create(&request("a-session", 1)).expect("create");
    pve.with_state(|s| s.unauthorized = true);

    let e = p.destroy(&m).unwrap_err();
    assert!(
        matches!(e, ProviderError::Unauthorized(_)),
        "a broken credential must not read as a missing machine, got: {e}"
    );
}

// ---------------------------------------------------------------------------
// Room on the storage
// ---------------------------------------------------------------------------

#[test]
fn a_session_that_would_fill_the_storage_is_refused_before_anything_is_made() {
    let pve = pve_with_template();
    let p = provider_for(&pve);

    // Room for the 64 GiB pool disk, but not for it plus the floor that must
    // be left behind.
    pve.storage_has(70 * 1024 * 1024 * 1024);

    let mut req = request("a-session", 1_700_000_000);
    req.data_disk_gb = Some(64);
    let err = p.create(&req).expect_err("should have refused");

    let message = err.to_string();
    assert!(message.contains("free"), "{message}");
    assert!(message.contains("min_free_gb"), "should say how to override: {message}");

    // And nothing was created on the way to finding out.
    assert!(
        !pve.paths().iter().any(|x| x.contains("/clone")),
        "refused after cloning: {:?}",
        pve.paths()
    );
}

#[test]
fn room_is_counted_per_storage_and_includes_the_template_being_copied() {
    let pve = pve_with_template();
    let p = provider_for(&pve);

    // No pool disk at all, so the only thing to weigh is the template's own
    // 8 GiB boot disk being copied. A check that priced only the disk it
    // attaches would find nothing to object to here.
    pve.storage_has(12 * 1024 * 1024 * 1024);

    let mut req = request("a-session", 1_700_000_000);
    req.data_disk_gb = None;
    assert!(p.create(&req).is_err(), "the boot copy has to count too");
}

#[test]
fn a_session_that_fits_is_created() {
    let pve = pve_with_template();
    let p = provider_for(&pve);
    pve.storage_has(500 * 1024 * 1024 * 1024);

    let mut req = request("a-session", 1_700_000_000);
    req.data_disk_gb = Some(64);
    assert!(p.create(&req).is_ok());
}

#[test]
fn a_storage_that_will_not_say_how_full_it_is_does_not_stop_a_session() {
    // Not knowing is not the same as knowing there is no room. Refusing every
    // session because one storage will not report itself would be a worse
    // failure than the one being guarded against.
    let pve = pve_with_template();
    let p = provider_for(&pve);
    pve.storage_has(0);

    let mut req = request("a-session", 1_700_000_000);
    req.data_disk_gb = Some(64);
    assert!(p.create(&req).is_ok(), "an unreadable storage must not block a session");
}

#[test]
fn a_disk_key_is_a_disk_key_and_not_merely_a_prefix() {
    // `virtiofs0` starts with a bus name and is not a disk. The stand-in
    // reports one precisely so a prefix match gets caught here.
    assert!(super::is_disk_key("virtio0"));
    assert!(super::is_disk_key("scsi12"));
    assert!(!super::is_disk_key("virtiofs0"));
    assert!(!super::is_disk_key("virtio"));
    assert!(!super::is_disk_key("net0"));
}

#[test]
fn disk_specs_are_read_the_way_proxmox_writes_them() {
    assert_eq!(
        super::disk_storage_and_size("some-storage:vm-9001-disk-0,iothread=1,size=8G"),
        Some(("some-storage".to_string(), 8 * 1024 * 1024 * 1024))
    );
    assert_eq!(
        super::disk_storage_and_size("s:vm-1-disk-0,size=512M"),
        Some(("s".to_string(), 512 * 1024 * 1024))
    );
    // A mounted image is not a disk that gets copied.
    assert_eq!(
        super::disk_storage_and_size("local:iso/x.iso,media=cdrom,size=900M"),
        None
    );
    // Nor is an entry with no size at all.
    assert_eq!(super::disk_storage_and_size("none,media=cdrom"), None);
}

// ---------------------------------------------------------------------------
// Hardening: defects found by adversarial review. Every test here was watched
// failing against the code as first written.
// ---------------------------------------------------------------------------

#[test]
fn refusing_a_disk_with_nowhere_to_put_it_creates_nothing() {
    // The data_storage check used to run *after* the clone, and the error
    // return leaked an untagged machine -- the one state nothing collects.
    // The refusal is pure configuration, so it must precede every request
    // that makes anything.
    let pve = pve_with_template();
    let mut config = Config {
        api: pve.url(),
        node: NODE.to_string(),
        pool: POOL.to_string(),
        ids: IdRange::new(9000, 9099).unwrap(),
        token_file: "/dev/null".into(),
        data_storage: None,
        data_bus: "virtio1".to_string(),
        min_free_gb: 10,
        tls: Tls::Insecure,
        task_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(5),
    };
    config.data_storage = None;
    let mut p = Proxmox::with_token(config, "someone@realm!test=secret".into()).unwrap();
    p.set_poll_interval(Duration::from_millis(5));

    let e = p.create(&request("a-session", 1)).unwrap_err();
    assert!(e.to_string().contains("data_storage"), "{e}");
    assert!(
        pve.session_machines().is_empty(),
        "a refused create must leave nothing: {:?}",
        pve.session_machines()
    );
    assert!(
        !pve.paths().iter().any(|p| p.contains("/clone")),
        "the refusal must come before the network, not after the clone"
    );
}

#[test]
fn a_failed_clone_leaves_nothing_behind() {
    // The task reported failure, so the outcome is known -- which licenses
    // the cleanup a timeout does not. Whatever the hypervisor left of the
    // half-made machine is destroyed rather than abandoned untagged.
    let pve = pve_with_template();
    pve.with_state(|s| s.next_task_fails = Some("clone failed: no space".into()));
    let p = provider_for(&pve);

    let e = p.create(&request("a-session", 1)).unwrap_err();
    assert!(e.to_string().contains("no space"), "{e}");
    assert!(e.to_string().contains("nothing was left behind"), "{e}");
    assert!(
        pve.session_machines().is_empty(),
        "the leftover must be destroyed: {:?}",
        pve.session_machines()
    );
}

#[test]
fn a_clone_timeout_names_the_possible_orphan() {
    // A timeout's outcome is unknown, so nothing is destroyed -- but the
    // generic "the expiry tag means nothing is leaked" is FALSE for this one
    // path, because no tag has been applied yet. The message must say so
    // instead of reassuring.
    let pve = pve_with_template();
    pve.with_state(|s| s.tasks_never_finish = true);
    let p = provider_for(&pve);

    let e = p.create(&request("a-session", 1)).unwrap_err();
    assert!(
        e.to_string().contains("no expiry tag yet"),
        "the one untagged window must not be papered over: {e}"
    );
    assert!(e.to_string().contains("destroy"), "{e}");
}

#[test]
fn a_blinking_api_does_not_fail_a_running_task() {
    // One 502 on a status poll used to abort the whole wait, reporting an
    // operation as failed whose task was still running -- and, on create's
    // path, leaking the untagged clone.
    let pve = pve_with_template();
    pve.flake_next(2);
    let p = provider_for(&pve);

    p.create(&request("a-session", 1))
        .expect("two blinks inside the deadline are absorbed");
}

#[test]
fn a_third_partys_expires_tag_is_not_ours_to_delete() {
    // with_expiry deleted anything starting "expires-", including forms
    // expiry_of() itself refuses to read. What this module cannot read it
    // may not delete.
    let out = crate::tags::with_expiry("expires-soon;owner-x;expires-1700000000", at(1_800_000_000));
    assert!(out.contains("expires-soon"), "{out}");
    assert!(out.contains("owner-x"), "{out}");
    assert!(out.contains("expires-1800000000"), "{out}");
    assert!(!out.contains("expires-1700000000"), "{out}");
}

#[test]
fn a_machine_destroyed_mid_wait_is_gone_not_pending() {
    // PVE answers the agent query for a missing machine with a 500 naming its
    // configuration file. Swallowing that as "agent not up yet" made a
    // destroyed machine look forever pending to anything polling for an
    // address.
    let pve = pve_with_template();
    let p = provider_for(&pve);
    let e = p
        .address(&reaper_core::MachineRef::new("9042"))
        .expect_err("a missing machine is an answer, not a wait");
    assert!(
        matches!(e, reaper_core::ProviderError::NotFound(_)),
        "wanted NotFound, got {e}"
    );
}

#[test]
fn room_is_checked_against_the_storage_that_is_actually_short() {
    // Each storage against its own figure. The mock used to answer one shared
    // number for every storage name, so an accounting bug that summed
    // everything against one figure would have passed.
    let pve = pve_with_template();
    // The template's own disk lives on STORAGE and has room; the data disk's
    // storage is nearly full.
    pve.storage_named_has("some-storage", 10 * 1024 * 1024 * 1024);
    let p = provider_for(&pve);

    let e = p.create(&request("a-session", 1)).unwrap_err();
    let msg = e.to_string();
    assert!(msg.contains("some-storage"), "{msg}");
    assert!(
        pve.session_machines().is_empty(),
        "refused before anything was made"
    );
}

#[test]
fn every_kind_of_disk_a_clone_copies_is_priced() {
    // efidisk, tpmstate and unused volumes ride along in a full clone.
    for key in ["efidisk0", "tpmstate0", "unused0", "virtio0", "scsi12"] {
        assert!(crate::is_disk_key(key), "{key} is copied, so it is priced");
    }
    for key in ["virtiofs0", "efidisk", "unused", "net0", "ide"] {
        assert!(!crate::is_disk_key(key), "{key} is not a disk");
    }
}

#[test]
fn fractional_sizes_count_instead_of_costing_zero() {
    // PVE writes `size=4.5G` without embarrassment. A disk whose size cannot
    // be parsed used to contribute zero bytes, silently, to a check whose
    // whole job is refusing sessions that do not fit.
    assert_eq!(
        crate::parse_size("4.5G"),
        Some((4.5 * (1u64 << 30) as f64) as u64)
    );
    assert_eq!(crate::parse_size("528K"), Some(528 * 1024));
    assert_eq!(crate::parse_size("8G"), Some(8 * (1u64 << 30)));
    // Unknown suffixes stay unknown -- and the caller now says so out loud
    // rather than counting the disk at nothing.
    assert_eq!(crate::parse_size("2P"), None);
    assert_eq!(crate::parse_size(""), None);
    // The absurd does not wrap into the tiny.
    assert_eq!(crate::parse_size("99999999999999999999T"), None);
}

#[test]
fn every_certificate_in_a_ca_bundle_is_loaded() {
    // ureq's from_pem takes the FIRST certificate and drops the rest, so a
    // bundle (a rotation, or intermediate-then-root) silently trusted less
    // than the operator configured. The blocks are split first now.
    let bundle = "\
-----BEGIN CERTIFICATE-----\naaa\n-----END CERTIFICATE-----\n\
# a comment between blocks\n\
-----BEGIN CERTIFICATE-----\nbbb\n-----END CERTIFICATE-----\n";
    let blocks = crate::http::pem_blocks(bundle);
    assert_eq!(blocks.len(), 2, "{blocks:?}");
    assert!(blocks[0].contains("aaa"));
    assert!(blocks[1].contains("bbb"));
    assert!(crate::http::pem_blocks("not pem at all").is_empty());
}

#[test]
fn a_wrong_token_is_refused_by_the_stand_in() {
    // The stand-in used to authorize any request whose header merely
    // contained the scheme prefix, so a provider change that corrupted the
    // token value would have passed the whole suite.
    let pve = pve_with_template();
    let mut config = Config {
        api: pve.url(),
        node: NODE.to_string(),
        pool: POOL.to_string(),
        ids: IdRange::new(9000, 9099).unwrap(),
        token_file: "/dev/null".into(),
        data_storage: Some("some-storage".to_string()),
        data_bus: "virtio1".to_string(),
        min_free_gb: 10,
        tls: Tls::Insecure,
        task_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(5),
    };
    config.data_storage = Some("some-storage".to_string());
    let p = Proxmox::with_token(config, "someone@realm!test=WRONG".into()).unwrap();
    let e = p.list().unwrap_err();
    assert!(
        matches!(e, reaper_core::ProviderError::Unauthorized(_)),
        "{e}"
    );
}

// ---------------------------------------------------------------------------
// Fresh battery: branches the adversarial review found no test exercising.
// ---------------------------------------------------------------------------

#[test]
fn a_double_failure_names_the_machine_to_destroy_by_hand() {
    // Tags PUT fails AND the cleanup destroy fails: the machine is untagged
    // and cannot be removed, which is the one state a person must be told
    // about in so many words. The clone inherits protection from the
    // template, which is exactly how the cleanup comes to fail.
    let pve = pve_with_template();
    pve.with_state(|s| {
        s.vms.get_mut(&9000).unwrap().protection = true;
        s.reject_config_writes = true;
    });
    let p = provider_for(&pve);

    let e = p.create(&request("a-session", 1)).unwrap_err();
    let msg = e.to_string();
    assert!(msg.contains("by hand"), "{msg}");
    assert!(msg.contains("9001"), "name the machine, not just the problem: {msg}");
}

#[test]
fn a_v6_only_guest_still_yields_its_address() {
    let pve = pve_with_template();
    pve.with_state(|s| {
        s.agent_interfaces = Some(serde_json::json!([
            {"name": "lo0", "ip-addresses": [{"ip-address": "::1"}]},
            {"name": "vtnet0", "ip-addresses": [
                {"ip-address": "fe80::1"},
                {"ip-address": "2001:db8::7"}
            ]}
        ]));
    });
    let p = provider_for(&pve);
    let m = p.create(&request("a-session", 1)).expect("create");
    p.start(&m).expect("start");
    let a = p.address(&m).expect("query").expect("an address");
    assert_eq!(a.to_string(), "2001:db8::7", "global v6 beats loopback and link-local");
}

#[test]
fn an_agent_reply_shaped_as_a_bare_array_still_parses() {
    // Older agents answer the array without the {"result": ...} wrapper; the
    // parser takes both shapes, and only the wrapped one had a test.
    let bare = serde_json::json!([
        {"name": "eth0", "ip-addresses": [{"ip-address": "192.0.2.9"}]}
    ]);
    assert_eq!(
        crate::first_usable_address(&bare).map(|a| a.to_string()),
        Some("192.0.2.9".to_string())
    );
}

#[test]
fn from_table_refuses_what_its_pieces_refuse() {
    // The composed entry point: a bad table and an unreadable token file must
    // both come back as Config errors, not panics or defaults.
    let bad = table("api = \"https://x:8006\"\n");
    let Err(e) = Proxmox::from_table(&bad) else {
        panic!("a table with no node, pool or range should be refused");
    };
    assert!(
        matches!(e, reaper_core::ProviderError::Config(_)),
        "{e}"
    );
}

#[test]
fn a_stopped_machine_has_no_address_yet_rather_than_an_error() {
    // Observed live: PVE answers "VM <id> is not running" (no mention of the
    // agent) for a machine that exists but is stopped -- momentarily, during
    // a reboot, or because it crashed. Either way the honest answer to
    // "what is its address" is "none yet", not a raw API error that kills
    // the caller's wait loop.
    let pve = pve_with_template();
    let p = provider_for(&pve);
    let m = p.create(&request("a-session", 1)).expect("create");
    // Created and never started.
    assert_eq!(p.address(&m).expect("not an error"), None);
}

// ---------------------------------------------------------------------------
// Battery: the cluster changed underneath you. The 403-for-a-gone-machine
// trap was fixed in address() and destroy(); these hold every sibling to the
// same standard. Written before the fix and watched failing.
// ---------------------------------------------------------------------------

#[test]
fn renewing_a_gone_machine_says_gone_not_permission_denied() {
    let pve = pve_with_template();
    let p = provider_for(&pve);
    let m = p.create(&request("a-session", 1)).expect("create");
    pve.collect(m.as_str());
    let e = p.set_expiry(&m, at(2_000_000_000)).unwrap_err();
    assert!(
        matches!(e, reaper_core::ProviderError::NotFound(_)),
        "a pool-scoped token gets 403 for a missing machine; the caller must \
         hear \"gone\", not \"permission denied\": {e}"
    );
}

#[test]
fn starting_or_stopping_a_gone_machine_says_gone() {
    let pve = pve_with_template();
    let p = provider_for(&pve);
    let m = p.create(&request("a-session", 1)).expect("create");
    pve.collect(m.as_str());
    for (what, e) in [("start", p.start(&m).unwrap_err()), ("stop", p.stop(&m).unwrap_err())] {
        assert!(
            matches!(e, reaper_core::ProviderError::NotFound(_)),
            "{what}: {e}"
        );
    }
}

#[test]
fn a_machine_that_exists_still_renews_and_starts() {
    // The disambiguation must not make the ordinary path slower to be wrong:
    // a real permission problem on an existing machine stays a permission
    // problem, and a healthy machine still works.
    let pve = pve_with_template();
    let p = provider_for(&pve);
    let m = p.create(&request("a-session", 1)).expect("create");
    p.set_expiry(&m, at(2_000_000_000)).expect("renew works");
    p.start(&m).expect("start works");
    pve.with_state(|s| s.unauthorized = true);
    let e = p.set_expiry(&m, at(2_000_000_001)).unwrap_err();
    assert!(
        matches!(e, reaper_core::ProviderError::Unauthorized(_)),
        "a refusal for a machine that IS listed stays a refusal: {e}"
    );
}
