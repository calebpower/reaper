//! Reading and validating a tenant manifest (`.reaper.yaml`).
//!
//! The manifest is the entire integration surface between a project and reaper,
//! so this crate is deliberately the only place that knows its shape. The
//! schema in `schema/v1.json` is normative and is embedded rather than located
//! at runtime, so the code and the schema can never disagree about which
//! version is in force.
//!
//! Validation is two passes:
//!
//! 1. The document as written, against the root schema. Catches unknown keys,
//!    unpinned image references, malformed names.
//!
//! 2. Each guest *after resolution*, against `$defs/resolvedGuest`. A manifest
//!    may reasonably declare `exec` at the top level and `build.image` inside
//!    one guest's overrides, so neither key alone tells you whether the pair is
//!    coherent. Only the merged form does, and merging is not something JSON
//!    Schema can compute.
//!
//! Callers get [`Manifest`], whose guests are already resolved -- the form the
//! runner would act on, not the form the file happens to be written in.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use serde::Deserialize;
use serde_json::{json, Map, Value};

/// The normative schema. `manifest/schema/v1.json` is the single source.
pub const SCHEMA: &str = include_str!("../schema/v1.json");

#[derive(Debug)]
pub enum Error {
    /// The file could not be read.
    Read { path: String, source: std::io::Error },
    /// The file is not parseable as YAML, or is not expressible as JSON.
    Parse { path: String, message: String },
    /// The manifest is well-formed but does not satisfy the schema. Every
    /// problem is reported, not just the first: a caller fixing one at a time
    /// learns less than a caller shown all of them.
    Invalid { path: String, problems: Vec<String> },
    /// The embedded schema is broken, or resolution produced something the
    /// typed model cannot represent. Either is a defect in this crate rather
    /// than anything the caller did, and is reported separately for that reason.
    Internal(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Read { path, source } => write!(f, "{path}: cannot read: {source}"),
            Error::Parse { path, message } => write!(f, "{path}: cannot parse as YAML: {message}"),
            Error::Invalid { path, problems } => {
                write!(f, "{path}: not a valid manifest")?;
                for p in problems {
                    write!(f, "\n  {p}")?;
                }
                Ok(())
            }
            Error::Internal(m) => write!(f, "internal error: {m}"),
        }
    }
}

impl std::error::Error for Error {}

/// How a guest's commands are executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Exec {
    /// In a digest-pinned toolchain image; the template stays generic.
    Container,
    /// Directly in the guest, with the toolchain supplied by the template.
    Host,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    /// Resolved: this verb's own mode if it declared one, otherwise the
    /// guest's. Execution mode belongs to a verb rather than to a guest,
    /// because the two verbs may legitimately disagree -- a project can need a
    /// pinned toolchain to build and the guest's own container engine to run.
    pub exec: Exec,
    /// Present exactly when this verb's `exec` is `Container`; the schema
    /// enforces both halves of that.
    #[serde(default)]
    pub image: Option<String>,
    pub cmd: String,
    /// The guest's caches. Declared here and mounted for every verb: a second
    /// list under `run` would be a key that exists to be forgotten.
    #[serde(default)]
    pub cache: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    /// As [`Build::exec`].
    pub exec: Exec,
    /// Under container execution, `build.image` unless this verb named its own.
    #[serde(default)]
    pub image: Option<String>,
    /// Opaque. This crate does not know what a stage, a journey or a seed is.
    pub cmd: String,
    /// May be empty: a project that runs no containers declares no images.
    #[serde(default)]
    pub images: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub cores: Option<u16>,
    pub ram_gb: Option<u16>,
    /// Size of the session's storage pool, in gibibytes. `None` defers to the
    /// site's default.
    pub disk_gb: Option<u32>,
}

/// One guest, with the top-level defaults already merged in.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Guest {
    /// Resolved against the site registry by the caller. Free-form here: this
    /// crate deliberately has no list of known operating systems.
    pub name: String,
    /// The guest's default mode, when one was stated. Every consumer reads the
    /// verbs' own resolved `exec`, which is where mode actually lives; a
    /// manifest that states it per verb and nowhere else is complete, and
    /// requiring an unread default was refusing coherent manifests.
    #[serde(default)]
    pub exec: Option<Exec>,
    #[serde(default)]
    pub build: Option<Build>,
    pub run: Run,
    #[serde(default)]
    pub resources: Resources,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Duration string; the schema constrains the syntax. Interpreting it is
    /// the caller's job, so that this crate needs no notion of time.
    #[serde(default)]
    pub ttl: Option<String>,
    #[serde(default)]
    pub warm_cache: Option<bool>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub project: String,
    pub guests: Vec<Guest>,
    /// Dataset names `reset` rolls back. The schema restricts this to `state`.
    pub reset: Vec<String>,
    /// rsync patterns the tenant keeps out of the forward sync. The results
    /// directory is excluded whatever this says; see `docs/tenants.md`.
    pub sync_exclude: Vec<String>,
    pub profiles: BTreeMap<String, Profile>,
}

