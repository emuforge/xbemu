/*
 * xbemu: xbased's native/state/def.h turns a .def file into record
 * descriptions. The APU's state.c has one record of its own, NcApuState (a
 * version number), described here by hand.
 *
 * GPL-3.0-or-later, as xbemu.
 */
#ifndef XBEMU_SHIM_NATIVE_STATE_DEF_H
#define XBEMU_SHIM_NATIVE_STATE_DEF_H

#include "native/state/state.h"

static const NcRecord nc_rec_NcApuState = { sizeof(int32_t) };

#endif
