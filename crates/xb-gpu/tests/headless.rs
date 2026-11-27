//! Starts the renderer on this machine's Vulkan device through the public
//! API with a mock machine, drives a few register accesses, and shuts it
//! down. One test: a process can create one `Gpu`.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use xb_gpu::regs::reg;
use xb_gpu::{FatalKind, Gpu, GpuConfig, GpuError, ShaderMode};
use xb_types::irq::{Irq, IrqHub};
use xb_types::settings::Settings;

#[test]
fn the_renderer_starts_on_the_host_gpu_and_shuts_down_cleanly() {
    common::init_log();
    let host = Arc::new(common::MockHost::new());
    let irq = Arc::new(IrqHub::new());
    let cache = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("xb-gpu-headless");
    let _ = std::fs::remove_dir_all(&cache);
    let config = GpuConfig {
        shaders: ShaderMode::Hybrid,
        ..GpuConfig::from_settings(&Settings::default(), &cache)
    };

    // SAFETY: the mock's RAM is a leaked allocation of RAM_SIZE bytes.
    let gpu = unsafe {
        Gpu::new(host.clone(), host.ram(), common::RAM_SIZE, irq.clone(), config.clone(), None)
    }
    .expect("the first GPU of the process");
    assert!(cache.is_dir(), "the cache directory is created");
    // SAFETY: as above.
    let second =
        unsafe { Gpu::new(host.clone(), host.ram(), common::RAM_SIZE, irq.clone(), config, None) };
    assert!(matches!(second, Err(GpuError::AlreadyCreated)));

    // The register front end works before any title.
    assert_eq!(gpu.mmio_read(reg::PFB_CSTATUS, 4), common::RAM_SIZE);
    gpu.mmio_write(reg::PCRTC_INTR_EN_0, 4, 1);
    gpu.mmio_write(reg::PMC_INTR_EN_0, 4, 1);
    let t0 = Instant::now();
    while irq.pending() & Irq::Gpu.mask() == 0 && t0.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_ne!(
        irq.pending() & Irq::Gpu.mask(),
        0,
        "the vblank thread raises the line within a frame or two"
    );
    gpu.mmio_write(reg::PCRTC_INTR_EN_0, 4, 0);
    assert_eq!(irq.pending() & Irq::Gpu.mask(), 0);
    assert!(!gpu.presenter().present(640, 480), "nothing to present before the renderer is up");

    match gpu.title_start() {
        Err(fatal) if fatal.kind == FatalKind::Driver => {
            println!("SKIPPED: no usable Vulkan device on this machine: {fatal}");
            assert_eq!(gpu.fatal(), Some(fatal));
            gpu.shutdown();
            return;
        }
        Err(fatal) => panic!("the title did not start: {fatal:?}"),
        Ok(info) => {
            assert_eq!(info.title_id, common::TITLE_ID);
            assert_eq!(info.timedate, common::TIMEDATE);
        }
    }
    assert_eq!(gpu.fatal(), None);
    assert!(host.kicks.load(Ordering::Relaxed) >= 1, "title start kicks the guest CPU");

    // The renderer took channel 0: the channel registers are CACHE1's, the
    // decoder set PGRAPH_FIFO, and an (empty) kick is consumed.
    assert_eq!(gpu.mmio_read(reg::PGRAPH_FIFO, 4), 1);
    gpu.mmio_write(reg::USER_DMA_PUT, 4, 0);
    assert_eq!(gpu.mmio_read(reg::USER_DMA_PUT, 4), 0);
    assert_eq!(
        gpu.mmio_read(reg::PFIFO_CACHE1_STATUS, 1),
        reg::PFIFO_LOW_MARK,
        "an empty FIFO is idle"
    );
    gpu.av_set_display_mode(0x0400_0801, 0x12, 640 * 4, 0);

    // Pausing stops the vblank; resuming restarts it.
    gpu.set_paused(true);
    std::thread::sleep(Duration::from_millis(40));
    let held = gpu.vblank_count();
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(gpu.vblank_count(), held, "no vblank while paused");
    gpu.set_paused(false);
    std::thread::sleep(Duration::from_millis(60));
    assert!(gpu.vblank_count() > held, "vblanks resume");

    let stats = gpu.stats();
    assert!(stats.vblanks > 0);
    assert_eq!(stats.frames, 0, "no frame without a push buffer");
    // Without a window there is no swapchain to present into.
    assert!(!gpu.presenter().present(640, 480));
    assert!(
        cache.join("hle_pipelines").is_dir(),
        "the renderer keeps its caches under the configured directory"
    );

    gpu.shutdown();
    gpu.shutdown();
    assert_eq!(gpu.fatal(), None);
    assert!(!gpu.presenter().present(640, 480));
    let after = gpu.vblank_count();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(gpu.vblank_count(), after, "the vblank thread is gone");
}
