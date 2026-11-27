//! Drawing the menu into a [`Canvas`].
//!
//! The layout is fixed on a logical [`LOGICAL_WIDTH`] by [`LOGICAL_HEIGHT`]
//! grid (40 by 30 text cells). It is drawn at the largest whole multiple
//! that fits the canvas and centered, so text stays crisp: 2x on 640x480,
//! 4x on 1280x960. A canvas smaller than the grid shows its middle at 1x.
//! Nothing is allocated; every string is a static or lives in the menu.

use core::fmt::Write as _;

use crate::canvas::{Canvas, Color, TRANSPARENT, text_width};
use crate::font::GLYPH_W;
use crate::items::{ITEMS, Info, Row};
use crate::menu::{Confirm, Menu, Picture};
use crate::text::TextBuf;

/// Width of the layout grid. A canvas this wide (or a whole multiple) is
/// filled edge to edge.
pub const LOGICAL_WIDTH: u32 = 320;
/// Height of the layout grid.
pub const LOGICAL_HEIGHT: u32 = 240;

const W: i32 = LOGICAL_WIDTH as i32;
const H: i32 = LOGICAL_HEIGHT as i32;

// Colors, straight alpha.
/// Over the whole picture, so the paused game recedes.
const DIM: Color = [0, 0, 0, 0x60];
const PANEL: Color = [0x10, 0x14, 0x18, 0xE8];
const BORDER: Color = [0x4C, 0xB0, 0x50, 0xFF];
const RULE: Color = [0x48, 0x50, 0x58, 0xFF];
const TITLE: Color = [0x8C, 0xE8, 0x70, 0xFF];
const TEXT: Color = [0xE8, 0xEC, 0xF0, 0xFF];
const FAINT: Color = [0x98, 0xA0, 0xA8, 0xFF];
const HILITE: Color = [0x2C, 0x8C, 0x3C, 0xFF];
const HILITE_TEXT: Color = [0xFF, 0xFF, 0xFF, 0xFF];
const NOTE: Color = [0xFF, 0xC4, 0x48, 0xFF];
const BOX: Color = [0x18, 0x1C, 0x22, 0xFF];

// Geometry, in logical pixels.
const PANEL_X: i32 = 8;
const PANEL_Y: i32 = 4;
const PANEL_W: i32 = W - 2 * PANEL_X;
const PANEL_H: i32 = H - 2 * PANEL_Y;
/// Left edge of text.
const LEFT: i32 = 16;
/// Right edge of text.
const RIGHT: i32 = W - LEFT;
/// Right edge of a row's value; the cells past it hold the `>` arrow.
const VALUE_RIGHT: i32 = RIGHT - 2 * GLYPH_W;
const TITLE_Y: i32 = 10;
pub(crate) const ROWS_Y: i32 = 25;
pub(crate) const ROW_H: i32 = 10;
/// Text offset inside a row.
const ROW_PAD: i32 = 1;
const ROWS_END: i32 = ROWS_Y + ITEMS.len() as i32 * ROW_H;
const LEGEND_Y: i32 = ROWS_END + 3;
const STATUS_Y: i32 = LEGEND_Y + 13;
/// Where status values start, past the longest status label.
const STATUS_X: i32 = LEFT + 7 * GLYPH_W;
const FOOTER_Y: i32 = STATUS_Y + 25;
/// The confirm box.
const BOX_W: i32 = 208;
const BOX_H: i32 = 60;
const BUTTON_W: i32 = 64;
const BUTTON_H: i32 = 14;

// The layout must fit its grid whatever rows are added to the table.
const _: () = assert!(FOOTER_Y + 8 <= PANEL_Y + PANEL_H - 2);

/// The mark on a row that applies at the next boot, and its explanation.
const NOTE_MARK: &str = "*";
const NOTE_LEGEND: &str = "* applies at next boot";
const HINTS: &str = "A/Enter select  B/Backspace back";
const HINTS_CONFIRM: &str = "Left/Right  A/Enter accept  B cancel";
const HINTS_SLOTS: &str = "A/Enter load  X/C save  B back";
/// The slots page: its rows, and the picture's box at their right.
const SLOT_X: i32 = LEFT;
const SLOT_W: i32 = 136;
const SLOT_ROW_H: i32 = 14;
const PIC_X: i32 = SLOT_X + SLOT_W + 8;
const PIC_Y: i32 = ROWS_Y + 2;
const PIC_W: i32 = RIGHT - PIC_X;
const PIC_H: i32 = PIC_W * 3 / 4;

