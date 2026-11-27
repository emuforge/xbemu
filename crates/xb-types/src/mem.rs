//! Memory traits that connect the address-space bookkeeping (`xb-mem`), the
//! CPU backends and the devices.
//!
//! Two views of guest memory exist:
//!
//! * the **guest's view**, by virtual address, enforced by the CPU backend
//!   (real page tables under KVM, host mappings under the translator). The
//!   kernel changes it through [`Mapper`].
//! * the **host's view**, by physical address, which never faults and is how
//!   the kernel, devices and the GPU read and write guest RAM ([`PhysMem`]).
//!
//! Write tracking (the GPU needs to know which RAM pages the guest wrote)
//! restricts only the guest's view, through [`PageProtector`].

/// What the guest may do with a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Any access faults.
    None,
    /// Reads and execution; a write faults.
    Read,
    /// Everything.
    ReadWrite,
}

/// The guest's virtual address space as the CPU backend enforces it.
///
/// Called on the guest-CPU thread only, while the CPU is not running guest
/// code. All arguments are page aligned.
pub trait Mapper {
    /// Maps guest virtual `[va, va + len)` onto guest physical RAM
    /// `[pa, pa + len)`, replacing what was mapped there.
    fn map(&mut self, va: u32, pa: u32, len: u32, access: Access);
    /// Maps `[va, va + len)` as device registers at physical `[pa, pa + len)`:
    /// every guest access is forwarded to the bus's MMIO handlers.
    fn map_mmio(&mut self, va: u32, pa: u32, len: u32);
    /// Removes the mappings of `[va, va + len)`; an access raises a page fault.
    fn unmap(&mut self, va: u32, len: u32);
}

/// Restricts the guest's access to physical RAM pages, whatever virtual
/// addresses map them. This is the mechanism behind write tracking.
///
/// Callable from any thread, and synchronous: when `protect` returns, no guest
/// access the new setting forbids can still succeed. The host's own view
/// ([`PhysMem`]) is never affected.
///
/// The write tracker calls `protect` while holding its own lock, and the
/// fault path (`Bus::page_fault`) takes that lock. An implementation must
/// therefore never wait for the guest-CPU thread to return from a fault
/// handler.
pub trait PageProtector: Send + Sync {
    /// Sets the guest's access to physical `[pa, pa + len)` (page aligned).
    /// `Access::ReadWrite` restores what the page tables alone allow.
    fn protect(&self, pa: u32, len: u32, access: Access);
}

/// Guest physical RAM as the host sees it: never faulting, bounds checked.
///
/// Implementations are cheap handles (`Clone`) usable from any thread. Access
/// races with the running guest exactly as a device's DMA does on hardware;
/// callers needing a consistent snapshot must stop the guest first.
pub trait PhysMem: Send + Sync {
    /// Size of RAM in bytes.
    fn size(&self) -> u32;
    /// Copies `buf.len()` bytes from physical address `pa`. Returns `false`,
    /// copying nothing, if the range leaves RAM.
    fn read(&self, pa: u32, buf: &mut [u8]) -> bool;
    /// Copies `data` to physical address `pa`. Returns `false`, writing
    /// nothing, if the range leaves RAM. Does not mark pages dirty for write
    /// tracking; the caller reports device writes to the tracker itself.
    fn write(&self, pa: u32, data: &[u8]) -> bool;

    /// Reads a little-endian `u32`, or 0 outside RAM.
    fn read_u32(&self, pa: u32) -> u32 {
        let mut b = [0u8; 4];
        self.read(pa, &mut b);
        u32::from_le_bytes(b)
    }
    /// Writes a little-endian `u32`; ignored outside RAM.
    fn write_u32(&self, pa: u32, v: u32) {
        self.write(pa, &v.to_le_bytes());
    }
}
