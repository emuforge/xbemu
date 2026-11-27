//! The disc: a read-only volume over a file tree someone else parses.
//!
//! The XDVDFS parser itself is not here: this module declares [`DiscTree`],
//! and an adapter over the parser implements it.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::namespace::DeviceKind;
use crate::status::NtStatus;
use crate::types::{
    CreateAction, CreateDisposition, DISC_TIME, FileFsSizeInformation, attributes, options,
};
use crate::volume::{
    DirEntry, FileFacts, LISTING_MAX, LISTING_NAME_MAX, OpenParams, SortKey, Volume, VolumeFile,
};

/// Bytes in a disc sector.
pub const DISC_SECTOR: u32 = 2048;

/// Sectors the disc volume reports: a full DVD-9 (7 GiB).
pub const DISC_TOTAL_SECTORS: u64 = 7 * 1024 * 1024 * 1024 / DISC_SECTOR as u64;

/// A file or directory of a disc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscNode {
    /// The name as the disc spells it; empty for the root.
    pub name: String,
    /// The first sector of the file's data (of a directory's table). It
    /// identifies the node to [`DiscTree::read`] and [`DiscTree::list`], and
    /// titles see it as the file's index and physical sort key.
    pub sector: u32,
    /// Bytes of data; for a directory, of its table.
    pub size: u64,
    /// A directory.
    pub is_dir: bool,
}

/// A disc's file tree, as this crate needs it. Implemented by the integrator
/// over the XDVDFS parser; [`MemDisc`] is the in-memory one.
pub trait DiscTree: Send + Sync {
    /// Finds a path inside the disc: components separated by `\` (a leading
    /// one or none), matched in any ASCII case. The empty path is the root
    /// directory.
    fn lookup(&self, path: &str) -> Option<DiscNode>;

    /// The entries of a directory, in the order the title should see them
    /// (the reference walks the directory table's tree in order, then any
    /// entry the tree does not reach).
    fn list(&self, dir: &DiscNode) -> Vec<DiscNode>;

    /// Fills `buf` from `offset` of a file's data; false on a read error.
    /// The caller stays inside the file's size. Called from several threads
    /// at once.
    fn read(&self, file: &DiscNode, offset: u64, buf: &mut [u8]) -> bool;
}

/// The disc as a run of bytes, for reads of `\Device\CdRom0` itself.
pub trait RawBlocks: Send + Sync {
    /// Bytes of the volume (the game partition).
    fn len(&self) -> u64;

    /// True for an empty device.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Fills `buf` from byte `offset` of the volume; false on a read error.
    /// The caller stays inside [`len`](Self::len).
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> bool;
}

/// The volume of `\Device\CdRom0`.
pub struct DiscVolume {
    tree: Arc<dyn DiscTree>,
    raw: Option<Arc<dyn RawBlocks>>,
}

impl DiscVolume {
    /// A volume over `tree`. Reads of the device itself are refused
    /// (`STATUS_INVALID_DEVICE_REQUEST`), as in the reference.
    #[must_use]
    pub fn new(tree: Arc<dyn DiscTree>) -> Self {
        Self { tree, raw: None }
    }

    /// A volume whose device handle (`\Device\CdRom0` opened as such) reads
    /// the disc's bytes from `raw`. This goes past the reference, which
    /// refuses those reads.
    #[must_use]
    pub fn with_raw(tree: Arc<dyn DiscTree>, raw: Arc<dyn RawBlocks>) -> Self {
        Self { tree, raw: Some(raw) }
    }

    /// The file tree.
    #[must_use]
    pub fn tree(&self) -> &Arc<dyn DiscTree> {
        &self.tree
    }
}

