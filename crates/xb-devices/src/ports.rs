//! x86 I/O ports (`IN` / `OUT`).
//!
//! With the kernel implemented on the host, what a title reaches through
//! ports is small: the kernel's own port users (the interrupt controller,
//! the system timer, the SMBus controller, PCI configuration) never run as
//! guest code. Two ports are claimed; the rest read as all ones and drop
//! writes, as unclaimed ports do on the bus. The first accesses to each
//! unclaimed port are logged.
//!
//! | Port | What |
//! |---|---|
//! | [`PORT_ACPI_PM_TIMER`] | ACPI power-management timer: free-running at 3.375 MHz |
//! | [`PORT_MCPX_GPIO`] | MCPX GPIO: bit 5 the video field, odd / even by turns at each vblank (the other bits as an unclaimed port's) |

use std::sync::Arc;

use crate::{Clock, VblankCounter, size_mask};

/// The ACPI power-management timer (read, any size).
pub const PORT_ACPI_PM_TIMER: u16 = 0x8008;
/// The MCPX GPIO port whose bit 5 is the video field.
///
/// D3D's vblank DPC reads the field from bit 5 of this port into the device,
/// and `GetDisplayFieldStatus` reports it. Read as all ones, every field is
/// the same one, and the XMV player, which starts a movie on the other field,
/// never starts (Riddick's movies stay black).
pub const PORT_MCPX_GPIO: u16 = 0x80C0;
/// The video field bit of [`PORT_MCPX_GPIO`]: set on odd vblank counts.
pub const GPIO_FIELD_BIT: u32 = 0x20;
/// Rate of the ACPI power-management timer.
pub const ACPI_PM_TIMER_HZ: u64 = 3_375_000;

/// How many distinct unclaimed ports have their accesses logged, and how
/// many accesses of each.
const LOGGED_PORTS: usize = 64;
const LOGGED_ACCESSES: u32 = 4;

/// The machine's I/O port space.
pub struct Ports {
    clock: Arc<dyn Clock>,
    vblanks: Arc<dyn VblankCounter>,
    seen: Vec<(u16, u32)>,
}

impl Ports {
    /// The port space. `clock` is the guest's time (the timer counts it),
    /// `vblanks` the GPU's vblank count (the video field follows it).
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>, vblanks: Arc<dyn VblankCounter>) -> Self {
        Self { clock, vblanks, seen: Vec::new() }
    }

    /// Nothing here holds state the guest can change; a reset only lets the
    /// first accesses be logged again.
    pub fn reset(&mut self) {
        self.seen.clear();
    }

    /// `IN`: reads `size` (1, 2 or 4) bytes from `port`.
    #[must_use]
    pub fn io_read(&mut self, port: u16, size: u8) -> u32 {
        let ones = size_mask(size);
        match port {
            PORT_ACPI_PM_TIMER => {
                let ticks =
                    u128::from(self.clock.now_ns()) * u128::from(ACPI_PM_TIMER_HZ) / 1_000_000_000;
                ticks as u32 & ones
            }
            PORT_MCPX_GPIO => {
                let field = if self.vblanks.vblanks() & 1 != 0 { GPIO_FIELD_BIT } else { 0 };
                (ones & !GPIO_FIELD_BIT) | field
            }
            _ => {
                self.note(port, false, size, ones);
                ones
            }
        }
    }

    /// `OUT`: writes the low `size` bytes of `value` to `port`. No port
    /// takes writes; they are dropped.
    pub fn io_write(&mut self, port: u16, size: u8, value: u32) {
        self.note(port, true, size, value & size_mask(size));
    }

    fn note(&mut self, port: u16, out: bool, size: u8, value: u32) {
        if let Some((_, count)) = self.seen.iter_mut().find(|(p, _)| *p == port) {
            *count += 1;
            if *count > LOGGED_ACCESSES {
                return;
            }
        } else if self.seen.len() < LOGGED_PORTS {
            self.seen.push((port, 1));
        } else {
            return;
        }
        let (dir, arrow) = if out { ("out", "<-") } else { ("in", "->") };
        log::debug!(target: "dev", "port {dir} {port:04x} {arrow} {value:0w$x} ({size})", w = 2 * usize::from(size));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::testutil::TestClock;

    #[derive(Default)]
    struct Vblanks(AtomicU64);

    impl VblankCounter for Vblanks {
        fn vblanks(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn ports() -> (Ports, Arc<TestClock>, Arc<Vblanks>) {
        let clock = TestClock::new();
        let vblanks = Arc::new(Vblanks::default());
        (Ports::new(clock.clone(), vblanks.clone()), clock, vblanks)
    }

    #[test]
    fn the_pm_timer_counts_guest_time_at_its_rate() {
        let (mut p, clock, _) = ports();
        let start = p.io_read(PORT_ACPI_PM_TIMER, 4);
        clock.step(1_000_000_000);
        let after_a_second = p.io_read(PORT_ACPI_PM_TIMER, 4);
        assert_eq!(u64::from(after_a_second - start), ACPI_PM_TIMER_HZ);
        // It stands still while the guest's clock does.
        assert_eq!(p.io_read(PORT_ACPI_PM_TIMER, 4), after_a_second);
    }

    #[test]
    fn the_pm_timer_wraps_at_32_bits_instead_of_saturating() {
        let (mut p, clock, _) = ports();
        // Just short of the 2^32 ticks the counter holds.
        let wrap_ns = (1u128 << 32) * 1_000_000_000 / u128::from(ACPI_PM_TIMER_HZ);
        clock.step(wrap_ns as u64 - 1_000_000);
        let before = p.io_read(PORT_ACPI_PM_TIMER, 4);
        clock.step(2_000_000);
        let after = p.io_read(PORT_ACPI_PM_TIMER, 4);
        assert!(after < before);
        assert_eq!(u64::from(after.wrapping_sub(before)), ACPI_PM_TIMER_HZ * 2 / 1000);
    }

    #[test]
    fn the_video_field_bit_alternates_with_each_vblank() {
        let (mut p, _, vblanks) = ports();
        let even = p.io_read(PORT_MCPX_GPIO, 1);
        vblanks.0.fetch_add(1, Ordering::SeqCst);
        let odd = p.io_read(PORT_MCPX_GPIO, 1);
        vblanks.0.fetch_add(1, Ordering::SeqCst);
        assert_eq!(even ^ odd, GPIO_FIELD_BIT, "only the field bit differs");
        assert_eq!(p.io_read(PORT_MCPX_GPIO, 1), even);
        assert_eq!(even | GPIO_FIELD_BIT, 0xFF, "the other bits read as an unclaimed port's");
    }

    #[test]
    fn unclaimed_ports_read_all_ones_at_each_size_and_drop_writes() {
        let (mut p, _, _) = ports();
        // The PIC, PIT, CMOS, PCI configuration and SMBus ports: the kernel's
        // business, not answered here.
        for port in [0x20, 0x21, 0x40, 0x43, 0x70, 0x71, 0xCF8, 0xCFC, 0xC000, 0xC004] {
            p.io_write(port, 1, 0x12);
            assert_eq!(p.io_read(port, 1), 0xFF);
            assert_eq!(p.io_read(port, 2), 0xFFFF);
            assert_eq!(p.io_read(port, 4), 0xFFFF_FFFF);
        }
        // Halo's write to the GPIO port changes nothing.
        let before = p.io_read(PORT_MCPX_GPIO, 1);
        p.io_write(PORT_MCPX_GPIO, 1, 0x01);
        assert_eq!(p.io_read(PORT_MCPX_GPIO, 1), before);
    }
}
