//! Savestates: the byte stream every part of the machine writes its state to
//! and reads it back from.
//!
//! One function per type serves both directions ([`Stateful::state`] with a
//! [`StateIo`] that is either saving or loading), so what is saved and what
//! is loaded cannot drift apart. Values are little-endian and unpadded;
//! sequences carry their length. The stream is cut into sections
//! ([`StateIo::section`]), each a four-letter tag, a version and a length, so
//! a reader checks it is where the writer was and a part can tell an older
//! layout of its own.
//!
//! A load that runs off the end or meets a wrong tag marks the stream failed
//! ([`StateIo::failed`]) and from then on reads zeroes: the caller checks
//! once at the end. A machine is loaded in place, so the whole stream is
//! checked ([`check`]) before anything is replaced.
//!
//! The layout follows the reference's (sections in load order, each owned by
//! the module whose state it is); the encoding is this crate's own.

use std::collections::VecDeque;

/// The first bytes of a savestate.
pub const MAGIC: [u8; 8] = *b"XBEMUST\x01";

/// A stream being written or read.
#[derive(Debug)]
pub struct StateIo {
    buf: Vec<u8>,
    pos: usize,
    loading: bool,
    /// Where the open section ends (loading).
    limit: usize,
    error: Option<String>,
    /// Probing: the shape of what was visited, instead of its bytes.
    shape: Option<String>,
}

impl StateIo {
    /// An empty stream to save into.
    #[must_use]
    pub const fn save() -> Self {
        Self { buf: Vec::new(), pos: 0, loading: false, limit: 0, error: None, shape: None }
    }

    /// A stream to load from.
    #[must_use]
    pub fn load(bytes: Vec<u8>) -> Self {
        let limit = bytes.len();
        Self { buf: bytes, pos: 0, loading: true, limit, error: None, shape: None }
    }

    /// A stream that keeps no bytes, only the shape of what is saved into
    /// it ([`StateIo::shape`]): for the tests that pin a section's layout.
    #[must_use]
    pub const fn probe() -> Self {
        Self {
            buf: Vec::new(),
            pos: 0,
            loading: false,
            limit: 0,
            error: None,
            shape: Some(String::new()),
        }
    }

    /// What a probe saw: the byte counts in order, with sequences, options
    /// and sections marked, whatever the values were. Two layouts with the
    /// same shape read each other's bytes; a field added, removed, moved or
    /// resized changes it.
    #[must_use]
    pub fn shape(&self) -> &str {
        self.shape.as_deref().unwrap_or("")
    }

