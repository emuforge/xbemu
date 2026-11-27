//! Savestate slots: the states a player keeps for a title, numbered, each a
//! file in the data directory with a picture of the moment beside it.
//!
//! `states/<title id>/slot<N>.state` holds [`crate::Core::save_state`]'s
//! bytes and `slot<N>.bmp` what was on screen (the presenter's own
//! screenshot, taken at the first frame shown after the save, with the menu
//! closed). The slot in use is the session's; a front end changes it and
//! saves and loads with its own keys, and the menu lists the slots.
//!
//! `states/<title id>/disk/` is the slots' store: the files of the title's
//! cache partitions as the slots were saved with them, each once, named by
//! its hash (`xb_kernel::DiskState`). `slot<N>.disk` lists the hashes its
//! slot needs, so a file no slot names any more can go.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// How many slots a title has.
pub const STATE_SLOTS: usize = 10;

/// What a slot holds, for a list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SlotInfo {
    /// A state is saved in it.
    pub used: bool,
    /// When it was saved, in seconds before now; 0 for an empty slot.
    pub age_secs: u64,
}

/// A slot's picture, straight alpha, top row first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Thumbnail {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes, R, G, B, A.
    pub rgba: Vec<u8>,
}

/// The folder a title's slots are in.
pub(crate) fn dir(data_dir: &Path, title_id: u32) -> PathBuf {
    data_dir.join("states").join(format!("{title_id:08x}"))
}

/// A slot's state file and its picture.
pub(crate) fn paths(data_dir: &Path, title_id: u32, slot: usize) -> (PathBuf, PathBuf) {
    let dir = dir(data_dir, title_id);
    (dir.join(format!("slot{slot}.state")), dir.join(format!("slot{slot}.bmp")))
}

/// The folder the slots of a title keep their hard disk files in.
pub(crate) fn store(data_dir: &Path, title_id: u32) -> PathBuf {
    dir(data_dir, title_id).join("disk")
}

/// The list beside a state file of the store's files it needs.
fn needs_path(state: &Path) -> PathBuf {
    state.with_extension("disk")
}

