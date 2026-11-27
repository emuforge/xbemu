//! The run loop and what comes out of it.
//!
//! The kernel calls [`xb_cpu::Cpu::run`] in a loop and handles each
//! [`xb_cpu::Exit`].

use xb_cpu::{Bus, Cpu, RunFlags, reg};
use xb_types::settings::Settings;

use crate::clock::{GuestClock, RunControl, WakeHandle};
use crate::exports;
use crate::kernel::Kernel;
use crate::platform::Platform;

/// The EEPROM's video flags (`XC_VIDEO_FLAGS`).
pub mod video {
    /// The console is set to widescreen.
    pub const WIDESCREEN: u32 = 0x0001_0000;
    /// 720p allowed.
    pub const HD_720P: u32 = 0x0002_0000;
    /// 1080i allowed.
    pub const HD_1080I: u32 = 0x0004_0000;
    /// 480p allowed.
    pub const HD_480P: u32 = 0x0008_0000;
    /// Letterbox.
    pub const LETTERBOX: u32 = 0x0010_0000;
    /// PAL titles at 60 Hz.
    pub const PAL60: u32 = 0x0040_0000;
}

/// The cable the console reports (the system controller's AV pack).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AvPack {
    /// Component (HDTV).
    #[default]
    Hdtv,
    /// Composite.
    Standard,
    /// S-Video.
    SVideo,
    /// SCART (RGB).
    Scart,
    /// VGA.
    Vga,
}

/// The console's settings as the dashboard keeps them in the EEPROM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsoleConfig {
    /// `XC_VIDEO_FLAGS` ([`video`] bits). The reference's default allows
    /// 480p.
    pub video_flags: u32,
    /// The AV pack.
    pub av_pack: AvPack,
    /// `XC_LANGUAGE`: 1 English, 2 Japanese, 3 German, 4 French, 5 Spanish,
    /// 6 Italian, 7 Korean, 8 Chinese (traditional), 9 Portuguese.
    pub language: u32,
    /// The time zone, minutes east of UTC.
    pub time_zone_minutes_east: i32,
    /// Dolby Digital on (`XC_AUDIO_FLAGS`).
    pub surround: bool,
    /// The console's clock at boot is the host's moved by this much.
    pub clock_offset_ms: i64,
    /// The game region; `None`: the one the title's certificate asks for.
    pub region: Option<u32>,
    /// Console-speed drive timing; `None`: the title's default (on for the
    /// titles that need it).
    pub device_timing: Option<xb_fs::TimingMode>,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            video_flags: video::HD_480P,
            av_pack: AvPack::Hdtv,
            language: 1,
            time_zone_minutes_east: 0,
            surround: false,
            clock_offset_ms: 0,
            region: None,
            device_timing: None,
        }
    }
}

impl ConsoleConfig {
    /// The defaults with what the user's settings say about the console
    /// (widescreen).
    #[must_use]
    pub fn from_settings(s: &Settings) -> Self {
        let mut c = Self::default();
        if s.widescreen {
            c.video_flags |= video::WIDESCREEN;
        }
        c
    }
}

/// How a title left.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TitleExit {
    /// `XLaunchNewImage`: a quick reboot (`HalReturnToFirmware(2)`) into
    /// another XBE with a launch data page. The machine boots `xbe_path`
    /// with a new kernel and passes the page ([`crate::BootOptions`]).
    Relaunch {
        /// The XBE as a device path (`\Device\CdRom0\...` or
        /// `\Device\Harddisk0\Partition2\...`).
        xbe_path: String,
        /// The 4 KiB launch data page.
        launch_page: Vec<u8>,
    },
    /// The title returned to the firmware: to the dashboard, a reboot.
    Firmware {
        /// `HalReturnToFirmware`'s routine.
        routine: u32,
        /// The launch path in the launch data page, if any.
        launch_path: String,
        /// The title asked the dashboard for a page and no dashboard is
        /// installed on the hard disk.
        dashboard_missing: bool,
    },
    /// The title shut the console down (`HalInitiateShutdown`).
    Shutdown,
}

/// Why the guest stopped for good.
#[derive(Clone, Debug)]
pub(crate) enum Stop {
    Fatal(String),
    Exit(TitleExit, String),
}

