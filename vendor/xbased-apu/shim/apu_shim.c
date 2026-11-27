/*
 * xbemu: the two things the vendored APU needs that Rust cannot provide
 * (see PATCHES.md):
 *
 *  - nc_log is variadic (and dsp_rec.cpp declares it itself, so it cannot be
 *    a macro); it is formatted here and handed to Rust as a string.
 *  - xbased never tears the APU down. Stopping the frame thread and freeing
 *    the device needs MCPXAPUState's layout, which only the C headers know.
 *
 * GPL-3.0-or-later, as xbemu.
 */
#include "native/dev/apu/apu_int.h"
#include "native/dev/apu/dsp/dsp.h"

void xb_apu_log(const char *tag, const char *msg);

void nc_log(const char *tag, const char *fmt, ...)
{
    char line[1024];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(line, sizeof(line), fmt, ap);
    va_end(ap);
    xb_apu_log(tag, line);
}

/* The frame thread told to leave its loop; its waits on d->cond woken. */
void xb_apu_shim_request_exit(void)
{
    MCPXAPUState *d = g_state;
    if (d) {
        apu_st_rel(&d->exiting, true);
        pthread_cond_broadcast(&d->cond);
    }
}

/* After xb_apu_shim_request_exit, with the frame thread out of
 * nc_pause_wait: joins it and frees what nc_dev_apu_init made. */
void xb_apu_shim_destroy(void)
{
    MCPXAPUState *d = g_state;
    if (!d) {
        return;
    }
    pthread_join(d->apu_thread, NULL);
    mcpx_apu_vp_finalize(d);
    dsp_destroy(d->gp.dsp);
    dsp_destroy(d->ep.dsp);
    pthread_cond_destroy(&d->cond);
    pthread_mutex_destroy(&d->irq_lock);
    pthread_mutex_destroy(&d->lock);
    g_state = NULL;
    g_apu_ram = NULL;
    g_apu_ram_size = 0;
    free(d);
}

/* The frame thread's waits on d->cond woken (a pause or a resume asked). */
void xb_apu_shim_kick(void)
{
    MCPXAPUState *d = g_state;
    if (d) {
        pthread_cond_broadcast(&d->cond);
    }
}

/* ---- savestates: the buffer state.c writes to and reads from, and the
 * device's state as bytes for Rust ---- */
#include "native/state/sections.h"

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

void nc_rec_put(NcBuf *b, const NcRecord *rec, const void *obj)
{
    nc_buf_put(b, obj, rec->size);
}

void nc_rec_get(NcRd *r, const NcRecord *rec, void *obj, unsigned v)
{
    (void)v;
    nc_rd_get(r, obj, rec->size);
}

/* The device's state (state.c nc_apu_state_save) in a buffer the caller
 * frees with xb_apu_shim_state_free; NULL without a device or memory. The
 * frame thread is held off inside. */
uint8_t *xb_apu_shim_state_save(size_t *len)
{
    NcBuf b = { 0 };
    if (!g_state || !nc_apu_state_save(&b) || b.fail) {
        free(b.p);
        *len = 0;
        return NULL;
    }
    *len = b.len;
    return b.p;
}

void xb_apu_shim_state_free(uint8_t *p)
{
    free(p);
}

/* The device's state replaced (state.c nc_apu_state_load): false if the
 * bytes are not a state this build reads; the device is then reset. */
bool xb_apu_shim_state_load(const uint8_t *p, size_t len)
{
    NcRd r = { p, len, 0, false };
    return g_state && nc_apu_state_load(&r, 1) && !r.fail;
}
