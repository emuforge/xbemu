/*
 * xbemu: the APU's two savestate functions (state.c), as xbased's
 * native/state/sections.h declares them.
 *
 * GPL-3.0-or-later, as xbemu.
 */
#ifndef XBEMU_SHIM_NATIVE_STATE_SECTIONS_H
#define XBEMU_SHIM_NATIVE_STATE_SECTIONS_H

#include "native/state/state.h"

bool nc_apu_state_save(NcBuf *b);
bool nc_apu_state_load(NcRd *r, unsigned v);

#endif