impl Volume for DiscVolume {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Disc
    }

    fn open(
        &self,
        rel: &str,
        params: &OpenParams,
    ) -> Result<(Box<dyn VolumeFile>, CreateAction), NtStatus> {
        if !matches!(params.disposition, CreateDisposition::Open | CreateDisposition::OpenIf) {
            return Err(NtStatus::MEDIA_WRITE_PROTECTED);
        }
        let node = match self.tree.lookup(rel) {
            Some(node) => node,
            None => match language_fallback(self.tree.as_ref(), rel) {
                Some((english, node)) => {
                    log::info!(target: "fs", "open CdRom0{rel}: not on the disc, its English one instead ({english})");
                    node
                }
                None => {
                    log::info!(target: "fs", "open CdRom0{rel}: not on the disc");
                    return Err(NtStatus::OBJECT_NAME_NOT_FOUND);
                }
            },
        };
        if params.options & options::FILE_DIRECTORY_FILE != 0 && !node.is_dir {
            return Err(NtStatus::NOT_A_DIRECTORY);
        }
        if params.options & options::FILE_NON_DIRECTORY_FILE != 0 && node.is_dir {
            return Err(NtStatus::FILE_IS_A_DIRECTORY);
        }
        let is_volume = rel.is_empty();
        let sort_sector = sort_sector(self.tree.as_ref(), rel, &node);
        Ok((
            Box::new(DiscFile {
                tree: Arc::clone(&self.tree),
                raw: if is_volume { self.raw.clone() } else { None },
                node,
                is_volume,
                sort_sector,
            }),
            CreateAction::Opened,
        ))
    }

    fn delete(&self, _rel: &str) -> NtStatus {
        NtStatus::ACCESS_DENIED // no file system to delete in
    }

    fn rename(&self, _: &dyn VolumeFile, _: &dyn Volume, _: &str, _: bool) -> NtStatus {
        NtStatus::ACCESS_DENIED
    }

    fn size_information(&self) -> FileFsSizeInformation {
        FileFsSizeInformation {
            total_allocation_units: DISC_TOTAL_SECTORS,
            available_allocation_units: 0,
            sectors_per_allocation_unit: 1,
            bytes_per_sector: DISC_SECTOR,
        }
    }
}

/// An open file or directory of the disc, or the disc itself.
struct DiscFile {
    tree: Arc<dyn DiscTree>,
    raw: Option<Arc<dyn RawBlocks>>,
    node: DiscNode,
    is_volume: bool,
    sort_sector: u32,
}

impl VolumeFile for DiscFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn is_dir(&self) -> bool {
        self.node.is_dir
    }

    fn is_volume(&self) -> bool {
        self.is_volume
    }

    fn is_raw(&self) -> bool {
        self.raw.is_some()
    }

    fn size(&self) -> u64 {
        if self.node.is_dir { 0 } else { self.node.size }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, NtStatus> {
        if let Some(raw) = &self.raw {
            // Past the end of the device: zeros, as unkept raw sectors read.
            let n = raw.len().saturating_sub(offset).min(buf.len() as u64) as usize;
            buf[n..].fill(0);
            return if raw.read_at(offset, &mut buf[..n]) {
                Ok(buf.len())
            } else {
                Err(NtStatus::DEVICE_DATA_ERROR)
            };
        }
        if self.node.is_dir {
            return Err(NtStatus::INVALID_DEVICE_REQUEST);
        }
        if self.tree.read(&self.node, offset, buf) {
            Ok(buf.len())
        } else {
            log::info!(target: "fs", "read {} +{offset:x} ({}) failed", self.node.name, buf.len());
            Err(NtStatus::DEVICE_DATA_ERROR)
        }
    }

    fn write_at(&self, _offset: u64, _data: &[u8]) -> Result<usize, NtStatus> {
        Err(NtStatus::ACCESS_DENIED)
    }

    fn set_end_of_file(&self, _size: u64) -> NtStatus {
        NtStatus::ACCESS_DENIED
    }

    fn flush(&self) -> NtStatus {
        NtStatus::SUCCESS
    }

    fn facts(&self) -> FileFacts {
        let dir = if self.node.is_dir { attributes::FILE_ATTRIBUTE_DIRECTORY } else { 0 };
        FileFacts {
            creation_time: DISC_TIME,
            last_access_time: DISC_TIME,
            last_write_time: DISC_TIME,
            change_time: DISC_TIME,
            size: self.size(),
            attributes: dir | attributes::FILE_ATTRIBUTE_READONLY,
        }
    }

    fn index_number(&self) -> u64 {
        if self.is_volume { 0 } else { u64::from(self.node.sector) }
    }

    fn sort_key(&self) -> SortKey {
        if self.is_volume {
            SortKey::None
        } else {
            SortKey::Gdfx { start_sector: self.sort_sector }
        }
    }

    fn set_times(&self, _: Option<u64>, _: Option<u64>) -> NtStatus {
        NtStatus::SUCCESS
    }

    fn set_delete_on_close(&self, delete: bool) -> NtStatus {
        if delete { NtStatus::ACCESS_DENIED } else { NtStatus::SUCCESS }
    }

    fn list(&self) -> Vec<DirEntry> {
        if !self.node.is_dir {
            return Vec::new();
        }
        self.tree
            .list(&self.node)
            .into_iter()
            .filter(|n| n.name.chars().count() < LISTING_NAME_MAX)
            .take(LISTING_MAX)
            .map(|n| DirEntry { name: n.name, size: n.size, is_dir: n.is_dir })
            .collect()
    }
}

