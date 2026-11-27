//! The pixel buffer the menu and the HUD draw into, and the few primitives
//! they need: clear, filled rectangle, text.
//!
//! # Pixel format
//!
//! Four bytes per pixel in the order R, G, B, A, rows top to bottom, no row
//! padding (the stride is `width * 4`). Alpha is **straight** (not
//! premultiplied): the color bytes are the color itself whatever the alpha.
//! A compositor blends a pixel over the picture as
//! `out = src.rgb * src.a + dst.rgb * (1 - src.a)`, which in Vulkan terms is
//! `SRC_ALPHA` / `ONE_MINUS_SRC_ALPHA` on an `R8G8B8A8_UNORM` texture. The
//! color bytes are sRGB-encoded like any UI color; sample the texture as
//! UNORM (or as SRGB into an sRGB target) so they are not converted twice.
//!
//! Drawing replaces pixels (color and alpha both), it does not blend inside
//! the canvas. Everything is clipped to the buffer, so any coordinates and
//! any buffer size are safe.

use crate::font;

/// A straight-alpha color: `[r, g, b, a]`.
pub type Color = [u8; 4];

/// Fully transparent.
pub const TRANSPARENT: Color = [0, 0, 0, 0];

/// A caller-provided RGBA8 buffer with its dimensions.
///
/// See the [module documentation](self) for the pixel format and the alpha
/// convention.
#[derive(Debug)]
pub struct Canvas<'a> {
    pixels: &'a mut [u8],
    width: u32,
    height: u32,
}

impl<'a> Canvas<'a> {
    /// Wraps `pixels` as a `width` by `height` image.
    ///
    /// Returns `None` when the buffer is shorter than `width * height * 4`
    /// bytes. A longer buffer is accepted and the excess is never touched. A
    /// zero width or height is valid and makes every drawing call a no-op.
    #[must_use]
    pub fn new(pixels: &'a mut [u8], width: u32, height: u32) -> Option<Self> {
        let needed = (u64::from(width) * u64::from(height)).checked_mul(4)?;
        let needed = usize::try_from(needed).ok()?;
        let pixels = pixels.get_mut(..needed)?;
        Some(Self { pixels, width, height })
    }

