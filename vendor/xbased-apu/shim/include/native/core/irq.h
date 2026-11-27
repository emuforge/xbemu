/*
 * xbemu: stands in for xbased native/core/irq.h when building the vendored APU
 * (vendor/xbased-apu). Only what native/dev/apu uses is declared; the
 * functions are implemented in Rust (crates/xb-apu/src/ffi.rs) unless noted.
 * GPL-3.0-or-later, as xbemu.
 */
#ifndef XBEMU_APU_SHIM_IRQ_H
#define XBEMU_APU_SHIM_IRQ_H

#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

/* The interrupt line (5: the APU's) asserted or deasserted. */
void nc_irq_set(unsigned line, bool level);
/* The frame thread holds off (not started, paused or stopped)... */
bool nc_paused(void);
/* ...and waits here until it may run again (or the APU is being destroyed). */
void nc_pause_wait(void);
void nc_pause_register(const char *name);
/* The frame thread idle in a wait of its own: counts as parked meanwhile. */
void nc_pause_idle(bool on);

#ifdef __cplusplus
}
#endif

#endif