/// After the state at `state` was written: records the store's files it
/// needs and removes from `store` the ones no state in the folder needs any
/// more. Nothing is removed while a state there has no list (it could need
/// anything).
pub(crate) fn keep(state: &Path, store: &Path, needs: &[xb_kernel::DiskHash]) {
    let names: Vec<String> = needs.iter().map(xb_kernel::disk_hash_name).collect();
    if let Err(e) = std::fs::write(needs_path(state), names.join("\n")) {
        log::warn!(target: "core", "{}: {e}", needs_path(state).display());
        return;
    }
    let Some(dir) = state.parent() else { return };
    let mut needed = std::collections::HashSet::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "state") {
            let Ok(list) = std::fs::read_to_string(needs_path(&path)) else { return };
            needed.extend(list.lines().map(str::to_owned));
        }
    }
    for entry in std::fs::read_dir(store).into_iter().flatten().flatten() {
        if !needed.contains(entry.file_name().to_string_lossy().as_ref()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// What each slot holds.
pub(crate) fn list(data_dir: &Path, title_id: u32) -> Vec<SlotInfo> {
    let now = SystemTime::now();
    (0..STATE_SLOTS)
        .map(|slot| {
            let (state, _) = paths(data_dir, title_id, slot);
            match std::fs::metadata(&state) {
                Ok(meta) if meta.is_file() => SlotInfo {
                    used: true,
                    age_secs: meta
                        .modified()
                        .ok()
                        .and_then(|t| now.duration_since(t).ok())
                        .map_or(0, |d| d.as_secs()),
                },
                _ => SlotInfo::default(),
            }
        })
        .collect()
}

/// How long ago, in a few characters: "now", "5 min", "3 h", "2 d".
#[must_use]
pub fn age_text(secs: u64) -> String {
    match secs {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

/// Reads the presenter's screenshot (a 32-bit top-down BMP: B, G, R, A) and
/// scales it down to at most `max_w` x `max_h`, keeping its shape, by
/// averaging the pixels each new one covers. `None` if the bytes are not
/// such a picture.
#[must_use]
pub fn thumbnail(bmp: &[u8], max_w: u32, max_h: u32) -> Option<Thumbnail> {
    let head = bmp.get(..54)?;
    if &head[..2] != b"BM" || u16::from_le_bytes([head[28], head[29]]) != 32 {
        return None;
    }
    let word = |at: usize| u32::from_le_bytes(head[at..at + 4].try_into().expect("4 bytes"));
    let (offset, w, h_raw) = (word(10) as usize, word(18), word(22) as i32);
    let (h, top_down) = (h_raw.unsigned_abs(), h_raw < 0);
    if w == 0 || h == 0 || w > 16_384 || h > 16_384 || max_w == 0 || max_h == 0 {
        return None;
    }
    let pixels = bmp.get(offset..offset.checked_add(w as usize * h as usize * 4)?)?;
    // The largest size with the picture's shape that fits.
    let (tw, th) = if u64::from(w) * u64::from(max_h) >= u64::from(h) * u64::from(max_w) {
        (max_w.min(w), (u64::from(h) * u64::from(max_w.min(w)) / u64::from(w)).max(1) as u32)
    } else {
        ((u64::from(w) * u64::from(max_h.min(h)) / u64::from(h)).max(1) as u32, max_h.min(h))
    };
    let mut rgba = Vec::with_capacity(tw as usize * th as usize * 4);
    for ty in 0..th {
        let (y0, y1) = (ty * h / th, ((ty + 1) * h / th).max(ty * h / th + 1));
        for tx in 0..tw {
            let (x0, x1) = (tx * w / tw, ((tx + 1) * w / tw).max(tx * w / tw + 1));
            let mut sum = [0u32; 3];
            for y in y0..y1 {
                let row = if top_down { y } else { h - 1 - y } as usize * w as usize * 4;
                for x in x0..x1 {
                    let p = &pixels[row + x as usize * 4..][..3];
                    sum[0] += u32::from(p[2]);
                    sum[1] += u32::from(p[1]);
                    sum[2] += u32::from(p[0]);
                }
            }
            let n = (y1 - y0) * (x1 - x0);
            rgba.extend([(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8, 0xFF]);
        }
    }
    Some(Thumbnail { width: tw, height: th, rgba })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_store_keeps_what_a_slot_needs_and_drops_what_none_does() {
        let dir = std::env::temp_dir().join(format!("xb-core-slots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = dir.join("disk");
        std::fs::create_dir_all(&store).unwrap();
        let (a, b, c) = ([1u8; 20], [2u8; 20], [3u8; 20]);
        for hash in [&a, &b, &c] {
            std::fs::write(store.join(xb_kernel::disk_hash_name(hash)), b"x").unwrap();
        }
        let stored = |hash: &[u8; 20]| store.join(xb_kernel::disk_hash_name(hash)).exists();

        // A state with no list beside it could need anything: nothing goes.
        std::fs::write(dir.join("slot1.state"), b"old").unwrap();
        std::fs::write(dir.join("slot0.state"), b"new").unwrap();
        keep(&dir.join("slot0.state"), &store, &[a, b]);
        assert!(stored(&a) && stored(&b) && stored(&c));

        // Every state listed: what none of them names goes.
        keep(&dir.join("slot1.state"), &store, &[b]);
        assert!(stored(&a) && stored(&b) && !stored(&c));

        // A slot saved again over a cache that changed.
        keep(&dir.join("slot0.state"), &store, &[b]);
        assert!(!stored(&a) && stored(&b));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A top-down 32-bit BMP as the presenter writes it.
    fn bmp(w: u32, h: u32, pixel: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
        let mut b = vec![0u8; 54];
        b[..2].copy_from_slice(b"BM");
        b[10..14].copy_from_slice(&54u32.to_le_bytes());
        b[14..18].copy_from_slice(&40u32.to_le_bytes());
        b[18..22].copy_from_slice(&w.to_le_bytes());
        b[22..26].copy_from_slice(&(-(h as i32)).to_le_bytes());
        b[26] = 1;
        b[28] = 32;
        for y in 0..h {
            for x in 0..w {
                let [r, g, bl] = pixel(x, y);
                b.extend([bl, g, r, 0xFF]);
            }
        }
        b
    }

    #[test]
    fn a_thumbnail_keeps_the_picture_s_shape_and_averages_its_pixels() {
        // Left half red, right half blue, 8x4.
        let src = bmp(8, 4, |x, _| if x < 4 { [200, 0, 0] } else { [0, 0, 100] });
        let t = thumbnail(&src, 4, 4).unwrap();
        assert_eq!((t.width, t.height), (4, 2), "8:4 into 4x4 is 4x2");
        assert_eq!(&t.rgba[..4], [200, 0, 0, 255]);
        assert_eq!(&t.rgba[12..16], [0, 0, 100, 255]);
        // One pixel for all of it: the average.
        let t = thumbnail(&src, 1, 1).unwrap();
        assert_eq!(t.rgba, [100, 0, 50, 255]);
        // Never scaled up.
        let t = thumbnail(&src, 100, 100).unwrap();
        assert_eq!((t.width, t.height), (8, 4));
    }

    #[test]
    fn bytes_that_are_not_the_presenter_s_picture_give_no_thumbnail() {
        assert_eq!(thumbnail(b"", 4, 4), None);
        assert_eq!(thumbnail(&[0u8; 100], 4, 4), None);
        let mut cut = bmp(8, 4, |_, _| [1, 2, 3]);
        cut.truncate(cut.len() - 1);
        assert_eq!(thumbnail(&cut, 4, 4), None);
    }

    #[test]
    fn ages_read_as_a_few_words() {
        assert_eq!(age_text(5), "just now");
        assert_eq!(age_text(125), "2 min ago");
        assert_eq!(age_text(7300), "2 h ago");
        assert_eq!(age_text(200_000), "2 d ago");
    }
}