impl Manifest {
    /// The guest with this name, if the manifest declares it.
    pub fn guest(&self, name: &str) -> Option<&Guest> {
        self.guests.iter().find(|g| g.name == name)
    }
}

/// Read and validate a manifest, returning it with guests resolved.
pub fn load(path: &Path) -> Result<Manifest, Error> {
    let display = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|e| Error::Read {
        path: display.clone(),
        source: e,
    })?;
    from_str(&text, &display)
}

/// As [`load`], for a manifest already in memory. `origin` names it in errors.
pub fn from_str(text: &str, origin: &str) -> Result<Manifest, Error> {
    // YAML in, JSON model out. A YAML document that cannot be represented as
    // JSON -- a non-string mapping key, say -- fails here, which is right: the
    // schema describes a JSON shape.
    let doc: Value = serde_yaml_ng::from_str(text).map_err(|e| Error::Parse {
        path: origin.to_string(),
        message: e.to_string(),
    })?;

    let problems = validate(&doc)?;
    if !problems.is_empty() {
        return Err(Error::Invalid {
            path: origin.to_string(),
            problems,
        });
    }

    let guests = resolve_guests(&doc)
        .map_err(|problem| Error::Invalid {
            path: origin.to_string(),
            problems: vec![problem],
        })?
        .into_iter()
        .map(|g| serde_json::from_value::<Guest>(g).map_err(|e| Error::Internal(e.to_string())))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Manifest {
        project: doc
            .get("project")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Internal("no project after validation".into()))?
            .to_string(),
        guests,
        reset: doc
            .get("reset")
            .and_then(|r| r.get("datasets"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        sync_exclude: doc
            .get("sync")
            .and_then(|s| s.get("exclude"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        profiles: doc
            .get("profiles")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| Error::Internal(e.to_string()))?
            .unwrap_or_default(),
    })
}

/// Every schema problem with the document, in both passes. Empty means valid.
pub fn validate(doc: &Value) -> Result<Vec<String>, Error> {
    let schema: Value = serde_json::from_str(SCHEMA)
        .map_err(|e| Error::Internal(format!("the embedded schema is not valid JSON: {e}")))?;

    // The second pass validates a single resolved guest, so it needs a schema
    // whose root *is* that definition. Rehosting `$defs` alongside a root
    // `$ref` keeps every internal reference resolvable without copying
    // definitions around.
    let resolved_schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": "#/$defs/resolvedGuest",
        "$defs": schema.get("$defs").cloned().unwrap_or(Value::Null),
    });

    let root = jsonschema::validator_for(&schema)
        .map_err(|e| Error::Internal(format!("the embedded schema does not compile: {e}")))?;
    let per_guest = jsonschema::validator_for(&resolved_schema)
        .map_err(|e| Error::Internal(format!("the resolved-guest schema does not compile: {e}")))?;

    let mut problems: Vec<String> = root
        .iter_errors(doc)
        .map(|e| format!("{}: {e}", location(&e.instance_path().to_string())))
        .collect();

    // Pass two only runs when pass one is clean. Resolution assumes the
    // document is shaped the way the root schema says it is, and reporting
    // resolution fallout on top of the structural errors that caused it would
    // bury the error that matters.
    if problems.is_empty() {
        // Resolution can refuse things the schema cannot express -- duplicate
        // guest names across the two spellings, cache names that collide
        // after env-var mangling. Those are the tenant's to fix, so they
        // join the problem list rather than masquerading as internal errors.
        let resolved = match resolve_guests(doc) {
            Ok(r) => r,
            Err(problem) => {
                problems.push(problem);
                return Ok(problems);
            }
        };
        for g in resolved {
            let name = g
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("<unnamed>")
                .to_string();
            for e in per_guest.iter_errors(&g) {
                problems.push(format!(
                    "guest {name}, once defaults are merged in: {}: {e}",
                    location(&e.instance_path().to_string())
                ));
            }
        }
    }

    Ok(problems)
}

/// JSON Pointer paths read poorly when empty; name the document root instead.
fn location(pointer: &str) -> String {
    if pointer.is_empty() {
        "(document root)".to_string()
    } else {
        pointer.to_string()
    }
}