/// What is known about a fatal guest condition (a bugcheck, an unhandled
/// exception, an unimplemented export, a CPU failure), as the reference logs it.
#[derive(Clone, Debug)]
pub struct FatalReport {
    /// One line: what happened.
    pub reason: String,
    /// The last kernel calls, oldest first.
    pub recent_calls: Vec<String>,
    /// Every guest thread: state, what it waits on, the top of its stack.
    pub threads: Vec<String>,
    /// The running thread's registers.
    pub registers: String,
}

impl std::fmt::Display for FatalReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "stop: {}", self.reason)?;
        writeln!(f, "  {}", self.registers)?;
        for l in &self.recent_calls {
            writeln!(f, "{l}")?;
        }
        for l in &self.threads {
            writeln!(f, "{l}")?;
        }
        Ok(())
    }
}

/// Why [`Kernel::run`] returned.
#[derive(Clone, Debug)]
pub enum RunOutcome {
    /// [`RunControl::request_stop`] was called. `run` may be called again.
    Stopped,
    /// [`RunControl::request_pause`] was called. Guest time stands still
    /// until `run` is called again.
    Paused,
    /// The title left; the kernel is finished.
    Exit(TitleExit),
    /// A fatal guest condition; the kernel is finished.
    Fatal(Box<FatalReport>),
}

impl Kernel {
    /// The handle that brings the guest-CPU thread back to the kernel. The
    /// machine installs it as the `IrqHub`'s kick hook.
    #[must_use]
    pub fn wake_handle(&self) -> WakeHandle {
        self.wake.clone()
    }

    /// The handle that asks [`Kernel::run`] to stop or pause.
    #[must_use]
    pub fn control(&self) -> RunControl {
        self.control.clone()
    }

    /// The guest's clock.
    #[must_use]
    pub fn clock(&self) -> GuestClock {
        self.clock.clone()
    }

    /// Copies guest memory at virtual address `va` into `buf`; `false`, with
    /// the bytes before it copied, if a page of the range is not RAM.
    pub fn read_guest(&self, va: u32, buf: &mut [u8]) -> bool {
        self.gm.read(va, buf).is_ok()
    }

    /// Copies `data` to guest memory at virtual address `va`, reporting the
    /// store to write tracking; `false` if a page of the range is not RAM.
    /// Guest-CPU thread, outside [`Kernel::run`].
    pub fn write_guest(&mut self, va: u32, data: &[u8]) -> bool {
        self.gm.write(va, data).is_ok()
    }

    /// The physical address behind guest virtual `va`, if RAM is mapped
    /// there.
    #[must_use]
    pub fn translate(&self, va: u32) -> Option<u32> {
        self.gm.phys_of(va)
    }

    /// Maps `[va, va + len)` as device registers at bus address `pa`
    /// (page aligned): the machine's device windows (`0xFD00_0000` and up),
    /// identity mapped as on the console. Reaches the CPU at the next
    /// [`Kernel::run`] or [`Kernel::sync_cpu`].
    pub fn map_device(&mut self, va: u32, pa: u32, len: u32) -> bool {
        self.gm.map_mmio(va, pa, len)
    }

    /// Kernel calls made so far.
    #[must_use]
    pub fn call_count(&self) -> u64 {
        self.calls
    }

    /// Runs the guest: schedules guest threads, runs them on `cpu`, handles
    /// every exit, and returns when asked to stop or pause, when the title
    /// leaves, or on a fatal guest condition.
    ///
    /// Call on the guest-CPU thread. `bus` serves device registers and ports
    /// inside `Cpu::run`; `platform` is called between runs. Guest time runs
    /// only inside this call.
    pub fn run(
        &mut self,
        cpu: &mut dyn Cpu,
        bus: &mut dyn Bus,
        platform: &mut dyn Platform,
    ) -> RunOutcome {
        if self.stop.is_some() {
            return self.finish();
        }
        self.wake.set_kicker(Some(cpu.kicker()));
        self.clock.pause_end();
        self.ticker.set_running(true);
        self.update_time();
        let outcome = self.run_loop(cpu, bus, platform);
        self.ticker.set_running(false);
        self.clock.pause_begin();
        self.wake.set_kicker(None);
        outcome
    }