    /// [`StateIo::shape`] as a number (FNV-1a), for a test to record.
    #[must_use]
    pub fn shape_hash(&self) -> u64 {
        self.shape().bytes().fold(0xCBF2_9CE4_8422_2325, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01B3)
        })
    }

    /// Adds to a probe's shape; whether this is a probe.
    fn note(&mut self, mark: &str) -> bool {
        match &mut self.shape {
            Some(shape) => {
                shape.push_str(mark);
                true
            }
            None => false,
        }
    }

    /// Whether values are read from the stream (else written to it).
    #[must_use]
    pub const fn loading(&self) -> bool {
        self.loading
    }

    /// The first thing that went wrong, if anything did.
    #[must_use]
    pub fn failed(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Marks the stream failed (the first reason is kept).
    pub fn fail(&mut self, why: impl Into<String>) {
        if self.error.is_none() {
            self.error = Some(why.into());
        }
    }

    /// The bytes written.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// Raw bytes, their number known to both sides.
    pub fn bytes(&mut self, data: &mut [u8]) {
        if self.shape.is_some() {
            let mark = format!("{},", data.len());
            self.note(&mark);
            return;
        }
        if self.loading {
            let end = self.pos.saturating_add(data.len());
            if end > self.limit {
                self.fail(format!("the state ends {} bytes early", end - self.limit));
                data.fill(0);
                return;
            }
            data.copy_from_slice(&self.buf[self.pos..end]);
            self.pos = end;
        } else {
            self.buf.extend_from_slice(data);
        }
    }

    /// A byte string with its length.
    pub fn blob(&mut self, data: &mut Vec<u8>) {
        if self.note("blob,") {
            return;
        }
        let mut n = data.len() as u64;
        n.state(self);
        if self.loading {
            if n > (self.limit - self.pos.min(self.limit)) as u64 {
                self.fail("a byte string runs past the state's end");
                data.clear();
                return;
            }
            data.resize(n as usize, 0);
        }
        self.bytes(data);
    }

    /// One value, either way.
    pub fn io<T: Stateful>(&mut self, value: &mut T) {
        value.state(self);
    }

    /// A section: `tag`, `version` and its length, then what `body` writes or
    /// reads. Loading, `body` gets the version the section was written with
    /// (at most `version`: a newer one fails), and whatever it leaves unread
    /// is skipped.
    pub fn section(&mut self, tag: [u8; 4], version: u32, body: impl FnOnce(&mut Self, u32)) {
        if self.shape.is_some() {
            let mark = format!("{}v{version}{{", String::from_utf8_lossy(&tag));
            self.note(&mark);
            body(self, version);
            self.note("}");
            return;
        }
        if self.loading {
            let mut got = [0u8; 4];
            self.bytes(&mut got);
            let (mut v, mut len) = (0u32, 0u64);
            v.state(self);
            len.state(self);
            if self.error.is_some() {
                return;
            }
            let name = String::from_utf8_lossy(&tag).into_owned();
            if got != tag {
                self.fail(format!(
                    "section {name} expected, {} found",
                    String::from_utf8_lossy(&got)
                ));
                return;
            }
            if v == 0 || v > version {
                self.fail(format!(
                    "section {name} is version {v}, this build reads up to {version}"
                ));
                return;
            }
            let end = self.pos as u64 + len;
            if end > self.limit as u64 {
                self.fail(format!("section {name} runs past the state's end"));
                return;
            }
            let outer = std::mem::replace(&mut self.limit, end as usize);
            body(self, v);
            self.pos = end as usize;
            self.limit = outer;
        } else {
            self.buf.extend_from_slice(&tag);
            self.buf.extend_from_slice(&version.to_le_bytes());
            let at = self.buf.len();
            self.buf.extend_from_slice(&0u64.to_le_bytes());
            body(self, version);
            let len = (self.buf.len() - at - 8) as u64;
            self.buf[at..at + 8].copy_from_slice(&len.to_le_bytes());
        }
    }
}

/// Walks a stream's sections without reading them: the tags in order, or why
/// the stream is not a whole savestate. `bytes` starts after the header the
/// caller wrote.
pub fn check(bytes: &[u8]) -> Result<Vec<[u8; 4]>, String> {
    let mut tags = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let head = bytes.get(pos..pos + 16).ok_or("a section header is cut short")?;
        let tag = [head[0], head[1], head[2], head[3]];
        let len = u64::from_le_bytes(head[8..16].try_into().expect("8 bytes"));
        let end = (pos as u64 + 16).checked_add(len).filter(|&e| e <= bytes.len() as u64);
        let Some(end) = end else {
            return Err(format!("section {} is cut short", String::from_utf8_lossy(&tag)));
        };
        tags.push(tag);
        pos = end as usize;
    }
    Ok(tags)
}

/// A value that can be saved to and loaded from a [`StateIo`].
pub trait Stateful {
    /// Writes the value, or replaces it with the stream's.
    fn state(&mut self, io: &mut StateIo);
}

macro_rules! numbers {
    ($($t:ty),*) => {$(
        impl Stateful for $t {
            fn state(&mut self, io: &mut StateIo) {
                let mut b = self.to_le_bytes();
                io.bytes(&mut b);
                if io.loading() {
                    *self = <$t>::from_le_bytes(b);
                }
            }
        }
    )*};
}
numbers!(u8, u16, u32, u64, i8, i16, i32, i64, f32, f64);

impl Stateful for bool {
    fn state(&mut self, io: &mut StateIo) {
        let mut b = u8::from(*self);
        b.state(io);
        if io.loading() {
            *self = b != 0;
        }
    }
}

