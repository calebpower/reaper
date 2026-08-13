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