    fn run_loop(
        &mut self,
        cpu: &mut dyn Cpu,
        bus: &mut dyn Bus,
        platform: &mut dyn Platform,
    ) -> RunOutcome {
        loop {
            if self.control.take_stop() {
                return RunOutcome::Stopped;
            }
            if self.control.take_pause() {
                log::info!(target: "kernel", "paused: the guest's time stands still");
                return RunOutcome::Paused;
            }
            if self.stop.is_none() && self.sched.picking {
                self.pick(platform); // the idle scheduler, after a pause
                if self.sched.picking && self.stop.is_none() {
                    continue; // a request ended its wait
                }
            }
            if self.stop.is_some() {
                return self.finish();
            }
            if self.current.is_none() {
                self.request_stop("no title is loaded".into());
                continue;
            }
            for (pa, len) in self.gm.take_freed() {
                platform.ram_freed(pa, len);
            }
            if self.gm.dirty_maps() {
                self.gm.flush(cpu);
            }
            if self.in_wait_loop() {
                self.spin_wait(platform);
                continue;
            }
            self.wake.take();
            let flags = RunFlags { exit_when_interruptible: self.irq_window };
            self.ticker.set_in_guest(true);
            let exit = cpu.run(&mut self.frame, bus, flags);
            self.ticker.set_in_guest(false);
            self.entry(platform, exit);
        }
    }

    /// The registers of the running thread, for reports.
    fn registers(&self) -> String {
        let r = &self.frame.regs;
        format!(
            "eip {:08x} eax {:08x} ebx {:08x} ecx {:08x} edx {:08x} esi {:08x} edi {:08x} ebp {:08x} esp {:08x} eflags {:08x} irql {} thread t{}",
            r.eip,
            r.gpr[reg::EAX],
            r.gpr[reg::EBX],
            r.gpr[reg::ECX],
            r.gpr[reg::EDX],
            r.gpr[reg::ESI],
            r.gpr[reg::EDI],
            r.gpr[reg::EBP],
            r.gpr[reg::ESP],
            r.eflags,
            self.irql(),
            self.current.map_or(0, |t| self.threads[t].id)
        )
    }

    /// The last kernel calls.
    pub(crate) fn dump_recent(&self) -> Vec<String> {
        let n = self.recent.len();
        let end = self.nrecent;
        (end.saturating_sub(n)..end)
            .map(|i| {
                let r = &self.recent[i % n];
                let name = exports::by_ordinal(u32::from(r.ordinal)).map_or("?", |e| e.name);
                let result =
                    if r.running { "(running)".to_owned() } else { format!("{:08x}", r.result) };
                format!(
                    "  call t{} {name} from {:08x} [{:08x} {:08x} {:08x} {:08x} {:08x}] -> {result}",
                    r.thread, r.ret, r.args[0], r.args[1], r.args[2], r.args[3], r.args[4]
                )
            })
            .collect()
    }

    /// The report of the guest's state now: for a fatal stop, or on demand
    /// (a watchdog's dump).
    #[must_use]
    pub fn report(&self, reason: &str) -> FatalReport {
        FatalReport {
            reason: reason.to_owned(),
            recent_calls: self.dump_recent(),
            threads: self.dump_threads(),
            registers: self.registers(),
        }
    }

    fn finish(&mut self) -> RunOutcome {
        let threads =
            self.threads.iter().filter(|t| t.state != crate::ps::ThreadState::Free).count();
        let first = exports::by_ordinal(self.first_ordinal).map_or("-", |e| e.name);
        let summary = format!(
            "summary stage={} kernel_calls={} first_call={first} threads={threads}",
            if self.first_ordinal != 0 { "S2" } else { "S1" },
            self.calls
        );
        match self.stop.clone() {
            Some(Stop::Exit(exit, why)) => {
                log::info!(target: "kernel", "stop: {why}");
                log::info!(target: "kernel", "{summary}");
                RunOutcome::Exit(exit)
            }
            Some(Stop::Fatal(reason)) => {
                let report = self.report(&reason);
                log::error!(target: "kernel", "stop: {reason}");
                log::error!(target: "kernel", "  {}", report.registers);
                for l in report.recent_calls.iter().chain(&report.threads) {
                    log::error!(target: "kernel", "{l}");
                }
                log::info!(target: "kernel", "{summary}");
                RunOutcome::Fatal(Box::new(report))
            }
            None => RunOutcome::Stopped,
        }
    }
}
