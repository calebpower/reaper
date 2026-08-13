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
        tls: Tls::Insecure,
        task_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(5),
    };
    let mut p = Proxmox::with_token(config, "cal@pve!test=secret".into()).expect("provider");
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

    let paths = pve.paths();
    let clone_at = paths.iter().position(|p| p.contains("/clone")).expect("cloned");
    let config_at = paths
        .iter()
        .position(|p| p.ends_with("/config") )
        .expect("configured");
    assert!(clone_at < config_at, "tagged before cloning? {paths:?}");
    assert!(
        !paths.iter().any(|p| p.contains("/status/start")),
        "create must not start the machine: {paths:?}"
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
        s.vms.insert(9001, Vm { name: "a-session".into(), pool: POOL.into(), ..Vm::default() });
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
        s.vms.insert(9001, Vm { name: "a-session".into(), pool: POOL.into(), ..Vm::default() });
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

fn token_file(name: &str, contents: &str, mode: u32) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("reaper-token-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("token");
    std::fs::write(&path, contents).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
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
        ("blank", "   \n"),
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

    let config_calls = pve
        .paths()
        .iter()
        .filter(|path| path.ends_with("/config"))
        .count();
    assert_eq!(config_calls, 1, "expected one config write, saw {config_calls}");
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
        tls: Tls::Insecure,
        task_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(5),
    };
    config.data_storage = None;
    let mut p = Proxmox::with_token(config, "someone@realm!t=s".into()).unwrap();
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
    let mut p = Proxmox::with_token(config, "someone@realm!t=s".into()).unwrap();
    p.set_poll_interval(Duration::from_millis(5));

    let m = p.create(&request("a-session", 1)).expect("create");
    let disks = pve.attached_disks(m.as_str());
    assert_eq!(disks.get("scsi3").map(String::as_str), Some("s:64"), "{disks:?}");
}
