/*
 * xbemu: xbased's native/state/state.h includes its core's native/core/nc.h.
 * The renderer's savestate file (hw/xbox/hle/pb/pbstate.c) needs only the
 * log from it.
 *
 * GPL-3.0-or-later, as xbemu.
 */
#ifndef XBEMU_SHIM_NATIVE_CORE_NC_H
#define XBEMU_SHIM_NATIVE_CORE_NC_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

void nc_log(const char *tag, const char *fmt, ...);

#endif
