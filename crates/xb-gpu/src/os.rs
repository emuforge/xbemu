//! Host operating system services the timing code needs: the monotonic clock
//! the renderer shares (`CLOCK_MONOTONIC`, nanoseconds) and sleeping until an
//! instant on it.
//!
//! The vendored renderer stamps frames with `clock_gettime(CLOCK_MONOTONIC)`
//! and compares those stamps with the vblank instants this crate hands it, so
//! the Rust side must read the very same clock. `std::time::Instant` is that
//! clock on Linux but does not expose its value, hence this module.

#[cfg(unix)]
mod imp {
    /// `CLOCK_MONOTONIC` in nanoseconds.
    pub fn monotonic_ns() -> u64 {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `ts` is a valid timespec to fill; CLOCK_MONOTONIC always exists.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }

    /// Sleeps until `deadline_ns` on `CLOCK_MONOTONIC`. Returns early only if
    /// the deadline has passed.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn sleep_until_ns(deadline_ns: u64) {
        let ts = libc::timespec {
            tv_sec: (deadline_ns / 1_000_000_000) as libc::time_t,
            tv_nsec: (deadline_ns % 1_000_000_000) as libc::c_long,
        };
        // SAFETY: `ts` is a valid timespec; a null remainder is allowed with
        // TIMER_ABSTIME. EINTR restarts the same absolute sleep.
        while unsafe {
            libc::clock_nanosleep(
                libc::CLOCK_MONOTONIC,
                libc::TIMER_ABSTIME,
                &ts,
                std::ptr::null_mut(),
            )
        } == libc::EINTR
        {}
    }

    /// Sleeps until `deadline_ns` on `CLOCK_MONOTONIC` (relative sleeps, for
    /// systems without `clock_nanosleep`).
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub fn sleep_until_ns(deadline_ns: u64) {
        loop {
            let now = monotonic_ns();
            if now >= deadline_ns {
                return;
            }
            std::thread::sleep(std::time::Duration::from_nanos(deadline_ns - now));
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::sync::OnceLock;
    use std::time::Instant;

    /// Nanoseconds since the first call. The renderer's own clock on such a
    /// host must be made to agree with this before the port is usable.
    pub fn monotonic_ns() -> u64 {
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_nanos() as u64
    }

    /// Sleeps until `deadline_ns` on [`monotonic_ns`]'s clock.
    pub fn sleep_until_ns(deadline_ns: u64) {
        let now = monotonic_ns();
        if deadline_ns > now {
            std::thread::sleep(std::time::Duration::from_nanos(deadline_ns - now));
        }
    }
}

pub use imp::{monotonic_ns, sleep_until_ns};