/// A disc file's start sector as titles should see it.
///
/// TOCA Race Driver 3's `.big` archives (`BIGF`) keep, at +0x20 of their
/// header, where they sat on the pressed disc from `default.xbe`: the low 16
/// bits of the sector difference, bytes lo hi lo hi, XORed with 538F5B39.
/// Opening one, the title compares it with the two sort keys, and on a
/// rebuilt image none matched: it dropped the special event its World Tour
/// starts on. Such a file gets the sector it had on the pressed disc instead:
/// `default.xbe`'s plus the difference. A disc laid out as pressed gives the
/// same number as its own sector; a file whose header is not that pattern
/// keeps its sector.
fn sort_sector(tree: &dyn DiscTree, rel: &str, node: &DiscNode) -> u32 {
    let is_big = rel.len() >= 4 && rel.as_bytes()[rel.len() - 4..].eq_ignore_ascii_case(b".big");
    let mut h = [0u8; 0x24];
    if node.is_dir
        || node.size < h.len() as u64
        || !is_big
        || !tree.read(node, 0, &mut h)
        || &h[..4] != b"BIGF"
    {
        return node.sector;
    }
    let e = u32::from_le_bytes([h[0x20], h[0x21], h[0x22], h[0x23]]) ^ 0x538F_5B39;
    if e >> 16 != e & 0xFFFF {
        return node.sector;
    }
    let Some(xbe) = tree.lookup("default.xbe") else { return node.sector };
    let delta = (e >> 24) | ((e >> 16) & 0xFF) << 8;
    if node.sector.wrapping_sub(xbe.sector) & 0xFFFF == delta {
        return node.sector;
    }
    xbe.sector.wrapping_add(delta)
}

/// Other languages' names in paths, with English's.
const LANGUAGE_NAMES: [(&str, &str); 28] = [
    ("french", "english"),
    ("german", "english"),
    ("italian", "english"),
    ("spanish", "english"),
    ("japanese", "english"),
    ("korean", "english"),
    ("chinese", "english"),
    ("portuguese", "english"),
    ("fre", "eng"),
    ("fra", "eng"),
    ("ger", "eng"),
    ("deu", "eng"),
    ("ita", "eng"),
    ("spa", "eng"),
    ("esp", "eng"),
    ("jpn", "eng"),
    ("jap", "eng"),
    ("kor", "eng"),
    ("chi", "eng"),
    ("por", "eng"),
    ("fr", "en"),
    ("de", "en"),
    ("it", "en"),
    ("es", "en"),
    ("ja", "en"),
    ("ko", "en"),
    ("zh", "en"),
    ("pt", "en"),
];

