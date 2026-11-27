//! The guest-CPU thread: builds the machine, boots the title and runs it.
//!
//! The CPU backend, the kernel and the register-level devices live on this
//! thread and nowhere else. The front end's thread gets an `Arc<Shared>`
//! back and talks to this one through it (pause, stop) and through the
//! handles the kernel gives out.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock};

use xb_apu::{Apu, ApuOptions, AudioSink, GuestRam};
use xb_cpu::Cpu;
use xb_devices::{Aci, Nic, Ohci, PadPorts, Ports};
use xb_formats::block::BlockSource;
use xb_gpu::{Gpu, GpuConfig, SurfaceProvider};
use xb_kernel::{BootOptions, ConsoleConfig, Kernel, KernelConfig, RunOutcome, TitleExit};
use xb_mem::{Ram, WriteTracker};
use xb_types::irq::IrqHub;
use xb_types::mem::PhysMem;
use xb_types::settings::{CpuBackend, Settings};

use crate::bus::{Devices, MISC_BASE, MachineBus, MachinePlatform};
use crate::machine::{
    CpuShared, KernelHandles, MachineCpu, Phase, Relocate, Shared, ToRenderer, lock,
    mark_guest_thread, unmark_guest_thread,
};

/// The end of the device windows the machine maps: the KVM backend keeps the
/// three pages from here (`xb_cpu_kvm::SYSTEM_VA`).
const DEVICE_END: u32 = 0xFFFE_0000;

/// What the guest-CPU thread needs to build the machine.
pub(crate) struct Build {
    pub(crate) disc: Arc<dyn BlockSource>,
    pub(crate) ram: Ram,
    pub(crate) settings: Settings,
    pub(crate) hdd_root: PathBuf,
    pub(crate) cache_dir: PathBuf,
    pub(crate) surface: Arc<dyn SurfaceProvider>,
    pub(crate) audio: Arc<dyn AudioSink>,
    pub(crate) pads: PadPorts,
}

/// What the front end's thread gets back from a successful boot.
pub(crate) struct Booted {
    pub(crate) shared: Arc<Shared>,
    pub(crate) title: String,
    pub(crate) title_id: u32,
    pub(crate) cpu_backend: &'static str,
}

/// Creates the CPU backend the settings ask for.
fn make_cpu(choice: CpuBackend, ram: &Ram) -> Result<Box<dyn Cpu>, String> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        use xb_cpu_kvm::KvmCpu;
        use xb_cpu_x64::X64Cpu;
        let kvm = || KvmCpu::new(ram.fd(), ram.size()).map(|c| Box::new(c) as Box<dyn Cpu>);
        let x64 = || X64Cpu::new(ram.fd(), ram.size()).map(|c| Box::new(c) as Box<dyn Cpu>);
        match choice {
            CpuBackend::Kvm => {
                log::info!(target: "core", "cpu: KVM (chosen in the settings)");
                kvm().map_err(|e| format!("KVM was asked for but cannot be used: {e}"))
            }
            CpuBackend::Translator => {
                log::info!(target: "core", "cpu: the x86-64 translator (chosen in the settings)");
                x64().map_err(|e| format!("the translator cannot start: {e}"))
            }
            CpuBackend::Auto => match KvmCpu::probe().and_then(|()| kvm()) {
                Ok(cpu) => {
                    log::info!(target: "core", "cpu: KVM (/dev/kvm is usable)");
                    Ok(cpu)
                }
                Err(why) => {
                    log::info!(target: "core", "cpu: the x86-64 translator (KVM is not usable: {why})");
                    x64().map_err(|e| format!("no CPU backend: KVM: {why}; translator: {e}"))
                }
            },
        }
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = (choice, ram);
        Err("no CPU backend exists for this host yet".into())
    }
}

/// The machine as the guest-CPU thread holds it.
pub(crate) struct Machine {
    pub(crate) shared: Arc<Shared>,
    pub(crate) cpu: MachineCpu,
    pub(crate) kernel: Kernel,
    pub(crate) bus: MachineBus,
    platform: MachinePlatform,
    /// The running title, for a savestate's header.
    pub(crate) title_id: u32,
    pub(crate) title: String,
}

/// Overrides `Settings::fast_disc` for this run: `console` (reads take the
/// console drives' time, which a few titles need and which makes a level's
/// first load as long as the console's), `quarter` or `fast`. The reference's
/// `devtime=` option; a title that needs the drives' own time keeps it.
const DISC_SPEED_ENV: &str = "XBEMU_DISC_SPEED";

fn disc_speed_from_env() -> Option<xb_fs::TimingMode> {
    let text = std::env::var(DISC_SPEED_ENV).ok()?;
    let mode = xb_fs::TimingMode::from_name(&text);
    match mode {
        Some(mode) => log::info!(target: "core", "{DISC_SPEED_ENV}: disc speed {mode:?}"),
        None => {
            log::warn!(target: "core", "{DISC_SPEED_ENV}: '{text}' is not console, quarter or fast; the console's speed is kept");
        }
    }
    mode
}