    /// Width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The image bytes, `width * height * 4` of them.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        self.pixels
    }

    /// The color at (`x`, `y`), or `None` outside the image.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<Color> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let at = (y as usize * self.width as usize + x as usize) * 4;
        self.pixels.get(at..at + 4)?.try_into().ok()
    }

    /// Sets every pixel to `color`.
    pub fn clear(&mut self, color: Color) {
        self.pixels.as_chunks_mut::<4>().0.fill(color);
    }

    /// Draws `rgba` (`src_w` x `src_h` opaque pixels, top row first) scaled
    /// to fill `w` x `h` at (`x`, `y`), each pixel taking its nearest
    /// source pixel. Clipped to the canvas; a source too short for its size
    /// draws nothing.
    #[expect(clippy::too_many_arguments, reason = "a rectangle and a picture")]
    pub fn draw_picture(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        src_w: u32,
        src_h: u32,
        rgba: &[u8],
    ) {
        let len = (src_w as usize).saturating_mul(src_h as usize).saturating_mul(4);
        if w <= 0 || h <= 0 || src_w == 0 || src_h == 0 || rgba.len() < len {
            return;
        }
        for dy in 0..h {
            let py = i64::from(y) + i64::from(dy);
            if py < 0 || py >= i64::from(self.height) {
                continue;
            }
            let sy = (i64::from(dy) * i64::from(src_h) / i64::from(h)) as usize;
            for dx in 0..w {
                let px = i64::from(x) + i64::from(dx);
                if px < 0 || px >= i64::from(self.width) {
                    continue;
                }
                let sx = (i64::from(dx) * i64::from(src_w) / i64::from(w)) as usize;
                let from = (sy * src_w as usize + sx) * 4;
                let to = (py as usize * self.width as usize + px as usize) * 4;
                self.pixels[to..to + 3].copy_from_slice(&rgba[from..from + 3]);
                self.pixels[to + 3] = 0xFF;
            }
        }
    }

    /// Sets the `w` by `h` rectangle whose top-left corner is (`x`, `y`) to
    /// `color`, clipped to the image. A non-positive size draws nothing.
    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color) {
        let clip = |start: i32, len: i32, limit: u32| {
            let a = i64::from(start).clamp(0, i64::from(limit));
            let b = (i64::from(start) + i64::from(len)).clamp(0, i64::from(limit));
            // Both ends are within 0..=limit, which fits usize wherever the
            // buffer itself does.
            (a as usize, b as usize)
        };
        let (x0, x1) = clip(x, w, self.width);
        let (y0, y1) = clip(y, h, self.height);
        if x0 >= x1 {
            return;
        }
        let stride = self.width as usize * 4;
        for row in self.pixels.chunks_exact_mut(stride).take(y1).skip(y0) {
            row[x0 * 4..x1 * 4].as_chunks_mut::<4>().0.fill(color);
        }
    }

    /// Draws `text` with its top-left corner at (`x`, `y`), each font pixel
    /// `scale` by `scale` image pixels, and returns the x coordinate just past
    /// the last character.
    ///
    /// Only the glyphs' set pixels are written; the background is left alone.
    /// Characters outside printable ASCII are drawn as `?`. There is no line
    /// wrapping: a newline is one more `?`.
    pub fn draw_text(&mut self, x: i32, y: i32, scale: i32, text: &str, color: Color) -> i32 {
        let mut pen = x;
        for c in text.chars() {
            self.draw_glyph(pen, y, scale, c, color);
            pen = pen.saturating_add(font::GLYPH_W.saturating_mul(scale));
        }
        pen
    }

    /// Draws one glyph as horizontal runs, one `fill_rect` per run.
    fn draw_glyph(&mut self, x: i32, y: i32, scale: i32, c: char, color: Color) {
        if scale <= 0 {
            return;
        }
        for (row, &bits) in (0i32..).zip(font::glyph(c)) {
            let gy = y.saturating_add(row.saturating_mul(scale));
            let mut col = 0;
            while col < font::GLYPH_W {
                if bits >> col & 1 == 0 {
                    col += 1;
                    continue;
                }
                let start = col;
                while col < font::GLYPH_W && bits >> col & 1 != 0 {
                    col += 1;
                }
                let gx = x.saturating_add(start.saturating_mul(scale));
                self.fill_rect(gx, gy, (col - start).saturating_mul(scale), scale, color);
            }
        }
    }
}

