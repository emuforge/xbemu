//! Host operating system services: the monotonic clock the renderer shares
//! (`CLOCK_MONOTONIC`, nanoseconds), sleeping until an instant on it, and
//! finding the Vulkan loader where volk's own search does not.
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

    /// Sleeps until `deadline_ns` on `CLOCK_MONOTONIC`: macOS has no
    /// `clock_nanosleep`; `mach_wait_until` is its absolute, uncoalesced
    /// wait. Returns early only if the deadline has passed.
    #[cfg(target_os = "macos")]
    pub fn sleep_until_ns(deadline_ns: u64) {
        /// `mach_timebase_info_data_t`.
        #[repr(C)]
        struct Timebase {
            numer: u32,
            denom: u32,
        }
        // libSystem's Mach time calls.
        unsafe extern "C" {
            /// Blocks until `deadline` on the `mach_absolute_time` clock.
            fn mach_wait_until(deadline: u64) -> libc::c_int;
            /// The clock `mach_wait_until` waits on, in ticks.
            fn mach_absolute_time() -> u64;
            /// The tick's length: `numer / denom` nanoseconds.
            fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
        }
        static TIMEBASE: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
        let &(numer, denom) = TIMEBASE.get_or_init(|| {
            let mut tb = Timebase { numer: 0, denom: 0 };
            // SAFETY: `tb` is a valid structure to fill.
            unsafe { mach_timebase_info(&raw mut tb) };
            (u64::from(tb.numer.max(1)), u64::from(tb.denom.max(1)))
        });
        loop {
            let now = monotonic_ns();
            if now >= deadline_ns {
                return;
            }
            // Ticks are `numer / denom` nanoseconds.
            let ticks = u128::from(deadline_ns - now) * u128::from(denom) / u128::from(numer);
            // SAFETY: plain system calls; any deadline is valid.
            unsafe { mach_wait_until(mach_absolute_time().saturating_add(ticks as u64)) };
        }
    }

    /// Sleeps until `deadline_ns` on `CLOCK_MONOTONIC` (relative sleeps, for
    /// systems without an absolute wait).
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
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

/// Where the Vulkan library is looked for on macOS, in order.
///
/// First the MoltenVK an app bundle carries (`Contents/Frameworks`, beside
/// the binary's `Contents/MacOS`), which is a Vulkan implementation of its
/// own and needs no loader; then the system's loader, which finds MoltenVK
/// through its manifest search. volk opens `libvulkan.dylib` by name, which
/// the dynamic linker looks for only in its default places; Homebrew
/// installs the loader in `/opt/homebrew/lib` (arm64) or `/usr/local/lib`
/// (x86-64).
///
/// The front end's window surface must come from the same library as the
/// renderer's instance, so the standalone app looks in the same order.
#[cfg(target_os = "macos")]
#[must_use]
pub fn vulkan_libraries() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Some(contents) = std::env::current_exe().ok().and_then(|exe| {
        exe.parent().and_then(std::path::Path::parent).map(std::path::Path::to_path_buf)
    }) {
        paths.push(contents.join("Frameworks/libMoltenVK.dylib"));
    }
    paths.extend(
        [
            "libvulkan.1.dylib",
            "/opt/homebrew/lib/libvulkan.1.dylib",
            "/usr/local/lib/libvulkan.1.dylib",
        ]
        .map(std::path::PathBuf::from),
    );
    paths
}

/// The Vulkan library's `vkGetInstanceProcAddr` on macOS
/// ([`vulkan_libraries`]), or null. The library stays open for the life of
/// the process, as volk's would.
#[cfg(target_os = "macos")]
pub fn vk_loader() -> *mut std::ffi::c_void {
    use std::os::unix::ffi::OsStrExt;
    for path in vulkan_libraries() {
        // A path in a bundle that is not there is skipped without asking
        // the dynamic linker, which would search for a bare name.
        if path.is_absolute() && !path.exists() {
            continue;
        }
        let Ok(name) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            continue;
        };
        // SAFETY: `name` is a NUL-terminated path; dlopen has no other
        // requirement.
        let lib = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if lib.is_null() {
            continue;
        }
        // SAFETY: `lib` is a handle dlopen returned and the symbol name is
        // NUL-terminated.
        let gipa = unsafe { libc::dlsym(lib, c"vkGetInstanceProcAddr".as_ptr()) };
        if !gipa.is_null() {
            log::debug!(target: "gpu", "vulkan loader: {}", name.to_string_lossy());
            return gipa;
        }
        // SAFETY: `lib` came from dlopen and nothing from it is in use.
        unsafe { libc::dlclose(lib) };
    }
    std::ptr::null_mut()
}

/// Null: volk's own search finds the system's Vulkan loader
/// (`libvulkan.so.1`).
#[cfg(not(target_os = "macos"))]
pub fn vk_loader() -> *mut std::ffi::c_void {
    std::ptr::null_mut()
}