/// The drives' timing for the "Disc speed" setting.
pub(crate) const fn disc_speed(fast: bool) -> xb_fs::TimingMode {
    if fast { xb_fs::TimingMode::Off } else { xb_fs::TimingMode::Console }
}

/// Whether the environment chose the disc speed for this run, over the
/// setting.
pub(crate) fn disc_speed_forced() -> bool {
    std::env::var(DISC_SPEED_ENV).is_ok_and(|text| xb_fs::TimingMode::from_name(&text).is_some())
}

fn build(b: Build) -> Result<(Machine, Booted), String> {
    let ram = b.ram;
    let cpu_shared = Arc::new(CpuShared::new());
    let mut cpu = MachineCpu::new(make_cpu(b.settings.cpu, &ram)?, cpu_shared.clone());
    let backend = cpu.name();
    let tracker = Arc::new(WriteTracker::new(ram.size(), cpu.protector()));
    let hub = Arc::new(IrqHub::new());

    let renderer_events = Arc::new(OnceLock::new());
    let mut cfg = KernelConfig::new(ram.clone(), hub.clone());
    // XBEMU_WAIT_LOOPS=off (to compare): the title's wait loops run as guest code.
    cfg.wait_loops = std::env::var("XBEMU_WAIT_LOOPS").as_deref() != Ok("off");
    cfg.tracker = Some(tracker.clone());
    cfg.relocate_hooks =
        Box::new(Relocate { tracker: tracker.clone(), events: renderer_events.clone() });
    cfg.console = ConsoleConfig::from_settings(&b.settings);
    cfg.console.device_timing = disc_speed_from_env().or(Some(disc_speed(b.settings.fast_disc)));
    let mut kernel = Kernel::new(cfg, &mut cpu).map_err(|e| format!("the kernel: {e}"))?;
    let wake = kernel.wake_handle();
    hub.set_kick(Box::new({
        let wake = wake.clone();
        move || wake.wake()
    }));
    let shared = Arc::new(Shared::new(
        ram.clone(),
        tracker.clone(),
        cpu_shared,
        hub.clone(),
        KernelHandles {
            clock: kernel.clock(),
            wake,
            control: kernel.control(),
            frame: kernel.frame_hold(),
        },
    ));

    // The renderer keeps the RAM mapping for the rest of the process; this
    // handle is never dropped so the mapping never goes.
    let forever: &'static Ram = Box::leak(Box::new(ram.clone()));
    let mut gpu_cfg = GpuConfig::from_settings(&b.settings, &b.cache_dir);
    // The frame rate is drawn by the core's own overlay (`xb-menu`).
    gpu_cfg.show_fps = false;
    // SAFETY: `forever` is a leaked handle of the shared RAM mapping, so the
    // pointer stays valid for reads and writes of `size` bytes from any
    // thread until the process ends; the mapping is shared memory, which
    // tolerates the renderer's accesses racing with the guest's.
    let gpu = unsafe {
        Gpu::new(
            shared.clone(),
            forever.as_ptr(),
            forever.size(),
            hub.clone(),
            gpu_cfg,
            Some(b.surface),
        )
    }
    .map_err(|e| format!("the GPU: {e}"))?;
    let events = gpu.mem_events();
    let _ = renderer_events.set(events);
    tracker.set_events(Some(Arc::new(ToRenderer(events))));
    if shared.gpu.set(gpu).is_err() {
        return Err("the GPU exists twice".into());
    }
    let gpu = shared.gpu.get().ok_or("the GPU is gone")?;

    let options =
        ApuOptions { volume: f32::from(b.settings.volume) / 100.0, ..ApuOptions::default() };
    // SAFETY: the pointer is the RAM mapping's, valid for `size` bytes and
    // page aligned; `shared` holds a `Ram` handle for as long as the APU
    // (which `Core::shutdown` drops before it lets go of `shared`) exists.
    let guest_ram = unsafe { GuestRam::new(ram.as_ptr(), ram.size()) };
    let apu = Arc::new(
        Apu::new(guest_ram, hub.clone(), shared.clone(), b.audio, &options)
            .map_err(|e| format!("the APU: {e}"))?,
    );
    *lock(&shared.apu) = Some(apu.clone());

    let mem: Arc<dyn PhysMem> = Arc::new(ram.clone());
    let clock = {
        let clock = kernel.clock();
        Arc::new(move || clock.now_ns())
    };
    let dev = Rc::new(RefCell::new(Devices::new(
        Ohci::new(mem.clone(), hub.clone(), clock.clone(), b.pads),
        Aci::new(mem, hub.clone()),
        Nic::new(hub),
        Ports::new(clock, shared.clone()),
    )));

    // The device windows, identity mapped: the NV2A, and above it one run
    // for the APU, AC'97, USB and NIC with the catch-all register file
    // between them, as the reference registers them.
    for (base, len) in
        [(xb_gpu::NV2A_MMIO_BASE, xb_gpu::NV2A_MMIO_SIZE), (MISC_BASE, DEVICE_END - MISC_BASE)]
    {
        if !kernel.map_device(base, base, len) {
            return Err(format!("the device window at {base:08x} cannot be mapped"));
        }
    }

    // Registers whose reads are plain memory are read by the guest without
    // leaving it, where the backend can do that.
    if std::env::var_os("XBEMU_NO_READ_THROUGH").is_none() {
        let mut n = 0;
        let mut bytes = 0u32;
        for (off, len, host) in gpu.read_through() {
            if cpu.read_through(xb_gpu::NV2A_MMIO_BASE + off, len, host) {
                n += 1;
                bytes += len;
            }
        }
        log::info!(target: "core", "register reads without an exit: {n} NV2A ranges, {} KiB", bytes >> 10);
    }

    let info = kernel
        .boot(
            &mut cpu,
            BootOptions { disc: Some(b.disc), hdd_root: b.hdd_root, ..BootOptions::default() },
        )
        .map_err(|e| e.0)?;
    if !info.unimplemented.is_empty() {
        log::info!(target: "core", "the title imports {} kernel functions that are not implemented: {}",
            info.unimplemented.len(), info.unimplemented.join(", "));
    }
    // The renderer reads the XBE's headers through the page table.
    kernel.sync_cpu(&mut cpu);
    gpu.title_start().map_err(|e| format!("the renderer: {e}"))?;
    apu.start();
    log::info!(
        target: "core",
        "booted '{}' ({:08x}) from {}: entry {:08x}, {} MiB of RAM, {backend} CPU",
        info.title_name, info.title_id, info.xbe_path, info.entry, ram.size() >> 20
    );

    let booted = Booted {
        shared: shared.clone(),
        title: info.title_name,
        title_id: info.title_id,
        cpu_backend: backend,
    };
    let bus = MachineBus::new(shared.clone(), apu.clone(), dev.clone());
    let platform = MachinePlatform::new(shared.clone(), apu, dev);
    let (title_id, title) = (booted.title_id, booted.title.clone());
    Ok((Machine { shared, cpu, kernel, bus, platform, title_id, title }, booted))
}

