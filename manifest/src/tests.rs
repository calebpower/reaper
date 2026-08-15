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
fn every_example_verb_agrees_with_its_execution_mode() {
    // Per verb, not per guest: the two verbs of one guest may legitimately
    // differ, so checking the guest's default against the build's image -- as
    // this once did -- would ask the wrong question of a manifest that splits
    // them.
    fn coherent(what: &str, exec: Exec, image: Option<&String>) {
        match exec {
            Exec::Container => assert!(
                image.is_some(),
                "{what}: container execution needs a toolchain image"
            ),
            Exec::Host => assert!(
                image.is_none(),
                "{what}: host execution has no image to run in"
            ),
        }
    }

    let dir = fixture("examples");
    for entry in std::fs::read_dir(&dir).expect("examples directory") {
        let path = entry.expect("readable entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        for g in &load(&path).expect("loads").guests {
            if let Some(b) = &g.build {
                coherent(&format!("{} build", g.name), b.exec, b.image.as_ref());
            }
            coherent(&format!("{} run", g.name), g.run.exec, g.run.image.as_ref());
            assert!(!g.run.cmd.is_empty());
        }
    }
}

#[test]
fn a_container_guest_carries_its_toolchain_and_pre_pulled_images() {
    let m = load_ok("test/valid/container-with-images.yaml");
    assert_eq!(m.guests.len(), 1);

    let g = &m.guests[0];
    assert_eq!(g.exec, Some(Exec::Container));
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
    assert_eq!(m.guests[0].exec, Some(Exec::Host));
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

    assert_eq!(a.exec, Some(Exec::Host));
    assert_eq!(b.exec, Some(Exec::Container));
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
    // Discovered rather than listed. A remembered list is a list that goes stale
    // the first time someone adds a fixture and does not think to come here.
    let dir = fixture("test/invalid");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).expect("invalid fixtures") {
        let path = entry.expect("readable entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let e = match load(&path) {
            Ok(_) => panic!("{} should have been rejected", path.display()),
            Err(e) => e,
        };
        assert!(
            matches!(e, Error::Invalid { .. }),
            "{} should be Invalid, got: {e}",
            path.display()
        );
        seen += 1;
    }
    assert!(seen >= 8, "expected the invalid fixtures, found {seen}");
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

/// Execution mode is a property of a verb, and the guest's is only a default.
///
/// This is the case that made the change necessary: a project whose build needs
/// a pinned toolchain and whose run drives the guest's own container engine.
/// Running the second inside the first cannot work -- a toolchain image carries
/// no engine client -- so the pair has to be expressible.
#[test]
fn a_verb_may_override_the_guests_execution_mode() {
    let m = load_ok("test/valid/per-verb-exec.yaml");
    let g = &m.guests[0];

    assert_eq!(g.exec, Some(Exec::Container), "the guest's default");
    assert_eq!(g.build.as_ref().unwrap().exec, Exec::Container);
    assert_eq!(g.run.exec, Exec::Host, "the run overrode it");

    // And the override does not drag an image along with it.
    assert!(g.build.as_ref().unwrap().image.is_some());
    assert_eq!(
        g.run.image, None,
        "a host-execution run has nowhere to run an image, so it must not \
         inherit one -- it would then be rejected for a key nobody wrote"
    );
}

/// Two verbs in one toolchain declare the digest once.
#[test]
fn a_container_run_inherits_the_build_image() {
    let m = load_ok("test/valid/run-inherits-the-build-image.yaml");
    let g = &m.guests[0];
    let build_image = g.build.as_ref().unwrap().image.clone();

    assert!(build_image.is_some());
    assert_eq!(
        g.run.image, build_image,
        "the run declared no image of its own, so it runs in the build's"
    );
}

/// A run that names its own image keeps it. Inheritance is a fallback, not an
/// overwrite -- the opposite would silently ignore what the tenant wrote.
#[test]
fn a_run_that_names_an_image_keeps_it() {
    let text = "
schema: 1
project: two-images
guests: [g]
exec: container
build:
  image: docker.io/library/builder@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  cmd: make
run:
  image: docker.io/library/driver@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
  cmd: make check
";
    let m = from_str(text, "<inline>").expect("valid");
    assert!(m.guests[0].run.image.as_deref().unwrap().contains("driver"));
}

