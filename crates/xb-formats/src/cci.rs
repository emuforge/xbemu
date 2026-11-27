//! CCI disc images: an XISO compressed a sector at a time with LZ4.
//!
//! The format is the one written by Repackinator and XboxToolkit and read by
//! Cerbios' loader. A split image (`Name.1.cci`, `Name.2.cci`, ..., each part
//! a CCI of its own, made for FATX's 4 GiB file limit) is read from its first
//! part as the one image the parts make in turn.
//!
//! A part's layout (little-endian):
//!
//! - header, 32 bytes: `"CCIM"`, u32 header size (32), u64 the bytes it holds,
//!   u64 the index's offset, u32 block size (2048), u8 version (1), u8 the
//!   index's shift (2), u16 reserved
//! - the blocks, one per sector, in order
//! - the index: a u32 per block and one more (where the blocks end). Each is
//!   the block's offset shifted right by the index's shift. The top bit set
//!   means an LZ4 block (a byte giving the padding's length, then the raw LZ4
//!   block, then the padding); clear, the sector's 2048 bytes as they are.
//!
//! Nothing is kept in memory but the parts' headers. A read takes the index
//! entries of its blocks in one positional read and the blocks themselves in
//! another (they are contiguous), and decodes them into the caller's buffer.
//! There is no lock between reads. The staging buffer for the stored bytes is
//! per thread and reused, so a read allocates nothing after a thread's first.

use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::block::check_range;
use crate::{BlockSource, Error, FileSource, Result, le32, le64};

/// The four bytes a CCI file starts with.
pub const MAGIC: [u8; 4] = *b"CCIM";

/// Where a split image's parts are cut (XboxToolkit's value, 16 MiB short of
/// FATX's 4 GiB file limit): each part but the last ends its blocks at the
/// first block's end past it. An image in one file ends them anywhere else,
/// so "filled to the cut" tells that another part follows.
pub const SPLIT_AT: u64 = 0xFF00_0000;

/// Parts a split image has at most: `Name.1.cci` to `Name.9.cci`.
pub const MAX_PARTS: u32 = 9;

const SECTOR: usize = crate::SECTOR_SIZE as usize;
const SECTOR64: u64 = crate::SECTOR_SIZE as u64;
const HEADER_SIZE: u64 = 32;
const LZ4_FLAG: u32 = 0x8000_0000;
/// Blocks located and fetched by one pair of reads.
const CHUNK: usize = 64;
/// The largest index shift accepted (XboxToolkit writes 2).
const MAX_SHIFT: u8 = 4;
/// A block's stored bytes at most: a sector as it is, or an LZ4 block worth
/// keeping (shorter than the sector) with its padding byte and padding.
const MAX_STORED: u64 = SECTOR64 + 1 + (1 << MAX_SHIFT);

