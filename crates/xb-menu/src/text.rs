//! A fixed-capacity ASCII string, so the menu can keep and format text
//! without allocating.

use core::fmt;

/// Up to `N` bytes of printable ASCII, stored inline.
///
/// Text pushed in is made drawable on the way: a character the font lacks
/// becomes `?`, and whatever does not fit is dropped. Being ASCII only, the
/// content can be cut at any byte.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct TextBuf<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> TextBuf<N> {
    /// An empty buffer.
    pub(crate) const fn new() -> Self {
        Self { bytes: [0; N], len: 0 }
    }

    /// A buffer holding as much of `text` as fits.
    pub(crate) fn from_text(text: &str) -> Self {
        let mut buf = Self::new();
        buf.push_str(text);
        buf
    }

    /// Appends as much of `text` as fits.
    pub(crate) fn push_str(&mut self, text: &str) {
        for c in text.chars() {
            let Some(slot) = self.bytes.get_mut(self.len) else { return };
            *slot = if c.is_ascii_graphic() || c == ' ' { c as u8 } else { b'?' };
            self.len += 1;
        }
    }

    /// The content.
    pub(crate) fn as_str(&self) -> &str {
        // Only ASCII is ever stored, so this cannot fail.
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }

    /// The content cut to at most `max` characters, the last two replaced by
    /// `..` when something was cut.
    pub(crate) fn fitted(&self, max: usize) -> Self {
        if self.len <= max {
            return *self;
        }
        let mut out = Self::new();
        out.push_str(&self.as_str()[..max.saturating_sub(2)]);
        out.push_str(&".."[..max.min(2)]);
        out
    }
}

impl<const N: usize> fmt::Write for TextBuf<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.push_str(s);
        Ok(())
    }
}

impl<const N: usize> fmt::Debug for TextBuf<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::fmt::Write as _;

    #[test]
    fn text_past_the_capacity_is_dropped() {
        let mut buf = TextBuf::<4>::from_text("abc");
        buf.push_str("def");
        assert_eq!(buf.as_str(), "abcd");
        write!(buf, "{}", 12345).unwrap();
        assert_eq!(buf.as_str(), "abcd");
    }

    #[test]
    fn characters_the_font_lacks_are_stored_as_question_marks() {
        assert_eq!(TextBuf::<8>::from_text("a\té日~").as_str(), "a???~");
    }

    #[test]
    fn fitting_marks_what_it_cut() {
        let buf = TextBuf::<16>::from_text("GeForce 3 Ti");
        assert_eq!(buf.fitted(12).as_str(), "GeForce 3 Ti");
        assert_eq!(buf.fitted(9).as_str(), "GeForce..");
        assert_eq!(buf.fitted(1).as_str(), ".");
        assert_eq!(buf.fitted(0).as_str(), "");
    }
}
