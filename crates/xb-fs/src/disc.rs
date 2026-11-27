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
        let Some(mut node) = self.tree.lookup(rel) else {
            // As the console's file system answers, and the reference: a
            // folder on the way missing (or a file) is a missing path, only
            // the last name missing a missing name. No other file stands in
            // for it: a title on a console set to a language its disc lacks
            // finds that language's folder missing and goes on as on the
            // console (Fable looks for Data\lang\Spanish\text.biz and
            // plays in English).
            let path_gone = parent_of(rel).is_some_and(|parent| {
                self.tree.lookup(parent).is_none_or(|p| !(p.is_dir || lost_folder(&p)))
            });
            log::info!(
                target: "fs",
                "open CdRom0{rel}: not on the disc{}",
                if path_gone { " (nor its folder)" } else { "" }
            );
            return Err(if path_gone {
                NtStatus::OBJECT_PATH_NOT_FOUND
            } else {
                NtStatus::OBJECT_NAME_NOT_FOUND
            });
        };
        let lost = lost_folder(&node) && params.options & options::FILE_NON_DIRECTORY_FILE == 0;
        if lost {
            log::debug!(
                target: "fs",
                "open CdRom0{rel}: an empty file with no extension, taken for the empty folder a rebuilt image wrote so"
            );
            node.is_dir = true;
        }
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
                lost,
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
    /// An empty file opened as the empty folder it was ([`lost_folder`]).
    lost: bool,
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

    fn list(&self, keep: &dyn Fn(&str) -> bool) -> Vec<DirEntry> {
        if !self.node.is_dir || self.lost {
            return Vec::new();
        }
        self.tree
            .list(&self.node)
            .into_iter()
            .filter(|n| n.name.chars().count() < LISTING_NAME_MAX && keep(&n.name))
            .take(LISTING_MAX)
            .map(|n| {
                let is_dir = n.is_dir || lost_folder(&n);
                DirEntry { name: n.name, size: n.size, is_dir }
            })
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

/// A disc entry that is an empty file with no extension in its name: the
/// empty folder an image rebuilt from extracted files wrote as an empty
/// file. It is taken for the empty folder it was, as the reference does:
/// a folder's attributes, opens and listings answer so, and a file open
/// (`FILE_NON_DIRECTORY_FILE`, what `CreateFile` asks) still opens the empty
/// file. Marvel vs. Capcom 2's rebuilt images have `Media\sounds` so, and
/// the title, unable to open its sound folder, stays on a black screen.
/// Empty files with an extension are files.
fn lost_folder(node: &DiscNode) -> bool {
    !node.is_dir && node.size == 0 && !node.name.is_empty() && !node.name.contains('.')
}

/// The folder a disc path's last name is in; `None` for a name in the root.
fn parent_of(rel: &str) -> Option<&str> {
    let rel = rel.trim_start_matches('\\');
    rel.rfind('\\').map(|slash| &rel[..slash])
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

    fn open(d: &DiscVolume, rel: &str, opts: u32) -> Result<Box<dyn VolumeFile>, NtStatus> {
        d.open(rel, &OpenParams::open(0, 0, opts)).map(|(f, _)| f)
    }

    #[test]
    fn a_miss_is_a_missing_path_when_its_folder_is_missing_and_a_missing_name_otherwise() {
        let d = DiscVolume::new(Arc::new(disc()));
        let status = |rel: &str| open(&d, rel, 0).err();
        // No language stands in for another: the folder is simply not there.
        assert_eq!(status(r"\sound\Spanish\intro.wav"), Some(NtStatus::OBJECT_PATH_NOT_FOUND));
        assert_eq!(status(r"\text\fra.txt"), Some(NtStatus::OBJECT_NAME_NOT_FOUND));
        assert_eq!(status(r"\sound\Spanish"), Some(NtStatus::OBJECT_NAME_NOT_FOUND));
        assert_eq!(status(r"\missing.txt"), Some(NtStatus::OBJECT_NAME_NOT_FOUND));
        // A file on the way is no folder.
        assert_eq!(status(r"\default.xbe\x"), Some(NtStatus::OBJECT_PATH_NOT_FOUND));
        // What is there opens as before, whatever the case.
        assert_eq!(open(&d, r"\SOUND\english\INTRO.WAV", 0).unwrap().size(), 5);
    }

    #[test]
    fn an_empty_file_without_an_extension_is_the_empty_folder_a_rebuilt_image_wrote() {
        let mut d = disc();
        d.add_file(r"Media\sounds", b"").add_file(r"Media\Dummy.txt", b"");
        let d = DiscVolume::new(Arc::new(d));
        // A folder for attributes, directory opens and listings ...
        for opts in [0, options::FILE_DIRECTORY_FILE] {
            let lost = open(&d, r"\media\SOUNDS", opts).unwrap();
            let real = open(&d, r"\empty", opts).unwrap();
            assert_eq!((lost.is_dir(), lost.size()), (real.is_dir(), real.size()));
            assert_eq!(lost.facts().attributes, real.facts().attributes);
            assert_eq!(lost.list(&|_| true), real.list(&|_| true));
        }
        let media = open(&d, r"\Media", 0).unwrap();
        let listed = |name: &str| media.list(&|n| n == name)[0].is_dir;
        assert!(listed("sounds") && !listed("Dummy.txt"));
        // ... a missing file under it has its folder ...
        assert_eq!(
            open(&d, r"\Media\sounds\a.wav", 0).err(),
            Some(NtStatus::OBJECT_NAME_NOT_FOUND)
        );
        // ... and a file open still opens the empty file.
        let file = open(&d, r"\Media\sounds", options::FILE_NON_DIRECTORY_FILE).unwrap();
        assert!(!file.is_dir());
        // One with an extension is a file.
        assert_eq!(
            open(&d, r"\Media\Dummy.txt", options::FILE_DIRECTORY_FILE).err(),
            Some(NtStatus::NOT_A_DIRECTORY)
        );
        assert!(!open(&d, r"\Media\Dummy.txt", 0).unwrap().is_dir());
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
