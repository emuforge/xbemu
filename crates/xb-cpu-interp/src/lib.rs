//! Guest CPU backend that interprets x86-32 guest code.
//!
//! [`InterpCpu`] implements `xb_cpu::Cpu` in portable Rust: it needs no
//! hardware virtualization and makes no assumption about the host's
//! architecture or page size, so it runs where the other backends cannot
//! (arm64 hosts, 16 KiB pages).
//!
//! In one paragraph: guest code is decoded once into blocks of
//! instructions (`cache`), each with a handler chosen for its form (`fast`),
//! which are kept until the guest or the host writes their bytes. Each
//! instruction is carried out on the guest's registers held in Rust, with the arithmetic flags computed when read
//! (`flags`). Guest memory is a page table in software (`mem`): every
//! access is checked against the guest's mapping and the write-tracking
//! restriction of its physical page, then made on RAM or forwarded to the
//! bus. An x87 register holds a double, and its exact 80 bits when a value
//! only passes through (`f80`).
//!
//! Blocks that have run a few times are compiled to host code where a code
//! generator exists (`jit`: AArch64 on macOS). Generated code does with host
//! instructions what the interpreter's handlers do for the common forms and
//! calls those handlers for the rest.
//!
//! `unsafe` is confined to the code generator (`jit::native`, running
//! generated code, and `jit::macos`, executable memory). The interpreter has
//! none: guest RAM is reached through [`xb_mem::Ram`]'s bounds-checked
//! copies.

#![deny(unsafe_code)]

mod cache;
mod cpu;
mod exec;
mod f80;
mod fast;
mod flags;
mod jit;
mod mem;
mod simd;
mod x87;

pub use cpu::InterpCpu;

#[cfg(test)]
mod tests;
