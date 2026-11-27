//! The host operating system's part: one shared memory object and its
//! mapping. Each host has a module exporting `SharedMem` with the same
//! methods (`new`, `as_ptr`, `len`, and the handle other mappings are made
//! from); Windows gets `windows.rs` over `CreateFileMappingW`.

#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) use linux::SharedMem;

#[cfg(not(any(target_os = "linux", target_os = "android")))]
compile_error!("xb-mem has no shared-memory implementation for this host yet (see src/host)");