/// A disc path in a language the disc does not have (a console set to
/// Spanish, a game without it, or a dump with its other languages taken out):
/// the same path in English, the language every title has, when that is
/// there. A path component (or a file name's stem) naming a language is what
/// is swapped, in the same case as the title wrote it. Returns the English
/// path and its node.
#[must_use]
pub fn language_fallback(tree: &dyn DiscTree, rel: &str) -> Option<(String, DiscNode)> {
    let bytes = rel.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        let end =
            bytes[start..].iter().position(|&b| b == b'\\').map_or(bytes.len(), |p| start + p);
        let stem = bytes[start..end].iter().position(|&b| b == b'.').map_or(end, |p| start + p);
        for (other, english) in LANGUAGE_NAMES {
            if !bytes[start..stem].eq_ignore_ascii_case(other.as_bytes()) {
                continue;
            }
            // ENGLISH, English or english, as the title's own name is cased.
            let first_up = bytes[start].is_ascii_uppercase();
            let all_up = first_up && (other.len() < 2 || bytes[start + 1].is_ascii_uppercase());
            let replacement: String = english
                .chars()
                .enumerate()
                .map(
                    |(i, c)| {
                        if all_up || (i == 0 && first_up) { c.to_ascii_uppercase() } else { c }
                    },
                )
                .collect();
            let path = format!("{}{replacement}{}", &rel[..start], &rel[stem..]);
            if let Some(node) = tree.lookup(&path) {
                return Some((path, node));
            }
        }
        start = end + 1;
    }
    None
}

/// Folder names for the console's language setting (`XC_LANGUAGE`: 1 English
/// to 9 Portuguese).
const LANGUAGE_DIRS: [&[&str]; 10] = [
    &[],
    &["english", "eng", "en"],
    &["japanese", "jpn", "jap", "ja", "jp"],
    &["german", "ger", "deu", "de"],
    &["french", "fre", "fra", "fr"],
    &["spanish", "spa", "esp", "es"],
    &["italian", "ita", "it"],
    &["korean", "kor", "ko", "kr"],
    &["chinese", "chi", "zh", "cn"],
    &["portuguese", "por", "pt"],
];

/// True if the disc has a folder for English somewhere in its first three
/// levels of folders but none, beside an English one, for `language`
/// (`XC_LANGUAGE`, 2 to 9): the title cannot run in that language, and the
/// kernel reports English instead.
#[must_use]
pub fn disc_lacks_language(tree: &dyn DiscTree, language: u32) -> bool {
    if !(2..=9).contains(&language) {
        return false;
    }
    let Some(root) = tree.lookup("") else { return false };
    let named =
        |name: &str, lang: usize| LANGUAGE_DIRS[lang].iter().any(|d| d.eq_ignore_ascii_case(name));
    // Breadth first, up to 64 folders, 16 subfolders of each.
    let mut queue = vec![(root, 0u32)];
    let mut head = 0;
    let mut lacks = false;
    while head < queue.len() {
        let (dir, depth) = queue[head].clone();
        head += 1;
        let (mut english, mut other) = (false, false);
        let mut subs = Vec::new();
        for entry in tree.list(&dir).into_iter().filter(|e| e.is_dir) {
            english |= named(&entry.name, 1);
            other |= named(&entry.name, language as usize);
            if subs.len() < 16 {
                subs.push(entry);
            }
        }
        if english && other {
            return false;
        }
        lacks |= english;
        for sub in subs {
            if depth < 2 && queue.len() < 64 {
                queue.push((sub, depth + 1));
            }
        }
    }
    lacks
}

/// A disc held in memory, for tests and for homebrew handed over as loose
/// files.
///
/// Files get sectors in the order they were added, from sector 265 on (where
/// a rebuilt image's files start), one file after another.
#[derive(Default)]
pub struct MemDisc {
    /// Directory key (upper-cased path, `""` for the root) to its entries.
    dirs: BTreeMap<String, Vec<DiscNode>>,
    data: BTreeMap<u32, Vec<u8>>,
    next_sector: u32,
}