/// The run loop: guest code until the front end stops it or the title ends.
fn run(m: &mut Machine) {
    // RDTSC counts guest time from the first instruction on, whichever
    // backend runs it (KVM's guest reads the processor's own counter).
    m.cpu.set_tsc(m.shared.tsc());
    loop {
        let outcome = m.kernel.run(&mut m.cpu, &mut m.bus, &mut m.platform);
        let mut run = lock(&m.shared.run);
        match outcome {
            RunOutcome::Paused => {
                run.phase = Phase::Paused;
                m.shared.run_cv.notify_all();
                while run.want_pause && !run.quit {
                    if let Some(job) = run.state_job.take() {
                        // A savestate, taken or put back while everything
                        // stands still.
                        drop(run);
                        m.state_job(job);
                        run = lock(&m.shared.run);
                        continue;
                    }
                    run = m
                        .shared
                        .run_cv
                        .wait(run)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                if run.quit {
                    run.phase = Phase::Stopped;
                    break;
                }
                run.phase = Phase::Running;
                m.shared.run_cv.notify_all();
                // Guest time stood still for the pause, or a state moved
                // it: the processor's own counter follows.
                m.cpu.set_tsc(m.shared.tsc());
            }
            RunOutcome::Stopped => {
                if run.quit {
                    run.phase = Phase::Stopped;
                    break;
                }
            }
            RunOutcome::Exit(exit) => {
                match exit {
                    TitleExit::Relaunch { xbe_path, .. } => {
                        // Booting another XBE needs a new kernel and, for
                        // the renderer, a new process.
                        log::warn!(target: "core", "the title relaunched into {xbe_path}, which is not supported yet");
                    }
                    TitleExit::Firmware { routine, launch_path, dashboard_missing } => {
                        log::info!(target: "core", "the title returned to the firmware (routine {routine}, launch path '{launch_path}'{})",
                            if dashboard_missing { ", for a dashboard that is not installed" } else { "" });
                    }
                    TitleExit::Shutdown => {
                        log::info!(target: "core", "the title shut the console down");
                    }
                }
                run.phase = Phase::Exited;
                break;
            }
            RunOutcome::Fatal(report) => {
                // The kernel has logged the whole report.
                run.phase = Phase::Fatal(format!("{} ({})", report.reason, report.registers));
                break;
            }
        }
    }
    m.shared.run_cv.notify_all();
}

/// The guest-CPU thread's body. Sends the boot's result once, then runs the
/// title until asked to stop.
pub(crate) fn thread_main(build_with: Build, booted: &Sender<Result<Booted, String>>) {
    mark_guest_thread();
    let mut machine = match build(build_with) {
        Ok((machine, info)) => {
            if booted.send(Ok(info)).is_err() {
                return;
            }
            machine
        }
        Err(why) => {
            let _ = booted.send(Err(why));
            return;
        }
    };
    run(&mut machine);
    unmark_guest_thread();
    log::info!(target: "core", "the guest CPU stopped after {} kernel calls", machine.kernel.call_count());
    // The bus and the platform (the register-level devices and this
    // thread's reference to the APU), the kernel and the CPU end here.
}
