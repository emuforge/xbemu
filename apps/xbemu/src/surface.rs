//! The window's Vulkan surface, as the core's renderer asks for it.
//!
//! The renderer creates the Vulkan instance and device; the front end only
//! says which instance extensions its window system needs and, when asked,
//! creates a surface for its window on the renderer's instance. `ash-window`
//! does both for Wayland and X11 (and the other window systems `winit`
//! knows).
//!
//! This is the one module of the app with `unsafe`: Vulkan calls and the
//! `unsafe fn` of [`SurfaceProvider`] cannot be written without it.
#![expect(unsafe_code, reason = "Vulkan surface creation through ash")]

use std::ffi::CStr;
use std::sync::{Arc, OnceLock};

use ash::vk::{self, Handle};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle};
use winit::window::Window;
use xb_core::{SurfaceProvider, VkInstance, VkSurfaceKHR};

/// Gives the core a surface for the window, once the window exists.
///
/// The provider holds the window for as long as the core holds the provider,
/// so the window cannot be destroyed under a surface the core still uses.
pub struct WindowSurface {
    extensions: Vec<&'static CStr>,
    window: OnceLock<Arc<Window>>,
}

impl WindowSurface {
    /// A provider for a window that will be created on `display`.
    ///
    /// # Errors
    /// The window system has no Vulkan surface support `ash-window` knows.
    pub fn new(display: RawDisplayHandle) -> Result<Self, String> {
        let names = ash_window::enumerate_required_extensions(display)
            .map_err(|e| format!("no Vulkan surface extensions for this display: {e}"))?;
        let extensions = names
            .iter()
            // SAFETY: `ash_window` returns pointers to its own static,
            // NUL-terminated extension names.
            .map(|&name| unsafe { CStr::from_ptr(name) })
            .collect();
        Ok(Self { extensions, window: OnceLock::new() })
    }

    /// Sets the window surfaces are made for. Only the first call counts.
    pub fn set_window(&self, window: Arc<Window>) {
        let _ = self.window.set(window);
    }

    /// The window, once it has been created.
    pub fn window(&self) -> Option<&Arc<Window>> {
        self.window.get()
    }
}

impl SurfaceProvider for WindowSurface {
    fn instance_extensions(&self) -> Vec<&'static CStr> {
        self.extensions.clone()
    }

    unsafe fn create_surface(&self, instance: VkInstance) -> Option<VkSurfaceKHR> {
        let window = self.window.get()?;
        // SAFETY: loads the system's Vulkan loader, the same library the
        // renderer's instance came from; nothing else is initialized by it.
        let entry = match unsafe { ash::Entry::load() } {
            Ok(entry) => entry,
            Err(e) => {
                log::error!(target: "app", "cannot load the Vulkan loader: {e}");
                return None;
            }
        };
        // SAFETY: `instance` is the renderer's live instance (the trait's
        // contract); this only loads its function pointers.
        let instance = unsafe {
            ash::Instance::load(entry.static_fn(), vk::Instance::from_raw(instance as u64))
        };
        let display = window.display_handle().ok()?.as_raw();
        let handle = window.window_handle().ok()?.as_raw();
        // SAFETY: the handles are the live window's (the provider keeps it
        // alive), and the instance was created with the extensions
        // `instance_extensions` returned.
        match unsafe { ash_window::create_surface(&entry, &instance, display, handle, None) } {
            Ok(surface) => Some(surface.as_raw()),
            Err(e) => {
                log::error!(target: "app", "cannot create the window's Vulkan surface: {e}");
                None
            }
        }
    }
}
