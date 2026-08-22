//! Writing to a terminal that may not be listening any more.
//!
//! Rust sets SIGPIPE to `SIG_IGN`, so a `println!` into a closed pipe returns
//! EPIPE and the print macros panic on it. `reaper list | head -1` is an
//! ordinary thing to do, and what it produced was a panic trace from the tool
//! that owns the test environment -- which is exactly the thing that gets read
//! as a real failure at three in the morning.
//!
//! Restoring the default SIGPIPE disposition in `main` is the shorter fix, and
//! it is the wrong one here. reaper writes the runner into an ssh process's
//! stdin, and an ssh that cannot connect exits before reading it. Today that
//! moment surfaces as
//!
//!     writing /tmp/reaper-runner.sh failed (255): ssh: connect to host ... timed out
//!
//! which is a sentence an operator can act on -- and it is one tenants have
//! quoted back in bug reports, so it is load-bearing. Under the default
//! disposition the identical moment kills reaper with signal 13 and says
//! nothing at all. So the panic is removed where it belongs, on reaper's own
//! output, and the EPIPE that reaches the transport is left exactly as it is.
//!
//! A closed pipe ends the process, which is what every other Unix tool does
//! and what the caller asked for by closing it. Mid-verb that abandons the
//! work: an `up` cut off here leaves a machine carrying its readiness grace,
//! which the sweeper collects. That is the same outcome the panic produced,
//! minus the trace that looked like a defect.

use std::io::Write;

pub enum Target {
    Out,
    Err,
}

pub fn line(target: Target, args: std::fmt::Arguments) {
    let wrote = match target {
        Target::Out => {
            let mut h = std::io::stdout().lock();
            h.write_fmt(args).and_then(|()| h.write_all(b"\n"))
        }
        Target::Err => {
            let mut h = std::io::stderr().lock();
            h.write_fmt(args).and_then(|()| h.write_all(b"\n"))
        }
    };

    if let Err(e) = wrote {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            // The reader has gone. Saying so would need somewhere to say it.
            std::process::exit(0);
        }
        // Anything else -- a full disk, a revoked terminal -- is unreportable
        // by definition, since the place a report would go is the thing that
        // just failed. Dropping the line beats panicking about it.
    }
}

/// `println!`, minus the panic when nobody is reading.
macro_rules! say {
    ($($arg:tt)*) => {
        $crate::out::line($crate::out::Target::Out, format_args!($($arg)*))
    };
}

/// `eprintln!`, minus the panic when nobody is reading.
macro_rules! warn_line {
    ($($arg:tt)*) => {
        $crate::out::line($crate::out::Target::Err, format_args!($($arg)*))
    };
}
