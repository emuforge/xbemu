//! The core's in-game settings menu: gamepad navigation and an RGBA overlay.
//!
//! The menu belongs to the core so that every front end gets settings by
//! forwarding pad state and showing frames. This crate is the menu's logic
//! and its picture, with no I/O, no clock and no graphics API: the core
//! passes pad state and elapsed time in, and gets back an [`Outcome`] and,
//! on request, pixels in a [`Canvas`] it provides.
//!
//! Each frame the core does this:
//!
//! ```
//! # use core::time::Duration;
//! # use xb_menu::{Canvas, Capabilities, Menu, Outcome, Status};
//! # use xb_types::pad::PadState;
//! # use xb_types::settings::Settings;
//! # let mut menu = Menu::new(Settings::default(), Capabilities::default());
//! # let pads = [PadState::default(); 4];
//! # let dt = Duration::from_millis(16);
//! # let mut pixels = vec![0u8; 640 * 480 * 4];
//! menu.set_status(&Status { cpu_backend: "KVM", gpu: "...", title: "...", game: "", fps: 60.0 });
//! match menu.update(dt, &pads) {
//!     Outcome::None => {}
//!     Outcome::Opened => { /* pause the title */ }
//!     Outcome::Closed => { /* resume the title */ }
//!     Outcome::SettingsChanged(settings) => { /* apply and persist */ }
//!     Outcome::SaveSlot(slot) => { /* save a state into the slot */ }
//!     Outcome::LoadSlot(slot) => { /* load the slot's state */ }
//!     Outcome::ResetRequested => { /* restart the title */ }
//!     Outcome::QuitRequested => { /* leave */ }
//! }
//! // What the title's controller ports see, open or closed.
//! let for_title = pads.map(|pad| menu.filter_pad(pad));
//! if menu.is_open() && menu.is_dirty() {
//!     let mut canvas = Canvas::new(&mut pixels, 640, 480).unwrap();
//!     menu.render(&mut canvas);
//!     // upload `pixels` as the overlay texture
//! }
//! # let _ = for_title;
//! ```
//!
//! While the menu is closed and "Show FPS" is on, [`draw_hud`] draws the
//! counter into the same kind of canvas.
#![forbid(unsafe_code)]

pub mod canvas;
pub mod font;
mod hud;
mod input;
mod items;
mod menu;
mod render;
mod text;

pub use canvas::Canvas;
pub use hud::draw_hud;
pub use input::{COMBO_HOLD, REPEAT_DELAY, REPEAT_INTERVAL};
pub use items::{Capabilities, Setting};
pub use menu::{Menu, Outcome, Picture, SLOT_ROWS, SlotRow, Status};
pub use render::{LOGICAL_HEIGHT, LOGICAL_WIDTH};
