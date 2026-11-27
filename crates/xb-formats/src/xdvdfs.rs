//! XDVDFS, the file system of Xbox game discs.
//!
//! Reads an extracted XISO or a full disc dump, where the game partition sits
//! at one of the known offsets. A mod overlay belongs to a higher layer.
//!
//! The volume descriptor is sector 32 of the game partition:
//! `"MICROSOFT*XBOX*MEDIA"` at its start and again at its end, the root
//! directory's sector and size after the first. A directory is a table of
//! entries linked as a binary tree ordered by name:
//!
//! ```text
//! u16 left, u16 right   child offsets in units of 4 bytes (0: none; the
//!                       root is nobody's child; 0xFFFF pads a sector's end)
//! u32 sector, u32 size
//! u8 attributes, u8 name length, name
//! ```

use std::collections::HashSet;

use crate::{BlockSource, Error, Result, SubSource, le16, le32, le64};

/// The volume descriptor's magic, at its first and last 20 bytes.
pub const MAGIC: &[u8; 20] = b"MICROSOFT*XBOX*MEDIA";

/// Game partition offsets tried in turn: an extracted XISO, then full XGD1,
/// XGD2 and XGD3 dumps (the reference's list).
pub const PARTITION_OFFSETS: [u64; 4] = [0, 0x1830_0000, 0x0FD9_0000, 0x0208_0000];

/// A directory table's size at most: far above any disc's (a folder of ten
/// thousand files is well under 1 MiB), below what a damaged or crafted size
/// field would have a reader allocate and fill (up to 4 GiB).
pub const DIR_MAX: u32 = 64 << 20;

/// Attribute bit: the file is read-only.
pub const ATTR_READ_ONLY: u8 = 0x01;
/// Attribute bit: the file is hidden.
pub const ATTR_HIDDEN: u8 = 0x02;
/// Attribute bit: a system file.
pub const ATTR_SYSTEM: u8 = 0x04;
/// Attribute bit: the entry is a directory.
pub const ATTR_DIRECTORY: u8 = 0x10;
/// Attribute bit: the archive bit.
pub const ATTR_ARCHIVE: u8 = 0x20;
/// Attribute bit: a file with no other attributes.
pub const ATTR_NORMAL: u8 = 0x80;

const SECTOR: usize = crate::SECTOR_SIZE as usize;
const SECTOR64: u64 = crate::SECTOR_SIZE as u64;
/// Bytes of an entry before its name.
const ENTRY_HEAD: usize = 14;
/// Nested directories a walk follows at most. A path component is at least a
/// byte and a separator and the kernel's paths are at most 260 characters,
/// so nothing a title can open is deeper.
const WALK_DEPTH: usize = 130;

/// A file or directory of the disc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// The name as the disc spells it. Disc names are single bytes; each is
    /// kept as the character with that code (Latin-1), so ASCII names are
    /// themselves and nothing is lost.
    pub name: String,
    /// First sector of the data, counted from the game partition's start.
    pub sector: u32,
    /// Size in bytes (for a directory, of its table).
    pub size: u32,
    /// Attribute bits (`ATTR_*`).
    pub attributes: u8,
}

impl DirEntry {
    /// True for a directory.
    pub fn is_dir(&self) -> bool {
        self.attributes & ATTR_DIRECTORY != 0
    }
}

/// An entry as it lies in a directory table.
#[derive(Clone, Copy)]
struct RawEntry<'a> {
    sector: u32,
    size: u32,
    attributes: u8,
    name: &'a [u8],
}

impl RawEntry<'_> {
    fn to_owned(self) -> DirEntry {
        DirEntry {
            name: self.name.iter().map(|&b| char::from(b)).collect(),
            sector: self.sector,
            size: self.size,
            attributes: self.attributes,
        }
    }

    /// Compares the name with a path component the way the reference does:
    /// byte for byte, ASCII letters without their case.
    fn is_named(&self, component: &str) -> bool {
        let mut chars = component.chars();
        self.name
            .iter()
            .all(|&b| chars.next().is_some_and(|c| c.eq_ignore_ascii_case(&char::from(b))))
            && chars.next().is_none()
    }
}

