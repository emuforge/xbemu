//! The hard disk in a savestate: the title's cache partitions kept and put
//! back with it, and its saved games checked against it.
//!
//! A title's memory assumes its disk: which cache file holds which level,
//! what the files it has open contain. A state loaded over a cache another
//! session rewrote resumed a title reading files that no longer held its
//! data: Halo froze, or drew what it streamed afterwards from zeros (a
//! level's textures). So a state names every file of the title's cache
//! folder ([`xb_fs::HostDisk::cache_root`]) by its contents, and a load puts
//! exactly those files there before anything is opened again.
//!
//! The contents are kept one of two ways:
//!
//! - in a **store**, a folder of files named by their hash, next to the
//!   slots: a second state of the same level costs nothing, and a load over
//!   an unchanged cache copies nothing;
//! - **in the state itself**, for one that must stand alone (the core's
//!   bytes-only save, a state written to a path).
//!
//! A load takes each file from wherever it is: already in place, the store,
//! or the state.
//!
//! Saved games (`UDATA` and `TDATA` of the title) are not put back: that
//! would undo the player's progress. Their names, sizes and hashes are in
//! the state and a load says when they differ.
//!
//! Files are read and written a block at a time and a block of zeros is a
//! hole both ways: a cache partition is mostly files a title sized and has
//! not filled.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use xb_types::state::{StateIo, Stateful};
use xb_types::state_fields;

use crate::kernel::Kernel;
use crate::xc_crypto::Sha1;

/// The unit files are read, hashed and written in.
const BLOCK: usize = 64 << 10;

/// The longest run of a file's bytes kept as one piece in a state.
const RUN_MAX: usize = 16 << 20;

/// A file changed this recently before it was hashed is hashed again at its
/// next use: its time alone would not show a later write on a file system
/// that keeps times to two seconds (FAT).
const SETTLED: Duration = Duration::from_secs(3);

/// A file's contents by name: SHA-1 over its length and its blocks that are
/// not all zero, each with its number.
pub type Hash = [u8; 20];

/// One file of the disk as a state knows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiskFile {
    /// Its path under the folder it belongs to, `/` between the names.
    pub rel: String,
    /// Its length in bytes.
    pub len: u64,
    /// Its contents.
    pub hash: Hash,
}
state_fields!(DiskFile { rel, len, hash });

/// A file's contents kept in the state: the runs that are not zeros.
#[derive(Default)]
struct Blob {
    hash: Hash,
    len: u64,
    runs: Vec<(u64, Vec<u8>)>,
}

impl Stateful for Blob {
    fn state(&mut self, io: &mut StateIo) {
        self.hash.state(io);
        self.len.state(io);
        let mut n = self.runs.len() as u64;
        n.state(io);
        if io.loading() {
            self.runs.clear();
        }
        for i in 0..n {
            if io.failed().is_some() {
                return;
            }
            if io.loading() {
                self.runs.push((0, Vec::new()));
            }
            let Some(run) = self.runs.get_mut(i as usize) else { return };
            run.0.state(io);
            io.blob(&mut run.1);
        }
    }
}

/// What a savestate holds of the hard disk (its `DISK` section).
#[derive(Default)]
pub struct DiskState {
    /// The cache folder's files.
    pub cache: Vec<DiskFile>,
    /// Its folders, so an empty one comes back too.
    pub dirs: Vec<String>,
    /// The title's saved games and own data, as `UDATA/...` and `TDATA/...`.
    pub saves: Vec<DiskFile>,
    /// Contents carried in the state.
    blobs: Vec<Blob>,
}
state_fields!(DiskState { cache, dirs, saves, blobs });

impl DiskState {
    /// The hashes of the cache's files: what a store must keep for this
    /// state.
    #[must_use]
    pub fn hashes(&self) -> Vec<Hash> {
        self.cache.iter().map(|f| f.hash).collect()
    }
}

/// Hashes already worked out, by path, for as long as the file's length and
/// time stay what they were.
#[derive(Default)]
pub(crate) struct Index {
    known: HashMap<PathBuf, (u64, SystemTime, Hash)>,
}

