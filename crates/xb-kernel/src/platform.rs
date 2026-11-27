//! What the kernel needs from the machine.
//!
//! The kernel does not depend on the device crates; the machine (`xb-core`)
//! implements [`Platform`] over them. Every method is called on the guest-CPU
//! thread, from inside [`crate::Kernel::run`], while no guest code runs.
//!
//! The calls are the ones the reference's kernel makes into its devices.

/// A video mode the title set with `AvSetDisplayMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayMode {
    /// The AV mode word (never 0; bit 30 marks the 50 Hz modes).
    pub mode: u32,
    /// The frame buffer's pixel format (`D3DFORMAT` as the encoder takes it).
    pub format: u32,
    /// Bytes per line of the frame buffer.
    pub pitch: u32,
    /// Physical address of the frame buffer.
    pub frame_buffer: u32,
    /// 50 or 60.
    pub refresh_hz: u32,
}

/// The machine as the kernel sees it.
pub trait Platform {
    /// Called when guest time reaches the deadline the last call returned
    /// (and once when the kernel first runs): the machine advances devices
    /// that work by deadline (the USB controller about every millisecond, the
    /// AC'97 every 5 ms) and returns the guest time, in nanoseconds, at which
    /// it wants the next call, or `None` for no call until the kernel is
    /// run again. The kernel wakes the guest CPU, or its own idle wait, for
    /// that time.
    fn tick(&mut self, _now_ns: u64) -> Option<u64> {
        None
    }

    /// Whether the deadline [`Platform::tick`] last returned must stop
    /// running guest code (an interrupt the guest has unmasked comes with
    /// it). When not, the kernel calls `tick` at its next entry past the
    /// deadline or at the clock's tick, whichever is first.
    fn tick_needs_kick(&mut self) -> bool {
        true
    }

    /// The guest CPU idles (every thread waits): when the devices next need
    /// [`Platform::tick`], given that no guest code runs before then. `due`
    /// is what `tick` last returned, and the default.
    fn idle_deadline(&mut self, _now_ns: u64, due: Option<u64>) -> Option<u64> {
        due
    }

    /// `AvSetDisplayMode` with a mode: the GPU takes the refresh rate and the
    /// frame buffer's shape (`Gpu::av_set_display_mode`).
    fn set_display_mode(&mut self, _mode: DisplayMode) {}

    /// The picture shape the title asked Direct3D for, found in its
    /// presentation parameters when it set a mode: 16:9 or 4:3.
    fn set_title_widescreen(&mut self, _wide: bool) {}

    /// `PhyGetLinkState`: the Ethernet link as `XNET_ETHERNET_LINK_*` flags
    /// (active 1, 100 Mb/s 2, 10 Mb/s 4, full duplex 8, half duplex 16);
    /// 0 without a link.
    fn phy_link_state(&mut self) -> u32 {
        0
    }

    /// `PhyInitialize`: the PHY negotiates the link again; the NIC raises its
    /// link-change interrupt when done.
    fn phy_initialize(&mut self) {}

    /// DirectSound waits for the audio processor's global DSP to clear the
    /// mailbox word at physical `word_pa` (`Apu::mailbox_wait`). May block
    /// for about a millisecond; the title's loop checks the word again
    /// whatever happened.
    fn apu_mailbox_wait(&mut self, _word_pa: u32) {}

    /// The kernel freed the RAM pages `[pa, pa + len)`: the GPU drops what it
    /// keeps over them (`Gpu::freed`).
    fn ram_freed(&mut self, _pa: u32, _len: u32) {}

    /// `HalWriteSMBusValue` / `HalReadSMBusValue` for a device the kernel
    /// does not answer itself. `value` is `Some` for a write. Returns the
    /// value read (0 for a write or an absent device).
    fn smbus(&mut self, _address: u8, _command: u8, _word: bool, _value: Option<u32>) -> u32 {
        0
    }
}

/// A machine with nothing attached: every call does nothing. For tests and
/// tools that run a title's code without devices.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullPlatform;

impl Platform for NullPlatform {}