/// Sync exclusions are read and handed on uninterpreted. The framework does not
/// know what any of these patterns mean, and must not learn.
#[test]
fn sync_exclusions_are_read_verbatim() {
    let m = load_ok("test/valid/sync-excludes.yaml");
    assert_eq!(m.sync_exclude, vec!["/target/", "*.tmp", ".venv/"]);

    // A manifest with no sync block excludes nothing of its own.
    assert!(load_ok("test/valid/minimal.yaml").sync_exclude.is_empty());
}

/// A verb cannot ask for container execution when no image exists to run in --
/// including the case where there is no build block to inherit one from.
#[test]
fn a_container_verb_with_no_image_anywhere_is_refused() {
    let e = load_err("test/invalid/container-run-without-any-image.yaml");
    let Error::Invalid { problems, .. } = &e else {
        panic!("expected Invalid, got {e}");
    };
    assert!(
        problems.iter().any(|p| p.contains("/run") && p.contains("image")),
        "the failure should name the run's missing image: {problems:?}"
    );
}

// ---------------------------------------------------------------------------
// Hardening: defects found by adversarial review. Every test here was watched
// failing against the code as first written.
// ---------------------------------------------------------------------------

#[test]
fn the_same_guest_in_two_spellings_is_refused() {
    let e = load_err("test/invalid/duplicate-guest.yaml");
    let Error::Invalid { problems, .. } = &e else {
        panic!("a doubled guest is the tenant's mistake, not ours: {e}");
    };
    assert!(
        problems.iter().any(|p| p.contains("more than once")),
        "{problems:?}"
    );
}

#[test]
fn cache_names_that_mangle_to_one_variable_are_refused() {
    let e = load_err("test/invalid/cache-name-collision.yaml");
    let Error::Invalid { problems, .. } = &e else {
        panic!("colliding caches are the tenant's mistake, not ours: {e}");
    };
    assert!(
        problems.iter().any(|p| p.contains("REAPER_CACHE_MY_CACHE")),
        "the refusal must name the variable both would become: {problems:?}"
    );
}

#[test]
fn an_image_without_a_real_registry_host_is_refused() {
    // A hub namespace is not a host, and uppercase paths fail at pull time.
    load_err("test/invalid/hostless-image.yaml");
    load_err("test/invalid/uppercase-image-path.yaml");
    // The forms real sites use still pass.
    load_ok("test/valid/per-verb-exec.yaml");
}

#[test]
fn a_manifest_that_states_exec_only_per_verb_is_complete() {
    // Both verbs carry their mode; requiring an unread guest-level default on
    // top refused coherent manifests.
    let m = load_str(
        r#"
schema: 1
project: verbwise
guests: [some-guest]
build: {exec: host, cmd: make deps}
run: {exec: host, cmd: make check}
"#,
    );
    let g = &m.guests[0];
    assert_eq!(g.exec, None, "no default was stated, and none is invented");
    assert_eq!(g.build.as_ref().unwrap().exec, Exec::Host);
    assert_eq!(g.run.exec, Exec::Host);
}

/// Load from a string by way of a scratch file, for shapes small enough that
/// a fixture file would just put distance between the test and its data.
fn load_str(text: &str) -> Manifest {
    // Drop, not a call at the end: a panic inside load() must still remove
    // the directory. The invariants battery refuses this file without it,
    // and it caught this very helper leaking on its first draft.
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let dir = Scratch(std::env::temp_dir().join(format!(
        "reaper-manifest-test-{}-{}",
        std::process::id(),
        text.len()
    )));
    std::fs::create_dir_all(&dir.0).unwrap();
    let path = dir.0.join("m.yaml");
    std::fs::write(&path, text).unwrap();
    load(&path).unwrap_or_else(|e| panic!("should load: {e}"))
}