/// The whole-number scale and the offset that center the grid on a canvas.
pub(crate) fn placement(width: u32, height: u32) -> (i32, i32, i32) {
    let scale = (width / LOGICAL_WIDTH).min(height / LOGICAL_HEIGHT).clamp(1, 1 << 12) as i32;
    let center = |size: u32, logical: i32| {
        let free = i64::from(size) - i64::from(logical) * i64::from(scale);
        (free / 2).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    };
    (scale, center(width, W), center(height, H))
}

/// A canvas addressed in logical pixels.
struct Painter<'c, 'p> {
    canvas: &'c mut Canvas<'p>,
    scale: i32,
    x0: i32,
    y0: i32,
}

impl Painter<'_, '_> {
    fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color) {
        let s = self.scale;
        self.canvas.fill_rect(self.x0 + x * s, self.y0 + y * s, w * s, h * s, color);
    }

    /// A filled box with a one-pixel border.
    fn frame(&mut self, x: i32, y: i32, w: i32, h: i32, fill: Color, border: Color) {
        self.rect(x, y, w, h, border);
        self.rect(x + 1, y + 1, w - 2, h - 2, fill);
    }

    fn text(&mut self, x: i32, y: i32, text: &str, color: Color) {
        let s = self.scale;
        self.canvas.draw_text(self.x0 + x * s, self.y0 + y * s, s, text, color);
    }

    /// Text ending at `right`; returns its left edge.
    fn text_right(&mut self, right: i32, y: i32, text: &str, color: Color) -> i32 {
        let x = right - text_width(text, 1);
        self.text(x, y, text, color);
        x
    }

    fn text_centered(&mut self, center: i32, y: i32, text: &str, color: Color) {
        self.text(center - text_width(text, 1) / 2, y, text, color);
    }
}

impl Menu {
    /// Draws the menu over the whole of `canvas` and clears
    /// [`Menu::is_dirty`].
    ///
    /// Every pixel is written, so the canvas need not be cleared first and
    /// may hold anything. A closed menu draws a fully transparent image. See
    /// [`crate::canvas`] for the pixel format and the alpha convention.
    pub fn render(&mut self, canvas: &mut Canvas<'_>) {
        self.render_with(canvas, None);
    }

