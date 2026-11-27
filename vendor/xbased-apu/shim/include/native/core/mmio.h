/*
 * xbemu: stands in for xbased native/core/mmio.h when building the vendored APU
 * (vendor/xbased-apu). Only what native/dev/apu uses is declared; the
 * functions are implemented in Rust (crates/xb-apu/src/ffi.rs) unless noted.
 * GPL-3.0-or-later, as xbemu.
 */
#ifndef XBEMU_APU_SHIM_MMIO_H
#define XBEMU_APU_SHIM_MMIO_H

#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Layout as xbased's (native/core/mmio.h); mirrored in ffi.rs. */
typedef struct NcMmioDev {
    const char *name;
    uint32_t base, size;
    /* off is relative to base; size is 1, 2, 4 or 8 bytes. */
    uint64_t (*read)(void *opaque, uint32_t off, unsigned size);
    void (*write)(void *opaque, uint32_t off, uint64_t value, unsigned size);
    void *opaque;
} NcMmioDev;

/* Rust keeps the handlers and calls them from Apu::mmio_read / mmio_write. */
void nc_mmio_register(const NcMmioDev *dev);
/* A write handler unable to complete yet (a voice's lock held by the frame
 * thread): true if the caller will run the guest's store again
 * (Apu::try_mmio_write), false if it must complete as it is. */
bool nc_mmio_retry(void);

#ifdef __cplusplus
}
#endif

#endif
