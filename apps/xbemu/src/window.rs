//! The window and the main thread's event loop (`winit`).
//!
//! The event loop does no emulation: it creates the window, tells the
//! emulation thread what it needs to know through [`Shared`] (the window's
//! size, the keys held, requests) and handles the app's few hotkeys.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::input;
use crate::surface::WindowSurface;

/// What the event loop and the emulation thread tell each other. All of it
/// is single values where the latest wins, so atomics do.
#[derive(Default)]
pub struct Shared {
    /// The window's inner width in pixels; 0 before it exists.
    pub width: AtomicU32,
    /// The window's inner height in pixels.
    pub height: AtomicU32,
    /// The bound keys held ([`input::key_bit`] mask).
    pub keys: AtomicU32,
    /// The emulation thread should shut the core down and end.
    pub stop: AtomicBool,
    /// F1 or Escape was pressed: open the core's menu, or close it.
    pub menu_toggle: AtomicBool,
    /// F12 was pressed: save a screenshot.
    pub screenshot: AtomicBool,
    /// F2: save a state to the current slot.
    pub state_save: AtomicBool,
    /// F4: load the current slot's state.
    pub state_load: AtomicBool,
    /// F6 and F7: slots to step back (negative) or on since last looked at.
    pub slot_step: AtomicI32,
    /// F11, Alt+Enter or a double click: change the core's fullscreen
    /// setting, which the emulation thread then applies to the window.
    pub fullscreen_toggle: AtomicBool,
    /// The window's surface was replaced.
    pub surface_changed: AtomicBool,
}

/// Two presses this close in time are a double click...
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// ... if the pointer moved no further than this between them, in pixels.
const DOUBLE_CLICK_SLOP: f64 = 8.0;

/// A press of the left mouse button.
#[derive(Clone, Copy, Debug)]
struct Click {
    at: Instant,
    x: f64,
    y: f64,
}

impl Click {
    /// Whether this press, after `first`, makes a double click.
    fn completes(self, first: Self) -> bool {
        self.at.duration_since(first.at) <= DOUBLE_CLICK
            && (self.x - first.x).abs() <= DOUBLE_CLICK_SLOP
            && (self.y - first.y).abs() <= DOUBLE_CLICK_SLOP
    }
}

/// Sets `window` to fill its screen or not. Callable from any thread.
pub fn set_fullscreen(window: &Window, on: bool) {
    window.set_fullscreen(on.then_some(Fullscreen::Borderless(None)));
}

/// The event sent to the event loop from other threads: the session is
/// over, leave the loop.
pub type Wake = ();

/// The `winit` application: one window.
pub struct App {
    shared: Arc<Shared>,
    surface: Arc<WindowSurface>,
    start_fullscreen: bool,
    modifiers: ModifiersState,
    /// Where the pointer is, in the window's pixels.
    pointer: (f64, f64),
    /// The last press of the left button that was not a double click's
    /// second.
    last_click: Option<Click>,
    /// When to leave the loop by itself (the self-test).
    deadline: Option<Instant>,
    /// Why the window could not be created, if it could not.
    pub error: Option<String>,
}

impl App {
    /// An application whose window surfaces are made through `surface`.
    pub fn new(
        shared: Arc<Shared>,
        surface: Arc<WindowSurface>,
        start_fullscreen: bool,
        deadline: Option<Instant>,
    ) -> Self {
        Self {
            shared,
            surface,
            start_fullscreen,
            modifiers: ModifiersState::empty(),
            pointer: (0.0, 0.0),
            last_click: None,
            deadline,
            error: None,
        }
    }

    /// Asks for fullscreen to be switched. The setting is the core's (its
    /// menu has a row for it, and it is saved), so the emulation thread
    /// changes it there and sets the window to match.
    fn toggle_fullscreen(&self) {
        self.shared.fullscreen_toggle.store(true, Ordering::Relaxed);
    }

    /// A press of the left button: the second of a double click switches
    /// fullscreen.
    fn click(&mut self) {
        let now = Click { at: Instant::now(), x: self.pointer.0, y: self.pointer.1 };
        if self.last_click.is_some_and(|first| now.completes(first)) {
            self.last_click = None; // a third press starts over
            self.toggle_fullscreen();
        } else {
            self.last_click = Some(now);
        }
    }

