/*
 * xbased HLE GPU � the window the presenter draws into (vk/swapchain.c):
 * Android's ANativeWindow, or on Windows the front end's window as the
 * front end keeps it (native/app/win_main.c: its HWND, its size in pixels
 * as the last resize left it).
 */
#ifndef HW_XBOX_HLE_VK_NATIVE_WINDOW_H
#define HW_XBOX_HLE_VK_NATIVE_WINDOW_H

#ifdef __ANDROID__
#include <android/native_window.h>
#else
#include <stdint.h>

struct ANativeWindow {
    void *hinstance;
    void *hwnd;
    void *sdl_window;   /* Linux: the SDL window (SDL_Vulkan_CreateSurface) */
    volatile int32_t width, height;
};

#if defined(XBEMU_HOST) && defined(NC_DESKTOP) && !defined(_WIN32)
/* xbemu: the window's surface comes from the front end through the host
 * (crates/xb-gpu), not from SDL. The instance extensions the surface needs
 * (NULL: none, a run without a window), and the surface made on inst (a
 * VkInstance) into *surf (a VkSurfaceKHR), which the presenter then owns. */
#include <stdbool.h>
const char *const *hle_host_surface_extensions(uint32_t *count);
bool hle_host_surface_create(void *inst, void *surf);
#endif

static inline int32_t ANativeWindow_getWidth(struct ANativeWindow *w)
{
    return w->width;
}

static inline int32_t ANativeWindow_getHeight(struct ANativeWindow *w)
{
    return w->height;
}
#endif

#endif
