/*
 * xbemu: stands in for xbased native/core/nc.h when building the vendored APU
 * (vendor/xbased-apu). Only what native/dev/apu uses is declared; the
 * functions are implemented in Rust (crates/xb-apu/src/ffi.rs) unless noted.
 * GPL-3.0-or-later, as xbemu.
 */
#ifndef XBEMU_APU_SHIM_NC_H
#define XBEMU_APU_SHIM_NC_H

#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

#ifdef __cplusplus
extern "C" {
#endif

/* shim/apu_shim.c: formats the line and hands it to Rust (xb_apu_log) */
void nc_log(const char *tag, const char *fmt, ...) __attribute__((format(printf, 2, 3)));

#ifdef __cplusplus
}
#endif

#endif