impl Index {
    /// The length and hash of the file at `path`.
    fn hash(&mut self, path: &Path) -> io::Result<(u64, Hash)> {
        let md = fs::metadata(path)?;
        let modified = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if let Some(&(len, at, hash)) = self.known.get(path)
            && len == md.len()
            && at == modified
        {
            return Ok((len, hash));
        }
        let (len, hash) = hash_file(path)?;
        // Only a file that has settled is remembered by its time.
        if modified.elapsed().is_ok_and(|age| age >= SETTLED) {
            self.known.insert(path.to_path_buf(), (len, modified, hash));
        } else {
            self.known.remove(path);
        }
        Ok((len, hash))
    }
}

/// The name of a hash's file in a store.
#[must_use]
pub fn hash_name(hash: &Hash) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reads up to a block; short only at the end of the file.
fn read_block(file: &mut fs::File, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match file.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

/// Walks a file's blocks that are not all zero, in order, and returns its
/// length.
fn blocks(path: &Path, mut each: impl FnMut(u64, &[u8]) -> io::Result<()>) -> io::Result<u64> {
    let mut file = fs::File::open(path)?;
    let mut buf = vec![0u8; BLOCK];
    let (mut at, mut len) = (0u64, 0u64);
    loop {
        let n = read_block(&mut file, &mut buf)?;
        if n == 0 {
            return Ok(len);
        }
        len += n as u64;
        if buf[..n].iter().any(|&b| b != 0) {
            each(at, &buf[..n])?;
        }
        at += 1;
    }
}

/// A file's length and hash.
fn hash_file(path: &Path) -> io::Result<(u64, Hash)> {
    let mut body = Sha1::new();
    let len = blocks(path, |at, data| {
        body.update(&at.to_le_bytes());
        body.update(data);
        Ok(())
    })?;
    // The length last: it is known only once the file is read.
    body.update(&len.to_le_bytes());
    Ok((len, body.finalize()))
}

/// A name for a file being written next to `path`, moved over it when whole.
fn partial(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".part-{}", std::process::id()));
    path.with_file_name(name)
}

/// Writes a file whole or not at all: under another name, then moved over
/// `path`. A handle open on the old file keeps the old bytes.
fn write_whole(path: &Path, fill: impl FnOnce(&mut fs::File) -> io::Result<u64>) -> io::Result<()> {
    let tmp = partial(path);
    let done = fs::File::create(&tmp).and_then(|mut file| {
        let len = fill(&mut file)?;
        file.set_len(len)
    });
    let done = done.and_then(|()| fs::rename(&tmp, path));
    if done.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    done
}

/// Copies `from` to `to`, zeros as holes.
fn copy_sparse(from: &Path, to: &Path) -> io::Result<()> {
    write_whole(to, |out| {
        blocks(from, |at, data| {
            out.seek(SeekFrom::Start(at * BLOCK as u64))?;
            out.write_all(data)
        })
    })
}

/// A file's contents as a state carries them.
fn read_blob(path: &Path, hash: Hash) -> io::Result<Blob> {
    let mut runs: Vec<(u64, Vec<u8>)> = Vec::new();
    let len = blocks(path, |at, data| {
        let offset = at * BLOCK as u64;
        match runs.last_mut() {
            Some((start, bytes))
                if *start + bytes.len() as u64 == offset && bytes.len() < RUN_MAX =>
            {
                bytes.extend_from_slice(data);
            }
            _ => runs.push((offset, data.to_vec())),
        }
        Ok(())
    })?;
    Ok(Blob { hash, len, runs })
}

fn write_blob(blob: &Blob, to: &Path) -> io::Result<()> {
    write_whole(to, |out| {
        for (offset, bytes) in &blob.runs {
            if offset.saturating_add(bytes.len() as u64) > blob.len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a run past the file's end",
                ));
            }
            out.seek(SeekFrom::Start(*offset))?;
            out.write_all(bytes)?;
        }
        Ok(blob.len)
    })
}

/// A state's path is safe to join to a folder: names only, none of them
/// `.` or `..`, no separator of another kind.
fn safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && rel.split('/').all(|name| {
            !name.is_empty() && name != "." && name != ".." && !name.contains(['\\', ':', '\0'])
        })
}