impl Stateful for usize {
    fn state(&mut self, io: &mut StateIo) {
        let mut v = *self as u64;
        v.state(io);
        if io.loading() {
            *self = v as usize;
        }
    }
}

impl Stateful for String {
    fn state(&mut self, io: &mut StateIo) {
        let mut b = std::mem::take(self).into_bytes();
        io.blob(&mut b);
        *self = String::from_utf8_lossy(&b).into_owned();
    }
}

impl<T: Stateful + Default> Stateful for Option<T> {
    fn state(&mut self, io: &mut StateIo) {
        if io.note("opt(") {
            T::default().state(io);
            io.note(")");
            return;
        }
        let mut some = self.is_some();
        some.state(io);
        if io.loading() {
            *self = some.then(T::default);
        }
        if let Some(v) = self {
            v.state(io);
        }
    }
}

/// A sequence's length, sane for what is left of the stream.
fn length(io: &mut StateIo, len: usize) -> usize {
    let mut n = len as u64;
    n.state(io);
    if io.loading() && n > (io.limit - io.pos.min(io.limit)) as u64 {
        io.fail("a sequence is longer than what is left of the state");
        return 0;
    }
    n as usize
}

impl<T: Stateful + Default> Stateful for Vec<T> {
    fn state(&mut self, io: &mut StateIo) {
        if io.note("seq(") {
            T::default().state(io);
            io.note(")");
            return;
        }
        let n = length(io, self.len());
        if io.loading() {
            self.clear();
            self.resize_with(n, T::default);
        }
        for v in self.iter_mut() {
            v.state(io);
        }
    }
}

impl<T: Stateful + Default> Stateful for VecDeque<T> {
    fn state(&mut self, io: &mut StateIo) {
        if io.note("seq(") {
            T::default().state(io);
            io.note(")");
            return;
        }
        let n = length(io, self.len());
        if io.loading() {
            self.clear();
            self.resize_with(n, T::default);
        }
        for v in self.iter_mut() {
            v.state(io);
        }
    }
}

impl<T: Stateful, const N: usize> Stateful for [T; N] {
    fn state(&mut self, io: &mut StateIo) {
        for v in self.iter_mut() {
            v.state(io);
        }
    }
}

impl<A: Stateful, B: Stateful> Stateful for (A, B) {
    fn state(&mut self, io: &mut StateIo) {
        self.0.state(io);
        self.1.state(io);
    }
}

impl<A: Stateful, B: Stateful, C: Stateful> Stateful for (A, B, C) {
    fn state(&mut self, io: &mut StateIo) {
        self.0.state(io);
        self.1.state(io);
        self.2.state(io);
    }
}

