//! Validates a tenant manifest (`.reaper.yaml`) against the normative schema.
//!
//! Two passes, because one is not enough:
//!
//! 1. The document as written, against the root schema. Catches unknown keys,
//!    unpinned image references, malformed names, and anything else that is
//!    wrong on the page.
//!
//! 2. Each guest *after resolution*, against `$defs/resolvedGuest`. A manifest
//!    may perfectly reasonably declare `exec` at the top level and `build.image`
//!    inside one guest's overrides, so neither key on its own tells you whether
//!    the pair is coherent. Only the merged form does, and merging is not
//!    something JSON Schema can compute. This is the pass that catches a
//!    `build.image` sitting alongside `exec: host`.
//!
//! The schema is embedded rather than located at runtime, so the binary and the
//! schema can never disagree about which version is in force. Cargo tracks the
//! `include_str!` and rebuilds when the schema changes.

use std::process::ExitCode;

use serde_json::{json, Map, Value};

/// The normative schema. `manifest/schema/v1.json` is the single source; this
/// is the same bytes, compiled in.
const SCHEMA_SRC: &str = include_str!("../../schema/v1.json");

fn main() -> ExitCode {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() || paths.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!("usage: reaper-manifest-validate <manifest.yaml> [...]");
        eprintln!();
        eprintln!("Validates each manifest against the reaper manifest schema, v1.");
        eprintln!("Exit 0 if every manifest is valid, 1 if any is invalid, 2 on a");
        eprintln!("usage or I/O error.");
        return ExitCode::from(2);
    }

    let schema: Value = match serde_json::from_str(SCHEMA_SRC) {
        Ok(v) => v,
        // The schema is compiled in, so this is a build-time defect rather
        // than anything the caller did. Fail loudly and distinctly.
        Err(e) => {
            eprintln!("internal error: the embedded schema is not valid JSON: {e}");
            return ExitCode::from(2);
        }
    };

    // The second pass validates a single resolved guest, so it needs a schema
    // whose root *is* that definition. Rehosting the `$defs` block alongside a
    // root `$ref` keeps every internal reference resolvable without copying
    // definitions around.
    let resolved_schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": "#/$defs/resolvedGuest",
        "$defs": schema.get("$defs").cloned().unwrap_or(Value::Null),
    });

    let root_validator = match jsonschema::validator_for(&schema) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("internal error: the embedded schema does not compile: {e}");
            return ExitCode::from(2);
        }
    };
    let guest_validator = match jsonschema::validator_for(&resolved_schema) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("internal error: the resolved-guest schema does not compile: {e}");
            return ExitCode::from(2);
        }
    };

    let mut invalid = 0usize;
    let mut errored = 0usize;

    for path in &paths {
        match check(path, &root_validator, &guest_validator) {
            Outcome::Valid(guests) => {
                println!("ok    {path}  ({guests} guest{})", plural(guests));
            }
            Outcome::Invalid(problems) => {
                invalid += 1;
                println!("FAIL  {path}");
                for p in problems {
                    println!("        {p}");
                }
            }
            Outcome::Error(msg) => {
                errored += 1;
                println!("ERROR {path}");
                println!("        {msg}");
            }
        }
    }

    if errored > 0 {
        ExitCode::from(2)
    } else if invalid > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

enum Outcome {
    /// Valid, carrying the number of guests that were resolved and checked.
    Valid(usize),
    Invalid(Vec<String>),
    Error(String),
}

fn check(path: &str, root: &jsonschema::Validator, guest: &jsonschema::Validator) -> Outcome {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return Outcome::Error(format!("cannot read: {e}")),
    };

    // YAML in, JSON model out. A YAML document that cannot be represented as
    // JSON -- a non-string mapping key, say -- fails here, which is the right
    // answer: the schema describes a JSON shape.
    let doc: Value = match serde_yaml_ng::from_str(&text) {
        Ok(v) => v,
        Err(e) => return Outcome::Error(format!("cannot parse as YAML: {e}")),
    };

    let mut problems: Vec<String> = root
        .iter_errors(&doc)
        .map(|e| format!("{}: {e}", location(&e.instance_path().to_string())))
        .collect();

    let mut guests = 0usize;

    // Pass two only runs when pass one is clean. Resolution assumes the
    // document is shaped the way the root schema says it is, and reporting
    // resolution fallout on top of the structural errors that caused it would
    // bury the error that matters.
    if problems.is_empty() {
        match resolve_guests(&doc) {
            Ok(resolved) => {
                guests = resolved.len();
                for g in &resolved {
                    let name = g
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("<unnamed>")
                        .to_string();
                    for e in guest.iter_errors(g) {
                        problems.push(format!(
                            "guest {name}, once defaults are merged in: {}: {e}",
                            location(&e.instance_path().to_string())
                        ));
                    }
                }
            }
            Err(e) => return Outcome::Error(e),
        }
    }

    // One decision, in one place. Deciding it earlier allowed an Invalid with
    // an empty problem list -- a verdict that reports a failure and then has
    // nothing to say about it.
    if problems.is_empty() {
        Outcome::Valid(guests)
    } else {
        Outcome::Invalid(problems)
    }
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
    match (base.and_then(Value::as_object), over.and_then(Value::as_object)) {
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