thread_local! {
    /// The stored bytes of the chunk being decoded. Grows to at most
    /// `CHUNK * MAX_STORED` bytes (about 130 KiB) per reading thread.
    static STAGING: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// One file of an image: a CCI with its own header and index.
struct Part {
    src: Box<dyn BlockSource>,
    /// Its first block's number in the whole image.
    first: u64,
    blocks: u64,
    /// Bytes of the image it holds.
    bytes: u64,
    index_off: u64,
    shift: u8,
    /// Filled to the cut: a split image's part with another after it.
    full: bool,
}

/// Where a chunk's blocks are stored: index entries and the offsets they
/// decode to, one more than the blocks.
struct Located {
    entry: [u32; CHUNK + 1],
    at: [u64; CHUNK + 1],
}

impl Part {
    /// Reads and checks a part's header.
    fn open(src: Box<dyn BlockSource>, split_at: u64) -> Result<Self> {
        let file_size = src.len();
        let mut h = [0u8; HEADER_SIZE as usize];
        if file_size < HEADER_SIZE {
            return Err(Error::Cci("shorter than its header".into()));
        }
        src.read_at(0, &mut h)?;
        if h[..4] != MAGIC {
            return Err(Error::Cci("no CCIM magic".into()));
        }
        // The slices are inside the 32-byte header.
        let header_size = le32(&h, 4).unwrap_or(0);
        let size = le64(&h, 8).unwrap_or(0);
        let index_off = le64(&h, 16).unwrap_or(0);
        let block_size = le32(&h, 24).unwrap_or(0);
        let (version, shift) = (h[28], h[29]);
        if u64::from(header_size) != HEADER_SIZE
            || block_size != crate::SECTOR_SIZE
            || version != 1
            || shift > MAX_SHIFT
            || size == 0
        {
            return Err(Error::Cci(format!(
                "of an unknown kind (header {header_size}, block {block_size}, version {version}, shift {shift}, {size} bytes)"
            )));
        }
        let blocks = size.div_ceil(SECTOR64);
        if index_off < HEADER_SIZE
            || index_off > file_size
            || blocks > 0x3FFF_FFFF
            || (blocks + 1) * 4 > file_size - index_off
        {
            return Err(Error::Cci(format!(
                "index is not in its file ({size} bytes, index at {index_off:#x} of {file_size})"
            )));
        }
        let full = index_off >= split_at && index_off - split_at < MAX_STORED;
        if full && !size.is_multiple_of(SECTOR64) {
            // Each part but the last ends on a sector, the next starting the next.
            return Err(Error::Cci(format!(
                "a split image's part of {size} bytes, not whole sectors"
            )));
        }
        Ok(Self { src, first: 0, blocks, bytes: size, index_off, shift, full })
    }

    /// Finds `n` blocks from the part's block `b` (`1 <= n <= CHUNK`) and
    /// reads what they are stored as into `staging`, block `i` at
    /// `at[i] - at[0] .. at[i + 1] - at[0]`.
    fn locate(&self, b: u64, n: usize, staging: &mut Vec<u8>) -> Result<Located> {
        let mut raw = [0u8; (CHUNK + 1) * 4];
        let raw = &mut raw[..(n + 1) * 4];
        self.src.read_at(self.index_off + b * 4, raw)?;
        let mut loc = Located { entry: [0; CHUNK + 1], at: [0; CHUNK + 1] };
        for (i, e) in raw.as_chunks::<4>().0.iter().enumerate() {
            let entry = u32::from_le_bytes(*e);
            let at = u64::from(entry & !LZ4_FLAG) << self.shift;
            if at < HEADER_SIZE
                || at > self.index_off
                || (i > 0 && (at < loc.at[i - 1] || at - loc.at[i - 1] > MAX_STORED))
            {
                return Err(Error::Cci(format!(
                    "block {}'s index entry is out of place",
                    self.first + b + i as u64
                )));
            }
            loc.entry[i] = entry;
            loc.at[i] = at;
        }
        // At most CHUNK * MAX_STORED: every step was checked above.
        let total = (loc.at[n] - loc.at[0]) as usize;
        if staging.len() < total {
            staging.resize(total, 0);
        }
        self.src.read_at(loc.at[0], &mut staging[..total])?;
        Ok(loc)
    }
}

/// Decodes one block's stored bytes into `dst`, a whole sector. False: a
/// block that does not decode to exactly a sector.
fn decode_block(lz4: bool, stored: &[u8], dst: &mut [u8]) -> bool {
    if !lz4 {
        return match stored.get(..SECTOR) {
            Some(s) => {
                dst.copy_from_slice(s);
                true
            }
            None => false,
        };
    }
    let Some((&pad, rest)) = stored.split_first() else {
        return false;
    };
    let Some(packed) = rest.len().checked_sub(usize::from(pad)).and_then(|n| rest.get(..n)) else {
        return false;
    };
    matches!(lz4_flex::block::decompress_into(packed, dst), Ok(SECTOR))
}

/// An open CCI image, all its parts: a [`BlockSource`] of the plain disc image
/// it holds.
pub struct CciImage {
    parts: Vec<Part>,
    /// The image's bytes.
    size: u64,
    /// The bytes its files take on disk.
    stored: u64,
}

impl std::fmt::Debug for CciImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CciImage")
            .field("parts", &self.parts.len())
            .field("size", &self.size)
            .field("stored", &self.stored)
            .finish()
    }
}