/// Implements [`Stateful`] for a struct by naming the fields that are state,
/// in order: `state_fields!(Thread { id, priority, wait });`. Fields left out
/// keep the value they have (what a boot made).
#[macro_export]
macro_rules! state_fields {
    ($t:ty { $($f:ident),* $(,)? }) => {
        impl $crate::state::Stateful for $t {
            fn state(&mut self, io: &mut $crate::state::StateIo) {
                $( $crate::state::Stateful::state(&mut self.$f, io); )*
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default, PartialEq)]
    struct Inner {
        a: u32,
        name: String,
        kept: u8,
    }
    state_fields!(Inner { a, name });

    #[derive(Debug, Default, PartialEq)]
    struct Outer {
        flag: bool,
        list: Vec<Inner>,
        queue: VecDeque<(u32, u64)>,
        maybe: Option<i16>,
        table: [u16; 3],
        n: usize,
        x: f32,
    }
    state_fields!(Outer { flag, list, queue, maybe, table, n, x });

    fn sample() -> Outer {
        Outer {
            flag: true,
            list: vec![
                Inner { a: 7, name: "one".into(), kept: 1 },
                Inner { a: 9, name: String::new(), kept: 2 },
            ],
            queue: VecDeque::from([(1, 2), (3, 4)]),
            maybe: Some(-5),
            table: [1, 2, 3],
            n: 123_456,
            x: 1.5,
        }
    }

    #[test]
    fn what_is_saved_loads_back_equal_and_unnamed_fields_are_left_alone() {
        let mut io = StateIo::save();
        io.section(*b"TEST", 2, |io, _| io.io(&mut sample()));
        let bytes = io.into_bytes();
        assert_eq!(check(&bytes).unwrap(), vec![*b"TEST"]);

        let mut io = StateIo::load(bytes);
        let mut back = Outer::default();
        let mut seen = 0;
        io.section(*b"TEST", 3, |io, v| {
            seen = v;
            io.io(&mut back);
        });
        assert_eq!(io.failed(), None);
        assert_eq!(seen, 2, "the body is told the version that wrote the section");
        let mut want = sample();
        for i in &mut want.list {
            i.kept = 0; // not state: a loaded value keeps what it had (the default here)
        }
        assert_eq!(back, want);
    }

    #[test]
    fn a_section_s_unread_tail_is_skipped_and_the_next_one_is_found() {
        let mut io = StateIo::save();
        io.section(*b"AAAA", 1, |io, _| io.io(&mut (1u32, 2u32, 3u32)));
        io.section(*b"BBBB", 1, |io, _| io.io(&mut 9u8));
        let mut io = StateIo::load(io.into_bytes());
        let (mut a, mut b) = (0u32, 0u8);
        io.section(*b"AAAA", 1, |io, _| io.io(&mut a));
        io.section(*b"BBBB", 1, |io, _| io.io(&mut b));
        assert_eq!((a, b, io.failed()), (1, 9, None));
    }

    #[test]
    fn a_wrong_tag_a_newer_version_and_a_short_stream_fail_without_panicking() {
        let mut io = StateIo::save();
        io.section(*b"AAAA", 2, |io, _| io.io(&mut vec![1u64, 2, 3]));
        let bytes = io.into_bytes();

        let mut io = StateIo::load(bytes.clone());
        io.section(*b"BBBB", 2, |_, _| panic!("the body of a wrong section must not run"));
        assert!(io.failed().unwrap().contains("AAAA found"));

        let mut io = StateIo::load(bytes.clone());
        io.section(*b"AAAA", 1, |_, _| panic!("nor a newer one's"));
        assert!(io.failed().unwrap().contains("version 2"));

        let cut = bytes[..bytes.len() - 5].to_vec();
        assert!(check(&cut).is_err());
        let mut io = StateIo::load(cut);
        io.section(*b"AAAA", 2, |_, _| panic!("nor a cut one's"));
        assert!(io.failed().is_some());

        // Inside a section, reading past its end fails and yields zeroes.
        let mut io = StateIo::load(bytes);
        let mut v = (Vec::<u64>::new(), 0xFFu8);
        io.section(*b"AAAA", 2, |io, _| io.io(&mut v));
        assert_eq!(v.0, [1, 2, 3]);
        assert_eq!(v.1, 0);
        assert!(io.failed().is_some());
    }

    #[test]
    fn a_probe_sees_the_layout_and_not_the_values() {
        let shape = |mut v: Outer| {
            let mut io = StateIo::probe();
            io.section(*b"TEST", 2, |io, _| io.io(&mut v));
            assert!(io.failed().is_none());
            (io.shape().to_owned(), io.shape_hash())
        };
        let (full, empty) = (shape(sample()), shape(Outer::default()));
        assert_eq!(full, empty, "the values and the lengths do not matter");
        assert_eq!(full.0, "TESTv2{1,seq(4,blob,)seq(4,8,)opt(2,)2,2,2,8,4,}");

        // A field more is another shape.
        #[derive(Default)]
        struct Wider {
            a: u32,
            b: u8,
        }
        state_fields!(Wider { a, b });
        let mut io = StateIo::probe();
        io.io(&mut Wider::default());
        let mut narrow = StateIo::probe();
        narrow.io(&mut 0u32);
        assert_ne!(io.shape_hash(), narrow.shape_hash());
    }

    #[test]
    fn a_length_larger_than_the_stream_does_not_allocate_it() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&u64::MAX.to_le_bytes());
        let mut io = StateIo::load(bytes);
        let mut v: Vec<u32> = Vec::new();
        io.io(&mut v);
        assert!(v.is_empty() && io.failed().is_some());
    }
}
