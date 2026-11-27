//! Helpers shared by the unit tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A directory under the host's temporary directory, removed when dropped.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    /// Creates a fresh directory whose name contains `tag`.
    pub(crate) fn new(tag: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("xb-formats-{}-{tag}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    /// The directory.
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A deterministic byte stream (the C library's classic LCG, as the reference
/// test uses) for noise that does not compress and for picking offsets.
pub(crate) struct Lcg(pub u32);

impl Lcg {
    /// Next 15-bit value.
    pub(crate) fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        (self.0 >> 16) & 0x7FFF
    }
}