impl CciImage {
    /// Opens the CCI at `path`.
    ///
    /// A part filled to the cut ([`SPLIT_AT`]) is a split image's: it must be
    /// named `Name.1.cci` and the next part is opened beside it, and so on to
    /// a part that is not full. A missing part fails the image
    /// ([`Error::MissingPart`]): past it the title would read zeros.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_cut(path.as_ref(), SPLIT_AT)
    }

    /// [`Self::open`] with the cut a test's images were made with.
    fn open_cut(path: &Path, split_at: u64) -> Result<Self> {
        let first = Part::open(Box::new(FileSource::open(path)?), split_at)?;
        if first.full && part_number(path) != Some(1) {
            return Err(Error::Cci(
                "a split image's first part, not named Name.1.cci: no other parts".into(),
            ));
        }
        let mut parts = vec![first];
        while parts.last().is_some_and(|p| p.full) {
            let k = parts.len() as u32 + 1;
            let next = if k <= MAX_PARTS { part_path(path, k) } else { None };
            let Some(next) = next else {
                return Err(Error::Cci(format!("a split image of more than {MAX_PARTS} parts")));
            };
            let part = FileSource::open(&next)
                .and_then(|f| Part::open(Box::new(f), split_at))
                .map_err(|e| {
                    log::info!(target: "disc", "split CCI image: part {k} ({}): {e}", next.display());
                    Error::MissingPart(next.clone())
                })?;
            parts.push(part);
        }
        let image = Self::assemble(parts)?;
        log::info!(
            target: "disc",
            "CCI image, {} MiB in {} MiB ({} part{})",
            image.size >> 20,
            image.stored >> 20,
            image.parts.len(),
            if image.parts.len() == 1 { "" } else { "s" }
        );
        Ok(image)
    }

    /// Makes an image of parts already open, in order.
    ///
    /// Every part but the last must hold whole sectors, and the last must not
    /// be one filled to the cut (its successor would be missing).
    pub fn from_parts(parts: Vec<Box<dyn BlockSource>>) -> Result<Self> {
        Self::from_parts_cut(parts, SPLIT_AT)
    }

    /// [`Self::from_parts`] with the cut a test's images were made with.
    fn from_parts_cut(parts: Vec<Box<dyn BlockSource>>, split_at: u64) -> Result<Self> {
        let parts =
            parts.into_iter().map(|s| Part::open(s, split_at)).collect::<Result<Vec<_>>>()?;
        if parts.last().is_none_or(|p| p.full) {
            return Err(Error::Cci("a split image without its last part".into()));
        }
        Self::assemble(parts)
    }

    fn assemble(mut parts: Vec<Part>) -> Result<Self> {
        let (mut size, mut stored) = (0u64, 0u64);
        let last = parts.len().saturating_sub(1);
        for (k, p) in parts.iter_mut().enumerate() {
            if k != last && !p.bytes.is_multiple_of(SECTOR64) {
                return Err(Error::Cci(format!(
                    "part {} of {} bytes, not whole sectors",
                    k + 1,
                    p.bytes
                )));
            }
            p.first = size / SECTOR64;
            // Each part holds under 2^41 bytes and there are few of them.
            size += p.bytes;
            stored = stored.saturating_add(p.src.len());
        }
        if parts.is_empty() {
            return Err(Error::Cci("no parts".into()));
        }
        Ok(Self { parts, size, stored })
    }

    /// Number of files the image is made of (1 unless it is a split image).
    pub fn parts(&self) -> usize {
        self.parts.len()
    }

    /// The bytes the image's files take on disk, all its parts'.
    pub fn stored_size(&self) -> u64 {
        self.stored
    }

    fn read_range(&self, offset: u64, buf: &mut [u8], staging: &mut Vec<u8>) -> Result<()> {
        // In range (the caller checked), so nothing here overflows.
        let end = offset + buf.len() as u64;
        let mut pos = offset;
        let mut out = buf;
        let mut one = [0u8; SECTOR];
        while pos < end {
            let blk = pos / SECTOR64;
            let part = self
                .parts
                .partition_point(|p| p.first <= blk)
                .checked_sub(1)
                .and_then(|i| self.parts.get(i))
                .filter(|p| blk - p.first < p.blocks)
                .ok_or_else(|| Error::Cci(format!("block {blk} is in no part")))?;
            // Up to a chunk of blocks, to the read's end or the part's.
            let last = (end - 1) / SECTOR64;
            let n = (last - blk + 1).min(CHUNK as u64).min(part.first + part.blocks - blk) as usize;
            let loc = part.locate(blk - part.first, n, staging)?;
            for i in 0..n {
                let start = (blk + i as u64) * SECTOR64;
                let lo = (pos.max(start) - start) as usize;
                let hi = (end.min(start + SECTOR64) - start) as usize;
                let stored = &staging
                    [(loc.at[i] - loc.at[0]) as usize..(loc.at[i + 1] - loc.at[0]) as usize];
                let lz4 = loc.entry[i] & LZ4_FLAG != 0;
                let (dst, rest) = std::mem::take(&mut out).split_at_mut(hi - lo);
                out = rest;
                // A whole sector decodes straight into the reader's buffer, a
                // piece of one through a sector of its own.
                let ok = if dst.len() == SECTOR {
                    decode_block(lz4, stored, dst)
                } else {
                    let ok = decode_block(lz4, stored, &mut one);
                    dst.copy_from_slice(&one[lo..hi]);
                    ok
                };
                if !ok {
                    return Err(Error::Cci(format!("block {} does not decode", blk + i as u64)));
                }
            }
            pos = ((blk + n as u64) * SECTOR64).min(end);
        }
        Ok(())
    }
}

impl BlockSource for CciImage {
    fn len(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.size)?;
        if buf.is_empty() {
            return Ok(());
        }
        STAGING.with(|s| match s.try_borrow_mut() {
            Ok(mut staging) => self.read_range(offset, buf, &mut staging),
            // A part that is itself read through a CCI on this thread: the
            // outer read holds the buffer.
            Err(_) => self.read_range(offset, buf, &mut Vec::new()),
        })
    }
}

/// True when the file at `path` starts with the CCI magic.
pub fn is_cci(path: impl AsRef<Path>) -> bool {
    let mut magic = [0u8; 4];
    FileSource::open(path).is_ok_and(|f| f.read_at(0, &mut magic).is_ok() && magic == MAGIC)
}

