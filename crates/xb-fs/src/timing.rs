//! How long the console's drives take, so the kernel can make I/O complete
//! no sooner.
//!
//! This crate only computes; the kernel waits.
//!
//! Why it matters: at a PC's speed a load runs without its thread ever
//! waiting, so a title's lower-priority threads do not run until it is done.
//! Morrowind starts its loading screen's thread as a cell load begins and
//! froze leaving a building when the load finished first.

use std::time::Duration;

use crate::namespace::DeviceKind;

/// A drive's speed: a fixed time per request plus a transfer rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceRate {
    /// Microseconds every request takes.
    pub request_us: u32,
    /// Kilobytes (1000 bytes) transferred per second; 0 for no transfer time.
    pub kb_per_s: u32,
}

impl DeviceRate {
    /// The time a request for `bytes` takes.
    #[must_use]
    pub fn cost(&self, bytes: u32) -> Duration {
        let transfer =
            if self.kb_per_s == 0 { 0 } else { u64::from(bytes) * 1000 / u64::from(self.kb_per_s) };
        Duration::from_micros(u64::from(self.request_us) + transfer)
    }
}

/// How closely I/O follows the console's drive speeds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimingMode {
    /// Synchronous reads take no drive time. Overlapped reads still take the
    /// quarter-speed cost: they must land a little later than the call.
    Off,
    /// A quarter of the drives' time.
    Quarter,
    /// The drives' own time: the default, and what a few titles need.
    #[default]
    Console,
}

impl TimingMode {
    /// The mode a disc-speed choice names: `console` the drives' own time,
    /// `fast` none for synchronous reads (the reference's `devtime=2` and
    /// `devtime=0`, its "disc speed" setting), `quarter` a quarter of it.
    /// A title that needs the drives' own keeps it whatever is chosen
    /// ([`title_timing_mode`]).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "console" => Some(Self::Console),
            "quarter" => Some(Self::Quarter),
            "fast" => Some(Self::Off),
            _ => None,
        }
    }
}

/// The drives' speeds and whether synchronous I/O waits for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceTiming {
    /// Synchronous reads the file system's cache does not hold wait for the
    /// drive. Overlapped reads always take their cost.
    pub enabled: bool,
    /// The DVD drive.
    pub disc: DeviceRate,
    /// The hard disk.
    pub hard_disk: DeviceRate,
}

impl Default for DeviceTiming {
    fn default() -> Self {
        Self::for_mode(TimingMode::default())
    }
}

impl DeviceTiming {
    /// The DVD drive at 5x: 500 us a request, 6900 kB/s.
    pub const CONSOLE_DISC: DeviceRate = DeviceRate { request_us: 500, kb_per_s: 6900 };
    /// The hard disk: 200 us a request, 20000 kB/s.
    pub const CONSOLE_HARD_DISK: DeviceRate = DeviceRate { request_us: 200, kb_per_s: 20000 };

    /// The timing of a mode.
    #[must_use]
    pub const fn for_mode(mode: TimingMode) -> Self {
        match mode {
            TimingMode::Console => {
                Self { enabled: true, disc: Self::CONSOLE_DISC, hard_disk: Self::CONSOLE_HARD_DISK }
            }
            TimingMode::Off | TimingMode::Quarter => Self {
                enabled: matches!(mode, TimingMode::Quarter),
                disc: DeviceRate { request_us: 125, kb_per_s: 27600 },
                hard_disk: DeviceRate { request_us: 50, kb_per_s: 80000 },
            },
        }
    }

    /// Replaces the rates from `disc_us,disc_kBps,hdd_us,hdd_kBps` (the
    /// reference's format); numbers missing or malformed from some point on
    /// leave the rest as they are.
    #[must_use]
    pub fn with_rates(mut self, spec: &str) -> Self {
        let slots = [
            &mut self.disc.request_us,
            &mut self.disc.kb_per_s,
            &mut self.hard_disk.request_us,
            &mut self.hard_disk.kb_per_s,
        ];
        for (slot, text) in slots.into_iter().zip(spec.split(',')) {
            match text.trim().parse() {
                Ok(v) => *slot = v,
                Err(_) => break,
            }
        }
        self
    }

    /// The drive's time for a transfer of `bytes` on a device of `kind`.
    ///
    /// An overlapped read always takes it, timing enabled or not: Total
    /// Immersion Racing polls its first video's read once right after issuing
    /// it and breaks if the data is already there. A synchronous read takes it
    /// only when [`enabled`](Self::enabled) and the file's cached window does
    /// not hold the bytes ([`File::cache_fill`](crate::File::cache_fill)
    /// gives the byte count to charge).
    #[must_use]
    pub fn cost(&self, kind: DeviceKind, bytes: u32) -> Duration {
        match kind {
            DeviceKind::Disc => self.disc.cost(bytes),
            DeviceKind::HardDisk => self.hard_disk.cost(bytes),
        }
    }

