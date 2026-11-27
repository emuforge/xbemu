//! A real title, when the titles stash has `halo-ce` (`titles/README.md`): loaded
//! through the loader and run on the KVM backend with a bus that logs device
//! accesses and answers zeros, until it stops making progress.
//!
//! A reference run (`halo-ce.ref.log` in the stash, when there) reaches Direct3D's
//! device creation, which begins touching NV2A registers at `0xFD00_0000`.
//! With no GPU behind the bus the title eventually waits on hardware that
//! never answers; reaching the first NV2A accesses with the same file opens
//! and stages as the reference up to that point is the success criterion.
//! The test prints how far it got and compares the `fs` lines with the
//! reference's `xbased-native-io:` lines.

mod common;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Access, TempDir, TestBus};
use xb_cpu_kvm::KvmCpu;
use xb_kernel::{BootOptions, Kernel, KernelConfig, NullPlatform, RunOutcome};
use xb_mem::Ram;
use xb_types::irq::IrqHub;

static LINES: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

struct Capture;

static VERBOSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CAPTURE: Capture = Capture;

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, r: &log::Record<'_>) {
        let line = r.args().to_string();
        if VERBOSE.load(Ordering::Relaxed) {
            eprintln!("{}: {line}", r.target());
        }
        if r.level() <= log::Level::Info {
            LINES.lock().unwrap().push((r.target().to_owned(), line));
        }
    }
    fn flush(&self) {}
}

/// The reference log's lines of one kind, without the prefix.
fn reference(prefix: &str) -> Vec<String> {
    xb_testkit::reference_log("halo-ce")
        .map(|s| s.lines().filter_map(|l| l.strip_prefix(prefix)).map(str::to_owned).collect())
        .unwrap_or_default()
}

