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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    /// Present exactly when `exec` is `Container`; the schema enforces both
    /// halves of that.
    #[serde(default)]
    pub image: Option<String>,
    pub cmd: String,
    #[serde(default)]
    pub cache: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
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
    pub exec: Exec,
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
        .map_err(Error::Internal)?
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
        for g in resolve_guests(doc).map_err(Error::Internal)? {
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

        let mut resolved = Map::new();
        resolved.insert("name".into(), Value::String(name));

        if let Some(exec) = over.get("exec").or_else(|| doc.get("exec")) {
            resolved.insert("exec".into(), exec.clone());
        }
        for key in ["build", "run", "resources"] {
            if let Some(merged) = merge(doc.get(key), over.get(key)) {
                resolved.insert(key.into(), merged);
            }
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
