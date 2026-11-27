//! The crate builds and links: every `host.h` function this crate exports,
//! and the renderer entry points it calls, resolve in the final binary.

#[test]
fn every_entry_point_resolves_to_code() {
    let symbols = xb_gpu::ffi::exported_symbols();
    // host.h declares 35 functions outside its diagnostic build; this tree
    // adds three hooks.
    assert!(symbols.len() >= 38, "{} symbols", symbols.len());
    for (name, addr) in &symbols {
        assert_ne!(*addr, 0, "{name} is not linked");
    }
    let mut addrs: Vec<usize> = symbols.iter().map(|s| s.1).collect();
    addrs.sort_unstable();
    addrs.dedup();
    assert_eq!(addrs.len(), symbols.len(), "every entry point is its own function");
}

#[test]
fn without_a_gpu_the_host_interface_answers_as_before_boot() {
    // This test binary never creates a `Gpu`, so the exported functions see
    // no machine: no memory, everything untracked, registers read as 0.
    let mut word = 0u32;
    // SAFETY: `word` is valid for 4 bytes.
    let rc = unsafe { xb_gpu::ffi::hle_va_rw(0x0001_0000, (&raw mut word).cast(), 4, false) };
    assert_eq!(rc, -1);
    assert!(xb_gpu::ffi::hle_host_mem_dirty(0x1000));
    assert!(!xb_gpu::ffi::hle_host_mem_test_and_clear(0x1000, 0x1000));
    assert_eq!(xb_gpu::ffi::hle_host_gpu_reg(0x40_0100), 0);
    assert_eq!(xb_gpu::ffi::hle_host_render_scale(), 1);
    assert!(xb_gpu::ffi::hle_host_cpu().is_null());
}