#[test]
fn halo_boots_to_its_first_gpu_register_accesses() {
    let Some(image) = xb_testkit::title("halo-ce") else { return };
    if let Err(why) = KvmCpu::probe() {
        println!("note: KVM is not usable here ({why}); test skipped");
        return;
    }
    let verbose = std::env::var("XBEMU_TEST_LOG").is_ok();
    let level = match std::env::var("XBEMU_TEST_LOG").as_deref() {
        Ok("trace") => log::LevelFilter::Trace,
        Ok("debug") => log::LevelFilter::Debug,
        _ => log::LevelFilter::Info,
    };
    VERBOSE.store(verbose, Ordering::Relaxed);
    log::set_logger(&CAPTURE).expect("logger");
    log::set_max_level(level);

    let disc = xb_formats::disc::open(&image).expect("the disc image opens");
    let ram_size = xb_kernel::ram_size_for(Some(&disc.source));
    let ram = Ram::new(ram_size).expect("guest RAM");
    let mut cpu = KvmCpu::new(ram.fd(), ram.size()).expect("KVM vCPU");
    let hub = Arc::new(IrqHub::new());
    let mut kernel =
        Kernel::new(KernelConfig::new(ram, Arc::clone(&hub)), &mut cpu).expect("kernel");
    let wake = kernel.wake_handle();
    hub.set_kick(Box::new(move || wake.wake()));
    // the console's device windows, identity mapped
    for (pa, len) in [
        (0xFD00_0000u32, 0x0100_0000u32), // NV2A
        (0xFE80_0000, 0x0008_0000),       // APU
        (0xFEC0_0000, 0x0000_1000),       // AC'97
        (0xFED0_0000, 0x0000_1000),       // USB 0
        (0xFED0_8000, 0x0000_1000),       // USB 1
        (0xFEF0_0000, 0x0000_1000),       // NIC
    ] {
        assert!(kernel.map_device(pa, pa, len));
    }
    // XBEMU_TEST_HDD: a hard disk folder (PartitionN under it) copied into
    // the test's own, so the run starts from a console that has run before
    let scratch = TempDir::new("halo");
    let hdd = scratch.0.join("hdd");
    std::fs::create_dir_all(&hdd).unwrap();
    if let Ok(from) = std::env::var("XBEMU_TEST_HDD") {
        copy_tree(std::path::Path::new(&from), &hdd);
    }
    let info = kernel
        .boot(
            &mut cpu,
            BootOptions {
                disc: Some(disc.source),
                hdd_root: hdd.clone(),
                ..BootOptions::default()
            },
        )
        .expect("the title boots");
    println!(
        "booted {:08x} \"{}\": {} imports, {} unimplemented {:?}",
        info.title_id,
        info.title_name,
        info.imports.len(),
        info.unimplemented.len(),
        info.unimplemented
    );

    // Run until the kernel-call count stops moving for a while after the
    // first GPU access, a fatal stop, or a time limit.
    let mut bus = TestBus::new(Arc::clone(&hub));
    bus.log_limit = 100_000;
    let limit =
        std::env::var("XBEMU_TEST_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(25u64);
    let control = kernel.control();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let dog = std::thread::spawn(move || {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(limit) {
            if !matches!(
                rx.recv_timeout(Duration::from_millis(200)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                return; // the run ended
            }
        }
        control.request_stop();
    });
    let started = Instant::now();
    let outcome = kernel.run(&mut cpu, &mut bus, &mut NullPlatform);
    drop(tx);
    dog.join().unwrap();

    let nv2a: Vec<&Access> = bus
        .log
        .iter()
        .filter(|a| matches!(a, Access::MmioRead(pa, _) | Access::MmioWrite(pa, _, _) if (0xFD00_0000..0xFE00_0000).contains(pa)))
        .collect();
    println!(
        "ran {:.1} s: {} kernel calls, {} device accesses ({} to the NV2A)",
        started.elapsed().as_secs_f64(),
        kernel.call_count(),
        bus.count,
        nv2a.len()
    );
    for a in nv2a.iter().take(12) {
        println!("  nv2a {a:x?}");
    }
    // the reference's first device accesses, as (write, offset): the same
    // code path touches the same registers in the same order until the
    // title reads something a real GPU would have answered differently
    let theirs_mmio: Vec<(bool, u32)> = reference("xbased-native-mmio: nv2a ")
        .iter()
        .filter_map(|l| {
            let off = u32::from_str_radix(l.get(2..8)?, 16).ok()?;
            Some((l.starts_with('w'), off))
        })
        .collect();
    let ours_mmio: Vec<(bool, u32)> = nv2a
        .iter()
        .map(|a| match a {
            Access::MmioWrite(pa, _, _) => (true, pa - 0xFD00_0000),
            Access::MmioRead(pa, _) => (false, pa - 0xFD00_0000),
            _ => unreachable!(),
        })
        .collect();
    let same_mmio = ours_mmio.iter().zip(&theirs_mmio).take_while(|(a, b)| a == b).count();
    println!(
        "nv2a accesses: the first {same_mmio} agree with the reference's ({} logged there)",
        theirs_mmio.len()
    );
    if let (Some(a), Some(b)) = (ours_mmio.get(same_mmio), theirs_mmio.get(same_mmio)) {
        println!("  then ours {a:x?}, the reference's {b:x?}");
    }
    // what the title spins on at the end: the last accesses, grouped
    let mut tail: Vec<(String, u32)> = Vec::new();
    for a in bus.log.iter().rev().take(2000) {
        let key = format!("{a:x?}");
        match tail.iter_mut().find(|(k, _)| *k == key) {
            Some((_, n)) => *n += 1,
            None => tail.push((key, 1)),
        }
    }
    tail.truncate(8);
    println!("  the last logged accesses: {tail:?}");
    match &outcome {
        RunOutcome::Fatal(r) => println!("outcome: fatal\n{r}"),
        RunOutcome::Stopped => {
            println!("outcome: still running at the time limit\n{}", kernel.report("time limit"))
        }
        other => println!("outcome: {other:?}"),
    }

    // Compare with the reference.
    let lines = LINES.lock().unwrap();
    let ours: Vec<&str> =
        lines.iter().filter(|(t, _)| t == "fs").map(|(_, l)| l.as_str()).collect();
    let theirs = reference("xbased-native-io: ");
    let theirs: Vec<&str> = theirs
        .iter()
        .map(String::as_str)
        .filter(|l| !l.starts_with("port ") && !l.starts_with("console timing"))
        .collect();
    let ours: Vec<&str> = ours.into_iter().filter(|l| !l.starts_with("console timing")).collect();
    // the reference ran over a hard disk that had the title's folders and
    // saves already; ours is new, so a folder it finds we create
    let norm = |l: &str| l.replace(" (created)", "").trim_end().to_owned();
    let same = ours.iter().zip(&theirs).take_while(|(a, b)| norm(a) == norm(b)).count();
    println!(
        "fs lines: {} of ours, {} in the reference, the first {same} agree",
        ours.len(),
        theirs.len()
    );
    if same < ours.len().min(theirs.len()) {
        println!("  first difference:\n    ours: {}\n    ref:  {}", ours[same], theirs[same]);
        for l in ours.iter().skip(same.saturating_sub(2)).take(12) {
            println!("    ours> {l}");
        }
    }
    for (t, l) in lines.iter().filter(|(t, l)| t == "kernel" && l.starts_with("stage ")) {
        println!("{t}: {l}");
    }
    let stage0 = reference("xbased-native-frontier: ");
    if let Some(s0) = stage0.iter().find(|l| l.starts_with("stage S0")) {
        let ours0 = lines.iter().find(|(_, l)| l.starts_with("stage S0")).map(|(_, l)| l.as_str());
        assert_eq!(ours0, Some(s0.as_str()), "the loader's stage line matches the reference");
    }
    assert!(!nv2a.is_empty(), "the title reached its first NV2A register accesses");
}

fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(from) else { return };
    for e in entries.flatten() {
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if src.is_dir() {
            let _ = std::fs::create_dir_all(&dst);
            copy_tree(&src, &dst);
        } else {
            let _ = std::fs::copy(&src, &dst);
        }
    }
}