    fn store_size(&self, width: u32, height: u32) {
        self.shared.width.store(width, Ordering::Relaxed);
        self.shared.height.store(height, Ordering::Relaxed);
    }

    fn key(&self, code: KeyCode, pressed: bool, repeat: bool) {
        if pressed && !repeat {
            match code {
                KeyCode::F11 => return self.toggle_fullscreen(),
                KeyCode::Enter | KeyCode::NumpadEnter if self.modifiers.alt_key() => {
                    return self.toggle_fullscreen();
                }
                KeyCode::F12 => {
                    return self.shared.screenshot.store(true, Ordering::Relaxed);
                }
                // Savestates, on the keys RetroArch has them on.
                KeyCode::F2 => return self.shared.state_save.store(true, Ordering::Relaxed),
                KeyCode::F4 => return self.shared.state_load.store(true, Ordering::Relaxed),
                KeyCode::F6 => {
                    self.shared.slot_step.fetch_sub(1, Ordering::Relaxed);
                    return;
                }
                KeyCode::F7 => {
                    self.shared.slot_step.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                key if input::is_menu_key(key) => {
                    log::debug!(target: "app", "key {key:?}: toggles the menu");
                    return self.shared.menu_toggle.store(true, Ordering::Relaxed);
                }
                _ => {}
            }
        }
        if let Some(bit) = input::key_bit(code) {
            if pressed {
                self.shared.keys.fetch_or(bit, Ordering::Relaxed);
            } else {
                self.shared.keys.fetch_and(!bit, Ordering::Relaxed);
            }
        }
    }
}

impl ApplicationHandler<Wake> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.surface.window().is_some() {
            // Hosts that take the surface away while suspended bring the
            // application back through here.
            self.shared.surface_changed.store(true, Ordering::Relaxed);
            return;
        }
        // Three times the console's 640x480.
        let attributes = Window::default_attributes()
            .with_title("xbemu")
            .with_inner_size(LogicalSize::new(960, 720))
            .with_fullscreen(self.start_fullscreen.then_some(Fullscreen::Borderless(None)));
        match event_loop.create_window(attributes) {
            Ok(window) => {
                let size = window.inner_size();
                self.store_size(size.width, size.height);
                log::info!(target: "app", "window: {}x{}", size.width, size.height);
                self.surface.set_window(Arc::new(window));
            }
            Err(e) => {
                self.error = Some(format!("cannot create a window: {e}"));
                self.shared.stop.store(true, Ordering::Relaxed);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, (): Wake) {
        event_loop.exit();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                log::info!(target: "app", "window closed");
                self.shared.stop.store(true, Ordering::Relaxed);
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                log::debug!(target: "app", "window: resized to {}x{}", size.width, size.height);
                self.store_size(size.width, size.height);
            }
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::CursorMoved { position, .. } => self.pointer = (position.x, position.y),
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => self.click(),
            // Keys released while another window has the keyboard are never
            // reported: let go of everything.
            WindowEvent::Focused(false) => self.shared.keys.store(0, Ordering::Relaxed),
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    self.key(code, event.state == ElementState::Pressed, event.repeat);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        match self.deadline {
            Some(deadline) if Instant::now() >= deadline => event_loop.exit(),
            Some(deadline) => event_loop.set_control_flow(ControlFlow::WaitUntil(deadline)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_quick_presses_in_one_place_are_a_double_click() {
        let at = Instant::now();
        let first = Click { at, x: 100.0, y: 100.0 };
        let second = |ms: u64, x: f64, y: f64| Click { at: at + Duration::from_millis(ms), x, y };
        assert!(second(150, 103.0, 98.0).completes(first));
        assert!(second(400, 100.0, 100.0).completes(first));
        assert!(!second(401, 100.0, 100.0).completes(first), "too slow");
        assert!(!second(150, 120.0, 100.0).completes(first), "the pointer moved");
        assert!(!second(150, 100.0, 80.0).completes(first));
    }
}