/// Every file and folder under `root`, as paths relative to it, sorted.
/// Files being written by [`write_whole`] and what is not a file or a folder
/// are left out.
fn walk(root: &Path) -> (Vec<String>, Vec<String>) {
    fn go(dir: &Path, rel: &str, files: &mut Vec<String>, dirs: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                log::warn!(target: "kernel", "state: {} has a name a state cannot hold", entry.path().display());
                continue;
            };
            let rel = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            let Ok(md) = fs::symlink_metadata(entry.path()) else { continue };
            if md.is_dir() {
                dirs.push(rel.clone());
                go(&entry.path(), &rel, files, dirs);
            } else if md.is_file() && !name.contains(".part-") {
                files.push(rel);
            }
        }
    }
    let (mut files, mut dirs) = (Vec::new(), Vec::new());
    go(root, "", &mut files, &mut dirs);
    files.sort_unstable();
    dirs.sort_unstable();
    (files, dirs)
}

/// The files under `root` with their lengths and hashes, each path with
/// `prefix` before it.
fn listing(root: &Path, prefix: &str, index: &mut Index) -> Result<Vec<DiskFile>, String> {
    let (files, _) = walk(root);
    files
        .into_iter()
        .map(|rel| {
            let path = root.join(&rel);
            let (len, hash) = index.hash(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(DiskFile { rel: format!("{prefix}{rel}"), len, hash })
        })
        .collect()
}

/// The saved games' files under the two folders, as `UDATA/...` and
/// `TDATA/...`.
fn saves(dirs: &[PathBuf; 2], index: &mut Index) -> Result<Vec<DiskFile>, String> {
    let mut all = listing(&dirs[0], "UDATA/", index)?;
    all.append(&mut listing(&dirs[1], "TDATA/", index)?);
    Ok(all)
}

/// Takes the disk for a state: the cache folder's files, kept in `store`
/// (made if needed) or, without one, in the state; and the saved games'
/// names and hashes.
///
/// # Errors
/// A file could not be read or kept.
pub(crate) fn capture(
    cache_root: &Path,
    save_dirs: &[PathBuf; 2],
    store: Option<&Path>,
    index: &mut Index,
) -> Result<DiskState, String> {
    let (_, dirs) = walk(cache_root);
    let cache = listing(cache_root, "", index)?;
    let mut blobs = Vec::new();
    if let Some(store) = store {
        fs::create_dir_all(store).map_err(|e| format!("{}: {e}", store.display()))?;
    }
    let mut kept = HashSet::new();
    for file in &cache {
        if !kept.insert(file.hash) {
            continue;
        }
        let from = cache_root.join(&file.rel);
        match store {
            Some(store) => {
                let to = store.join(hash_name(&file.hash));
                if fs::metadata(&to).is_ok_and(|m| m.len() == file.len) {
                    continue;
                }
                copy_sparse(&from, &to).map_err(|e| format!("{}: {e}", to.display()))?;
            }
            None => blobs
                .push(read_blob(&from, file.hash).map_err(|e| format!("{}: {e}", from.display()))?),
        }
    }
    Ok(DiskState { cache, dirs, saves: saves(save_dirs, index)?, blobs })
}

/// Makes the cache folder what `state` says: files it does not name go,
/// files that differ are written from the state or from `store`. Nothing is
/// changed unless everything needed is at hand.
///
/// # Errors
/// The state names a path that is not one, or contents that are in neither
/// the state nor the store; or a file could not be written (the folder is
/// then part the state's and part not).
pub(crate) fn restore(
    state: &DiskState,
    cache_root: &Path,
    store: Option<&Path>,
    index: &mut Index,
) -> Result<(), String> {
    if let Some(bad) =
        state.cache.iter().map(|f| &f.rel).chain(&state.dirs).find(|rel| !safe_rel(rel))
    {
        return Err(format!("the state names a hard disk path that is not one ({bad:?})"));
    }
    let now = listing(cache_root, "", index)?;
    let have: HashMap<&str, &DiskFile> = now.iter().map(|f| (f.rel.as_str(), f)).collect();
    let blobs: HashMap<Hash, &Blob> = state.blobs.iter().map(|b| (b.hash, b)).collect();
    let stored = |hash: &Hash| store.map(|s| s.join(hash_name(hash))).filter(|p| p.is_file());

    // What must be written, and that all of it can be.
    let wanted: Vec<&DiskFile> = state
        .cache
        .iter()
        .filter(|f| !have.get(f.rel.as_str()).is_some_and(|h| *h == *f))
        .collect();
    if let Some(lost) =
        wanted.iter().find(|f| !blobs.contains_key(&f.hash) && stored(&f.hash).is_none())
    {
        return Err(format!(
            "the state's hard disk file {} is not kept here (it was saved with another data directory)",
            lost.rel
        ));
    }

    let named: HashSet<&str> = state.cache.iter().map(|f| f.rel.as_str()).collect();
    for old in now.iter().filter(|f| !named.contains(f.rel.as_str())) {
        let _ = fs::remove_file(cache_root.join(&old.rel));
    }
    let folders: HashSet<&str> = state.dirs.iter().map(String::as_str).collect();
    let (_, there) = walk(cache_root);
    for old in there.iter().rev().filter(|d| !folders.contains(d.as_str())) {
        let _ = fs::remove_dir_all(cache_root.join(old));
    }
    for dir in &state.dirs {
        let path = cache_root.join(dir);
        if fs::symlink_metadata(&path).is_ok_and(|m| !m.is_dir()) {
            let _ = fs::remove_file(&path);
        }
        fs::create_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    for file in &wanted {
        let to = cache_root.join(&file.rel);
        if fs::symlink_metadata(&to).is_ok_and(|m| m.is_dir()) {
            let _ = fs::remove_dir_all(&to);
        }
        let written = match (blobs.get(&file.hash), stored(&file.hash)) {
            (Some(blob), _) => write_blob(blob, &to),
            (None, Some(from)) => copy_sparse(&from, &to),
            (None, None) => Err(io::Error::new(io::ErrorKind::NotFound, "its contents are gone")),
        };
        written.map_err(|e| format!("{}: {e}", to.display()))?;
        index.known.remove(&to);
    }
    if !wanted.is_empty() {
        log::info!(
            target: "kernel",
            "state: {} of the cache's {} file(s) put back ({} MiB)",
            wanted.len(),
            state.cache.len(),
            wanted.iter().map(|f| f.len).sum::<u64>() >> 20
        );
    }
    Ok(())
}

/// The saved games that are not what the state was taken with: a line each,
/// none when they agree.
pub(crate) fn saves_changed(
    state: &DiskState,
    save_dirs: &[PathBuf; 2],
    index: &mut Index,
) -> Vec<String> {
    let Ok(now) = saves(save_dirs, index) else {
        return vec!["the saved games could not be read".to_owned()];
    };
    let then: HashMap<&str, &DiskFile> = state.saves.iter().map(|f| (f.rel.as_str(), f)).collect();
    let have: HashMap<&str, &DiskFile> = now.iter().map(|f| (f.rel.as_str(), f)).collect();
    let mut lines: Vec<String> = Vec::new();
    for file in &state.saves {
        match have.get(file.rel.as_str()) {
            None => lines.push(format!("{} is gone", file.rel)),
            Some(f) if *f != file => lines.push(format!("{} has changed", file.rel)),
            Some(_) => {}
        }
    }
    lines.extend(
        now.iter()
            .filter(|f| !then.contains_key(f.rel.as_str()))
            .map(|f| format!("{} is new", f.rel)),
    );
    lines
}

impl Kernel {
    /// The hard disk for a state taken now (`capture`): call at a pause,
    /// after [`Kernel::state_quiesce`]. Without a hard disk, nothing.
    ///
    /// # Errors
    /// A file of the cache could not be read or kept.
    pub fn disk_state_save(&mut self, store: Option<&Path>) -> Result<DiskState, String> {
        let Some(disk) = self.io_fs().hard_disk().cloned() else {
            return Ok(DiskState::default());
        };
        let t0 = std::time::Instant::now();
        let title = self.title.title_id;
        let state =
            capture(&disk.cache_root(), &disk.title_data_dirs(title), store, &mut self.io.disk)?;
        log::debug!(
            target: "kernel",
            "state: the disk in {:.0} ms: {} cache file(s), {} MiB, {}; {} saved-game file(s)",
            t0.elapsed().as_secs_f64() * 1e3,
            state.cache.len(),
            state.cache.iter().map(|f| f.len).sum::<u64>() >> 20,
            if store.is_some() { "in the store" } else { "in the state" },
            state.saves.len()
        );
        Ok(state)
    }

    /// Puts a state's cache files in place (`restore`) before its open
    /// files are opened again, and returns what differs in the saved games.
    ///
    /// # Errors
    /// As `restore`; nothing has changed unless a file could not be
    /// written.
    pub fn disk_state_load(
        &mut self,
        state: &DiskState,
        store: Option<&Path>,
    ) -> Result<Vec<String>, String> {
        let Some(disk) = self.io_fs().hard_disk().cloned() else {
            return Ok(Vec::new());
        };
        restore(state, &disk.cache_root(), store, &mut self.io.disk)?;
        disk.cache_replaced();
        Ok(saves_changed(state, &disk.title_data_dirs(self.title.title_id), &mut self.io.disk))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch folder with `cache`, `store`, `udata` and `tdata` in it.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("xb-kernel-disk-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            for sub in ["cache", "udata", "tdata"] {
                fs::create_dir_all(dir.join(sub)).unwrap();
            }
            Self(dir)
        }
        fn cache(&self) -> PathBuf {
            self.0.join("cache")
        }
        fn store(&self) -> PathBuf {
            self.0.join("store")
        }
        fn saves(&self) -> [PathBuf; 2] {
            [self.0.join("udata"), self.0.join("tdata")]
        }
        fn put(&self, rel: &str, bytes: &[u8]) {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        /// Every file under the cache with its bytes.
        fn contents(&self) -> Vec<(String, Vec<u8>)> {
            let (files, _) = walk(&self.cache());
            files
                .into_iter()
                .map(|f| (f.clone(), fs::read(self.cache().join(f)).unwrap()))
                .collect()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A file a title sized and filled in two places: zeros, a block of
    /// data, zeros, and a tail that is not a whole block.
    fn holed() -> Vec<u8> {
        let mut bytes = vec![0u8; 5 * BLOCK + 123];
        bytes[BLOCK + 7..2 * BLOCK].fill(0xA5);
        bytes[5 * BLOCK..].fill(0x3C);
        bytes
    }

    fn fill(s: &Scratch) {
        s.put("cache/Partition3/cache000.map", &holed());
        s.put("cache/Partition3/saved/deep/x.bin", b"a level's checkpoint");
        s.put("cache/Partition3/empty.map", &vec![0u8; 3 * BLOCK]);
        s.put("cache/raw-partition3.bin", b"FATX....");
        fs::create_dir_all(s.cache().join("Partition4")).unwrap();
        s.put("udata/0001/save.bin", b"progress");
    }

    /// What a later session leaves: files rewritten, gone and new.
    fn disturb(s: &Scratch) {
        s.put("cache/Partition3/cache000.map", &vec![0u8; 5 * BLOCK + 123]);
        fs::remove_dir_all(s.cache().join("Partition3/saved")).unwrap();
        fs::remove_dir_all(s.cache().join("Partition4")).unwrap();
        s.put("cache/Partition5/other.map", b"another title's level");
    }

    /// A state's disk through the bytes of a savestate.
    fn through_a_state(state: &mut DiskState) -> DiskState {
        let mut io = StateIo::save();
        io.section(*b"DISK", 1, |io, _| state.state(io));
        let mut io = StateIo::load(io.into_bytes());
        let mut back = DiskState::default();
        io.section(*b"DISK", 1, |io, _| back.state(io));
        assert_eq!(io.failed(), None);
        back
    }

    #[test]
    fn a_cache_put_back_from_the_store_is_the_cache_that_was_saved() {
        let s = Scratch::new("store");
        fill(&s);
        let before = s.contents();
        let mut index = Index::default();
        let mut state = capture(&s.cache(), &s.saves(), Some(&s.store()), &mut index).unwrap();
        let state = through_a_state(&mut state);
        disturb(&s);
        assert_ne!(s.contents(), before);
        restore(&state, &s.cache(), Some(&s.store()), &mut index).unwrap();
        assert_eq!(s.contents(), before);
        assert!(s.cache().join("Partition4").is_dir(), "an empty folder comes back");
        assert!(!s.cache().join("Partition5").exists(), "what the state does not name goes");
    }

    #[test]
    fn a_state_that_carries_its_files_needs_no_store() {
        let s = Scratch::new("carried");
        fill(&s);
        let before = s.contents();
        let mut state = capture(&s.cache(), &s.saves(), None, &mut Index::default()).unwrap();
        let state = through_a_state(&mut state);
        disturb(&s);
        // Another process, another data directory: nothing remembered.
        restore(&state, &s.cache(), None, &mut Index::default()).unwrap();
        assert_eq!(s.contents(), before);
    }

    #[test]
    fn a_state_whose_files_are_nowhere_is_refused_and_changes_nothing() {
        let s = Scratch::new("lost");
        fill(&s);
        let mut index = Index::default();
        let state = capture(&s.cache(), &s.saves(), Some(&s.store()), &mut index).unwrap();
        disturb(&s);
        let disturbed = s.contents();
        fs::remove_dir_all(s.store()).unwrap();
        let why = restore(&state, &s.cache(), Some(&s.store()), &mut index).unwrap_err();
        assert!(why.contains("not kept here"), "{why}");
        assert_eq!(s.contents(), disturbed);
    }

    #[test]
    fn the_store_keeps_one_file_for_equal_contents_and_holes_stay_holes() {
        let s = Scratch::new("dedup");
        fill(&s);
        s.put("cache/Partition3/cache001.map", &holed());
        let state =
            capture(&s.cache(), &s.saves(), Some(&s.store()), &mut Index::default()).unwrap();
        let names: HashSet<Hash> = state.hashes().into_iter().collect();
        assert_eq!(fs::read_dir(s.store()).unwrap().count(), names.len());
        assert!(names.len() < state.cache.len());
        // The same bytes at another length are another file.
        let (_, a) = hash_file(&s.cache().join("Partition3/empty.map")).unwrap();
        s.put("cache/Partition3/empty2.map", &vec![0u8; 3 * BLOCK + 1]);
        let (_, b) = hash_file(&s.cache().join("Partition3/empty2.map")).unwrap();
        assert_ne!(a, b);
        // Data that moved by a block is another file too.
        let mut moved = holed();
        moved.copy_within(BLOCK..2 * BLOCK, 2 * BLOCK);
        moved[BLOCK..2 * BLOCK].fill(0);
        s.put("cache/Partition3/moved.map", &moved);
        let (_, c) = hash_file(&s.cache().join("Partition3/moved.map")).unwrap();
        let (_, d) = hash_file(&s.cache().join("Partition3/cache000.map")).unwrap();
        assert_ne!(c, d);
    }

    #[test]
    fn a_state_cannot_name_a_path_outside_the_cache() {
        let s = Scratch::new("escape");
        fill(&s);
        let mut index = Index::default();
        for bad in ["../udata/0001/save.bin", "/etc/passwd", "a//b", "a/./b", "c:\\x", ""] {
            let mut state = capture(&s.cache(), &s.saves(), None, &mut index).unwrap();
            state.cache[0].rel = bad.to_owned();
            assert!(restore(&state, &s.cache(), None, &mut index).is_err(), "{bad:?}");
            let mut state = capture(&s.cache(), &s.saves(), None, &mut index).unwrap();
            state.dirs.push(bad.to_owned());
            assert!(restore(&state, &s.cache(), None, &mut index).is_err(), "{bad:?}");
        }
        assert_eq!(fs::read(s.0.join("udata/0001/save.bin")).unwrap(), b"progress");
    }

    #[test]
    fn saved_games_are_left_alone_and_what_changed_is_told() {
        let s = Scratch::new("saves");
        fill(&s);
        let mut index = Index::default();
        let state = capture(&s.cache(), &s.saves(), None, &mut index).unwrap();
        assert!(saves_changed(&state, &s.saves(), &mut index).is_empty());
        s.put("udata/0001/save.bin", b"more progress");
        s.put("tdata/new.bin", b"x");
        restore(&state, &s.cache(), None, &mut index).unwrap();
        assert_eq!(fs::read(s.0.join("udata/0001/save.bin")).unwrap(), b"more progress");
        let told = saves_changed(&state, &s.saves(), &mut index);
        assert_eq!(told, ["UDATA/0001/save.bin has changed", "TDATA/new.bin is new"]);
    }

    #[test]
    fn a_file_rewritten_at_the_same_length_is_hashed_again() {
        let s = Scratch::new("index");
        s.put("cache/a.bin", b"one");
        let mut index = Index::default();
        let (_, one) = index.hash(&s.cache().join("a.bin")).unwrap();
        s.put("cache/a.bin", b"two");
        let (_, two) = index.hash(&s.cache().join("a.bin")).unwrap();
        assert_ne!(one, two);
    }
}
