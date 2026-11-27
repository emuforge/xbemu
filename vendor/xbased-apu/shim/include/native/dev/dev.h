/*
 * xbemu: stands in for xbased native/dev/dev.h when building the vendored APU
 * (vendor/xbased-apu). Only what native/dev/apu uses is declared; the
 * functions are implemented in Rust (crates/xb-apu/src/ffi.rs) unless noted.
 * GPL-3.0-or-later, as xbemu.
 */
#ifndef XBEMU_APU_SHIM_DEV_H
#define XBEMU_APU_SHIM_DEV_H

#include <stdint.h>

struct NcVm;
void nc_dev_apu_init(struct NcVm *vm);
void nc_dev_apu_spin_wait(const uint32_t *word);
void nc_dev_apu_time_held(uint64_t ns);

#endif
