//! What the host's scheduler is told about the threads that pace the guest
//! (`xb_types::thread`). On macOS such a thread asks for the
//! user-interactive class of service, the one the scheduler keeps on the
//! performance cores; a new thread starts at the default class whatever its
//! creator's. Other hosts install nothing: their schedulers need no word.

/// Installs this host's pacing hook. Called before the machine's threads
/// start.
pub(crate) fn install() {
    #[cfg(target_os = "macos")]
    xb_types::thread::set_pacing_hook(macos::user_interactive);
}

#[cfg(target_os = "macos")]
mod macos {
    /// `QOS_CLASS_USER_INTERACTIVE` (`<sys/qos.h>`).
    const QOS_CLASS_USER_INTERACTIVE: libc::c_uint = 0x21;

    // libSystem's pthread QoS call (`<pthread/qos.h>`).
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(class: libc::c_uint, priority: libc::c_int)
        -> libc::c_int;
    }

    /// Raises the calling thread to the user-interactive class.
    pub(super) fn user_interactive() {
        // SAFETY: affects only the calling thread; any class value is
        // accepted (an invalid one fails with EINVAL, which is harmless).
        let r = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
        if r != 0 {
            log::debug!(target: "core", "thread class of service not raised: error {r}");
        }
    }
}