    /// The line the reference logs when timing is set.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "console timing {}: disc {} us + {} kB/s, hard disk {} us + {} kB/s",
            if self.enabled { "on" } else { "off" },
            self.disc.request_us,
            self.disc.kb_per_s,
            self.hard_disk.request_us,
            self.hard_disk.kb_per_s
        )
    }
}

/// The timing a title needs whatever the settings say, by its title ID and
/// certificate name; `None` for the rest.
///
/// KotOR II's T3-M4 scene on Peragus stays black at any faster disc. Prince
/// of Persia: Warrior Within's fall into the ship's hold calls through a freed
/// object now and then when its data comes in sooner; The Sands of Time and
/// The Two Thrones are on the same engine.
#[must_use]
pub fn title_timing_mode(title_id: u32, title_name: &str) -> Option<TimingMode> {
    const TITLES: [(u32, Option<&str>); 5] = [
        (0x4C41_0014, Some("kotor ii")),
        (0, Some("sith lords")),
        (0x5553_003B, Some("prince of persia")),
        (0x5553_001D, None),
        (0x5553_0058, None),
    ];
    let name = title_name.to_ascii_lowercase();
    TITLES
        .iter()
        .any(|&(id, part)| (id != 0 && id == title_id) || part.is_some_and(|p| name.contains(p)))
        .then_some(TimingMode::Console)
}

/// A synchronous read this long is worth handing to an I/O worker with only
/// the calling thread waiting; shorter ones cost less in place.
pub const SYNC_LATER_MIN: u32 = 0x8000;

/// Page size of the file system's cache.
pub const FS_PAGE: u64 = 4096;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_the_console_timing_the_reference_logs() {
        assert_eq!(
            DeviceTiming::default().describe(),
            "console timing on: disc 500 us + 6900 kB/s, hard disk 200 us + 20000 kB/s"
        );
    }

    #[test]
    fn cost_is_request_time_plus_transfer_time() {
        let t = DeviceTiming::default();
        assert_eq!(t.cost(DeviceKind::Disc, 0), Duration::from_micros(500));
        assert_eq!(t.cost(DeviceKind::Disc, 6_900_000), Duration::from_micros(500 + 1_000_000));
        assert_eq!(t.cost(DeviceKind::HardDisk, 20_000), Duration::from_micros(200 + 1000));
        let rate = DeviceRate { request_us: 7, kb_per_s: 0 };
        assert_eq!(rate.cost(u32::MAX), Duration::from_micros(7));
    }

    #[test]
    fn a_quarter_of_the_time_is_a_quarter() {
        let (full, quarter) = (
            DeviceTiming::for_mode(TimingMode::Console),
            DeviceTiming::for_mode(TimingMode::Quarter),
        );
        for bytes in [0, 2048, 1 << 20] {
            for kind in [DeviceKind::Disc, DeviceKind::HardDisk] {
                let (f, q) =
                    (full.cost(kind, bytes).as_micros(), quarter.cost(kind, bytes).as_micros());
                assert!(q * 4 <= f && f <= q * 4 + 4, "{kind:?} {bytes}: {f} vs {q}");
            }
        }
        assert!(quarter.enabled);
        assert!(!DeviceTiming::for_mode(TimingMode::Off).enabled);
    }

    #[test]
    fn a_disc_speed_is_chosen_by_name() {
        assert_eq!(TimingMode::from_name("console"), Some(TimingMode::default()));
        assert_eq!(TimingMode::from_name(" Fast\n"), Some(TimingMode::Off));
        assert_eq!(TimingMode::from_name("quarter"), Some(TimingMode::Quarter));
        assert_eq!(TimingMode::from_name("warp"), None);
        // fast: synchronous reads do not wait, overlapped ones still land later
        let fast = DeviceTiming::for_mode(TimingMode::Off);
        assert!(!fast.enabled && !fast.cost(DeviceKind::Disc, 2048).is_zero());
    }

    #[test]
    fn rates_can_be_overridden_from_text() {
        let t = DeviceTiming::default().with_rates("1,2,3,4");
        assert_eq!((t.disc.request_us, t.disc.kb_per_s), (1, 2));
        assert_eq!((t.hard_disk.request_us, t.hard_disk.kb_per_s), (3, 4));
        let t = DeviceTiming::default().with_rates("9,x,3");
        assert_eq!((t.disc.request_us, t.disc.kb_per_s, t.hard_disk.request_us), (9, 6900, 200));
    }

    #[test]
    fn some_titles_need_the_drives_own_time() {
        assert_eq!(title_timing_mode(0x4C41_0014, ""), Some(TimingMode::Console));
        assert_eq!(
            title_timing_mode(1, "Prince of Persia: Warrior Within"),
            Some(TimingMode::Console)
        );
        assert_eq!(title_timing_mode(1, "STAR WARS KOTOR II"), Some(TimingMode::Console));
        assert_eq!(title_timing_mode(0x4D53_0004, "Halo"), None);
        assert_eq!(title_timing_mode(0, ""), None);
    }
}
