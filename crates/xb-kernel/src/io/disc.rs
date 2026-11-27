//! The disc in the tray as `xb-fs` sees it: the XDVDFS parser behind
//! `xb_fs::DiscTree`.

use std::sync::Arc;

use xb_formats::block::BlockSource;
use xb_formats::xdvdfs::{ATTR_DIRECTORY, DirEntry, Xdvdfs};
use xb_fs::{DiscNode, DiscTree};

/// An XDVDFS volume on a disc image.
pub(super) struct XdvdfsTree {
    fs: Xdvdfs<Arc<dyn BlockSource>>,
}

impl XdvdfsTree {
    /// Finds the volume on `image` (an extracted XISO or a full dump).
    pub(super) fn open(image: Arc<dyn BlockSource>) -> xb_formats::Result<Self> {
        Ok(Self { fs: Xdvdfs::open(image)? })
    }
}

/// The parser's entry as a node: its start sector is what titles see as the
/// file's index and physical sort key.
fn node(e: DirEntry) -> DiscNode {
    let is_dir = e.is_dir();
    DiscNode { name: e.name, sector: e.sector, size: u64::from(e.size), is_dir }
}

/// The node back as the parser's entry: the sector and size are all it reads
/// by. A size no disc has reads as nothing.
fn entry(n: &DiscNode) -> DirEntry {
    DirEntry {
        name: n.name.clone(),
        sector: n.sector,
        size: u32::try_from(n.size).unwrap_or(0),
        attributes: if n.is_dir { ATTR_DIRECTORY } else { 0 },
    }
}

impl DiscTree for XdvdfsTree {
    fn lookup(&self, path: &str) -> Option<DiscNode> {
        self.fs.lookup(path).ok().map(node)
    }

    fn list(&self, dir: &DiscNode) -> Vec<DiscNode> {
        match self.fs.read_dir(&entry(dir)) {
            Ok(entries) => entries.into_iter().map(node).collect(),
            Err(e) => {
                log::info!(target: "fs", "list of the disc's {} (sector {}): {e}", dir.name, dir.sector);
                Vec::new()
            }
        }
    }

    fn read(&self, file: &DiscNode, offset: u64, buf: &mut [u8]) -> bool {
        self.fs.read_file(&entry(file), offset, buf).is_ok()
    }
}
