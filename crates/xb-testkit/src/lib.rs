//! Test support: the titles stash.
//!
//! Tests that need a real game cannot ship one. A developer links their own
//! disc images into the stash (`titles/` at the repository root, or the
//! directory `XBEMU_TITLES_DIR` names), each under the key `titles/README.md`
//! lists for it, and the tests find them here. A test whose title is not in
//! the stash prints a note saying which link is missing and passes, which is
//! what happens in CI: it has no game data and no GPU.
//!
//! An image is `<key>.<extension>` (any extension: `.cci`, `.iso`, `.xiso`),
//! or `<key>.1.cci` for a split set with its other parts beside it. A link
//! works as well as a file.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

/// The stash: `XBEMU_TITLES_DIR` when set, else `titles/` at the repository
/// root.
pub fn titles_dir() -> PathBuf {
    match std::env::var_os("XBEMU_TITLES_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../titles"),
    }
}

/// The key a stash entry is filed under, when it is an image: the file name
/// up to its first dot. `None` for a split set's later parts, for the stash's
/// own files and for anything hidden.
fn key_of(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let (key, rest) = name.split_once('.')?;
    let later_part = rest
        .split_once('.')
        .and_then(|(part, _)| part.parse::<u32>().ok())
        .is_some_and(|part| part != 1);
    let own = matches!(rest, "md" | "log" | "ref.log");
    (!key.is_empty() && !later_part && !own).then_some(key)
}

/// Every image in `dir`, as (key, path), in key order.
fn images_in(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<(String, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter_map(|p| key_of(&p).map(str::to_owned).map(|key| (key, p)))
        .collect();
    found.sort();
    found
}

/// Every image in the stash, as (key, path), in key order. Empty, with a
/// note, when the stash holds none.
pub fn all_titles() -> Vec<(String, PathBuf)> {
    let dir = titles_dir();
    let found = images_in(&dir);
    if found.is_empty() {
        println!(
            "note: no game images in {} (see titles/README.md); nothing checked",
            dir.display()
        );
    }
    found
}

/// The image filed under `key`, or `None` with a note naming the link to
/// make.
pub fn title(key: &str) -> Option<PathBuf> {
    let dir = titles_dir();
    let found = images_in(&dir).into_iter().find(|(k, _)| k == key).map(|(_, path)| path);
    if found.is_none() {
        println!(
            "note: no '{key}' in the titles stash; link your copy as {} (see titles/README.md); test skipped",
            dir.join(format!("{key}.cci")).display()
        );
    }
    found
}

/// Any one image from the stash (the first in key order), for checks that
/// hold for every title. `None`, with a note, when the stash holds none.
pub fn any_title() -> Option<PathBuf> {
    all_titles().into_iter().next().map(|(_, path)| path)
}

/// The text of `<key>.ref.log` in the stash: a log of another emulator
/// running the title, which some tests compare their own run with. `None`
/// when there is none; such a test then checks only what it can alone.
pub fn reference_log(key: &str) -> Option<String> {
    std::fs::read_to_string(titles_dir().join(format!("{key}.ref.log"))).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_is_keyed_by_its_name_up_to_the_first_dot() {
        assert_eq!(key_of(Path::new("/t/halo-ce.cci")), Some("halo-ce"));
        assert_eq!(key_of(Path::new("/t/halo-ce.iso")), Some("halo-ce"));
        assert_eq!(key_of(Path::new("/t/ninja-gaiden.1.cci")), Some("ninja-gaiden"));
    }

    #[test]
    fn later_parts_and_the_stashs_own_files_are_not_images() {
        assert_eq!(key_of(Path::new("/t/ninja-gaiden.2.cci")), None);
        assert_eq!(key_of(Path::new("/t/README.md")), None);
        assert_eq!(key_of(Path::new("/t/halo-ce.ref.log")), None);
        assert_eq!(key_of(Path::new("/t/.gitignore")), None);
        assert_eq!(key_of(Path::new("/t/noextension")), None);
    }

    #[test]
    fn a_stash_lists_its_images_in_key_order_and_only_them() {
        let dir = std::env::temp_dir().join(format!("xb-testkit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["b.1.cci", "b.2.cci", "a.iso", "README.md", "a.ref.log"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let keys: Vec<String> = images_in(&dir).into_iter().map(|(key, _)| key).collect();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(keys, ["a", "b"]);
    }
}
