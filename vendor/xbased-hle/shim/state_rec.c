/*
 * xbemu: the byte buffer, the reader and the record functions that
 * native/state/state.h declares, for the renderer's savestate section
 * (hw/xbox/hle/pb/pbstate.c, compiled as it is).
 *
 * Taken from xbased 1.2.5 native/state/state.c (GPLv3+), which also holds
 * the state file's container, its compression and its checks: xbemu's
 * container is Rust's (crates/xb-types/src/state.rs), so only these
 * functions are here, unchanged but for nc_state_trace (always off).
 */
#include "native/state/state.h"

#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>

void hle_log(const char *tag, const char *fmt, ...);

void nc_log(const char *tag, const char *fmt, ...)
{
    char line[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(line, sizeof(line), fmt, ap);
    va_end(ap);
    hle_log(tag, "%s", line);
}

void nc_buf_put(NcBuf *b, const void *p, size_t n)
{
    if (b->fail) {
        return;
    }
    if (b->len + n > b->cap) {
        size_t cap = b->cap ? b->cap * 2 : 1 << 16;
        while (cap < b->len + n) {
            cap *= 2;
        }
        uint8_t *q = realloc(b->p, cap);
        if (!q) {
            b->fail = true;
            return;
        }
        b->p = q;
        b->cap = cap;
    }
    memcpy(b->p + b->len, p, n);
    b->len += n;
}

void nc_buf_free(NcBuf *b)
{
    free(b->p);
    memset(b, 0, sizeof(*b));
}

bool nc_rd_get(NcRd *r, void *out, size_t n)
{
    if (r->fail || n > r->len - r->pos) {
        r->fail = true;
        memset(out, 0, n);
        return false;
    }
    memcpy(out, r->p + r->pos, n);
    r->pos += n;
    return true;
}

const void *nc_rd_ptr(NcRd *r, size_t n)
{
    if (r->fail || n > r->len - r->pos) {
        r->fail = true;
        return NULL;
    }
    const void *p = r->p + r->pos;
    r->pos += n;
    return p;
}

/* ---- records -------------------------------------------------------------- */

void nc_rec_put(NcBuf *b, const NcRecord *rec, const void *obj)
{
    const uint8_t *o = obj;
    for (unsigned i = 0; i < rec->nfields; i++) {
        const NcField *f = &rec->fields[i];
        if (f->until) {
            continue;                   /* no longer stored */
        }
        if (f->type == NC_FT_REC) {
            nc_recs_put(b, f->rec, o + f->offset, f->esize, f->count);
        } else {
            nc_buf_put(b, o + f->offset, (size_t)f->esize * f->count);
        }
    }
}

void nc_recs_put(NcBuf *b, const NcRecord *rec, const void *obj, size_t stride, unsigned n)
{
    for (unsigned i = 0; i < n; i++) {
        nc_rec_put(b, rec, (const uint8_t *)obj + i * stride);
    }
}

/* A field the version stored and this core dropped: its bytes, skipped. */
static void skip_field(NcRd *r, const NcField *f, unsigned v);

static void skip_rec(NcRd *r, const NcRecord *rec, unsigned v)
{
    for (unsigned i = 0; i < rec->nfields; i++) {
        const NcField *f = &rec->fields[i];
        if (v >= f->since && (!f->until || v < f->until)) {
            skip_field(r, f, v);
        }
    }
}

static void skip_field(NcRd *r, const NcField *f, unsigned v)
{
    if (f->type == NC_FT_REC) {
        for (unsigned k = 0; k < f->count; k++) {
            skip_rec(r, f->rec, v);
        }
    } else {
        nc_rd_ptr(r, (size_t)f->esize * f->count);
    }
}

void nc_rec_get(NcRd *r, const NcRecord *rec, void *obj, unsigned v)
{
    uint8_t *o = obj;
    for (unsigned i = 0; i < rec->nfields; i++) {
        const NcField *f = &rec->fields[i];
        const bool stored = v >= f->since && (!f->until || v < f->until);
        if (!stored) {
            continue;                   /* not in version v: the default kept */
        }
        if (f->until) {
            skip_field(r, f, v);        /* stored then, dropped since */
        } else if (f->type == NC_FT_REC) {
            nc_recs_get(r, f->rec, o + f->offset, f->esize, f->count, v);
        } else {
            nc_rd_get(r, o + f->offset, (size_t)f->esize * f->count);
            if (f->type == NC_FT_BOOL) {
                /* a bool holds 0 or 1: any other byte of a damaged file is
                 * true, not a value the compiled code never expects in one */
                uint8_t *b = o + f->offset;
                for (size_t n = 0; n < (size_t)f->esize * f->count; n++) {
                    b[n] = b[n] != 0;
                }
            }
        }
    }
}

void nc_recs_get(NcRd *r, const NcRecord *rec, void *obj, size_t stride, unsigned n, unsigned v)
{
    for (unsigned i = 0; i < n; i++) {
        nc_rec_get(r, rec, (uint8_t *)obj + i * stride, v);
    }
}