impl MemDisc {
    /// An empty disc.
    #[must_use]
    pub fn new() -> Self {
        let mut dirs = BTreeMap::new();
        dirs.insert(String::new(), Vec::new());
        Self { dirs, data: BTreeMap::new(), next_sector: 265 }
    }

    fn take_sectors(&mut self, bytes: u64) -> u32 {
        let sector = self.next_sector;
        self.next_sector +=
            u32::try_from(bytes.div_ceil(u64::from(DISC_SECTOR)).max(1)).unwrap_or(1);
        sector
    }

    /// Makes the directory `path` (components separated by `\` or `/`) and
    /// its parents.
    pub fn add_dir(&mut self, path: &str) -> &mut Self {
        let mut key = String::new();
        for part in path.split(['\\', '/']).filter(|p| !p.is_empty()) {
            let child = format!("{key}\\{}", part.to_ascii_uppercase());
            if !self.dirs.contains_key(&child) {
                let sector = self.take_sectors(1);
                self.dirs.insert(child.clone(), Vec::new());
                let node = DiscNode {
                    name: part.to_owned(),
                    sector,
                    size: u64::from(DISC_SECTOR),
                    is_dir: true,
                };
                self.dirs.entry(key).or_default().push(node);
            }
            key = child;
        }
        self
    }

    /// Adds the file `path` with `data`, making its directories.
    pub fn add_file(&mut self, path: &str, data: &[u8]) -> &mut Self {
        let parts: Vec<&str> = path.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
        let Some((name, dirs)) = parts.split_last() else { return self };
        self.add_dir(&dirs.join("\\"));
        let key: String = dirs.iter().map(|d| format!("\\{}", d.to_ascii_uppercase())).collect();
        let sector = self.take_sectors(data.len() as u64);
        self.data.insert(sector, data.to_vec());
        let node =
            DiscNode { name: (*name).to_owned(), sector, size: data.len() as u64, is_dir: false };
        self.dirs.entry(key).or_default().push(node);
        self
    }
}

impl DiscTree for MemDisc {
    fn lookup(&self, path: &str) -> Option<DiscNode> {
        let mut node = DiscNode {
            name: String::new(),
            sector: 264,
            size: u64::from(DISC_SECTOR),
            is_dir: true,
        };
        let mut key = String::new();
        for part in path.split(['\\', '/']).filter(|p| !p.is_empty()) {
            if !node.is_dir {
                return None;
            }
            node = self.dirs.get(&key)?.iter().find(|n| n.name.eq_ignore_ascii_case(part))?.clone();
            key = format!("{key}\\{}", part.to_ascii_uppercase());
        }
        Some(node)
    }

    fn list(&self, dir: &DiscNode) -> Vec<DiscNode> {
        // Directories are found again by their sector, which is unique.
        if dir.sector == 264 {
            return self.dirs.get("").cloned().unwrap_or_default();
        }
        let key = self.dirs.iter().find_map(|(key, entries)| {
            entries
                .iter()
                .find(|n| n.is_dir && n.sector == dir.sector)
                .map(|n| format!("{key}\\{}", n.name.to_ascii_uppercase()))
        });
        key.and_then(|k| self.dirs.get(&k).cloned()).unwrap_or_default()
    }

    fn read(&self, file: &DiscNode, offset: u64, buf: &mut [u8]) -> bool {
        let Some(data) = self.data.get(&file.sector) else { return false };
        let Ok(offset) = usize::try_from(offset) else { return false };
        match offset.checked_add(buf.len()).and_then(|end| data.get(offset..end)) {
            Some(bytes) => {
                buf.copy_from_slice(bytes);
                true
            }
            None => false,
        }
    }
}

