//! The window's Vulkan surface, as the core's renderer asks for it.
//!
//! The renderer creates the Vulkan instance and device; the front end only
//! says which instance extensions its window system needs and, when asked,
//! creates a surface for its window on the renderer's instance. `ash-window`
//! does both for Wayland and X11 (and the other window systems `winit`
//! knows).
//!
//! macOS is different in one way: AppKit views may only be touched on the
//! main thread, and `winit` hands out the window's handle only there, while
//! the renderer asks for the surface on its presenting thread. The window's
//! CAMetalLayer is therefore made when the window is handed over (on the
//! main thread, in the `macos` module), and the surface is made from the layer, which
//! `VK_EXT_metal_surface` allows from any thread.
//!
//! This is the one module of the app with `unsafe`: Vulkan calls and the
//! `unsafe fn` of [`SurfaceProvider`] cannot be written without it.
#![expect(unsafe_code, reason = "Vulkan surface creation through ash")]

use std::ffi::CStr;
use std::sync::{Arc, OnceLock};

use ash::vk::{self, Handle};
use winit::raw_window_handle::RawDisplayHandle;
#[cfg(not(target_os = "macos"))]
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::Window;
use xb_core::{SurfaceProvider, VkInstance, VkSurfaceKHR};

/// Gives the core a surface for the window, once the window exists.
///
/// The provider holds the window for as long as the core holds the provider,
/// so the window cannot be destroyed under a surface the core still uses.
pub struct WindowSurface {
    extensions: Vec<&'static CStr>,
    window: OnceLock<Arc<Window>>,
    /// The window's CAMetalLayer, made on the main thread.
    #[cfg(target_os = "macos")]
    layer: OnceLock<macos::MetalLayer>,
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
        Ok(Self {
            extensions,
            window: OnceLock::new(),
            #[cfg(target_os = "macos")]
            layer: OnceLock::new(),
        })
    }

    /// Sets the window surfaces are made for. Only the first call counts.
    /// Called on the main thread.
    pub fn set_window(&self, window: Arc<Window>) {
        #[cfg(target_os = "macos")]
        if self.layer.get().is_none() {
            match macos::MetalLayer::of(&window) {
                Some(layer) => {
                    let _ = self.layer.set(layer);
                }
                None => {
                    log::error!(target: "app", "the window has no AppKit view for a Metal layer")
                }
            }
        }
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
        let entry = match unsafe { load_entry() } {
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
        #[cfg(target_os = "macos")]
        {
            let _ = window;
            let layer = self.layer.get()?;
            // SAFETY: the instance was created with VK_EXT_metal_surface
            // (`instance_extensions`) and the layer lives as long as `self`.
            unsafe { layer.create_surface(&entry, &instance) }
        }
        #[cfg(not(target_os = "macos"))]
        {
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
}

/// Loads the Vulkan library the renderer uses: the system's loader, or on
/// macOS the first of `xb_core::vulkan_libraries` that loads (the MoltenVK
/// an app bundle carries comes first). The surface has to come from the
/// library the renderer's instance came from.
///
/// # Safety
/// As [`ash::Entry::load`]: the library's initialization must be sound.
unsafe fn load_entry() -> Result<ash::Entry, ash::LoadingError> {
    #[cfg(target_os = "macos")]
    for path in xb_core::vulkan_libraries() {
        if path.is_absolute() && !path.exists() {
            continue;
        }
        // SAFETY: the caller's contract.
        if let Ok(entry) = unsafe { ash::Entry::load_from(&path) } {
            return Ok(entry);
        }
    }
    // SAFETY: the caller's contract.
    unsafe { ash::Entry::load() }
}

#[cfg(target_os = "macos")]
mod macos {
    //! The window's CAMetalLayer and the Vulkan surface made from it.

    use super::{Handle, VkSurfaceKHR, Window, vk};
    use ash::ext::metal_surface;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    /// A retained CAMetalLayer attached to the window's view.
    pub(super) struct MetalLayer(std::ptr::NonNull<std::ffi::c_void>);

    // SAFETY: the layer is only passed to vkCreateMetalSurfaceEXT, which
    // may be called from any thread; nothing here messages it.
    unsafe impl Send for MetalLayer {}
    // SAFETY: as above.
    unsafe impl Sync for MetalLayer {}

    impl MetalLayer {
        /// The layer of `window`'s view, made if it has none. Main thread
        /// only (winit refuses the handle elsewhere).
        pub(super) fn of(window: &Window) -> Option<Self> {
            let RawWindowHandle::AppKit(h) = window.window_handle().ok()?.as_raw() else {
                return None;
            };
            // SAFETY: `h` is the live window's AppKit handle, used on the
            // main thread.
            let layer = unsafe { raw_window_metal::appkit::metal_layer_from_handle(h) };
            let ptr = match layer {
                raw_window_metal::Layer::Existing(p) | raw_window_metal::Layer::Allocated(p) => p,
            };
            std::ptr::NonNull::new(ptr.cast()).map(Self)
        }

        /// A Vulkan surface on this layer.
        ///
        /// # Safety
        /// `instance` is live and has VK_EXT_metal_surface enabled.
        pub(super) unsafe fn create_surface(
            &self,
            entry: &ash::Entry,
            instance: &ash::Instance,
        ) -> Option<VkSurfaceKHR> {
            let info = vk::MetalSurfaceCreateInfoEXT::default().layer(self.0.as_ptr().cast());
            let f = metal_surface::Instance::new(entry, instance);
            // SAFETY: the caller's contract; `info` points at a live layer.
            match unsafe { f.create_metal_surface(&info, None) } {
                Ok(s) => Some(s.as_raw()),
                Err(e) => {
                    log::error!(target: "app", "cannot create the Metal surface: {e}");
                    None
                }
            }
        }
    }
}