/// Splits a file name of the form `Name.N.cci` into `Name`, `N` and the
/// extension as it is spelled. `N` is one or two digits without a leading
/// zero, and `Name` is not empty.
fn split_name(path: &Path) -> Option<(&OsStr, u32, &OsStr)> {
    let ext = path.extension()?;
    if !ext.eq_ignore_ascii_case("cci") {
        return None;
    }
    let stem = Path::new(path.file_stem()?);
    let digits = stem.extension()?.to_str()?;
    let name = stem.file_stem()?;
    let ok = (1..=2).contains(&digits.len())
        && digits.bytes().all(|b| b.is_ascii_digit())
        && !digits.starts_with('0')
        && !name.is_empty();
    ok.then(|| (name, digits.parse().unwrap_or(0), ext))
}

/// The part number of a path named like a split image's part (`Name.N.cci`:
/// N, 1 to 99), else `None`. The name alone does not make a file a part:
/// "Game v1.2.cci" may be an image of its own (see [`is_later_part`]).
pub fn part_number(path: impl AsRef<Path>) -> Option<u32> {
    split_name(path.as_ref()).map(|(_, n, _)| n)
}

/// The path of part `k` of the split image that `path` (`Name.N.cci`) is named
/// as a part of. `None`: not such a name, or `k` outside `1..=MAX_PARTS`.
pub fn part_path(path: impl AsRef<Path>, k: u32) -> Option<PathBuf> {
    let path = path.as_ref();
    let (name, _, ext) = split_name(path)?;
    if !(1..=MAX_PARTS).contains(&k) {
        return None;
    }
    let mut file = OsString::from(name);
    file.push(format!(".{k}."));
    file.push(ext);
    Some(path.with_file_name(file))
}

/// True when `path` is a split image's later part (`Name.2.cci`,
/// `Name.3.cci`, ... beside a `Name.1.cci` that is a split image's first
/// part): not a game of its own, the first part is.
///
/// "Game v1.2.cci" beside an image of its own named "Game v1.1.cci" is a game
/// too, which is why the first part's header decides and not the name.
pub fn is_later_part(path: impl AsRef<Path>) -> bool {
    later_part_cut(path.as_ref(), SPLIT_AT)
}

fn later_part_cut(path: &Path, split_at: u64) -> bool {
    if part_number(path).is_none_or(|n| n < 2) {
        return false;
    }
    part_path(path, 1)
        .and_then(|first| FileSource::open(first).ok())
        .and_then(|f| Part::open(Box::new(f), split_at).ok())
        .is_some_and(|p| p.full)
}