    /// [`Menu::render`], with the picture of the slot the savestate page
    /// highlights ([`Menu::shown_slot`]) when the caller has one. The
    /// picture is drawn into the page's box in its own shape; other pages
    /// ignore it.
    pub fn render_with(&mut self, canvas: &mut Canvas<'_>, picture: Option<Picture<'_>>) {
        self.dirty = false;
        if !self.open {
            canvas.clear(TRANSPARENT);
            return;
        }
        if let Some(slot) = self.slots_page {
            self.render_slots(canvas, slot, picture);
            return;
        }
        canvas.clear(DIM);
        let (scale, x0, y0) = placement(canvas.width(), canvas.height());
        let mut p = Painter { canvas, scale, x0, y0 };

        p.frame(PANEL_X, PANEL_Y, PANEL_W, PANEL_H, PANEL, BORDER);
        p.text(LEFT, TITLE_Y, "xbemu", TITLE);
        let mut fps = TextBuf::<12>::new();
        // TextBuf's writer truncates rather than fails.
        let _ = write!(fps, "{} fps", self.status.fps);
        p.text_right(RIGHT, TITLE_Y, fps.as_str(), FAINT);
        p.rect(LEFT, TITLE_Y + 11, RIGHT - LEFT, 1, RULE);

        let mut any_note = false;
        for (index, item) in ITEMS.iter().enumerate() {
            let y = ROWS_Y + index as i32 * ROW_H;
            let selected = index == self.selected;
            let ink = match (selected, item.row) {
                (true, _) => HILITE_TEXT,
                (false, Row::Info(_)) => FAINT,
                (false, _) => TEXT,
            };
            if selected {
                p.rect(PANEL_X + 4, y, PANEL_W - 8, ROW_H, HILITE);
            }
            let ty = y + ROW_PAD;
            p.text(LEFT, ty, item.label, ink);
            match item.row {
                Row::Setting { id, values, get, .. } => {
                    if self.caps.needs_restart(id) {
                        any_note = true;
                        let x = LEFT + text_width(item.label, 1) + GLYPH_W / 2;
                        p.text(x, ty, NOTE_MARK, NOTE);
                    }
                    let current = get(&self.settings);
                    let mut shown = TextBuf::<16>::new();
                    values.format(current, &mut shown);
                    let left = p.text_right(VALUE_RIGHT, ty, shown.as_str(), ink);
                    if selected {
                        // An arrow only where left or right would change
                        // the value.
                        if values.can_step(current, -1) {
                            p.text(left - 2 * GLYPH_W, ty, "<", ink);
                        }
                        if values.can_step(current, 1) {
                            p.text(RIGHT - GLYPH_W, ty, ">", ink);
                        }
                    }
                }
                Row::Info(info) => {
                    let value = match info {
                        Info::ActiveCpu => &self.status.cpu_backend,
                    };
                    p.text_right(VALUE_RIGHT, ty, or_dash(value.fitted(16).as_str()), ink);
                }
                Row::Resume | Row::Confirm(_) | Row::Slots => {}
            }
        }
        if any_note {
            p.text(LEFT, LEGEND_Y, NOTE_LEGEND, NOTE);
        }

        p.rect(LEFT, STATUS_Y - 4, RIGHT - LEFT, 1, RULE);
        let room = ((RIGHT - STATUS_X) / GLYPH_W) as usize;
        for (line, (label, value)) in
            (0i32..).zip([("Title", &self.status.title), ("Game", &self.status.game)])
        {
            let y = STATUS_Y + line * 10;
            p.text(LEFT, y, label, FAINT);
            p.text(STATUS_X, y, or_dash(value.fitted(room).as_str()), TEXT);
        }

        p.rect(LEFT, FOOTER_Y - 4, RIGHT - LEFT, 1, RULE);
        let hints = if self.confirm.is_some() { HINTS_CONFIRM } else { HINTS };
        p.text(LEFT, FOOTER_Y, hints, FAINT);

        if let Some(confirm) = self.confirm {
            draw_confirm(&mut p, confirm);
        }
    }
}

impl Menu {
    /// The savestate slots page: the ten slots at the left, the highlighted
    /// one's picture at the right.
    fn render_slots(&self, canvas: &mut Canvas<'_>, selected: usize, picture: Option<Picture<'_>>) {
        canvas.clear(DIM);
        let (scale, x0, y0) = placement(canvas.width(), canvas.height());
        let mut p = Painter { canvas, scale, x0, y0 };
        p.frame(PANEL_X, PANEL_Y, PANEL_W, PANEL_H, PANEL, BORDER);
        p.text(LEFT, TITLE_Y, "Save states", TITLE);
        p.rect(LEFT, TITLE_Y + 11, RIGHT - LEFT, 1, RULE);

        for (slot, (used, label)) in self.slot_rows.iter().enumerate() {
            let y = ROWS_Y + 2 + slot as i32 * SLOT_ROW_H;
            let chosen = slot == selected;
            if chosen {
                p.rect(PANEL_X + 4, y, SLOT_W + 4, SLOT_ROW_H - 2, HILITE);
            }
            let ink = match (chosen, used) {
                (true, _) => HILITE_TEXT,
                (false, true) => TEXT,
                (false, false) => FAINT,
            };
            let mut name = TextBuf::<8>::new();
            // TextBuf's writer truncates rather than fails.
            let _ = write!(name, "Slot {slot}");
            p.text(SLOT_X, y + 2, name.as_str(), ink);
            let room = ((SLOT_W - 7 * GLYPH_W) / GLYPH_W) as usize;
            let what = if *used { label.fitted(room) } else { TextBuf::from_text("empty") };
            p.text_right(SLOT_X + SLOT_W, y + 2, what.as_str(), ink);
        }

        p.frame(PIC_X - 1, PIC_Y - 1, PIC_W + 2, PIC_H + 2, BOX, RULE);
        match picture {
            Some(pic) if self.slot_rows[selected].0 && pic.width != 0 && pic.height != 0 => {
                // In its own shape, centered in the box.
                let (bw, bh) = (PIC_W * scale, PIC_H * scale);
                let (pw, ph) = (pic.width as i32, pic.height as i32);
                let (w, h) = if i64::from(pw) * i64::from(bh) >= i64::from(ph) * i64::from(bw) {
                    (bw, (i64::from(ph) * i64::from(bw) / i64::from(pw)).max(1) as i32)
                } else {
                    ((i64::from(pw) * i64::from(bh) / i64::from(ph)).max(1) as i32, bh)
                };
                let (x, y) = (x0 + PIC_X * scale + (bw - w) / 2, y0 + PIC_Y * scale + (bh - h) / 2);
                p.canvas.draw_picture(x, y, w, h, pic.width, pic.height, pic.rgba);
            }
            _ => {
                let text = if self.slot_rows[selected].0 { "no picture" } else { "empty" };
                p.text_centered(PIC_X + PIC_W / 2, PIC_Y + PIC_H / 2 - 4, text, FAINT);
            }
        }

        p.rect(LEFT, FOOTER_Y - 4, RIGHT - LEFT, 1, RULE);
        p.text(LEFT, FOOTER_Y, HINTS_SLOTS, FAINT);
    }
}

