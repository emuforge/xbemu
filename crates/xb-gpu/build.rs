//! Builds the vendored renderer (`vendor/xbased-hle`) and the third-party
//! libraries it needs into static libraries and links them.
//!
//! The sources, defines and flags follow the reference's Linux desktop build
//! of the renderer, without the SDL and application parts. Third-party
//! sources come from `XBEMU_DEPS_DIR`, or from `third_party/` at the
//! workspace root (`tools/fetch-deps.sh` fills it).
//!
//! The C is always built optimized: it is the emulator's hot path and a debug
//! build of the Rust workspace must stay playable.

use std::env;
use std::path::{Path, PathBuf};

/// The renderer's sources, relative to `vendor/xbased-hle/hw/xbox/hle`: the
/// reference's list with `core/xbe.c` (the XBE header parser, which the
/// reference builds into its core library).
const HLE_SOURCES: &[&str] = &[
    "hle.c",
    "hitch.c",
    "core/title.c",
    "core/probe.c",
    "core/xbe.c",
    "core/xbe_guest.c",
    "front/arena.c",
    "front/fe.c",
    "front/ring.c",
    "mem/barrier.c",
    "mem/guestmem.c",
    "mem/pageuse.c",
    "pb/kelvin.c",
    "pb/pbdraw.c",
    "pb/pbmode.c",
    "pb/pbres.c",
    "pb/pbstate.c",
    "pb/pusher.c",
    "pb/vimm.c",
    "pb/kvfmt.c",
    "shader/ff_gen.c",
    "shader/ffhw.c",
    "shader/glsl_compile.c",
    "shader/psh.c",
    "shader/uber.c",
    "shader/vsh.c",
    "tex/xdk_res.c",
    "tex/xg_swizzle.c",
    "uma/uma.c",
    "util/thread.c",
    "vk/back.c",
    "vk/display.c",
    "vk/swapchain.c",
    "vk/batch.c",
    "vk/compat.c",
    "vk/draw.c",
    "vk/drvcrash.c",
    "vk/gpl.c",
    "vk/quirks.c",
    "vk/retire.c",
    "vk/gamma.c",
    "vk/guest_ram.c",
    "vk/pcache.c",
    "vk/pipeasync.c",
    "vk/surf.c",
    "vk/timeline.c",
    "vk/vkdev.c",
    "vk/zread.c",
];

/// nv2a_vsh_cpu's sources (the vertex-program interpreter), relative to its
/// `src` directory.
const VSH_SOURCES: &[&str] = &[
    "nv2a_vsh_disassembler.c",
    "nv2a_vsh_cpu.c",
    "nv2a_vsh_emulator.c",
    "nv2a_vsh_emulator_execution_state.c",
];

/// One third-party checkout and a file that proves it is there.
const DEPS: &[(&str, &str)] = &[
    ("Vulkan-Headers", "include/vulkan/vulkan.h"),
    ("volk", "volk.c"),
    ("glslang", "CMakeLists.txt"),
    ("spirv_reflect", "spirv_reflect.c"),
    ("xxhash", "xxhash.h"),
    ("root/subprojects/nv2a_vsh_cpu", "src/nv2a_vsh_cpu.c"),
];

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace = manifest.join("../..");
    let vendor = workspace.join("vendor/xbased-hle");
    let deps = deps_dir(&workspace);
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=XBEMU_DEPS_DIR");
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=CXX");
    rerun_if_tree_changed(&vendor);
    for (dir, probe) in DEPS {
        println!("cargo:rerun-if-changed={}", deps.join(dir).join(probe).display());
    }

    build_glslang(&deps.join("glslang"));

    // Third-party C: volk (the Vulkan loader), SPIRV-Reflect, the vertex
    // program interpreter, and the vendored GLib subset.
    let mut third = c_build(&target_os, &target_arch);
    third
        .define("VK_NO_PROTOTYPES", None)
        .include(deps.join("Vulkan-Headers/include"))
        .include(deps.join("volk"))
        .include(deps.join("spirv_reflect"))
        .include(vendor.join("native/compat/glib"))
        .file(deps.join("volk/volk.c"))
        .file(deps.join("spirv_reflect/spirv_reflect.c"))
        .file(vendor.join("native/compat/glib/glib.c"))
        .warnings(false);
    let vsh = deps.join("root/subprojects/nv2a_vsh_cpu/src");
    for f in VSH_SOURCES {
        third.file(vsh.join(f));
    }
    third.compile("xbased_hle_deps");

    // The renderer.
    let mut hle = c_build(&target_os, &target_arch);
    if target_os == "linux" {
        // Android's <sys/system_properties.h>, which the renderer's debug
        // switches read; `src/ffi.rs` implements it.
        hle.include(vendor.join("native/compat/linux/include"));
    }
    hle.define("CONFIG_VULKAN", "1")
        .define("XBASED_HLE_GPU", "1")
        .define("XBASED_NATIVE_CORE", "1")
        .define("VK_NO_PROTOTYPES", None)
        // Guards every change made to the vendored files (PATCHES.md).
        .define("XBEMU_HOST", "1")
        // What the vendored files include of the reference's core, as far as
        // they need it (PATCHES.md).
        .include(vendor.join("shim/include"))
        .include(&vendor)
        .include(deps.join("root"))
        .include(deps.join("glslang"))
        .include(deps.join("xxhash"))
        .include(vendor.join("native/compat/glib"))
        .include(deps.join("Vulkan-Headers/include"))
        .include(deps.join("volk"))
        .include(deps.join("spirv_reflect"))
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-function")
        .flag_if_supported("-Wno-sign-compare")
        .warnings(false);
    let src = vendor.join("hw/xbox/hle");
    for f in HLE_SOURCES {
        hle.file(src.join(f));
    }
    // The record functions pbstate.c writes its section with (PATCHES.md).
    hle.file(vendor.join("shim/state_rec.c"));
    hle.compile("xbased_hle");

    // glslang is C++.
    match (target_os.as_str(), target_env.as_str()) {
        ("linux", _) | ("android", _) if target_env != "musl" => {
            let cxx = if target_os == "android" { "c++_shared" } else { "stdc++" };
            println!("cargo:rustc-link-lib={cxx}");
        }
        ("macos", _) | ("ios", _) => println!("cargo:rustc-link-lib=c++"),
        _ => {}
    }
    if target_os == "linux" {
        println!("cargo:rustc-link-lib=m");
        println!("cargo:rustc-link-lib=pthread");
        println!("cargo:rustc-link-lib=dl");
    }
}

