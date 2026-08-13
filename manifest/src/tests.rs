//! Tests for manifest loading.
//!
//! The fixtures under `test/` are shared with `test/run.sh`, which drives the
//! same cases through the binary. Both matter: this module proves the typed
//! model is right, and the shell suite proves the tool a person actually runs
//! behaves the same way.

use super::*;

fn fixture(rel: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn load_ok(rel: &str) -> Manifest {
    load(&fixture(rel)).unwrap_or_else(|e| panic!("{rel} should load: {e}"))
}

fn load_err(rel: &str) -> Error {
    match load(&fixture(rel)) {
        Ok(_) => panic!("{rel} should have been rejected"),
        Err(e) => e,
    }
}

/// Every shipped example must load. Discovered rather than listed, so that
/// adding an example extends this test with no edit here -- and so that these
/// tests name no tenant, which is the same rule the framework itself follows.
#[test]
fn every_shipped_example_loads() {
    let dir = fixture("examples");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).expect("examples directory") {
        let path = entry.expect("readable entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let m = load(&path).unwrap_or_else(|e| panic!("{} should load: {e}", path.display()));
        assert!(!m.project.is_empty());
        assert!(!m.guests.is_empty());
        seen += 1;
    }
    assert!(seen >= 2, "expected at least two worked examples, found {seen}");
}

/// The execution-mode invariant, asserted over whatever examples exist rather
/// than over a remembered list of their contents. Asserting the property is
/// also a better test than asserting the fixture: it keeps holding when the
/// examples change.
#[test]
fn every_example_guest_agrees_with_its_execution_mode() {
    let dir = fixture("examples");
    for entry in std::fs::read_dir(&dir).expect("examples directory") {
        let path = entry.expect("readable entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        for g in &load(&path).expect("loads").guests {
            match (g.exec, g.build.as_ref().and_then(|b| b.image.as_ref())) {
                (Exec::Container, image) => assert!(
                    image.is_some() || g.build.is_none(),
                    "{}: container execution needs a toolchain image",
                    g.name
                ),
                (Exec::Host, image) => assert!(
                    image.is_none(),
                    "{}: host execution has no image to run in",
                    g.name
                ),
            }
            assert!(!g.run.cmd.is_empty());
        }
    }
}

#[test]
fn a_container_guest_carries_its_toolchain_and_pre_pulled_images() {
    let m = load_ok("test/valid/container-with-images.yaml");
    assert_eq!(m.guests.len(), 1);

    let g = &m.guests[0];
    assert_eq!(g.exec, Exec::Container);
    assert!(g.build.as_ref().unwrap().image.is_some());
    assert_eq!(g.run.images.len(), 3);
    assert_eq!(m.reset, vec!["state".to_string()]);
    assert_eq!(g.resources.cores, Some(4));
    assert_eq!(g.resources.ram_gb, Some(8));
}

#[test]
fn a_project_that_runs_no_containers_declares_no_images() {
    // The assertion that the schema is not shaped around one reference tenant.
    let m = load_ok("test/valid/no-images.yaml");
    assert_eq!(m.guests[0].exec, Exec::Host);
    assert!(m.guests[0].run.images.is_empty());
    assert!(m.guests[0].build.as_ref().unwrap().image.is_none());
}

#[test]
fn the_smallest_legal_manifest_needs_no_build_and_no_reset() {
    let m = load_ok("test/valid/minimal.yaml");
    assert_eq!(m.guests.len(), 1);
    assert!(m.guests[0].build.is_none());
    assert!(m.reset.is_empty());
    assert!(m.profiles.is_empty());
}

#[test]
fn defaults_and_overrides_merge_across_scopes() {
    let m = load_ok("test/valid/inherited-across-scopes.yaml");
    let a = m.guest("guest-a").unwrap();
    let b = m.guest("guest-b").unwrap();

    assert_eq!(a.exec, Exec::Host);
    assert_eq!(b.exec, Exec::Container);
    // Command from the top level, image from the guest: neither scope holds
    // both, so this passes only if resolution merges before the typed model is
    // built.
    assert_eq!(a.build.as_ref().unwrap().cmd, "make build");
    assert_eq!(b.build.as_ref().unwrap().cmd, "make build");
    assert!(b.build.as_ref().unwrap().image.is_some());
}

#[test]
fn profiles_are_read_but_not_interpreted() {
    let m = load_ok("test/valid/container-with-images.yaml");
    let nightly = m.profiles.get("nightly").expect("nightly profile");
    // Kept as written. This crate has no notion of time, deliberately: turning
    // "12h" into a duration is the caller's job.
    assert_eq!(nightly.ttl.as_deref(), Some("12h"));
    assert_eq!(nightly.warm_cache, Some(false));
}

/// Every invalid fixture must be rejected as *invalid* rather than blowing up
/// as an internal error, because the difference is what a user is told.
#[test]
fn invalid_fixtures_are_reported_as_invalid() {
    for name in [
        "container-exec-without-image",
        "host-exec-with-image",
        "no-guests",
        "reset-work-dataset",
        "tag-and-digest",
        "tag-not-digest",
        "unregistered-key",
        "wrong-schema-version",
    ] {
        let e = load_err(&format!("test/invalid/{name}.yaml"));
        assert!(
            matches!(e, Error::Invalid { .. }),
            "{name} should be Invalid, got: {e}"
        );
    }
}

#[test]
fn the_exec_conditional_is_checked_after_merging() {
    // exec on the guest, build.image at the top level. Neither location is
    // wrong alone; only the merged form is.
    let e = load_err("test/invalid/host-exec-with-image.yaml");
    let Error::Invalid { problems, .. } = &e else {
        panic!("expected Invalid, got {e}");
    };
    assert!(
        problems.iter().any(|p| p.contains("once defaults are merged in")),
        "the failure should name the merged form: {problems:?}"
    );
}

#[test]
fn every_problem_is_reported_not_just_the_first() {
    // Two independent faults: an unpinned image and an unknown key.
    let text = "
schema: 1
project: two-faults
guests: [g]
exec: container
build: { image: 'docker.io/library/x:latest', cmd: 'make' }
run: { cmd: 'make test' }
nonsense: true
";
    let e = match from_str(text, "<inline>") {
        Err(e) => e,
        Ok(_) => panic!("should be invalid"),
    };
    let Error::Invalid { problems, .. } = &e else {
        panic!("expected Invalid, got {e}");
    };
    assert!(
        problems.len() >= 2,
        "both faults should be reported, got: {problems:?}"
    );
}

#[test]
fn unparseable_yaml_is_a_parse_error_not_an_invalid_manifest() {
    // The distinction matters: "your YAML is broken" and "your manifest is
    // wrong" send a reader to different places.
    let e = match from_str("guests: [unclosed", "<inline>") {
        Err(e) => e,
        Ok(_) => panic!("should not parse"),
    };
    assert!(matches!(e, Error::Parse { .. }), "got {e}");
}

#[test]
fn a_missing_file_is_a_read_error() {
    let e = match load(&fixture("test/does-not-exist.yaml")) {
        Err(e) => e,
        Ok(_) => panic!("should not load"),
    };
    assert!(matches!(e, Error::Read { .. }), "got {e}");
}

#[test]
fn the_embedded_schema_compiles() {
    // If this fails, every other test in this file is failing for the wrong
    // reason.
    let schema: Value = serde_json::from_str(SCHEMA).expect("schema is JSON");
    jsonschema::validator_for(&schema).expect("schema compiles");
}
