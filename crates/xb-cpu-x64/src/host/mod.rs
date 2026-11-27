//! Everything the translator needs from the host operating system.
//!
//! One submodule per OS, each exporting the same items:
//!
//! * `GuestSpace`: the reservation holding the guest's 4 GiB, views of the RAM
//!   object mapped into it, and page protection.
//! * `ExecRegion` and `ShadowStack`: memory for translated code and for the
//!   stack translated code runs on.
//! * `set_guest_segment_base` and `HAS_FAST_SEGMENT_BASE`: how the segment
//!   register translated code addresses guest memory through gets its base.
//! * `install_fault_handler`, `register_cpu`, `unregister_cpu`: the
//!   process-wide fault handler and the table of translators it serves.
//!
//! Only Linux exists so far. A Windows module would reserve with
//! `VirtualAlloc2` placeholders, map with `MapViewOfFile3`, take faults in a
//! vectored exception handler, and address guest memory through `FS`
//! (64-bit Windows keeps the TEB in `GS`).

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::*;
