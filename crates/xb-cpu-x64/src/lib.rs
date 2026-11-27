//! Guest CPU backend translating x86-32 guest code to x86-64 host code.
//!
//! [`X64Cpu`] implements `xb_cpu::Cpu` without help from the host's
//! virtualization: the title's code is translated block by block into host
//! code that runs in this process. It is the fallback when `xb-cpu-kvm` is
//! unavailable and is built on every x86-64 Linux host. On other targets the
//! crate holds only `testing`, the guest-code snippets every backend runs.
//!
//! The scheme is described in `docs/hw/cpu-x64.md`; in one paragraph:
//!
//! A guest instruction keeps its encoding wherever possible. It is decoded
//! as 32-bit code and encoded again for 64-bit mode, with the guest's
//! registers in the host registers of the same name (ESP in R12), the
//! guest's flags in the host's flags, x87 and SSE state in the host's, and
//! guest memory address `x` reached as `gs:[x]`, where `GS`'s base is the
//! start of a 4 GiB reservation holding views of the RAM memory object.
//! Blocks end at control transfers and jump to each other directly; calls
//! and returns are host calls and returns checked against the guest's
//! return address. Whatever the host cannot run as it is leaves translated
//! code and is carried out by `run` in Rust.
//!
//! `unsafe` is allowed in this crate: it generates and runs machine code,
//! maps memory and handles signals.

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod asm;
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod cache;
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod cpu;
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod emulate;
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod frame;
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod host;
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod mem;
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod stubs;
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod translate;

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
pub use cpu::{Stats, X64Cpu};
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
pub use translate::DescriptorTables;

#[cfg(any(test, feature = "testing"))]
pub mod testing;
#[cfg(all(test, target_arch = "x86_64", target_os = "linux"))]
mod tests;
