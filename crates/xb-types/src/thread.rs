//! The threads that pace the guest, and how the host treats them.
//!
//! Some hosts schedule a thread by what it says about itself: macOS puts a
//! thread of the default class of service on whatever core is free, the
//! slow efficiency cores included, and a new thread does not inherit its
//! creator's class. A thread whose lateness the player sees (the guest CPU,
//! the guest's clocks, the vertical blank, the audio drain, the presenting
//! thread) therefore says so when it starts, with [`pacing`]. What that does
//! is the host's: `xb-core` installs it with [`set_pacing_hook`]; with no
//! hook (Linux) it does nothing.
//!
//! The hook lives here because crates that spawn such threads may not call
//! the host's thread API themselves (they forbid `unsafe`).

use std::sync::OnceLock;

static PACING: OnceLock<fn()> = OnceLock::new();

/// Installs what [`pacing`] does on this host. Only the first call counts.
pub fn set_pacing_hook(hook: fn()) {
    let _ = PACING.set(hook);
}

/// Called at the start of a thread that paces the guest: tells the host
/// scheduler the thread is latency-critical.
pub fn pacing() {
    if let Some(hook) = PACING.get() {
        hook();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static CALLS: AtomicU32 = AtomicU32::new(0);

    #[test]
    fn a_pacing_thread_runs_the_installed_hook() {
        set_pacing_hook(|| {
            CALLS.fetch_add(1, Ordering::Relaxed);
        });
        let before = CALLS.load(Ordering::Relaxed);
        std::thread::spawn(pacing).join().unwrap();
        assert_eq!(CALLS.load(Ordering::Relaxed), before + 1);
    }
}
