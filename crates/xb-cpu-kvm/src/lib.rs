//! Guest CPU backend on Linux KVM hardware virtualization.
//!
//! [`KvmCpu`] implements [`xb_cpu::Cpu`] by running the title natively in a
//! one-vCPU virtual machine. `docs/hw/cpu-kvm.md` describes the guest
//! environment (page tables, exception stubs, trap page, write protection) and
//! why each piece is built the way it is.
//!
//! The backend exists on Linux x86-64 only. On every other target the crate
//! still compiles: [`KvmCpu::probe`] reports that KVM is unsupported and the
//! machine falls back to another backend.
//!
//! `testing` (cargo feature `testing`) holds guest-code cases written
//! against the `xb_cpu::Cpu` trait alone, so the same snippets check every
//! backend and the backends against each other.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use linux::{HIDDEN_GPA, HIDDEN_LEN, KICK_SIGNAL_OFFSET, KvmCpu, SYSTEM_VA, SYSTEM_VA_LEN};

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
mod unsupported {
    //! The stand-in for targets without KVM.

    // The interface crates are used by the real backend and by `testing`.
    use xb_cpu as _;
    use xb_types as _;

    /// The KVM backend; it cannot be created on this target.
    pub struct KvmCpu(());

    impl KvmCpu {
        /// Reports that KVM is unsupported on this target.
        ///
        /// # Errors
        ///
        /// Always: the text names the reason for the log.
        pub fn probe() -> Result<(), String> {
            Err("KVM backend is built for Linux x86-64 only".to_owned())
        }
    }
}
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub use unsupported::KvmCpu;

#[cfg(any(test, feature = "testing"))]
pub mod testing;