/// `XBEMU_DEPS_DIR`, or `third_party/` at the workspace root; stops the build
/// with instructions when a library is missing.
fn deps_dir(workspace: &Path) -> PathBuf {
    let dir = env::var_os("XBEMU_DEPS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("third_party"));
    let missing: Vec<&str> = DEPS
        .iter()
        .filter(|(d, probe)| !dir.join(d).join(probe).is_file())
        .map(|(d, _)| *d)
        .collect();
    if !missing.is_empty() {
        panic!(
            "\n\nxb-gpu: third-party sources missing from {}: {}.\n\
             Run tools/fetch-deps.sh (it fetches them into third_party/ at the pinned revisions),\n\
             or set XBEMU_DEPS_DIR to a directory that has them.\n\n",
            dir.display(),
            missing.join(", ")
        );
    }
    dir
}

/// A `cc` build with the flags every C file here shares: the reference's
/// release flags (`-O3 -DNDEBUG -std=gnu11`, its Linux and x86 options).
fn c_build(target_os: &str, target_arch: &str) -> cc::Build {
    let mut b = cc::Build::new();
    let clang = env::var_os("CC").is_none() && target_os != "windows";
    if clang {
        // The renderer uses GNU C extensions clang and GCC share (statement
        // expressions, __atomic builtins); the reference builds it with clang
        // only.
        b.compiler("clang");
    }
    if c_lto(clang) {
        b.flag("-flto=thin");
    }
    b.opt_level(3)
        .debug(false)
        .pic(true)
        .std("gnu11")
        .define("NDEBUG", None)
        .define("XBASED_DIAG", "0")
        .flag_if_supported("-g1")
        .flag_if_supported("-fno-delete-null-pointer-checks");
    if target_os == "linux" || target_os == "android" {
        b.define("_GNU_SOURCE", None);
    }
    if target_os != "android" {
        // Both PC platforms' code paths in the renderer.
        b.define("NC_DESKTOP", "1");
    }
    if target_arch == "x86_64" {
        b.flag_if_supported("-msse4.1").flag_if_supported("-mpopcnt");
    }
    b
}

/// Whether the C here is compiled for link-time optimisation (`-flto=thin`):
/// its objects are then LLVM bitcode, and the linker inlines across the
/// files.
///
/// Only where the link is known to take bitcode: a release build with clang
/// for x86-64 Linux, whose linker since Rust 1.90 is the toolchain's own
/// LLD. `XBEMU_C_LTO=0` turns it off (another linker), `=1` asks for it
/// elsewhere.
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

/// glslang (the GLSL to SPIR-V compiler) through its own CMake build, with
/// the options `hle.cmake` sets.
fn build_glslang(src: &Path) {
    let mut cfg = cmake::Config::new(src);
    cfg.profile("Release")
        .define("ENABLE_OPT", "OFF")
        .define("ENABLE_HLSL", "OFF")
        .define("ENABLE_GLSLANG_BINARIES", "OFF")
        .define("ENABLE_SPVREMAPPER", "OFF")
        .define("ENABLE_CTEST", "OFF")
        .define("GLSLANG_TESTS", "OFF")
        .define("GLSLANG_ENABLE_INSTALL", "ON")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("BUILD_EXTERNAL", "OFF")
        .define("CMAKE_POSITION_INDEPENDENT_CODE", "ON")
        .define("CMAKE_INSTALL_LIBDIR", "lib");
    if env::var_os("CC").is_none() && env::var_os("CXX").is_none() && !cfg!(windows) {
        cfg.define("CMAKE_C_COMPILER", "clang").define("CMAKE_CXX_COMPILER", "clang++");
    }
    let dst = cfg.build();
    println!("cargo:rustc-link-search=native={}", dst.join("lib").display());
    for lib in ["glslang", "SPIRV", "glslang-default-resource-limits"] {
        if dst.join("lib").join(format!("lib{lib}.a")).is_file()
            || dst.join("lib").join(format!("{lib}.lib")).is_file()
        {
            println!("cargo:rustc-link-lib=static={lib}");
        }
    }
    // Older glslang trees split these out of libglslang.
    for lib in ["MachineIndependent", "GenericCodeGen", "OSDependent"] {
        if dst.join("lib").join(format!("lib{lib}.a")).is_file() {
            println!("cargo:rustc-link-lib=static={lib}");
        }
    }
}

/// Emits `rerun-if-changed` for every file under `dir`.
fn rerun_if_tree_changed(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        panic!("xb-gpu: {} is missing (the vendored renderer)", dir.display());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rerun_if_tree_changed(&path);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
