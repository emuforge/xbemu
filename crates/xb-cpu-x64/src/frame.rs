//! The frame: the block of memory translated code, the stubs, the fault
//! handler and `X64Cpu::run` share.
//!
//! Translated code keeps a pointer to it in `R15` for as long as it runs. The
//! guest's registers are in host registers then and are stored here only on
//! the way out (the exit stub); everything else here is control state.

use std::cell::UnsafeCell;
use std::mem::offset_of;
use std::sync::atomic::AtomicU32;

/// Why translated code returned to `run` ([`Frame::reason`]).
pub(crate) mod reason {
    /// The dispatcher has no translation for `eip`.
    pub(crate) const LOOKUP: u32 = 0;
    /// The kick flag was set at a block start, or interrupts became enabled
    /// while `exit_if` was set. `eip` is the next instruction.
    pub(crate) const KICK: u32 = 1;
    /// A host fault in translated code (`fault_*` describe it).
    pub(crate) const FAULT: u32 = 2;
    /// `HLT`; `eip` is past it.
    pub(crate) const HLT: u32 = 3;
    /// An instruction `run` executes itself; `eip` is its address.
    pub(crate) const SYSOP: u32 = 4;
    /// The single-step thunk finished; `eip` is the next instruction.
    pub(crate) const STEP: u32 = 5;
    /// An instruction with no translation; `eip` is its address.
    pub(crate) const UNTRANSLATED: u32 = 6;
    /// Bytes that are no instruction, or `UD2`; `eip` is their address.
    pub(crate) const INVALID: u32 = 7;
}

/// State shared with translated code. `repr(C)`: the stubs and translations
/// address the fields by offset.
#[repr(C, align(64))]
pub(crate) struct Frame {
    /// Guest EAX..EDI in x86 encoding order (index 4 is ESP), zero-extended.
    pub(crate) gregs: [u64; 8],
    /// The host's RFLAGS image holding the guest's arithmetic flags, DF and
    /// ID. The guest's IF and AC are in `vif` and `vac`.
    pub(crate) eflags: u64,
    /// Guest instruction pointer, valid outside translated code.
    pub(crate) eip: u32,
    /// One of [`reason`].
    pub(crate) reason: u32,
    /// The guest's interrupt flag (0 or 1).
    pub(crate) vif: u32,
    /// The guest's alignment-check flag (0 or 1). Kept out of the host's
    /// flags, where it would make unaligned accesses fault in user mode.
    pub(crate) vac: u32,
    /// Nonzero: leave with [`reason::KICK`] as soon as `vif` is 1
    /// (`RunFlags::exit_when_interruptible`).
    pub(crate) exit_if: u32,
    /// Set by a `Kicker` from any thread; polled at block starts.
    pub(crate) kick: AtomicU32,
    /// The host's MXCSR, restored on the way out.
    pub(crate) host_mxcsr: u32,
    _pad: u32,
    /// The host's stack pointer inside the enter stub.
    pub(crate) host_rsp: u64,
    /// The stack pointer of an empty shadow stack.
    pub(crate) shadow_top: u64,
    /// Host address the enter stub jumps to.
    pub(crate) next_code: u64,
    /// The running context's FXSAVE image.
    pub(crate) fx_ptr: u64,
    /// Guest FS base, zero-extended (lives in `R14`).
    pub(crate) fs_base: u64,
    /// Host address of guest address 0 (lives in `R8` and in `GS`'s base).
    pub(crate) mem_base: u64,
    /// Host `rip` of the last fault in translated code.
    pub(crate) fault_rip: u64,
    /// Its faulting host address (`CR2`).
    pub(crate) fault_addr: u64,
    /// Its processor vector.
    pub(crate) fault_trapno: u32,
    /// Its error code.
    pub(crate) fault_err: u32,
    /// `R13` at the fault (a borrowed register's own value, see
    /// `translate::InsnMap::borrow`).
    pub(crate) fault_r13: u64,
}

impl Frame {
    pub(crate) const GREGS: i64 = offset_of!(Frame, gregs) as i64;
    pub(crate) const EFLAGS: i64 = offset_of!(Frame, eflags) as i64;
    pub(crate) const EIP: i64 = offset_of!(Frame, eip) as i64;
    pub(crate) const REASON: i64 = offset_of!(Frame, reason) as i64;
    pub(crate) const VIF: i64 = offset_of!(Frame, vif) as i64;
    pub(crate) const VAC: i64 = offset_of!(Frame, vac) as i64;
    pub(crate) const EXIT_IF: i64 = offset_of!(Frame, exit_if) as i64;
    pub(crate) const KICK: i64 = offset_of!(Frame, kick) as i64;
    pub(crate) const HOST_MXCSR: i64 = offset_of!(Frame, host_mxcsr) as i64;
    pub(crate) const HOST_RSP: i64 = offset_of!(Frame, host_rsp) as i64;
    pub(crate) const SHADOW_TOP: i64 = offset_of!(Frame, shadow_top) as i64;
    pub(crate) const NEXT_CODE: i64 = offset_of!(Frame, next_code) as i64;
    pub(crate) const FX_PTR: i64 = offset_of!(Frame, fx_ptr) as i64;
    pub(crate) const FS_BASE: i64 = offset_of!(Frame, fs_base) as i64;
    pub(crate) const MEM_BASE: i64 = offset_of!(Frame, mem_base) as i64;
}

/// The frame's allocation, shared between the translator and its kickers.
///
/// Everything except `kick` belongs to the guest-CPU thread (its Rust code,
/// the translated code it calls and the fault handler interrupting that
/// code); other threads touch only the atomic `kick`.
pub(crate) struct FrameCell(UnsafeCell<Frame>);

// SAFETY: see the type's documentation: cross-thread access is limited to an
// atomic field, reached through a raw pointer.
unsafe impl Sync for FrameCell {}
// SAFETY: plain data.
unsafe impl Send for FrameCell {}

impl FrameCell {
    pub(crate) fn new() -> Self {
        // SAFETY: every field of `Frame` is valid when zero.
        Self(UnsafeCell::new(unsafe { std::mem::zeroed() }))
    }

    /// The frame, for the guest-CPU thread.
    pub(crate) fn get(&self) -> *mut Frame {
        self.0.get()
    }

    /// The kick flag, for any thread.
    pub(crate) fn kick(&self) -> &AtomicU32 {
        // SAFETY: the field is an atomic inside a live allocation; a shared
        // reference to it alone does not alias the fields the CPU thread
        // writes.
        unsafe { &(*self.0.get()).kick }
    }
}
