//! What the GPU needs from the machine and from the front end.
//!
//! The vendored renderer asks its host for everything through one C header,
//! `vendor/xbased-hle/hw/xbox/hle/host.h`. This crate implements that header
//! (`ffi.rs`); the parts only the machine can answer are forwarded to a
//! [`GpuHost`], which `xb-core` implements. The front end supplies a
//! [`SurfaceProvider`] so the presenter can draw into its window without
//! knowing which window system made it.

use std::ffi::{CStr, c_void};

/// The guest CPU's registers, for the renderer's diagnostics (its stall
/// monitor and PC log). Segments are flat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuRegs {
    /// General registers in x86 encoding order (EAX, ECX, EDX, EBX, ESP, EBP,
    /// ESI, EDI).
    pub gpr: [u32; 8],
    /// Address of the next instruction.
    pub eip: u32,
    /// EFLAGS; the interrupt flag (bit 9) at least.
    pub eflags: u32,
    /// Linear base of FS (the KPCR).
    pub fs_base: u32,
    /// The guest CPU idles in `HLT`.
    pub halted: bool,
}

/// Disc reads so far, for the renderer's stutter monitor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiscStats {
    /// Read operations.
    pub ops: u64,
    /// Bytes read.
    pub bytes: u64,
    /// Host time spent reading, nanoseconds.
    pub ns: u64,
}

/// The machine as the GPU sees it. `xb-core` implements this.
///
/// Every method may be called from any thread (the renderer's decode, render
/// and timeline threads, the vblank thread, the guest-CPU thread inside an
/// MMIO write or a write-tracking callback) and must not block on the GPU.
///
/// The memory methods are hot: the decoder calls the write-tracking family
/// for every texture and vertex range of every draw. Implement them without
/// allocation or locks, as the tracker in `xb-mem` does.
pub trait GpuHost: Send + Sync + 'static {
    // ---- guest memory by virtual address ----

    /// Copies guest memory at virtual address `va` into `buf` by walking the
    /// guest's page table into the RAM view; never faults. Returns `false` if
    /// a page is unmapped or lies in a device window (`va >= 0xFD00_0000`);
    /// pages before it have been copied. (`hle_va_rw`, read.)
    fn va_read(&self, va: u32, buf: &mut [u8]) -> bool;

    /// Copies `data` into guest memory at `va`, the same way. The write counts
    /// as a guest write: each page written is reported to write tracking as
    /// [`GpuHost::mem_set_dirty`] does. (`hle_va_rw`, write.)
    fn va_write(&self, va: u32, data: &[u8]) -> bool;

    /// Host code is about to store into `[va, va + len)` through the RAM
    /// view: for each mapped RAM page, give back a page without access (a
    /// stale access with `write = true`) and report a first write if the page
    /// is clean. (`hle_va_store`.)
    fn va_store(&self, va: u32, len: u32);

    // ---- write tracking of guest RAM, by physical address ----
    //
    // The semantics are the reference's; the tracker in `xb-mem` implements
    // them. Each tracked page is clean (write-protected in the guest's view)
    // or written. Addresses past the end of RAM are ignored (reads of state
    // answer "written").

    /// Whether the page of `pa` was written since the last
    /// [`GpuHost::mem_test_and_clear`] over it. An untracked page counts as
    /// written. (`hle_host_mem_dirty`.)
    fn mem_dirty(&self, pa: u32) -> bool;

    /// Whether the page of `pa` is open: not tracked and not without access,
    /// so the guest's stores to it go unseen. (`hle_host_mem_open`.)
    fn mem_open(&self, pa: u32) -> bool;

    /// Whether any page of `[pa, pa + len)` was written since the last call,
    /// and from now on tracks them again: the guest's next store to each page
    /// reports a first write ([`crate::MemEvents::first_write`]).
    /// (`hle_host_mem_test_and_clear`.)
    fn mem_test_and_clear(&self, pa: u32, len: u32) -> bool;

    /// `[pa, pa + len)` was written by a device or the host: the pages become
    /// written and untracked. Before changing protection the tracker reports
    /// [`crate::MemEvents::host_wrote`] for the range and, if a page of it has no
    /// guest access, [`crate::MemEvents::unguard_range`]. (`hle_host_mem_set_dirty`.)
    fn mem_set_dirty(&self, pa: u32, len: u32);

    /// A render target's image landed in `[pa, pa + len)`: the pages count as
    /// written for the next test-and-clear, their protection left as it is.
    /// The tracker reports [`crate::MemEvents::host_wrote`] first.
    /// (`hle_host_mem_landed`.)
    fn mem_landed(&self, pa: u32, len: u32);

    /// Protects `[pa, pa + len)` again so the guest's next store to each page
    /// is seen, keeping that the pages were written for a later
    /// test-and-clear. (`hle_host_mem_rearm`.)
    fn mem_rearm(&self, pa: u32, len: u32);

    /// Takes all guest access (reads too) from the pages of `[pa, pa + len)`
    /// (`on`), so the guest's next access reports
    /// [`crate::MemEvents::stale_access`]; or gives back what write tracking wants
    /// (`!on`). The host's own view keeps its access.
    /// (`hle_host_mem_noaccess`.)
    fn mem_noaccess(&self, pa: u32, len: u32, on: bool);

    // ---- the guest CPU ----

    /// Makes the guest CPU leave guest code at the next instruction boundary
    /// (`xb_cpu::Kicker::kick`). (`hle_host_kick`.)
    fn kick(&self);

    /// The guest CPU's own thread starts (`on`) or ends waiting for the GPU
    /// inside a write-tracking callback or an MMIO write. Calls nest. The
    /// guest's clocks go on for the first 8 ms of a wait and stand still
    /// beyond (Def Jam: Fight for NY's 60 Hz timer callback stops for good if
    /// the wait shows in guest time). Calls from other threads must be
    /// ignored. (`hle_host_guest_hold`.)
    fn guest_hold(&self, on: bool);

    /// Frame latency: the guest thread that just pushed a finished frame
    /// should wait, from its next instruction, until the next flip
    /// ([`GpuHost::frame_flipped`]). Called on the guest-CPU thread inside the
    /// DMA_PUT write. (`hle_host_frame_wait`.)
    fn frame_wait(&self);

    /// A flip came; releases a thread held by [`GpuHost::frame_wait`].
    /// (`hle_host_frame_flipped`.)
    fn frame_flipped(&self);

    /// The guest CPU's registers as last known, or `None` before boot.
    /// Diagnostics only. (`hle_host_cpu`.)
    fn cpu_regs(&self) -> Option<CpuRegs>;

    /// Guest code the CPU backend has translated or cached may be stale (the
    /// renderer changed its probes). A backend that runs guest code natively
    /// ignores it. (`hle_host_code_changed`.)
    fn code_changed(&self) {}

    /// How many times [`GpuHost::code_changed`] took effect. Diagnostics.
    /// (`hle_host_code_flushes`.)
    fn code_flushes(&self) -> u32 {
        0
    }

    // ---- guest time ----

    /// The guest's clock in nanoseconds: host time less the time the guest
    /// was paused or held. PTIMER's time registers read it.
    fn guest_time_ns(&self) -> u64;

    /// The host instant (`CLOCK_MONOTONIC`, ns) at which the guest's clock
    /// will read `guest_ns` if it keeps running. The PTIMER alarm sleeps
    /// until then.
    fn guest_to_host_ns(&self, guest_ns: u64) -> u64;

    // ---- diagnostics ----

    /// Disc read counters, or `None` if the machine does not count them.
    /// (`hle_host_disc_stats`.)
    fn disc_stats(&self) -> Option<DiscStats> {
        None
    }
}

