//! Turning a tenant's command into something safe to deliver.
//!
//! A job is the unit of execution: the tenant's command, its environment, and
//! nothing else. It is rendered here, in a language with tests, rather than
//! assembled by a shell script out of arguments -- and it is delivered over
//! stdin rather than as a quoted argument.
//!
//! That is not a stylistic preference. An apostrophe inside a command, passed
//! inline through a quoted remote argument, closes the quote and hands the rest
//! of the line to the *local* shell. It has happened on this project once
//! already, and `rm -f` ran on the workstation rather than in the guest. Every
//! value below is single-quote escaped, and the rendered script goes over a
//! pipe.
//!
//! What is deliberately **not** here: where anything lives. The working tree,
//! the results directory and the caches have different paths in the two
//! execution modes, and the runner is the component that knows them -- it is
//! the platform module, and it is the one choosing the mode. Computing them
//! here as well would leave two places obliged to agree forever.
//!
//! Nothing here knows what a command means. `cmd` is opaque, and the framework
//! never learns a tenant's vocabulary.

use std::collections::BTreeMap;

/// Render a job script.
///
/// The runner has already exported `REAPER_WORK`, `REAPER_OUT` and one
/// `REAPER_CACHE_*` per cache, and has already made the working tree the
/// current directory. This adds the tenant's own environment and then the
/// tenant's command.
pub fn render(cmd: &str, env: &BTreeMap<String, String>) -> String {
    let mut s = String::new();
    s.push_str("#!/bin/sh\n");
    s.push_str("# Rendered by reaper and delivered over stdin. Every value below is\n");
    s.push_str("# quoted by the CLI; the command itself deliberately is not.\n");
    // Traced, so a session log says which command ran. Not -e: the command is
    // the job, and its exit status is the job's already. Not -u either, since
    // a test entry point referring to an optional variable is an ordinary
    // thing to write and not an error.
    s.push_str("set -x\n");

    for (k, v) in env {
        s.push_str(&format!("{k}={}\nexport {k}\n", quote(v)));
    }

    // Unquoted and last: this is a shell command, not a program name. Quoting
    // it would break every pipe, redirect and variable reference, all of which
    // a manifest may legally contain and one of the shipped examples does.
    s.push_str(cmd);
    s.push('\n');
    s
}

/// A string as a single shell word, however hostile.
///
/// Single quotes protect everything a shell would otherwise interpret --
/// `$`, backticks, backslashes, newlines, semicolons -- and the only character
/// they cannot contain is a single quote, which is closed, escaped and reopened
/// in the usual way. The empty string still needs quotes, or it disappears.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// The tenant's environment for one verb, with a profile's overlaid on top.
///
/// A profile changes how a session is run, never what is run, so it is the
/// profile that wins where the two name the same variable -- that is the whole
/// use of the nightly profile in the shipped examples.
pub fn overlay(
    verb: &BTreeMap<String, String>,
    profile: Option<&BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
    let mut env = verb.clone();
    if let Some(p) = profile {
        for (k, v) in p {
            env.insert(k.clone(), v.clone());
        }
    }
    env
}