/// Width in image pixels of `text` drawn at `scale`.
#[must_use]
pub fn text_width(text: &str, scale: i32) -> i32 {
    let chars = i32::try_from(text.chars().count()).unwrap_or(i32::MAX);
    chars.saturating_mul(font::GLYPH_W).saturating_mul(scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INK: Color = [1, 2, 3, 255];

    #[test]
    fn a_buffer_that_is_too_short_is_refused() {
        let mut buf = [0u8; 4 * 4 * 4 - 1];
        assert!(Canvas::new(&mut buf, 4, 4).is_none());
        assert!(Canvas::new(&mut buf, 4, 3).is_some());
        assert!(Canvas::new(&mut buf, u32::MAX, u32::MAX).is_none());
    }

    #[test]
    fn bytes_past_the_image_are_never_touched() {
        let mut buf = [0xAAu8; 2 * 2 * 4 + 8];
        let mut canvas = Canvas::new(&mut buf, 2, 2).unwrap();
        canvas.clear(INK);
        canvas.fill_rect(-5, -5, 100, 100, INK);
        assert_eq!(buf[16..], [0xAA; 8]);
    }

    #[test]
    fn fill_rect_covers_exactly_its_clipped_rectangle() {
        let (w, h) = (7u32, 5u32);
        for (x, y, rw, rh) in [
            (1, 1, 3, 2),
            (-2, -2, 4, 4),
            (5, 3, 10, 10),
            (0, 0, 0, 5),
            (3, 2, -1, 4),
            (i32::MAX, i32::MAX, i32::MAX, i32::MAX),
            (i32::MIN, i32::MIN, i32::MAX, i32::MAX),
            (-1, -1, i32::MAX, i32::MAX),
        ] {
            let mut buf = vec![0u8; (w * h * 4) as usize];
            let mut canvas = Canvas::new(&mut buf, w, h).unwrap();
            canvas.fill_rect(x, y, rw, rh, INK);
            for py in 0..h {
                for px in 0..w {
                    let (px64, py64) = (i64::from(px), i64::from(py));
                    let inside = px64 >= i64::from(x)
                        && px64 < i64::from(x) + i64::from(rw)
                        && py64 >= i64::from(y)
                        && py64 < i64::from(y) + i64::from(rh);
                    let want = if inside { INK } else { TRANSPARENT };
                    assert_eq!(canvas.pixel(px, py), Some(want), "rect {x},{y} {rw}x{rh}");
                }
            }
        }
    }

    #[test]
    fn a_zero_sized_canvas_accepts_every_call() {
        let mut buf = [];
        let mut canvas = Canvas::new(&mut buf, 0, 0).unwrap();
        canvas.clear(INK);
        canvas.fill_rect(0, 0, 10, 10, INK);
        canvas.draw_text(0, 0, 3, "text", INK);
        assert_eq!(canvas.pixel(0, 0), None);
    }

    #[test]
    fn text_marks_exactly_the_set_bits_of_each_glyph() {
        // Every pixel of the string's box is compared with the font table,
        // at two scales, so both the bit order and the scaling are covered.
        let text = "Ag!";
        for scale in [1, 3] {
            let (ox, oy) = (2, 1);
            let w = (text_width(text, scale) + 4) as u32;
            let h = (font::GLYPH_H * scale + 2) as u32;
            let mut buf = vec![0u8; (w * h * 4) as usize];
            let mut canvas = Canvas::new(&mut buf, w, h).unwrap();
            let end = canvas.draw_text(ox, oy, scale, text, INK);
            assert_eq!(end, ox + text_width(text, scale));
            let mut ink = 0;
            for py in 0..h as i32 {
                for px in 0..w as i32 {
                    let (tx, ty) = ((px - ox).div_euclid(scale), (py - oy).div_euclid(scale));
                    let in_box = (0..text.len() as i32 * font::GLYPH_W).contains(&tx)
                        && (0..font::GLYPH_H).contains(&ty);
                    let set = in_box && {
                        let c = text.as_bytes()[(tx / font::GLYPH_W) as usize] as char;
                        font::glyph(c)[ty as usize] >> (tx % font::GLYPH_W) & 1 != 0
                    };
                    let want = if set { INK } else { TRANSPARENT };
                    assert_eq!(canvas.pixel(px as u32, py as u32), Some(want), "at {px},{py}");
                    ink += u32::from(set);
                }
            }
            assert!(ink > 0);
        }
    }

    #[test]
    fn the_letter_l_is_a_stem_on_the_left_with_a_foot_at_the_bottom() {
        // A shape check that does not go through the font table: whatever
        // the exact glyph, an "L" has ink in its lower-left and none in its
        // upper-right quarter, and its mirror image fails that.
        let mut buf = [0u8; 8 * 8 * 4];
        let mut canvas = Canvas::new(&mut buf, 8, 8).unwrap();
        canvas.draw_text(0, 0, 1, "L", INK);
        let count = |xs: std::ops::Range<u32>, ys: std::ops::Range<u32>| {
            ys.flat_map(|y| xs.clone().map(move |x| (x, y)))
                .filter(|&(x, y)| canvas.pixel(x, y) == Some(INK))
                .count()
        };
        assert!(count(0..4, 4..8) > 0);
        assert_eq!(count(5..8, 0..4), 0);
        assert!(count(0..4, 0..4) > count(4..8, 0..4));
    }

    #[test]
    fn text_far_outside_the_canvas_draws_nothing() {
        let mut buf = [0u8; 8 * 8 * 4];
        let mut canvas = Canvas::new(&mut buf, 8, 8).unwrap();
        canvas.draw_text(i32::MAX - 3, 0, i32::MAX, "overflow", INK);
        canvas.draw_text(-1000, -1000, 2, "off", INK);
        canvas.draw_text(0, 0, 0, "zero scale", INK);
        canvas.draw_text(0, 0, -2, "negative scale", INK);
        assert!(buf.iter().all(|&b| b == 0));
    }
}