/// Merge the top-level defaults with each guest's own overrides, producing the
/// form the runner would actually act on.
///
/// `build`, `run` and `resources` merge key by key, so a guest can supply just
/// `build.image` and inherit the command and caches. `exec` is a scalar and is
/// replaced outright.
fn resolve_guests(doc: &Value) -> Result<Vec<Value>, String> {
    let entries = doc
        .get("guests")
        .and_then(Value::as_array)
        .ok_or_else(|| "no guests array; the root schema should have caught this".to_string())?;

    let mut out = Vec::with_capacity(entries.len());
    let mut seen: Vec<String> = Vec::new();

    for entry in entries {
        // Shorthand is a bare name; expanded form is an object carrying it.
        let (name, over) = match entry {
            Value::String(s) => (s.clone(), Map::new()),
            Value::Object(o) => {
                let n = o
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "guest object without a name".to_string())?;
                (n.to_string(), o.clone())
            }
            _ => return Err("guest entry is neither a name nor an object".to_string()),
        };

        // The schema's uniqueItems compares whole JSON values, so the same
        // guest as a bare name and as an object slips through -- and the
        // second entry then silently loses to the first everywhere downstream.
        if seen.contains(&name) {
            return Err(format!(
                "guests declares {name:?} more than once; a guest gets one entry, whichever form it is written in"
            ));
        }
        seen.push(name.clone());

        let mut resolved = Map::new();
        resolved.insert("name".into(), Value::String(name));

        // The guest's default execution mode. Each verb may override it.
        let guest_exec = over.get("exec").or_else(|| doc.get("exec")).cloned();
        if let Some(e) = &guest_exec {
            resolved.insert("exec".into(), e.clone());
        }
        if let Some(merged) = merge(doc.get("resources"), over.get("resources")) {
            resolved.insert("resources".into(), merged);
        }

        let mut build = merge(doc.get("build"), over.get("build"));
        let mut run = merge(doc.get("run"), over.get("run"));

        // Execution mode belongs to a verb, not to a guest. A project can
        // perfectly reasonably need a pinned toolchain to build and the guest's
        // own container engine to run -- a toolchain image has no engine client
        // inside it -- so the guest's mode is only the default.
        for block in [&mut build, &mut run] {
            if let (Some(Value::Object(o)), Some(e)) = (block, &guest_exec) {
                o.entry("exec").or_insert_with(|| e.clone());
            }
        }

        // A container-execution `run` with no image of its own runs in the
        // toolchain the build declared, so a project whose two verbs share one
        // image writes the digest once. Deliberately never for a
        // host-execution run: it has nowhere to run an image, and inheriting
        // one would earn a rejection for a key the tenant never wrote.
        if let (Some(Value::Object(r)), Some(Value::Object(b))) = (&mut run, &build) {
            if r.get("exec") == Some(&json!("container")) && !r.contains_key("image") {
                if let Some(image) = b.get("image") {
                    r.insert("image".into(), image.clone());
                }
            }
        }

        // Cache names become REAPER_CACHE_<NAME> with [a-z.-] mangled to
        // [A-Z__], so my-cache, my.cache and my_cache all become the same
        // variable and only one of them is reachable the documented way.
        // uniqueItems cannot see that; this can.
        if let Some(cache) = build
            .as_ref()
            .and_then(|b| b.get("cache"))
            .and_then(Value::as_array)
        {
            let mut vars: Vec<(String, String)> = Vec::new();
            for c in cache.iter().filter_map(Value::as_str) {
                let var: String = c
                    .chars()
                    .map(|ch| match ch {
                        '.' | '-' => '_',
                        other => other.to_ascii_uppercase(),
                    })
                    .collect();
                if let Some((prev, _)) = vars.iter().find(|(_, v)| *v == var) {
                    return Err(format!(
                        "caches {prev:?} and {c:?} would share the environment variable REAPER_CACHE_{var}; only one of them would be reachable. Pick names that differ in more than punctuation"
                    ));
                }
                vars.push((c.to_string(), var));
            }
        }

        if let Some(b) = build {
            resolved.insert("build".into(), b);
        }
        if let Some(r) = run {
            resolved.insert("run".into(), r);
        }

        out.push(Value::Object(resolved));
    }

    Ok(out)
}

/// Shallow merge of two optional objects, with the override winning per key.
fn merge(base: Option<&Value>, over: Option<&Value>) -> Option<Value> {
    match (
        base.and_then(Value::as_object),
        over.and_then(Value::as_object),
    ) {
        (None, None) => None,
        (Some(b), None) => Some(Value::Object(b.clone())),
        (None, Some(o)) => Some(Value::Object(o.clone())),
        (Some(b), Some(o)) => {
            let mut m = b.clone();
            for (k, v) in o {
                m.insert(k.clone(), v.clone());
            }
            Some(Value::Object(m))
        }
    }
}

#[cfg(test)]
mod tests;