impl RawBlocks for Vec<u8> {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> bool {
        let Ok(offset) = usize::try_from(offset) else { return false };
        match offset.checked_add(buf.len()).and_then(|end| self.get(offset..end)) {
            Some(bytes) => {
                buf.copy_from_slice(bytes);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disc() -> MemDisc {
        let mut d = MemDisc::new();
        d.add_file("default.xbe", b"XBEH")
            .add_file(r"maps\a10.map", &[7; 5000])
            .add_file(r"sound\English\intro.wav", b"hello")
            .add_file(r"text\eng.txt", b"eng")
            .add_file(r"movies\ENG\a.bik", b"bik")
            .add_dir("empty");
        d
    }

    #[test]
    fn the_memory_disc_finds_paths_in_any_case() {
        let d = disc();
        assert!(d.lookup("").unwrap().is_dir);
        assert_eq!(d.lookup(r"\MAPS\A10.MAP").unwrap().size, 5000);
        assert_eq!(d.lookup("maps/a10.map").unwrap().name, "a10.map");
        assert!(d.lookup(r"\maps\missing").is_none());
        assert!(d.lookup(r"\default.xbe\x").is_none());
        let maps = d.lookup("maps").unwrap();
        assert_eq!(d.list(&maps).len(), 1);
        assert_eq!(d.list(&d.lookup("").unwrap()).len(), 6);
        assert!(d.list(&d.lookup("empty").unwrap()).is_empty());
        let mut buf = [0; 4];
        assert!(d.read(&d.lookup("default.xbe").unwrap(), 0, &mut buf));
        assert_eq!(&buf, b"XBEH");
        assert!(!d.read(&d.lookup("default.xbe").unwrap(), 2, &mut buf));
    }

    #[test]
    fn a_missing_language_falls_back_to_english_in_the_same_case() {
        let d = disc();
        let (path, node) = language_fallback(&d, r"\sound\Spanish\intro.wav").unwrap();
        assert_eq!(path, r"\sound\English\intro.wav");
        assert_eq!(node.size, 5);
        assert_eq!(language_fallback(&d, r"\text\fra.txt").unwrap().0, r"\text\eng.txt");
        assert_eq!(language_fallback(&d, r"\movies\DEU\a.bik").unwrap().0, r"\movies\ENG\a.bik");
        assert!(language_fallback(&d, r"\sound\Klingon\intro.wav").is_none());
        assert!(language_fallback(&d, r"\sound\Spanish\other.wav").is_none());
    }

    #[test]
    fn a_disc_lacks_a_language_when_only_english_has_a_folder() {
        let d = disc();
        assert!(disc_lacks_language(&d, 5));
        assert!(!disc_lacks_language(&d, 1));
        assert!(!disc_lacks_language(&d, 12));
        let mut both = disc();
        both.add_dir(r"sound\Spanish");
        assert!(!disc_lacks_language(&both, 5));
        assert!(disc_lacks_language(&both, 3));
        let mut none = MemDisc::new();
        none.add_file("default.xbe", b"x");
        assert!(!disc_lacks_language(&none, 5));
    }

    #[test]
    fn a_big_archive_reports_its_pressed_disc_sector() {
        let mut d = MemDisc::new();
        d.add_file("default.xbe", &[0; 4096]);
        let xbe = d.lookup("default.xbe").unwrap().sector;
        // The archive sat 0x1234 sectors after default.xbe on the pressed disc.
        let delta: u32 = 0x1234;
        let e = (delta >> 8 | (delta & 0xFF) << 8) << 16 | (delta >> 8 | (delta & 0xFF) << 8);
        let mut big = vec![0u8; 0x40];
        big[..4].copy_from_slice(b"BIGF");
        big[0x20..0x24].copy_from_slice(&(e ^ 0x538F_5B39).to_le_bytes());
        d.add_file("data.big", &big).add_file("other.big", &[0; 0x40]);
        let node = d.lookup("data.big").unwrap();
        assert_ne!(node.sector, xbe + delta);
        assert_eq!(sort_sector(&d, r"\data.big", &node), xbe + delta);
        assert_eq!(sort_sector(&d, r"\DATA.BIG", &node), xbe + delta);
        // Not that pattern, or not a .big: the file's own sector.
        let other = d.lookup("other.big").unwrap();
        assert_eq!(sort_sector(&d, r"\other.big", &other), other.sector);
        assert_eq!(sort_sector(&d, r"\data.bin", &node), node.sector);
    }
}