/// The entry at `off` of a table, if one lies there: in the table, not
/// padding, with a name that is not empty.
fn entry_at(dir: &[u8], off: usize) -> Option<RawEntry<'_>> {
    let e = dir.get(off..)?;
    let head = e.get(..ENTRY_HEAD)?;
    if le16(head, 0) == Some(0xFFFF) && le16(head, 2) == Some(0xFFFF) {
        return None;
    }
    let len = usize::from(head[13]);
    if len == 0 {
        return None;
    }
    let name = e.get(ENTRY_HEAD..ENTRY_HEAD + len)?;
    Some(RawEntry { sector: le32(head, 4)?, size: le32(head, 8)?, attributes: head[12], name })
}

/// The table offset a child link names, if it names an entry.
fn child(dir: &[u8], link: u16) -> Option<usize> {
    let off = usize::from(link) * 4;
    (link != 0 && link != 0xFFFF && entry_at(dir, off).is_some()).then_some(off)
}

/// Calls `f` with every entry of a directory table: its tree in order (what a
/// listing shows first), then the entries a pass over the table finds that the
/// tree does not reach. Stops when `f` returns true, and returns that.
///
/// The tree's depth is bounded by the entries the table can hold, not a fixed
/// count (an image tool may leave the tree a list: Conker: Live & Reloaded's
/// Audio, 78 entries 77 deep), with no recursion; a loop in a damaged table
/// ends where it meets an entry already seen. Links may name entries that
/// overlap (nothing checks that they don't), one every 4 bytes, so the
/// "seen" set has a bit per 4 bytes of the table. The pass reads the entries
/// as they are laid out, each sector's from its start until its padding, so a
/// table whose links an image tool got wrong still shows every file.
fn each_entry(dir: &[u8], mut f: impl FnMut(RawEntry<'_>) -> bool) -> bool {
    if dir.len() < ENTRY_HEAD {
        return false;
    }
    let mut seen = vec![0u8; (dir.len() / 4 + 1).div_ceil(8)];
    // True when the entry was already seen; marks it either way.
    let mut mark = |off: usize| {
        let (byte, bit) = (off / 4 / 8, 1u8 << (off / 4 % 8));
        let was = seen[byte] & bit != 0;
        seen[byte] |= bit;
        was
    };
    // In order: down the left links, then the entry, then its right subtree.
    let mut stack: Vec<usize> = Vec::new();
    let mut cur = entry_at(dir, 0).map(|_| 0usize);
    loop {
        while let Some(off) = cur {
            if mark(off) {
                break;
            }
            stack.push(off);
            cur = le16(dir, off).and_then(|l| child(dir, l));
        }
        let Some(off) = stack.pop() else { break };
        // Entries on the stack were validated when pushed.
        if entry_at(dir, off).is_some_and(&mut f) {
            return true;
        }
        cur = le16(dir, off + 2).and_then(|r| child(dir, r));
    }
    // The table as laid out.
    let mut off = 0;
    while off + ENTRY_HEAD <= dir.len() {
        let entry = if SECTOR - off % SECTOR < ENTRY_HEAD { None } else { entry_at(dir, off) };
        let Some(e) = entry else {
            off = (off / SECTOR + 1) * SECTOR;
            continue;
        };
        if !mark(off) && f(e) {
            return true;
        }
        off += (ENTRY_HEAD + e.name.len() + 3) & !3;
    }
    false
}

/// An XDVDFS volume on a block source.
///
/// Holds only the volume descriptor's fields; directory tables are read when
/// asked for. All methods take `&self` and may run on several threads.
#[derive(Debug)]
pub struct Xdvdfs<S> {
    src: S,
    part_off: u64,
    root_sector: u32,
    root_size: u32,
    creation_time: u64,
}

impl<S: BlockSource> Xdvdfs<S> {
    /// Finds the volume on `src`, trying each of [`PARTITION_OFFSETS`].
    /// [`Error::NoVolume`] when none has a descriptor.
    pub fn open(src: S) -> Result<Self> {
        let mut vd = vec![0u8; SECTOR];
        for part_off in PARTITION_OFFSETS {
            if src.read_padded(part_off + 32 * SECTOR64, &mut vd).is_err() {
                continue;
            }
            if vd[..20] != MAGIC[..] || vd[0x7EC..0x7EC + 20] != MAGIC[..] {
                continue;
            }
            let (Some(root_sector), Some(root_size), Some(creation_time)) =
                (le32(&vd, 20), le32(&vd, 24), le64(&vd, 28))
            else {
                continue;
            };
            log::info!(target: "disc", "XDVDFS at +{part_off:#x}, root sector {root_sector} ({root_size} bytes)");
            return Ok(Self { src, part_off, root_sector, root_size, creation_time });
        }
        log::info!(target: "disc", "no XDVDFS volume found");
        Err(Error::NoVolume)
    }

    /// The image the volume is on.
    pub fn source(&self) -> &S {
        &self.src
    }

    /// Byte offset of the game partition in the image.
    pub fn partition_offset(&self) -> u64 {
        self.part_off
    }

    /// The volume's creation time, a Windows `FILETIME` (the descriptor's
    /// field after the root directory's size).
    pub fn creation_time(&self) -> u64 {
        self.creation_time
    }

    /// The root directory as an entry (its name is empty).
    pub fn root(&self) -> DirEntry {
        DirEntry {
            name: String::new(),
            sector: self.root_sector,
            size: self.root_size,
            attributes: ATTR_DIRECTORY,
        }
    }

    /// Byte offset in the image of an entry's data.
    pub fn data_offset(&self, entry: &DirEntry) -> u64 {
        self.part_off + u64::from(entry.sector) * SECTOR64
    }

    /// Reads a directory's table. An empty directory has none.
    fn table(&self, dir: &DirEntry) -> Result<Vec<u8>> {
        if !dir.is_dir() {
            return Err(Error::NotADirectory);
        }
        if dir.size > DIR_MAX {
            return Err(Error::Xdvdfs(format!("a directory table of {} bytes", dir.size)));
        }
        let mut table = vec![0u8; dir.size as usize];
        self.src.read_padded(self.data_offset(dir), &mut table)?;
        Ok(table)
    }

    /// Finds `name` in the directory `dir`, ASCII case-insensitively.
    pub fn lookup_in(&self, dir: &DirEntry, name: &str) -> Result<DirEntry> {
        let table = self.table(dir)?;
        let mut found = None;
        each_entry(&table, |e| {
            if e.is_named(name) {
                found = Some(e.to_owned());
            }
            found.is_some()
        });
        found.ok_or(Error::NotFound)
    }

    /// Finds a path such as `\default.xbe` or `media/x.bin` (either slash,
    /// ASCII case-insensitive, empty components skipped). The empty path is
    /// the root. [`Error::NotFound`] when a component names nothing,
    /// [`Error::NotADirectory`] when one before the last is a file.
    pub fn lookup(&self, path: &str) -> Result<DirEntry> {
        let mut cur = self.root();
        for component in path.split(['\\', '/']).filter(|c| !c.is_empty()) {
            cur = self.lookup_in(&cur, component)?;
        }
        Ok(cur)
    }

    /// Lists a directory: the entries of its tree in name order, then any the
    /// tree does not reach.
    pub fn read_dir(&self, dir: &DirEntry) -> Result<Vec<DirEntry>> {
        let table = self.table(dir)?;
        let mut entries = Vec::new();
        each_entry(&table, |e| {
            entries.push(e.to_owned());
            false
        });
        Ok(entries)
    }

    /// Lists the directory at `path`.
    pub fn list(&self, path: &str) -> Result<Vec<DirEntry>> {
        self.read_dir(&self.lookup(path)?)
    }

    /// Reads `buf.len()` bytes at `offset` of a file. The range must lie in
    /// the file. Bytes the image does not hold (a trimmed image) are zeros.
    pub fn read_file(&self, entry: &DirEntry, offset: u64, buf: &mut [u8]) -> Result<()> {
        crate::block::check_range(offset, buf.len(), u64::from(entry.size))?;
        self.src.read_padded(self.data_offset(entry) + offset, buf)
    }

    /// A file's bytes as a block source of their own, for a parser.
    pub fn file_source(&self, entry: &DirEntry) -> SubSource<&S> {
        SubSource::new(&self.src, self.data_offset(entry), u64::from(entry.size))
    }

    /// Calls `f` with every file and directory of the volume and its path
    /// (backslashes, no leading one), a directory before its contents.
    ///
    /// A directory is entered once however many entries name its table, and
    /// nesting is followed to a fixed depth, so a damaged or crafted tree
    /// cannot keep the walk going.
    pub fn walk(&self, mut f: impl FnMut(&str, &DirEntry)) -> Result<()> {
        let mut entered = HashSet::new();
        // (directory, its path, nesting)
        let mut todo = vec![(self.root(), String::new(), 0usize)];
        while let Some((dir, path, depth)) = todo.pop() {
            if dir.size == 0 || !entered.insert(dir.sector) {
                continue;
            }
            let mut dirs = Vec::new();
            for e in self.read_dir(&dir)? {
                let full =
                    if path.is_empty() { e.name.clone() } else { format!("{path}\\{}", e.name) };
                f(&full, &e);
                if e.is_dir() && depth < WALK_DEPTH {
                    dirs.push((e, full, depth + 1));
                }
            }
            // Popped from the end: keep the listing's order.
            todo.extend(dirs.into_iter().rev());
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::MemSource;
    use crate::testutil::Lcg;

    /// A file or directory of a test volume.
    pub(crate) enum Node {
        File(String, Vec<u8>),
        Dir(String, Vec<Node>),
    }

    impl Node {
        pub(crate) fn file(name: &str, data: Vec<u8>) -> Self {
            Self::File(name.to_owned(), data)
        }

        pub(crate) fn dir(name: &str, kids: Vec<Node>) -> Self {
            Self::Dir(name.to_owned(), kids)
        }

        fn name(&self) -> &str {
            match self {
                Self::File(n, _) | Self::Dir(n, _) => n,
            }
        }
    }

    /// How a builder links a directory's entries.
    #[derive(Clone, Copy, PartialEq)]
    pub(crate) enum Links {
        /// A balanced tree, as image tools write.
        Balanced,
        /// Every entry its predecessor's right child: a list.
        List,
        /// No links at all: only the pass over the table finds the entries.
        None,
    }

    /// Builds XDVDFS images in memory.
    struct Builder {
        image: Vec<u8>,
        part_off: usize,
        links: Links,
    }

    /// The balanced tree over the name-ordered items `lo..hi`: appends the
    /// items to `order` root first, and returns the root.
    fn preorder(lo: usize, hi: usize, order: &mut Vec<usize>) -> Option<usize> {
        (lo < hi).then(|| {
            let mid = (lo + hi) / 2;
            order.push(mid);
            preorder(lo, mid, order);
            preorder(mid + 1, hi, order);
            mid
        })
    }

    impl Builder {
        /// Room for `bytes` at the image's end: its sector.
        fn alloc(&mut self, bytes: usize) -> u32 {
            let sector = (self.image.len() - self.part_off) / SECTOR;
            self.image.resize(self.image.len() + bytes.div_ceil(SECTOR).max(1) * SECTOR, 0);
            sector as u32
        }

        fn put(&mut self, data: &[u8]) -> u32 {
            let sector = self.alloc(data.len());
            let at = self.part_off + sector as usize * SECTOR;
            self.image[at..at + data.len()].copy_from_slice(data);
            sector
        }

        /// Writes a directory's table and everything under it; its sector and
        /// table size.
        fn dir(&mut self, nodes: &[Node]) -> (u32, u32) {
            if nodes.is_empty() {
                return (0, 0);
            }
            // (name, sector, size, attributes), in name order.
            let mut items: Vec<(&str, u32, u32, u8)> = nodes
                .iter()
                .map(|n| match n {
                    Node::File(name, data) => {
                        (name.as_str(), self.put(data), data.len() as u32, ATTR_ARCHIVE)
                    }
                    Node::Dir(name, kids) => {
                        let (sector, size) = self.dir(kids);
                        (name.as_str(), sector, size, ATTR_DIRECTORY)
                    }
                })
                .collect();
            items.sort_by_key(|i| i.0.to_ascii_uppercase());
            let n = items.len();
            // The order the entries lie in the table. The tree's root is the
            // entry at offset 0.
            let mut order = Vec::new();
            if self.links == Links::Balanced {
                preorder(0, n, &mut order);
            } else {
                order.extend(0..n);
            }
            // No entry lies across a sector's end.
            let mut offs = vec![0usize; n];
            let mut off = 0usize;
            for &i in &order {
                let len = (ENTRY_HEAD + items[i].0.len() + 3) & !3;
                if off % SECTOR + len > SECTOR {
                    off = (off / SECTOR + 1) * SECTOR;
                }
                offs[i] = off;
                off += len;
            }
            let mut table = vec![0xFFu8; off.div_ceil(SECTOR) * SECTOR];
            let link = |i: Option<usize>| i.map_or(0, |i| (offs[i] / 4) as u16);
            // The balanced tree again, by the same halving: an item's
            // children are the middles of the halves beside it.
            fn children(
                lo: usize,
                hi: usize,
                out: &mut [(Option<usize>, Option<usize>)],
            ) -> Option<usize> {
                (lo < hi).then(|| {
                    let mid = (lo + hi) / 2;
                    out[mid] = (children(lo, mid, out), children(mid + 1, hi, out));
                    mid
                })
            }
            let mut kids = vec![(None, None); n];
            match self.links {
                Links::Balanced => {
                    children(0, n, &mut kids);
                }
                Links::List => {
                    for (i, k) in kids.iter_mut().enumerate().take(n - 1) {
                        k.1 = Some(i + 1);
                    }
                }
                Links::None => {}
            }
            for i in 0..n {
                let (name, sector, size, attr) = items[i];
                let e = &mut table[offs[i]..];
                e[0..2].copy_from_slice(&link(kids[i].0).to_le_bytes());
                e[2..4].copy_from_slice(&link(kids[i].1).to_le_bytes());
                e[4..8].copy_from_slice(&sector.to_le_bytes());
                e[8..12].copy_from_slice(&size.to_le_bytes());
                e[12] = attr;
                e[13] = name.len() as u8;
                e[14..14 + name.len()].copy_from_slice(name.as_bytes());
            }
            (self.put(&table), table.len() as u32)
        }
    }

    /// An image with the volume at `part_off` holding `root`.
    pub(crate) fn build(part_off: usize, links: Links, root: &[Node]) -> Vec<u8> {
        let mut b = Builder { image: vec![0u8; part_off + 33 * SECTOR], part_off, links };
        let (sector, size) = b.dir(root);
        let vd = &mut b.image[part_off + 32 * SECTOR..];
        vd[..20].copy_from_slice(MAGIC);
        vd[20..24].copy_from_slice(&sector.to_le_bytes());
        vd[24..28].copy_from_slice(&size.to_le_bytes());
        vd[28..36].copy_from_slice(&0x01C3_0000_1234_5678u64.to_le_bytes());
        vd[0x7EC..0x7EC + 20].copy_from_slice(MAGIC);
        b.image
    }

    fn bytes(seed: u32, len: usize) -> Vec<u8> {
        let mut rng = Lcg(seed);
        (0..len).map(|_| rng.next() as u8).collect()
    }

    /// A tree with nested directories, an empty one, an empty file, files
    /// larger than a sector and a directory whose table spans sectors.
    pub(crate) fn sample_tree() -> Vec<Node> {
        let many = (0..120)
            .map(|i| {
                Node::file(
                    &format!("file_{i:03}_with_a_long_name.dat"),
                    bytes(1000 + i, 10 + i as usize),
                )
            })
            .collect();
        vec![
            Node::file("default.xbe", bytes(1, 5000)),
            Node::file("Empty.txt", Vec::new()),
            Node::dir(
                "Maps",
                vec![
                    Node::file("a10.map", bytes(2, 3 * SECTOR)),
                    Node::file("B30.MAP", bytes(3, 2049)),
                    Node::dir(
                        "Deep",
                        vec![Node::dir("Deeper", vec![Node::file("x.bin", bytes(4, 77))])],
                    ),
                ],
            ),
            Node::dir("Nothing", Vec::new()),
            Node::dir("many", many),
            Node::file("zz.bik", bytes(5, 1)),
        ]
    }

    /// Checks every file and directory of `nodes` through `fs`, and that the
    /// listing holds exactly them.
    fn check_tree(fs: &Xdvdfs<MemSource>, path: &str, nodes: &[Node]) {
        let listed = fs.list(path).unwrap();
        let mut got: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
        let mut want: Vec<&str> = nodes.iter().map(Node::name).collect();
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want, "listing of {path:?}");
        for n in nodes {
            let full = format!("{path}\\{}", n.name());
            let e = fs.lookup(&full).unwrap();
            assert_eq!(e.name, n.name());
            // Either slash, any case.
            assert_eq!(fs.lookup(&full.replace('\\', "/").to_ascii_uppercase()).unwrap(), e);
            assert_eq!(fs.lookup(&full.to_ascii_lowercase()).unwrap(), e);
            match n {
                Node::File(_, data) => {
                    assert!(!e.is_dir());
                    assert_eq!(e.size as usize, data.len());
                    let mut buf = vec![0xAA; data.len()];
                    fs.read_file(&e, 0, &mut buf).unwrap();
                    assert!(buf == *data, "{full}");
                    if data.len() > 2 {
                        let mut mid = vec![0; data.len() - 2];
                        fs.read_file(&e, 1, &mut mid).unwrap();
                        assert!(mid == data[1..data.len() - 1]);
                        let src = fs.file_source(&e);
                        assert_eq!(src.len(), data.len() as u64);
                        src.read_at(1, &mut mid).unwrap();
                        assert!(mid == data[1..data.len() - 1]);
                    }
                    let mut over = vec![0; data.len() + 1];
                    assert!(matches!(
                        fs.read_file(&e, 0, &mut over),
                        Err(Error::OutOfRange { .. })
                    ));
                    assert!(matches!(
                        fs.read_file(&e, u64::MAX, &mut over[..1]),
                        Err(Error::OutOfRange { .. })
                    ));
                    assert!(matches!(fs.read_dir(&e), Err(Error::NotADirectory)));
                    assert!(matches!(fs.lookup(&format!("{full}\\x")), Err(Error::NotADirectory)));
                }
                Node::Dir(_, kids) => {
                    assert!(e.is_dir());
                    check_tree(fs, &full, kids);
                }
            }
        }
        assert!(matches!(fs.lookup(&format!("{path}\\no such file")), Err(Error::NotFound)));
        assert!(matches!(fs.lookup(&format!("{path}\\default.xb")), Err(Error::NotFound)));
        assert!(matches!(fs.lookup(&format!("{path}\\default.xbe2")), Err(Error::NotFound)));
    }

    fn count(nodes: &[Node]) -> usize {
        nodes.iter().map(|n| 1 + if let Node::Dir(_, k) = n { count(k) } else { 0 }).sum()
    }

    #[test]
    fn every_file_of_a_volume_is_found_listed_and_read_at_each_partition_offset() {
        let tree = sample_tree();
        for part_off in [0usize, 0x0208_0000] {
            let fs = Xdvdfs::open(MemSource(build(part_off, Links::Balanced, &tree))).unwrap();
            assert_eq!(fs.partition_offset(), part_off as u64);
            assert_eq!(fs.creation_time(), 0x01C3_0000_1234_5678);
            assert_eq!(fs.lookup("").unwrap(), fs.root());
            assert_eq!(fs.lookup("\\//").unwrap(), fs.root());
            check_tree(&fs, "", &tree);
            let mut walked = Vec::new();
            fs.walk(|p, e| walked.push((p.to_owned(), e.clone()))).unwrap();
            assert_eq!(walked.len(), count(&tree));
            for (p, e) in walked {
                assert_eq!(fs.lookup(&p).unwrap(), e);
            }
        }
    }

    #[test]
    fn a_balanced_tree_lists_in_name_order() {
        let tree = sample_tree();
        let fs = Xdvdfs::open(MemSource(build(0, Links::Balanced, &tree))).unwrap();
        for dir in ["", "many", "Maps"] {
            let names: Vec<String> =
                fs.list(dir).unwrap().into_iter().map(|e| e.name.to_ascii_uppercase()).collect();
            let mut sorted = names.clone();
            sorted.sort();
            assert_eq!(names, sorted);
        }
    }

    #[test]
    fn a_tree_left_a_list_and_a_table_without_links_still_show_every_file() {
        let tree = sample_tree();
        for links in [Links::List, Links::None] {
            let fs = Xdvdfs::open(MemSource(build(0, links, &tree))).unwrap();
            check_tree(&fs, "", &tree);
        }
    }

    #[test]
    fn an_image_without_a_descriptor_is_no_volume() {
        assert!(matches!(Xdvdfs::open(MemSource(Vec::new())), Err(Error::NoVolume)));
        assert!(matches!(Xdvdfs::open(MemSource(vec![0; 40 * SECTOR])), Err(Error::NoVolume)));
        // The first magic without the second.
        let mut image = build(0, Links::Balanced, &sample_tree());
        image[32 * SECTOR + 0x7EC] = b'X';
        assert!(matches!(Xdvdfs::open(MemSource(image)), Err(Error::NoVolume)));
    }

    #[test]
    fn a_trimmed_image_reads_zeros_where_it_was_cut() {
        let data = bytes(9, 3000);
        let mut image = build(0, Links::Balanced, &[Node::file("a.bin", data.clone())]);
        let fs = Xdvdfs::open(MemSource(image.clone())).unwrap();
        // Move the file to the image's end and keep only its first 1000
        // bytes: the only entry of the root table now names that sector.
        let table_at = fs.data_offset(&fs.root()) as usize;
        let end_sector = (image.len() / SECTOR) as u32;
        image[table_at + 4..table_at + 8].copy_from_slice(&end_sector.to_le_bytes());
        image.extend_from_slice(&data[..1000]);
        let fs = Xdvdfs::open(MemSource(image)).unwrap();
        let e = fs.lookup("a.bin").unwrap();
        let mut got = vec![0xAA; 3000];
        fs.read_file(&e, 0, &mut got).unwrap();
        assert_eq!(&got[..1000], &data[..1000]);
        assert!(got[1000..].iter().all(|&b| b == 0));
    }

    #[test]
    fn links_that_loop_or_point_outside_the_table_end_the_walk() {
        let tree = sample_tree();
        let good = build(0, Links::Balanced, &tree);
        let fs = Xdvdfs::open(MemSource(good.clone())).unwrap();
        let many = fs.lookup("many").unwrap();
        let at = fs.data_offset(&many) as usize;
        let n = fs.read_dir(&many).unwrap().len();
        for (l, r) in [(0u16, 0u16), (0, 1), (1, 1), (0x7FFF, 0x7FFF), (0xFFFE, 3), (5, 0)] {
            let mut image = good.clone();
            // The root entry's links: itself, entries inside names, far
            // outside the table.
            image[at..at + 2].copy_from_slice(&l.to_le_bytes());
            image[at + 2..at + 4].copy_from_slice(&r.to_le_bytes());
            let fs = Xdvdfs::open(MemSource(image)).unwrap();
            let listed = fs.read_dir(&many).unwrap();
            // The pass over the table still finds every real entry.
            assert!(listed.len() >= n, "links {l:#x},{r:#x}");
            for i in [0, 59, 119] {
                fs.lookup(&format!("many\\file_{i:03}_with_a_long_name.dat")).unwrap();
            }
        }
    }

    #[test]
    fn damaged_tables_never_panic() {
        let tree = sample_tree();
        let good = build(0, Links::Balanced, &tree);
        let mut rng = Lcg(31);
        for _ in 0..200 {
            let mut image = good.clone();
            for _ in 0..40 {
                let at = 32 * SECTOR
                    + (rng.next() << 15 | rng.next()) as usize % (image.len() - 32 * SECTOR);
                image[at] = rng.next() as u8;
            }
            let Ok(fs) = Xdvdfs::open(MemSource(image)) else { continue };
            let mut seen = 0usize;
            let _ = fs.walk(|_, _| seen += 1);
            let _ = fs.lookup("many\\file_050_with_a_long_name.dat");
            let _ = fs.lookup("Maps\\Deep\\Deeper\\x.bin").map(|e| {
                let mut buf = vec![0; e.size.min(4096) as usize];
                let _ = fs.read_file(&e, 0, &mut buf);
            });
        }
    }

    #[test]
    fn a_directory_that_contains_itself_is_walked_once() {
        let tree = vec![Node::dir("loop", vec![Node::file("f", vec![1, 2, 3])])];
        let mut image = build(0, Links::Balanced, &tree);
        let fs = Xdvdfs::open(MemSource(image.clone())).unwrap();
        let root_at = fs.data_offset(&fs.root()) as usize;
        // "loop" now names the root's own table.
        let root = fs.root();
        image[root_at + 4..root_at + 8].copy_from_slice(&root.sector.to_le_bytes());
        image[root_at + 8..root_at + 12].copy_from_slice(&root.size.to_le_bytes());
        let fs = Xdvdfs::open(MemSource(image)).unwrap();
        let mut seen = Vec::new();
        fs.walk(|p, _| seen.push(p.to_owned())).unwrap();
        assert_eq!(seen, ["loop"]);
        // A lookup follows it as far as the path goes, and ends.
        assert!(fs.lookup("loop\\loop\\loop").unwrap().is_dir());
    }

    #[test]
    fn a_directory_table_of_an_absurd_size_is_refused() {
        let tree = vec![Node::dir("d", vec![Node::file("f", vec![1])])];
        let mut image = build(0, Links::Balanced, &tree);
        let fs = Xdvdfs::open(MemSource(image.clone())).unwrap();
        let root_at = fs.data_offset(&fs.root()) as usize;
        image[root_at + 8..root_at + 12].copy_from_slice(&(DIR_MAX + 1).to_le_bytes());
        let fs = Xdvdfs::open(MemSource(image)).unwrap();
        assert!(matches!(fs.list("d"), Err(Error::Xdvdfs(_))));
        assert!(matches!(fs.lookup("d\\f"), Err(Error::Xdvdfs(_))));
    }
}
