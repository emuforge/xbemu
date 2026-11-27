/*
 * xbemu: stands in for xbased native/core/vm.h when building the vendored APU
 * (vendor/xbased-apu). Only what native/dev/apu uses is declared; the
 * functions are implemented in Rust (crates/xb-apu/src/ffi.rs) unless noted.
 * GPL-3.0-or-later, as xbemu.
 */
#ifndef XBEMU_APU_SHIM_VM_H
#define XBEMU_APU_SHIM_VM_H

#include <stdint.h>

/* The two fields nc_dev_apu_init reads; mirrored in ffi.rs. */
typedef struct NcVm {
    uint8_t *phys;      /* host view of physical RAM (never protected) */
    uint64_t ram_size;
} NcVm;

#endif
