/*
 * xbemu: what the vendored APU's state.c needs of xbased's native/state
 * (its byte buffer and reader), so that file compiles as it is. The
 * functions are in shim/apu_shim.c.
 *
 * GPL-3.0-or-later, as xbemu.
 */
#ifndef XBEMU_SHIM_NATIVE_STATE_STATE_H
#define XBEMU_SHIM_NATIVE_STATE_STATE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef struct NcBuf {
    uint8_t *p;
    size_t len, cap;
    bool fail;                  /* out of memory: everything after is dropped */
} NcBuf;

typedef struct NcRd {
    const uint8_t *p;
    size_t len, pos;
    bool fail;                  /* read past the end: everything after reads 0 */
} NcRd;

void nc_buf_put(NcBuf *b, const void *p, size_t n);
static inline void nc_buf_u8(NcBuf *b, uint8_t v) { nc_buf_put(b, &v, 1); }
static inline void nc_buf_u32(NcBuf *b, uint32_t v) { nc_buf_put(b, &v, 4); }

bool nc_rd_get(NcRd *r, void *out, size_t n);
const void *nc_rd_ptr(NcRd *r, size_t n);
static inline uint8_t nc_rd_u8(NcRd *r) { uint8_t v = 0; nc_rd_get(r, &v, 1); return v; }
static inline uint32_t nc_rd_u32(NcRd *r) { uint32_t v = 0; nc_rd_get(r, &v, 4); return v; }

/* A record: here only its size, written and read as it is in memory. */
typedef struct NcRecord {
    size_t size;
} NcRecord;
void nc_rec_put(NcBuf *b, const NcRecord *rec, const void *obj);
void nc_rec_get(NcRd *r, const NcRecord *rec, void *obj, unsigned v);

#endif
