//! The one-line HUD shown while the menu is closed: the frame-rate counter.

use crate::canvas::{Canvas, Color, TRANSPARENT, text_width};
use crate::font::GLYPH_H;
use crate::render::placement;

/// Behind the text, so it reads over a bright or busy picture.
const BACKING: Color = [0, 0, 0, 0xB0];
const INK: Color = [0xF4, 0xF4, 0x60, 0xFF];

/// Distance from the canvas corner, and the backing box's padding, in
/// logical pixels.
const MARGIN: i32 = 4;
const PAD: i32 = 2;

/// Draws `text` as one line in the top-left corner of `canvas`, on a
/// translucent dark box, and makes the rest of the canvas transparent.
///
/// The text is drawn at the same whole-number scale as the menu, so on the
/// overlay canvas the two match. Every pixel is written; nothing is
/// allocated. A line longer than the canvas is clipped. Call it when the
/// text changes, not every frame, and skip the overlay altogether when the
/// menu is closed and "Show FPS" is off.
///
/// Pixel format and alpha convention: see [`crate::canvas`].
pub fn draw_hud(canvas: &mut Canvas<'_>, text: &str) {
    canvas.clear(TRANSPARENT);
    if text.is_empty() {
        return;
    }
    let (scale, _, _) = placement(canvas.width(), canvas.height());
    let at = MARGIN * scale;
    let pad = PAD * scale;
    let width = text_width(text, scale).saturating_add(2 * pad);
    canvas.fill_rect(at, at, width, GLYPH_H * scale + 2 * pad, BACKING);
    canvas.draw_text(at + pad, at + pad, scale, text, INK);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hud(text: &str, width: u32, height: u32) -> Vec<u8> {
        let mut buf = vec![0x77u8; (width * height * 4) as usize];
        let mut canvas = Canvas::new(&mut buf, width, height).unwrap();
        draw_hud(&mut canvas, text);
        buf
    }

    fn count(image: &[u8], color: Color) -> usize {
        image.as_chunks::<4>().0.iter().filter(|px| **px == color).count()
    }

    #[test]
    fn the_hud_is_text_on_a_backing_box_in_an_otherwise_clear_canvas() {
        let image = hud("60 fps", 640, 480);
        let (ink, backing, clear) =
            (count(&image, INK), count(&image, BACKING), count(&image, TRANSPARENT));
        assert!(ink > 0 && backing > ink);
        assert_eq!(ink + backing + clear, 640 * 480);
        // The box hugs the text: six 16-pixel characters plus padding.
        assert_eq!(ink + backing, (6 * 16 + 8) * (16 + 8));
    }

    #[test]
    fn every_inked_pixel_is_surrounded_by_the_box() {
        let (w, h) = (320usize, 240usize);
        let image = hud("59 fps", w as u32, h as u32);
        let px = |x: usize, y: usize| &image[(y * w + x) * 4..][..4];
        for y in 0..h {
            for x in 0..w {
                if px(x, y) == INK {
                    for (nx, ny) in [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)] {
                        assert_ne!(px(nx, ny), TRANSPARENT, "ink at {x},{y} touches the picture");
                    }
                }
            }
        }
    }

    #[test]
    fn different_text_gives_a_different_image_and_no_text_a_clear_one() {
        assert_ne!(hud("59 fps", 320, 240), hud("60 fps", 320, 240));
        assert!(hud("", 320, 240).iter().all(|&b| b == 0));
    }

    #[test]
    fn a_line_longer_than_the_canvas_is_clipped() {
        let long = "x".repeat(500);
        for (w, h) in [(0, 0), (1, 1), (5, 40), (40, 5), (96, 16)] {
            let image = hud(&long, w, h);
            assert_eq!(image.len(), (w * h * 4) as usize);
            assert_eq!(count(&image, [0x77; 4]), 0);
        }
    }
}