/// `VkInstance`, as a raw handle. A front end using `ash` converts with
/// `ash::vk::Instance::from_raw(handle as u64)`.
pub type VkInstance = *mut c_void;
/// `VkSurfaceKHR`, as a raw 64-bit handle (`ash::vk::SurfaceKHR::as_raw`).
pub type VkSurfaceKHR = u64;

/// How the presenter gets a surface for the front end's window.
///
/// The renderer creates the `VkInstance` itself (it needs to choose the API
/// version and its own extensions), so the front end cannot hand it a
/// finished surface; it hands over this instead.
pub trait SurfaceProvider: Send + Sync + 'static {
    /// The instance extensions the surface needs (`VK_KHR_surface` and the
    /// window system's, for example `VK_KHR_xcb_surface`), at most six. They
    /// are enabled when the instance is created, at title start; if the
    /// driver lacks one, nothing can be presented.
    fn instance_extensions(&self) -> Vec<&'static CStr>;

    /// Creates the window's surface on `instance`. Called on the thread that
    /// presents, the first time it presents and again after the surface was
    /// lost or [`crate::Presenter::reset_surface`] was called. The presenter
    /// owns the surface afterwards and destroys it. `None` if the window is
    /// gone; the presenter tries again later.
    ///
    /// # Safety
    ///
    /// `instance` is a live `VkInstance` created by the renderer with the
    /// extensions from [`SurfaceProvider::instance_extensions`]. The
    /// implementation must return a surface created on that instance.
    unsafe fn create_surface(&self, instance: VkInstance) -> Option<VkSurfaceKHR>;
}

/// What a guest access to a page without access turned out to be
/// ([`crate::MemEvents::stale_neighbour`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaleNeighbour {
    /// The bytes belong to an image newer than memory: report a stale access.
    No,
    /// The bytes are the guest's own on a page a render target only shares:
    /// the host should perform the access for the guest through the RAM view
    /// and keep the page closed.
    Emulate,
    /// Such accesses come often on this page; it has been given back and the
    /// access can simply run again.
    Reopened,
}
