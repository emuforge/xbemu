//! Builds the vendored MCPX APU (`vendor/xbased-apu`) into one static library.
//!
//! The flags follow the reference's Linux desktop build of this library:
//! `-O3 -DNDEBUG`, gnu11 / gnu++20, SSE4.1 and POPCNT on x86-64,
//! `NC_DESKTOP`. Left out: SDL (the output is Rust's) and the renderer's
//! defines the APU never reads. The C is optimized whatever the cargo profile: the DSP and the voice mixer run
//! 1500 times a second.

use std::env;
use std::path::{Path, PathBuf};

/// Functions the APU calls outside itself, renamed so that the Rust
/// definitions (`src/ffi.rs`) cannot collide with another crate's (the GPU
/// renderer's host interface also has an `hle_host_mem_set_dirty`).
const RENAMES: &[(&str, &str)] = &[
    ("nc_log", "xb_apu_nc_log"),
    ("nc_irq_set", "xb_apu_irq_set"),
    ("nc_paused", "xb_apu_paused"),
    ("nc_pause_wait", "xb_apu_pause_wait"),
    ("nc_pause_register", "xb_apu_pause_register"),
    ("nc_pause_idle", "xb_apu_pause_idle"),
    ("nc_mmio_register", "xb_apu_mmio_register"),
    ("nc_mmio_retry", "xb_apu_mmio_retry"),
    ("nc_ke_now_ns", "xb_apu_guest_now_ns"),
    ("hle_host_mem_set_dirty", "xb_apu_ram_written"),
    // state.c's buffer and record functions (shim/apu_shim.c): the renderer
    // has the reference's own under these names.
    ("nc_buf_put", "xb_apu_buf_put"),
    ("nc_rd_get", "xb_apu_rd_get"),
    ("nc_rd_ptr", "xb_apu_rd_ptr"),
    ("nc_rec_put", "xb_apu_rec_put"),
    ("nc_rec_get", "xb_apu_rec_get"),
    ("nc_apu_state_save", "xb_apu_state_save_c"),
    ("nc_apu_state_load", "xb_apu_state_load_c"),
];

const C_SOURCES: &[&str] = &[
    "native/dev/apu/apu.c",
    "native/dev/apu/dsp/debug.c",
    "native/dev/apu/dsp/dsp.c",
    "native/dev/apu/dsp/dsp_cpu.c",
    "native/dev/apu/dsp/dsp_dma.c",
    "native/dev/apu/dsp/gp_ep.c",
    "native/dev/apu/state.c",
    "native/dev/apu/vp/resample.c",
    "native/dev/apu/vp/vp.c",
    "shim/apu_shim.c",
];

const ZYDIS_SOURCES: &[&str] = &[
    "third_party/zydis/src/Encoder.c",
    "third_party/zydis/src/EncoderData.c",
    "third_party/zydis/src/Mnemonic.c",
    "third_party/zydis/src/Register.c",
    "third_party/zydis/src/SharedData.c",
];

/// Whether the C here is compiled for link-time optimisation, as
/// `xb-gpu`'s build decides it (the same rule and the same switch,
/// `XBEMU_C_LTO`; build scripts share no code).
fn c_lto(compiler_is_default_clang: bool) -> bool {
    println!("cargo:rerun-if-env-changed=XBEMU_C_LTO");
    match env::var("XBEMU_C_LTO").as_deref() {
        Ok("0") => false,
        Ok("1") => true,
        _ => {
            compiler_is_default_clang
                && env::var("PROFILE").as_deref() == Ok("release")
                && env::var("TARGET").as_deref() == Ok("x86_64-unknown-linux-gnu")
        }
    }
}

/// A build with the reference's common flags. clang unless `CC`/`CXX` say
/// otherwise (the reference is built and tested with clang only).
fn base(vendor: &Path, cpp: bool, x86_64: bool) -> cc::Build {
    let mut b = cc::Build::new();
    let (var, default) = if cpp { ("CXX", "clang++") } else { ("CC", "clang") };
    let clang = env::var_os(var).is_none()
        && env::var_os(format!("{var}_{}", env::var("TARGET").unwrap())).is_none();
    if clang {
        b.compiler(default);
    }
    if c_lto(clang) {
        b.flag("-flto=thin");
    }
    b.cpp(cpp)
        .opt_level(3)
        .debug(false)
        .warnings(false)
        .define("NDEBUG", None)
        .define("_GNU_SOURCE", None)
        .define("NC_DESKTOP", "1")
        .define("XBEMU_HOST", "1")
        .define("XBASED_NATIVE_CORE", "1")
        .define("ZYDIS_STATIC_BUILD", None)
        .define("ZYCORE_STATIC_BUILD", None)
        .flag("-fno-delete-null-pointer-checks")
        .flag(if cpp { "-std=gnu++20" } else { "-std=gnu11" })
        .include(vendor.join("shim/include"))
        .include(vendor);
    if x86_64 {
        b.flag("-msse4.1").flag("-mpopcnt");
    }
    for (from, to) in RENAMES {
        b.define(from, *to);
    }
    b
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest.join("../../vendor/xbased-apu");
    let x86_64 = env::var("CARGO_CFG_TARGET_ARCH").unwrap() == "x86_64";
    let recompiler = x86_64 && env::var_os("CARGO_FEATURE_DSP_RECOMPILER").is_some();

    println!("cargo:rerun-if-changed=build.rs");
    for dir in ["native", "shim", "third_party"] {
        println!("cargo:rerun-if-changed={}", vendor.join(dir).display());
    }
    println!("cargo:rustc-check-cfg=cfg(xb_apu_recompiler)");

    let mut lib = base(&vendor, false, x86_64);
    lib.files(C_SOURCES.iter().map(|f| vendor.join(f)));

    if recompiler {
        println!("cargo:rustc-cfg=xb_apu_recompiler");
        let zydis = vendor.join("third_party/zydis");

        // The recompiler (C++) and the x86-64 encoder it emits code with go
        // into the same archive as the C: the three reference one another.
        let mut rec = base(&vendor, true, x86_64);
        rec.include(zydis.join("include"))
            .include(zydis.join("zycore/include"))
            .file(vendor.join("native/dev/apu/dsp/dsp_rec.cpp"));
        lib.objects(rec.compile_intermediates());

        let mut enc = base(&vendor, false, x86_64);
        enc.define("ZYDIS_DISABLE_DECODER", None)
            .define("ZYDIS_DISABLE_FORMATTER", None)
            .include(zydis.join("include"))
            .include(zydis.join("zycore/include"))
            .include(zydis.join("src"))
            .files(ZYDIS_SOURCES.iter().map(|f| vendor.join(f)));
        lib.objects(enc.compile_intermediates());

        let os = env::var("CARGO_CFG_TARGET_OS").unwrap();
        let cxx = if os == "macos" || os == "ios" { "c++" } else { "stdc++" };
        println!("cargo:rustc-link-lib={cxx}");
    }

    lib.compile("xbased_apu");
    println!("cargo:rustc-link-lib=m");
    println!("cargo:rustc-link-lib=pthread");
}
