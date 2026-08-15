//! The small amount of process handling the heartbeat needs.

/// Is a process with this identifier alive?
///
/// Signal 0 performs the permission and existence checks without delivering
/// anything, which is the standard way to ask. It cannot distinguish "gone"
/// from "alive but not ours"; for a process this one started, that difference
/// does not arise.
pub fn is_alive(pid: u32) -> bool {
    // Safety: kill with signal 0 delivers nothing and only inspects.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Ask a process to stop, and confirm it did.
///
/// Polite first: the heartbeat has nothing to clean up, but a process that is
/// given no chance to exit is a process whose logs end mid-sentence. Escalates
/// only if the request is ignored.
pub fn stop(pid: u32) -> bool {
    if !is_alive(pid) {
        return true;
    }
    // Safety: sending a signal to a process identifier this process recorded.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };

    for _ in 0..20 {
        if !is_alive(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    std::thread::sleep(std::time::Duration::from_millis(50));
    !is_alive(pid)
}

/// Is this pid still the heartbeat that was recorded, or has the OS reused it?
///
/// `Some(args.contains("heartbeat"))` when the process exists and its command line says heartbeat,
/// `Some(false)` when it exists and says something else, `None` when it is
/// gone. Asked through `ps`, which is the one spelling that works on both
/// systems reaper's CLI runs on; if `ps` itself cannot answer, the honest
/// reply is "not verifiably ours", never "go ahead".
pub fn looks_like_heartbeat(pid: u32) -> Option<bool> {
    if !is_alive(pid) {
        return None;
    }
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "args="])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let args = String::from_utf8_lossy(&o.stdout);
            Some(args.contains("heartbeat"))
        }
        // ps failing for a live pid usually means it exited between the two
        // checks; anything else still must not license a kill.
        _ => Some(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reused_pid_is_recognized_as_not_ours() {
        // This test process is alive and is not a heartbeat; killing it on
        // the strength of a recorded number would be the bug.
        assert_eq!(looks_like_heartbeat(std::process::id()), Some(false));
    }

    #[test]
    fn a_dead_pid_is_gone_not_foreign() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        child.wait().expect("reap");
        // Reaped, so the identifier is free; the answer is "gone", which the
        // caller treats as nothing to do.
        assert_eq!(looks_like_heartbeat(pid), None);
    }
}
