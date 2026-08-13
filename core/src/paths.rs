//! Where things live on the workstation.

use std::path::{Path, PathBuf};

/// Expand a leading `~` against `$HOME`.
///
/// Only a leading one, and only when it is the whole first component: `~/x` is
/// a home-relative path, `~other/x` is another user's home and is not something
/// this project resolves, and `a/~/b` is a directory that someone unwisely
/// named `~`.
pub fn expand_tilde(p: &str) -> PathBuf {
    let Some(rest) = p.strip_prefix('~') else {
        return PathBuf::from(p);
    };
    let Ok(home) = std::env::var("HOME") else {
        return PathBuf::from(p);
    };
    match rest {
        "" => PathBuf::from(home),
        r if r.starts_with('/') => PathBuf::from(home).join(r.trim_start_matches('/')),
        _ => PathBuf::from(p),
    }
}

fn home_relative(sub: &str) -> Option<PathBuf> {
    std::env::var("HOME").ok().map(|h| Path::new(&h).join(sub))
}

/// Candidate configuration files, most specific first.
///
/// `REAPER_CONFIG` wins outright when set, so a caller can be explicit without
/// having to reason about the search order at all.
pub fn config_candidates() -> Vec<PathBuf> {
    if let Ok(explicit) = std::env::var("REAPER_CONFIG") {
        return vec![expand_tilde(&explicit)];
    }

    let mut out = Vec::new();
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            out.push(Path::new(&xdg).join("reaper/config.toml"));
        }
    }
    if let Some(p) = home_relative(".config/reaper/config.toml") {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out.push(PathBuf::from("/etc/reaper/config.toml"));
    out
}

/// Where session state lives.
///
/// State, not configuration: it is machine-local, rewritten constantly, and
/// losing it costs you the ability to find your sessions rather than the
/// ability to make them.
pub fn state_file() -> PathBuf {
    if let Ok(explicit) = std::env::var("REAPER_STATE") {
        return expand_tilde(&explicit);
    }
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return Path::new(&xdg).join("reaper/sessions.json");
        }
    }
    home_relative(".local/state/reaper/sessions.json")
        .unwrap_or_else(|| PathBuf::from("reaper-sessions.json"))
}