/// The confirm box, over the middle of the list.
fn draw_confirm(p: &mut Painter<'_, '_>, confirm: Confirm) {
    let (x, y) = ((W - BOX_W) / 2, (H - BOX_H) / 2);
    p.frame(x, y, BOX_W, BOX_H, BOX, BORDER);
    p.text_centered(W / 2, y + 12, confirm.action.question(), TEXT);
    let by = y + BOX_H - BUTTON_H - 10;
    for (label, yes, center) in [("No", false, W / 2 - 44), ("Yes", true, W / 2 + 44)] {
        let chosen = confirm.yes == yes;
        let (fill, ink) = if chosen { (HILITE, HILITE_TEXT) } else { (BOX, FAINT) };
        p.frame(
            center - BUTTON_W / 2,
            by,
            BUTTON_W,
            BUTTON_H,
            fill,
            if chosen { BORDER } else { RULE },
        );
        p.text_centered(center, by + 3, label, ink);
    }
}

/// A placeholder for a status string the caller has not set.
fn or_dash(text: &str) -> &str {
    if text.is_empty() { "-" } else { text }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::items::{Capabilities, Setting};
    use crate::menu::Status;
    use core::time::Duration;
    use xb_types::pad::{PadState, analog, button};
    use xb_types::settings::Settings;

    const FRAME: Duration = Duration::from_millis(16);

    fn open_menu() -> Menu {
        let mut menu = Menu::new(Settings::default(), Capabilities::default());
        menu.set_status(&Status {
            cpu_backend: "KVM",
            gpu: "A graphics adapter with a name much longer than the line",
            title: "Halo",
            game: "draws 4:3",
            fps: 60.0,
        });
        menu.update(FRAME, &[PadState::default()]);
        menu.open();
        menu
    }

    fn press(menu: &mut Menu, buttons: u16, a: bool) {
        let mut pad = PadState { buttons, ..PadState::default() };
        pad.analog[analog::A] = if a { 255 } else { 0 };
        menu.update(FRAME, &[pad]);
        menu.update(FRAME, &[PadState::default()]);
    }

    fn render(menu: &mut Menu, width: u32, height: u32) -> Vec<u8> {
        let mut buf = vec![0x55u8; (width * height * 4) as usize];
        let mut canvas = Canvas::new(&mut buf, width, height).unwrap();
        menu.render(&mut canvas);
        buf
    }

    /// The pixels of logical row `index` of the list on a 320x240 image.
    fn row_band(image: &[u8], index: usize) -> &[u8] {
        let stride = LOGICAL_WIDTH as usize * 4;
        let top = (ROWS_Y + index as i32 * ROW_H) as usize;
        &image[top * stride..(top + ROW_H as usize) * stride]
    }

    fn count(image: &[u8], color: Color) -> usize {
        image.as_chunks::<4>().0.iter().filter(|px| **px == color).count()
    }

    #[test]
    fn an_open_menu_draws_a_panel_with_text_over_a_dimmed_picture() {
        let mut menu = open_menu();
        let image = render(&mut menu, 640, 480);
        assert!(count(&image, PANEL) > 640 * 480 / 2, "the panel covers most of the canvas");
        assert!(count(&image, TEXT) > 500, "item text");
        assert!(count(&image, TITLE) > 0 && count(&image, FAINT) > 0 && count(&image, NOTE) > 0);
        assert!(image.as_chunks::<4>().0.iter().all(|px| px[3] != 0), "every pixel at least dims");
        assert_eq!(count(&image, [0x55; 4]), 0, "nothing of the old buffer is left");
    }

    #[test]
    fn a_wider_canvas_keeps_the_dim_outside_the_centered_panel() {
        let mut menu = open_menu();
        let image = render(&mut menu, 854, 480);
        let px = |x: usize, y: usize| &image[(y * 854 + x) * 4..][..4];
        assert_eq!(px(0, 240), DIM);
        assert_eq!(px(853, 240), DIM);
        assert_eq!(px(427, 12), PANEL);
    }

    #[test]
    fn a_closed_menu_draws_a_transparent_image() {
        let mut menu = Menu::new(Settings::default(), Capabilities::default());
        let image = render(&mut menu, 64, 48);
        assert!(image.iter().all(|&b| b == 0));
    }

    #[test]
    fn only_the_selected_row_is_highlighted_and_the_highlight_follows_the_selection() {
        let mut menu = open_menu();
        let first = render(&mut menu, 320, 240);
        for index in 0..ITEMS.len() {
            let lit = count(row_band(&first, index), HILITE);
            assert_eq!(lit > 0, index == 0, "row {index}");
        }
        press(&mut menu, button::DPAD_DOWN, false);
        let second = render(&mut menu, 320, 240);
        assert_eq!(count(row_band(&second, 0), HILITE), 0);
        assert!(count(row_band(&second, 1), HILITE) > 0);
        assert_ne!(row_band(&first, 0), row_band(&second, 0));
        assert_ne!(row_band(&first, 1), row_band(&second, 1));
        // The rest of the picture is untouched by a selection move.
        for index in 2..ITEMS.len() {
            assert_eq!(row_band(&first, index), row_band(&second, index), "row {index}");
        }
    }

    #[test]
    fn changing_a_value_changes_only_its_row() {
        let mut menu = open_menu();
        press(&mut menu, button::DPAD_DOWN, false);
        let before = render(&mut menu, 320, 240);
        press(&mut menu, button::DPAD_RIGHT, false);
        let after = render(&mut menu, 320, 240);
        assert_ne!(row_band(&before, 1), row_band(&after, 1));
        let stride = 320 * 4;
        let (top, bottom) =
            ((ROWS_Y + ROW_H) as usize * stride, (ROWS_Y + 2 * ROW_H) as usize * stride);
        assert_eq!(before[..top], after[..top]);
        assert_eq!(before[bottom..], after[bottom..]);
    }

    #[test]
    fn rendering_stays_inside_buffers_of_every_size() {
        let sizes = [
            (0, 0),
            (1, 1),
            (0, 7),
            (7, 0),
            (3, 200),
            (200, 3),
            (17, 9),
            (160, 120),
            (319, 239),
            (320, 240),
            (321, 241),
            (640, 480),
            (720, 480),
            (1000, 300),
            (300, 1000),
            (1280, 960),
        ];
        let mut menu = open_menu();
        let confirm_row = ITEMS.iter().position(|i| matches!(i.row, Row::Confirm(_))).unwrap();
        for with_confirm in [false, true] {
            if with_confirm {
                while menu.selected != confirm_row {
                    press(&mut menu, button::DPAD_DOWN, false);
                }
                press(&mut menu, 0, true);
                assert!(menu.confirm.is_some());
            }
            for (width, height) in sizes {
                // Guard bytes on both sides of the image catch a stray write.
                let len = (width * height * 4) as usize;
                let mut buf = vec![0xA5u8; len + 64];
                let mut canvas = Canvas::new(&mut buf[32..], width, height).unwrap();
                menu.render(&mut canvas);
                crate::draw_hud(&mut canvas, "60 fps");
                menu.render(&mut canvas);
                assert!(buf[..32].iter().all(|&b| b == 0xA5), "{width}x{height}");
                assert!(buf[32 + len..].iter().all(|&b| b == 0xA5), "{width}x{height}");
                if len > 0 {
                    assert!(buf[32..32 + len].as_chunks::<4>().0.iter().all(|px| px[3] != 0));
                }
            }
        }
    }

    #[test]
    fn the_layout_scales_by_whole_factors() {
        assert_eq!(placement(320, 240), (1, 0, 0));
        assert_eq!(placement(640, 480), (2, 0, 0));
        assert_eq!(placement(1280, 960), (4, 0, 0));
        assert_eq!(placement(1279, 960), (3, 159, 120));
        assert_eq!(placement(854, 480), (2, 107, 0));
        assert_eq!(placement(100, 100), (1, -110, -70));
        assert_eq!(placement(0, 0).0, 1);
        assert!(placement(u32::MAX, u32::MAX).0 >= 1);
    }

    #[test]
    fn a_scaled_render_is_the_small_one_with_each_pixel_repeated() {
        let mut menu = open_menu();
        let small = render(&mut menu, 320, 240);
        let big = render(&mut menu, 960, 720);
        for y in 0..720 {
            for x in 0..960 {
                let b = &big[(y * 960 + x) * 4..][..4];
                let s = &small[(y / 3 * 320 + x / 3) * 4..][..4];
                assert_eq!(b, s, "at {x},{y}");
            }
        }
    }

    #[test]
    fn the_next_boot_mark_follows_the_capabilities() {
        let mut menu = open_menu();
        menu.set_capabilities(Capabilities::all_live());
        let none = render(&mut menu, 320, 240);
        assert_eq!(count(&none, NOTE), 0, "no mark and no legend when everything is live");

        let fxaa = ITEMS.iter().position(|i| i.label == "FXAA").unwrap();
        menu.set_capabilities(Capabilities::all_live().next_boot(Setting::Fxaa));
        let one = render(&mut menu, 320, 240);
        for index in 0..ITEMS.len() {
            assert_eq!(count(row_band(&one, index), NOTE) > 0, index == fxaa, "row {index}");
        }
        assert!(count(&one, NOTE) > count(row_band(&one, fxaa), NOTE), "the legend is shown");
    }

    #[test]
    fn the_confirm_box_highlights_the_chosen_answer() {
        let mut menu = open_menu();
        let quit = ITEMS.iter().position(|i| i.label == "Quit").unwrap();
        while menu.selected != quit {
            press(&mut menu, button::DPAD_DOWN, false);
        }
        let list = render(&mut menu, 320, 240);
        press(&mut menu, 0, true);
        let on_no = render(&mut menu, 320, 240);
        press(&mut menu, button::DPAD_RIGHT, false);
        let on_yes = render(&mut menu, 320, 240);
        assert!(count(&on_no, BOX) > 0 && count(&list, BOX) == 0);
        assert_ne!(on_no, on_yes);
        // The highlight sits left of center for "No", right for "Yes".
        let lit_left = |image: &[u8]| {
            let by = ((H - BOX_H) / 2 + BOX_H - BUTTON_H - 10 + 1) as usize;
            let row = &image[by * 320 * 4..][..320 * 4];
            (count(&row[..160 * 4], HILITE), count(&row[160 * 4..], HILITE))
        };
        let (no_left, no_right) = lit_left(&on_no);
        let (yes_left, yes_right) = lit_left(&on_yes);
        assert!(no_left > 0 && no_right == 0, "{no_left} {no_right}");
        assert!(yes_left == 0 && yes_right > 0, "{yes_left} {yes_right}");
    }

    #[test]
    fn status_text_reaches_the_picture() {
        let mut menu = open_menu();
        let with = render(&mut menu, 320, 240);
        menu.set_status(&Status::default());
        let without = render(&mut menu, 320, 240);
        assert_ne!(with, without);
        let stride = 320 * 4;
        let status = (STATUS_Y as usize * stride)..((STATUS_Y + 20) as usize * stride);
        assert!(count(&with[status.clone()], TEXT) > count(&without[status], TEXT));
    }
}
