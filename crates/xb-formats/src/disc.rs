//! Opening a disc image file, whatever its container.
//!
//! [`open`] gives the block source of the plain image (an XISO or a full
//! dump) however the file stores it: as it is, or as a CCI in one file or a
//! split set. The file system on it is [`crate::xdvdfs::Xdvdfs`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::cci::{self, CciImage};
use crate::{BlockSource, FileSource, Result};

/// How a disc image is stored on the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageKind {
    /// The image's bytes as they are (`.iso`, `.xiso`).
    Raw,
    /// A CCI, in this many files.
    Cci {
        /// Files of the set: 1 unless it is a split image.
        parts: usize,
    },
}

/// An open disc image.
#[derive(Clone)]
pub struct DiscImage {
    /// The plain image. Shared: clones read the same files.
    pub source: Arc<dyn BlockSource>,
    /// The container it was read through.
    pub kind: ImageKind,
    /// The file that was opened: the path given, or a split set's first part
    /// when the path named a later one.
    pub path: PathBuf,
    /// The bytes the image's files take on the host.
    pub stored_size: u64,
}

impl std::fmt::Debug for DiscImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscImage")
            .field("kind", &self.kind)
            .field("path", &self.path)
            .field("size", &self.source.len())
            .field("stored_size", &self.stored_size)
            .finish()
    }
}

/// Opens the disc image at `path`.
///
/// - A later part of a split CCI (`Name.2.cci` beside a `Name.1.cci` that is
///   filled to the cut) opens the set from its first part.
/// - A file that starts with the CCI magic is opened as a CCI, with the other
///   parts beside it when it is a split set's first.
/// - Anything else is taken as a raw image.
///
/// No file system is looked for here: a raw file that is no disc image opens,
/// and [`crate::xdvdfs::Xdvdfs::open`] then finds no volume on it.
pub fn open(path: impl AsRef<Path>) -> Result<DiscImage> {
    let given = path.as_ref();
    let path = cci::first_part_of(given);
    if path != given {
        log::info!(target: "disc", "{} is a later part of a split CCI image: opening {}", given.display(), path.display());
    }
    if cci::is_cci(&path) {
        let image = CciImage::open(&path)?;
        let (kind, stored_size) = (ImageKind::Cci { parts: image.parts() }, image.stored_size());
        return Ok(DiscImage { source: Arc::new(image), kind, path, stored_size });
    }
    let file = FileSource::open(&path)?;
    let stored_size = file.len();
    log::info!(target: "disc", "raw image, {} MiB", stored_size >> 20);
    Ok(DiscImage { source: Arc::new(file), kind: ImageKind::Raw, path, stored_size })
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::{Seek, SeekFrom, Write};

    use super::*;
    use crate::cci::tests::{check_image, make_image, put_cci};
    use crate::testutil::TempDir;
    use crate::xdvdfs::Xdvdfs;
    use crate::xdvdfs::tests::{Links, build, sample_tree};
    use crate::{Error, SECTOR_SIZE};

    #[test]
    fn a_raw_image_and_its_cci_open_as_the_same_bytes_and_the_same_volume() {
        let dir = TempDir::new("disc");
        let image = build(0, Links::Balanced, &sample_tree());
        let (iso, cci) = (dir.path().join("game.iso"), dir.path().join("game.cci"));
        std::fs::write(&iso, &image).unwrap();
        std::fs::write(&cci, put_cci(&image, 2, 0).file).unwrap();
        let raw = open(&iso).unwrap();
        let packed = open(&cci).unwrap();
        assert_eq!(raw.kind, ImageKind::Raw);
        assert_eq!(packed.kind, ImageKind::Cci { parts: 1 });
        assert_eq!((raw.path.as_path(), packed.path.as_path()), (iso.as_path(), cci.as_path()));
        assert_eq!(raw.stored_size, image.len() as u64);
        assert!(packed.stored_size < raw.stored_size);
        check_image(&raw.source, &image);
        check_image(&packed.source, &image);

        let (a, b) =
            (Xdvdfs::open(raw.source.clone()).unwrap(), Xdvdfs::open(packed.source).unwrap());
        let mut seen = Vec::new();
        a.walk(|p, e| seen.push((p.to_owned(), e.clone()))).unwrap();
        assert!(!seen.is_empty());
        for (p, e) in seen {
            assert_eq!(b.lookup(&p).unwrap(), e);
            if !e.is_dir() {
                let (mut x, mut y) = (vec![0; e.size as usize], vec![1; e.size as usize]);
                a.read_file(&e, 0, &mut x).unwrap();
                b.read_file(&e, 0, &mut y).unwrap();
                assert!(x == y, "{p}");
            }
        }
    }

    #[test]
    fn a_file_that_is_neither_opens_raw_and_has_no_volume() {
        let dir = TempDir::new("disc-none");
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"CCI? no.").unwrap();
        let img = open(&path).unwrap();
        assert_eq!(img.kind, ImageKind::Raw);
        assert!(matches!(Xdvdfs::open(img.source), Err(Error::NoVolume)));
        assert!(matches!(open(dir.path().join("missing.iso")), Err(Error::Io(_))));
        // A CCI magic with nothing sound behind it is an error, not a raw image.
        let bad = dir.path().join("bad.cci");
        std::fs::write(&bad, b"CCIM and then nothing a reader could use").unwrap();
        assert!(matches!(open(&bad), Err(Error::Cci(_))));
    }

    /// A part of a split set cut where real ones are: a sparse file whose few
    /// blocks lie just below the cut, so its index starts on it.
    fn put_full_part(path: &Path, sectors: &[u8]) {
        let n = sectors.len() / SECTOR_SIZE as usize;
        let data_at = cci::SPLIT_AT - sectors.len() as u64;
        let mut tail = sectors.to_vec();
        for i in 0..=n as u64 {
            tail.extend_from_slice(&(((data_at + i * 2048) >> 2) as u32).to_le_bytes());
        }
        let mut h = [0u8; 32];
        h[..4].copy_from_slice(&cci::MAGIC);
        h[4..8].copy_from_slice(&32u32.to_le_bytes());
        h[8..16].copy_from_slice(&(sectors.len() as u64).to_le_bytes());
        h[16..24].copy_from_slice(&cci::SPLIT_AT.to_le_bytes());
        h[24..28].copy_from_slice(&2048u32.to_le_bytes());
        (h[28], h[29]) = (1, 2);
        let mut f = File::create(path).unwrap();
        f.write_all(&h).unwrap();
        f.seek(SeekFrom::Start(data_at)).unwrap();
        f.write_all(&tail).unwrap();
    }

    #[test]
    fn a_split_set_cut_at_the_real_limit_opens_from_any_of_its_parts() {
        let dir = TempDir::new("disc-split");
        let image = make_image(24);
        let (head, rest) = image.split_at(8 * 2048);
        let (p1, p2) = (dir.path().join("Big Game.1.cci"), dir.path().join("Big Game.2.cci"));
        put_full_part(&p1, head);
        std::fs::write(&p2, put_cci(rest, 2, 0).file).unwrap();

        assert!(cci::is_later_part(&p2));
        assert!(!cci::is_later_part(&p1));
        for given in [&p1, &p2] {
            let img = open(given).unwrap();
            assert_eq!(img.kind, ImageKind::Cci { parts: 2 });
            assert_eq!(img.path, p1);
            check_image(&img.source, &image);
        }
        // Without the second part the first is no image.
        std::fs::remove_file(&p2).unwrap();
        assert!(matches!(open(&p1), Err(Error::MissingPart(p)) if p == p2));
    }
}