/// Opens the image `path` belongs to when the path may be a later part: the
/// first part's path if `path` is a later part of a split set, else `path`.
pub(crate) fn first_part_of(path: &Path) -> PathBuf {
    match part_path(path, 1) {
        Some(first) if is_later_part(path) => first,
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::MemSource;
    use crate::testutil::{Lcg, TempDir};

    /// The cut the tests' split images are made with.
    const CUT: u64 = 0x8000;
    /// More than a read's chunk several times over.
    const SECTORS: usize = 200;

    /// A plain image: by the sector's number zeros, a short pattern repeated
    /// (matches that overlap what they copy), slow runs, or noise (no LZ4
    /// block shorter than the sector).
    pub(crate) fn make_image(sectors: usize) -> Vec<u8> {
        let mut img = vec![0u8; sectors * SECTOR];
        let mut rng = Lcg(12_345);
        for (s, p) in img.as_chunks_mut::<SECTOR>().0.iter_mut().enumerate() {
            for (i, b) in p.iter_mut().enumerate() {
                *b = match s % 4 {
                    0 => 0,
                    1 => b"abcdefg"[i % 7].wrapping_add(s as u8),
                    2 => ((i / 61) * 3 + s) as u8,
                    _ => rng.next() as u8,
                };
            }
        }
        img
    }

    /// What [`put_cci`] wrote.
    pub(crate) struct Built {
        pub(crate) file: Vec<u8>,
        /// Sectors of the image it took.
        pub(crate) took: usize,
        /// How many of them as LZ4 blocks.
        pub(crate) compressed: usize,
    }

    /// The image's bytes as a CCI, as XboxToolkit writes one: an LZ4 block
    /// where it is shorter than the sector (a byte, the padding's length, then
    /// the block and the padding to `1 << shift`), else the sector as it is.
    /// With `cut` (0: none) a split image's part, its blocks ending at the
    /// first block's end past it.
    pub(crate) fn put_cci(image: &[u8], shift: u8, cut: u64) -> Built {
        let mut file = vec![0u8; HEADER_SIZE as usize];
        let mut index = Vec::new();
        let (mut took, mut compressed) = (0, 0);
        let align = 1usize << shift;
        for chunk in image.chunks(SECTOR) {
            if cut != 0 && file.len() as u64 >= cut {
                break;
            }
            let mut sector = [0u8; SECTOR];
            sector[..chunk.len()].copy_from_slice(chunk);
            let packed = lz4_flex::block::compress(&sector);
            let pos = (file.len() >> shift) as u32;
            if packed.len() + 1 < SECTOR {
                let end = file.len() + 1 + packed.len();
                let pad = (align - end % align) % align;
                index.push(pos | LZ4_FLAG);
                file.push(pad as u8);
                file.extend_from_slice(&packed);
                file.resize(end + pad, 0);
                compressed += 1;
            } else {
                index.push(pos);
                file.extend_from_slice(&sector);
                // Stored sectors keep the index's alignment too.
                assert_eq!(file.len() % align, 0);
            }
            took += 1;
        }
        let index_off = file.len() as u64;
        index.push((file.len() >> shift) as u32);
        for e in index {
            file.extend_from_slice(&e.to_le_bytes());
        }
        let bytes = (took * SECTOR).min(image.len()) as u64;
        file[..4].copy_from_slice(&MAGIC);
        file[4..8].copy_from_slice(&32u32.to_le_bytes());
        file[8..16].copy_from_slice(&bytes.to_le_bytes());
        file[16..24].copy_from_slice(&index_off.to_le_bytes());
        file[24..28].copy_from_slice(&2048u32.to_le_bytes());
        file[28] = 1;
        file[29] = shift;
        Built { file, took, compressed }
    }

    /// The image as a split CCI's parts.
    fn put_split(image: &[u8]) -> Vec<Vec<u8>> {
        let mut parts = Vec::new();
        let mut first = 0;
        while first < image.len() {
            let b = put_cci(&image[first..], 2, CUT);
            assert!(b.took > 0);
            first += b.took * SECTOR;
            parts.push(b.file);
        }
        parts
    }

    fn from_mem(parts: &[Vec<u8>]) -> Result<CciImage> {
        let parts =
            parts.iter().map(|p| Box::new(MemSource(p.clone())) as Box<dyn BlockSource>).collect();
        CciImage::from_parts_cut(parts, CUT)
    }

    /// The image read back through `cci`: whole, in pieces at random places
    /// (a byte, a sector's piece, sectors across chunks), and at its end.
    pub(crate) fn check_image(cci: &dyn BlockSource, image: &[u8]) {
        assert_eq!(cci.len(), image.len() as u64);
        let mut got = vec![0xAAu8; image.len()];
        cci.read_at(0, &mut got).unwrap();
        assert!(got == image, "the whole image differs");
        let mut rng = Lcg(777);
        for k in 0..2000 {
            let off = ((rng.next() << 15 | rng.next()) as usize) % image.len();
            let cap = [3, SECTOR + 500, 150 * SECTOR][k % 3];
            let len = (1 + (rng.next() << 15 | rng.next()) as usize % cap).min(image.len() - off);
            let got = &mut got[..len];
            got.fill(0xAA);
            cci.read_at(off as u64, got).unwrap();
            assert!(got == &image[off..off + len], "read of {len} at {off} differs");
        }
        // Up to the image's end and past it: strict reads refuse, padded
        // reads give zeros.
        let tail = SECTOR + 10;
        let got = &mut got[..3 * SECTOR];
        got.fill(0xAA);
        let at = (image.len() - tail) as u64;
        assert!(matches!(cci.read_at(at, got), Err(Error::OutOfRange { .. })));
        cci.read_padded(at, got).unwrap();
        assert_eq!(&got[..tail], &image[image.len() - tail..]);
        assert!(got[tail..].iter().all(|&b| b == 0));
        cci.read_padded(image.len() as u64 + 5, got).unwrap();
        assert!(got.iter().all(|&b| b == 0));
        cci.read_at(image.len() as u64, &mut []).unwrap();
    }

    /// The reference's own LZ4 block decoder, kept as the oracle for the
    /// library's.
    pub(crate) fn lz4_reference(src: &[u8], dst: &mut [u8]) -> bool {
        let (mut s, mut d) = (0usize, 0usize);
        let run = |s: &mut usize, mut len: usize| -> Option<usize> {
            if len == 15 {
                loop {
                    let b = *src.get(*s)?;
                    *s += 1;
                    len += usize::from(b);
                    if b != 255 {
                        break;
                    }
                }
            }
            Some(len)
        };
        while s < src.len() {
            let token = src[s];
            s += 1;
            let Some(lit) = run(&mut s, usize::from(token >> 4)) else {
                return false;
            };
            if lit > src.len() - s || lit > dst.len() - d {
                return false;
            }
            dst[d..d + lit].copy_from_slice(&src[s..s + lit]);
            d += lit;
            s += lit;
            if s == src.len() {
                break;
            }
            if src.len() - s < 2 {
                return false;
            }
            let back = usize::from(src[s]) | usize::from(src[s + 1]) << 8;
            s += 2;
            if back == 0 || back > d {
                return false;
            }
            let Some(len) = run(&mut s, usize::from(token & 15)) else {
                return false;
            };
            let len = len + 4;
            if len > dst.len() - d {
                return false;
            }
            for _ in 0..len {
                dst[d] = dst[d - back];
                d += 1;
            }
        }
        d == dst.len()
    }

    #[test]
    fn an_image_in_one_file_reads_back_as_the_image_it_was_made_from() {
        let image = make_image(SECTORS);
        let b = put_cci(&image, 2, 0);
        assert_eq!(b.took, SECTORS);
        // Both kinds of block are in it.
        assert!(b.compressed > 0 && b.compressed < SECTORS);
        let cci = from_mem(std::slice::from_ref(&b.file)).unwrap();
        assert_eq!(cci.parts(), 1);
        assert_eq!(cci.stored_size(), b.file.len() as u64);
        check_image(&cci, &image);
    }

    #[test]
    fn every_index_shift_up_to_four_reads_back_and_a_larger_one_is_refused() {
        let image = make_image(40);
        for shift in 0..=MAX_SHIFT {
            let b = put_cci(&image, shift, 0);
            check_image(&from_mem(&[b.file]).unwrap(), &image);
        }
        let mut b = put_cci(&image, 2, 0);
        b.file[29] = MAX_SHIFT + 1;
        assert!(matches!(from_mem(&[b.file]), Err(Error::Cci(_))));
    }

    #[test]
    fn an_image_that_ends_inside_a_sector_reads_back_to_its_last_byte() {
        let mut image = make_image(10);
        image.truncate(9 * SECTOR + 700);
        let b = put_cci(&image, 2, 0);
        let cci = from_mem(&[b.file]).unwrap();
        check_image(&cci, &image);
    }

    #[test]
    fn a_split_image_reads_back_across_its_parts_as_one_image() {
        let image = make_image(SECTORS);
        let parts = put_split(&image);
        assert!(parts.len() >= 3, "the cut makes several parts");
        let cci = from_mem(&parts).unwrap();
        assert_eq!(cci.parts(), parts.len());
        check_image(&cci, &image);
        // Without its last part it is no image.
        assert!(matches!(from_mem(&parts[..parts.len() - 1]), Err(Error::Cci(_))));
    }

    #[test]
    fn a_split_image_on_disk_opens_from_its_first_part_and_fails_with_a_part_missing() {
        let dir = TempDir::new("cci-split");
        let image = make_image(SECTORS);
        let parts = put_split(&image);
        for (k, p) in parts.iter().enumerate() {
            std::fs::write(dir.path().join(format!("Game (USA).{}.cci", k + 1)), p).unwrap();
        }
        let first = dir.path().join("Game (USA).1.cci");
        let cci = CciImage::open_cut(&first, CUT).unwrap();
        assert_eq!(cci.parts(), parts.len());
        assert_eq!(cci.stored_size(), parts.iter().map(|p| p.len() as u64).sum::<u64>());
        check_image(&cci, &image);

        // The later parts are no games of their own; the first is.
        let second = dir.path().join("Game (USA).2.cci");
        assert!(later_part_cut(&second, CUT));
        assert!(!later_part_cut(&first, CUT));

        // The first part under another name has no other parts.
        let alone = dir.path().join("Game.cci");
        std::fs::copy(&first, &alone).unwrap();
        assert!(matches!(CciImage::open_cut(&alone, CUT), Err(Error::Cci(_))));

        // A part missing fails the image.
        let last = dir.path().join(format!("Game (USA).{}.cci", parts.len()));
        std::fs::remove_file(&last).unwrap();
        match CciImage::open_cut(&first, CUT) {
            Err(Error::MissingPart(p)) => assert_eq!(p, last),
            other => panic!("expected a missing part, got {other:?}"),
        }
    }

    #[test]
    fn two_images_of_their_own_named_like_parts_are_two_images() {
        let dir = TempDir::new("cci-named");
        let (a, b) = (make_image(8), make_image(12));
        let (pa, pb) = (dir.path().join("Game v1.1.cci"), dir.path().join("Game v1.2.cci"));
        std::fs::write(&pa, put_cci(&a, 2, 0).file).unwrap();
        std::fs::write(&pb, put_cci(&b, 2, 0).file).unwrap();
        assert!(!later_part_cut(&pb, CUT));
        assert!(!is_later_part(&pb));
        assert_eq!(first_part_of(&pb), pb);
        check_image(&CciImage::open_cut(&pa, CUT).unwrap(), &a);
        check_image(&CciImage::open(&pb).unwrap(), &b);
        assert!(is_cci(&pa));
    }

    #[test]
    fn part_names_are_one_or_two_digits_between_a_name_and_the_extension() {
        assert_eq!(part_number("Halo.1.cci"), Some(1));
        assert_eq!(part_number("/x/y.z/Halo 2 (USA).12.CCI"), Some(12));
        assert_eq!(part_number("a.9.Cci"), Some(9));
        for no in [
            "Halo.cci",
            "Halo.0.cci",
            "Halo.01.cci",
            "Halo.123.cci",
            ".1.cci",
            "Halo.1.iso",
            "Halo.x.cci",
            "1.cci",
        ] {
            assert_eq!(part_number(no), None, "{no}");
        }
        assert_eq!(part_path("/d/Halo.1.cci", 2), Some(PathBuf::from("/d/Halo.2.cci")));
        assert_eq!(part_path("/d/Halo.12.CCI", 1), Some(PathBuf::from("/d/Halo.1.CCI")));
        assert_eq!(part_path("/d/Halo.1.cci", 0), None);
        assert_eq!(part_path("/d/Halo.1.cci", MAX_PARTS + 1), None);
        assert_eq!(part_path("/d/Halo.cci", 2), None);
    }

    #[test]
    fn a_header_of_another_kind_is_refused() {
        let image = make_image(8);
        let good = put_cci(&image, 2, 0).file;
        let index_off = le64(&good, 16).unwrap();
        let cases: Vec<(usize, Vec<u8>)> = vec![
            (0, b"CCIX".to_vec()),
            (4, 33u32.to_le_bytes().to_vec()),
            (8, 0u64.to_le_bytes().to_vec()),
            // More blocks than the index has room for.
            (8, (9 * 2048u64).to_le_bytes().to_vec()),
            (8, u64::MAX.to_le_bytes().to_vec()),
            (16, 31u64.to_le_bytes().to_vec()),
            (16, (index_off + 8).to_le_bytes().to_vec()),
            (16, u64::MAX.to_le_bytes().to_vec()),
            (24, 4096u32.to_le_bytes().to_vec()),
            (28, vec![2]),
        ];
        for (at, bytes) in cases {
            let mut f = good.clone();
            f[at..at + bytes.len()].copy_from_slice(&bytes);
            assert!(matches!(from_mem(&[f]), Err(Error::Cci(_))), "field at {at} = {bytes:?}");
        }
        for len in [0, 3, 31] {
            assert!(from_mem(&[good[..len].to_vec()]).is_err());
        }
        assert!(from_mem(&[]).is_err());
    }

    #[test]
    fn an_index_entry_out_of_place_fails_the_read_of_its_blocks_only() {
        let image = make_image(SECTORS);
        let good = put_cci(&image, 2, 0).file;
        let index_off = le64(&good, 16).unwrap() as usize;
        let entry = |f: &[u8], i: usize| le32(f, index_off + i * 4).unwrap();
        let bad_values = [
            0u32,                                  // inside the header
            (index_off as u32 >> 2) + 1,           // past the index
            entry(&good, 99) & !LZ4_FLAG,          // before its predecessor
            (entry(&good, 100) & !LZ4_FLAG) + 600, // its predecessor too long
        ];
        for v in bad_values {
            let mut f = good.clone();
            f[index_off + 100 * 4..][..4].copy_from_slice(&v.to_le_bytes());
            let cci = from_mem(&[f]).unwrap();
            let mut buf = vec![0u8; 4 * SECTOR];
            assert!(matches!(cci.read_at(99 * 2048, &mut buf), Err(Error::Cci(_))), "entry {v:#x}");
            // Blocks whose entries are sound still read.
            cci.read_at(0, &mut buf).unwrap();
            assert_eq!(buf, &image[..buf.len()]);
        }
    }

    #[test]
    fn damaged_blocks_fail_or_decode_without_a_panic() {
        let image = make_image(SECTORS);
        let good = put_cci(&image, 2, 0).file;
        let index_off = le64(&good, 16).unwrap() as usize;
        let mut rng = Lcg(4242);
        let mut buf = vec![0u8; image.len()];
        let mut failed = 0;
        for _ in 0..300 {
            let mut f = good.clone();
            // A few bits of the blocks, or of the index.
            for _ in 0..3 {
                let at = 32 + (rng.next() << 15 | rng.next()) as usize % (f.len() - 32);
                f[at] ^= 1 << (rng.next() % 8);
            }
            let cci = from_mem(&[f]).unwrap();
            failed += usize::from(cci.read_at(0, &mut buf).is_err());
        }
        // Flipped bits in LZ4 blocks and index entries are mostly caught;
        // none may panic or read outside a buffer.
        assert!(failed > 0);

        // A padding length longer than the block, and an LZ4 flag on a stored
        // sector of noise.
        let first_lz4 = (0..SECTORS)
            .find(|&i| le32(&good, index_off + i * 4).unwrap() & LZ4_FLAG != 0)
            .unwrap();
        let at = ((le32(&good, index_off + first_lz4 * 4).unwrap() & !LZ4_FLAG) as usize) << 2;
        let mut f = good.clone();
        f[at] = 0xFF;
        let cci = from_mem(&[f]).unwrap();
        assert!(matches!(
            cci.read_at(first_lz4 as u64 * 2048, &mut buf[..SECTOR]),
            Err(Error::Cci(_))
        ));
    }

    #[test]
    fn the_library_decoder_agrees_with_the_reference_decoder() {
        let image = make_image(64);
        let mut rng = Lcg(99);
        let (mut a, mut b) = ([0u8; SECTOR], [0u8; SECTOR]);
        for sector in image.as_chunks::<SECTOR>().0 {
            let packed = lz4_flex::block::compress(sector);
            assert!(lz4_reference(&packed, &mut a));
            assert!(matches!(lz4_flex::block::decompress_into(&packed, &mut b), Ok(SECTOR)));
            assert!(a == *sector && b == *sector);
            // Damaged: whenever both accept the block they decode it alike.
            for _ in 0..50 {
                let mut bad = packed.clone();
                let at = rng.next() as usize % bad.len();
                bad[at] ^= 1 << (rng.next() % 8);
                let ra = lz4_reference(&bad, &mut a);
                let rb = matches!(lz4_flex::block::decompress_into(&bad, &mut b), Ok(SECTOR));
                if ra && rb {
                    assert!(a == b);
                }
            }
        }
        // Overlapping matches and an empty block.
        assert!(!lz4_reference(&[], &mut a));
        assert!(!decode_block(true, &[0], &mut b));
        assert!(!decode_block(true, &[], &mut b));
        assert!(!decode_block(false, &[0; SECTOR - 1], &mut b));
    }

    #[test]
    fn several_threads_read_one_image_at_once() {
        let image = make_image(SECTORS);
        let cci = from_mem(&put_split(&image)).unwrap();
        std::thread::scope(|s| {
            for t in 0..4u32 {
                let (cci, image) = (&cci, &image);
                s.spawn(move || {
                    let mut rng = Lcg(t + 1);
                    let mut buf = vec![0u8; 70 * SECTOR];
                    for _ in 0..300 {
                        let off = (rng.next() << 15 | rng.next()) as usize % image.len();
                        let len = (1 + rng.next() as usize * 5 % buf.len()).min(image.len() - off);
                        cci.read_at(off as u64, &mut buf[..len]).unwrap();
                        assert!(buf[..len] == image[off..off + len]);
                    }
                });
            }
        });
    }

    #[test]
    fn a_cci_whose_part_is_read_through_another_cci_still_reads() {
        // The inner image's plain bytes are themselves a CCI file.
        let image = make_image(16);
        let inner_file = put_cci(&image, 2, 0).file;
        let outer_file = put_cci(&inner_file, 2, 0).file;
        let outer = from_mem(&[outer_file]).unwrap();
        assert_eq!(outer.len(), inner_file.len() as u64);
        let inner = CciImage::from_parts_cut(vec![Box::new(outer)], CUT).unwrap();
        check_image(&inner, &image);
    }

    #[test]
    fn the_game_image_decodes_alike_however_it_is_read() {
        let Some(path) = xb_testkit::any_title() else { return };
        let path = first_part_of(&path);
        if !is_cci(&path) {
            println!("cci: {} is not a CCI, nothing checked", path.display());
            return;
        }
        let cci = CciImage::open(&path).unwrap();
        println!(
            "cci: {} MiB in {} MiB, {} part(s)",
            cci.len() >> 20,
            cci.stored_size() >> 20,
            cci.parts()
        );
        assert!(cci.len() >= 33 * SECTOR64);

        // The same range read in large pieces, in odd pieces, and decoded by
        // the reference's decoder block by block.
        let span = (48usize << 20).min(cci.len() as usize) / SECTOR * SECTOR;
        let starts = [0u64, (cci.len() / 2) / SECTOR64 * SECTOR64, cci.len() - span as u64];
        let mut big = vec![0u8; span];
        let mut odd = vec![0u8; span];
        let mut staging = Vec::new();
        for start in starts {
            let t = std::time::Instant::now();
            for (i, c) in big.chunks_mut(1 << 20).enumerate() {
                cci.read_at(start + ((i as u64) << 20), c).unwrap();
            }
            let secs = t.elapsed().as_secs_f64();
            println!(
                "cci: {} MiB at {start:#x} in {:.0} ms ({:.0} MiB/s)",
                span >> 20,
                secs * 1e3,
                (span >> 20) as f64 / secs
            );
            let mut at = 0;
            let mut k = 0usize;
            while at < span {
                let len = [1usize, 2047, 2049, 5000, 300_001, 64 * 2048 + 1][k % 6].min(span - at);
                cci.read_at(start + at as u64, &mut odd[at..at + len]).unwrap();
                at += len;
                k += 1;
            }
            assert!(big == odd, "piecewise reads differ at {start:#x}");

            let mut one = [0u8; SECTOR];
            let (mut lz4, mut plain) = (0u32, 0u32);
            for (i, want) in big.as_chunks::<SECTOR>().0.iter().enumerate().step_by(7) {
                let blk = start / SECTOR64 + i as u64;
                let part = cci.parts.iter().rev().find(|p| p.first <= blk).unwrap();
                let loc = part.locate(blk - part.first, 1, &mut staging).unwrap();
                let stored = &staging[..(loc.at[1] - loc.at[0]) as usize];
                if loc.entry[0] & LZ4_FLAG != 0 {
                    let packed = &stored[1..stored.len() - usize::from(stored[0])];
                    assert!(lz4_reference(packed, &mut one), "block {blk}");
                    lz4 += 1;
                } else {
                    one.copy_from_slice(&stored[..SECTOR]);
                    plain += 1;
                }
                assert!(one == *want, "block {blk} differs between the decoders");
            }
            println!(
                "cci: at {start:#x} checked {lz4} LZ4 and {plain} stored blocks against the reference decoder"
            );
        }
    }
}
