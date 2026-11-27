//! End-to-end checks against a real game image.
//!
//! Run on every image in the titles stash (`titles/README.md`); with none
//! every test passes with a note. Checks that hold for any title run on each
//! image. One title's known values are checked when the image is that title.

use std::collections::BTreeSet;

use xb_formats::disc::{self, DiscImage, ImageKind};
use xb_formats::xbe::{self, Sections, Xbe};
use xb_formats::xdvdfs::Xdvdfs;
use xb_formats::{BlockSource, SECTOR_SIZE};

/// Halo: Combat Evolved (USA).
const HALO: u32 = 0x4D53_0004;

/// The kernel ordinals in thunk table order, from the imports line the
/// reference logs for title 4d530004.
const HALO_IMPORTS: [u32; 134] = [
    294, 277, 250, 143, 246, 259, 224, 302, 238, 258, 255, 187, 189, 16, 289, 225, 186, 192, 221,
    234, 235, 99, 305, 128, 156, 304, 184, 199, 217, 181, 308, 113, 14, 149, 97, 145, 159, 161,
    129, 107, 65, 74, 301, 226, 178, 165, 164, 171, 49, 215, 203, 236, 219, 190, 211, 172, 202,
    167, 210, 207, 233, 218, 198, 232, 200, 196, 166, 180, 182, 179, 40, 67, 336, 335, 337, 323,
    325, 24, 356, 269, 291, 322, 125, 360, 83, 87, 81, 17, 359, 358, 44, 160, 47, 98, 109, 151,
    279, 327, 328, 173, 175, 119, 176, 1, 2, 3, 4, 8, 100, 168, 23, 15, 137, 153, 142, 139, 252,
    253, 158, 339, 338, 150, 349, 347, 346, 340, 345, 353, 42, 41, 95, 312, 126, 127,
];

/// Every image in the titles stash, opened.
fn images() -> Vec<DiscImage> {
    xb_testkit::all_titles()
        .into_iter()
        .map(|(_, path)| disc::open(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .collect()
}

/// The pages a loader backs at boot: the headers' and the preload sections'.
fn boot_pages(xbe: &Xbe) -> BTreeSet<u32> {
    let mut pages = BTreeSet::new();
    for seg in xbe.segments(Sections::Preload).unwrap() {
        let end = u64::from(seg.virtual_addr) + u64::from(seg.virtual_size);
        pages.extend((u64::from(seg.virtual_addr) >> 12..end.div_ceil(4096)).map(|p| p as u32));
    }
    pages
}

#[test]
fn the_boot_executable_parses_and_its_parts_agree() {
    for img in images() {
        println!("{}", img.path.display());
        the_boot_executable_parses_and_its_parts_agree_on(img);
    }
}

fn the_boot_executable_parses_and_its_parts_agree_on(img: DiscImage) {
    let fs = Xdvdfs::open(img.source.clone()).unwrap();
    let entry = fs.lookup("\\default.xbe").unwrap();
    assert_eq!(fs.lookup("DEFAULT.XBE").unwrap(), entry);
    assert!(!entry.is_dir());
    let file = fs.file_source(&entry);
    let xbe = Xbe::read(&file).unwrap();
    let cert = xbe.certificate.as_ref().expect("a certificate");
    println!(
        "xbe: title {:08x} {:?} v{} region {:#x}, base {:08x}, image {} KiB, {} sections ({} preload), entry {:08x} ({:?}), thunk {:08x}, tls {:08x}",
        cert.title_id,
        cert.title_name,
        cert.version,
        cert.game_region,
        xbe.base,
        xbe.image_span() >> 10,
        xbe.sections.len(),
        xbe.sections.iter().filter(|s| s.is_preload()).count(),
        xbe.entry_point,
        xbe.encoding,
        xbe.kernel_thunk_addr,
        xbe.tls_addr,
    );
    for l in &xbe.libraries {
        println!("xbe: library {:8} {}.{}.{} qfe {}", l.name, l.major, l.minor, l.build, l.qfe);
    }

    // The headers the parser kept are the file's first bytes.
    let mut head = vec![0u8; xbe.headers_size as usize];
    fs.read_file(&entry, 0, &mut head).unwrap();
    assert!(head == xbe.headers());

    // The entry point is code that is loaded at boot.
    let code = xbe.section_at(xbe.entry_point).expect("the entry point is in a section");
    assert!(code.is_preload() && code.flags & xbe::SECTION_EXECUTABLE != 0);

    // The image as a loader places it.
    let segs = xbe.segments(Sections::All).unwrap();
    assert_eq!(segs.len(), xbe.sections.len() + 1);
    let span = xbe.image_span() as usize;
    let mut mem = vec![0u8; span];
    let mut loaded = 0u64;
    for seg in xbe.segments(Sections::Preload).unwrap() {
        let at = (seg.virtual_addr - xbe.base) as usize;
        xbe.read_segment(&file, &seg, &mut mem[at..at + seg.virtual_size as usize]).unwrap();
        loaded += u64::from(seg.file_size);
    }
    assert!(loaded <= u64::from(entry.size));

    // The import list read from the loaded image is the one read from the file.
    let read_mem = |va: u32, buf: &mut [u8]| {
        let off = va.checked_sub(xbe.base).ok_or(xb_formats::Error::NotFound)? as usize;
        buf.copy_from_slice(mem.get(off..off + buf.len()).ok_or(xb_formats::Error::NotFound)?);
        Ok(())
    };
    let imports = xbe.kernel_imports_with(read_mem).unwrap();
    assert_eq!(imports, xbe.kernel_imports(&file).unwrap());
    assert!(!imports.is_empty());
    assert!(imports.iter().all(|i| (1..=378).contains(&i.ordinal)), "ordinals the kernel exports");
    let unique: BTreeSet<u32> = imports.iter().map(|i| i.ordinal).collect();
    assert_eq!(unique.len(), imports.len(), "no ordinal is imported twice");
    assert!(xbe.section_at(xbe.kernel_thunk_addr).is_some_and(|s| s.is_preload()));

    // The TLS directory, likewise, and its data lies in the image.
    let tls = xbe.tls_directory_with(read_mem).unwrap();
    assert_eq!(tls, xbe.tls_directory(&file).unwrap());
    if let Some(t) = tls {
        println!(
            "xbe: tls data {:08x}..{:08x} zero fill {} index at {:08x}",
            t.raw_data_start, t.raw_data_end, t.zero_fill_size, t.index_addr
        );
        assert!(t.raw_data_start <= t.raw_data_end);
        // A title without initialized thread-local data has no template.
        if t.raw_data_start != t.raw_data_end {
            assert!(xbe.is_mapped(t.raw_data_start, t.raw_data_end - t.raw_data_start));
        }
        assert!(xbe.is_mapped(t.index_addr, 4));
    }

    // The file's view of the image agrees with the loaded image where a
    // preload section or the headers lie.
    let mut buf = vec![0u8; 0x3000];
    for seg in xbe.segments(Sections::Preload).unwrap() {
        let len = buf.len().min(seg.virtual_size as usize);
        xbe.read_image(&file, seg.virtual_addr, &mut buf[..len]).unwrap();
        let at = (seg.virtual_addr - xbe.base) as usize;
        assert!(buf[..len] == mem[at..at + len], "segment at {:08x}", seg.virtual_addr);
    }

    if cert.title_id != HALO {
        println!("xbe: not Halo, the reference run's values are not checked");
        return;
    }
    // "stage S0 xbe loaded: title 4d530004 base 00010000 image 3732 KiB (3716
    // KiB in memory), 22/24 sections, entry 00015cc9, 134 kernel imports, tls
    // 001d6a80"
    assert_eq!(xbe.base, 0x0001_0000);
    assert_eq!(xbe.entry_point, 0x0001_5CC9);
    assert_eq!(xbe.encoding, xbe::Encoding::Retail);
    assert_eq!(xbe.image_span() >> 10, 3732);
    assert_eq!(boot_pages(&xbe).len() * 4, 3716);
    assert_eq!(xbe.sections.len(), 24);
    assert_eq!(xbe.sections.iter().filter(|s| s.is_preload()).count(), 22);
    assert_eq!(xbe.tls_addr, 0x001D_6A80);
    assert_eq!(imports.len(), 134);
    assert_eq!(imports.iter().map(|i| i.ordinal).collect::<Vec<_>>(), HALO_IMPORTS);
    assert!(cert.title_name.starts_with("Halo"), "{:?}", cert.title_name);
}

#[test]
fn the_volume_walks_and_every_entry_is_found_by_its_path() {
    for img in images() {
        println!("{}", img.path.display());
        the_volume_walks_and_every_entry_is_found_by_its_path_on(img);
    }
}

fn the_volume_walks_and_every_entry_is_found_by_its_path_on(img: DiscImage) {
    let fs = Xdvdfs::open(img.source.clone()).unwrap();
    let root = fs.root();
    println!(
        "xdvdfs: {:?}, image {} MiB stored in {} MiB, volume at +{:#x}, root sector {} ({} bytes)",
        img.kind,
        img.source.len() >> 20,
        img.stored_size >> 20,
        fs.partition_offset(),
        root.sector,
        root.size
    );
    let mut entries = Vec::new();
    fs.walk(|p, e| entries.push((p.to_owned(), e.clone()))).unwrap();
    let (files, dirs) = (
        entries.iter().filter(|e| !e.1.is_dir()).count(),
        entries.iter().filter(|e| e.1.is_dir()).count(),
    );
    let bytes: u64 = entries.iter().filter(|e| !e.1.is_dir()).map(|e| u64::from(e.1.size)).sum();
    println!("xdvdfs: {files} files, {dirs} directories, {} MiB of files", bytes >> 20);
    assert!(files > 0);

    let mut past_end = 0;
    let mut head = [0u8; 4096];
    let mut tail = [0u8; 4096];
    for (path, e) in &entries {
        // Found again by its path, in either case and with either slash.
        assert_eq!(&fs.lookup(path).unwrap(), e, "{path}");
        assert_eq!(&fs.lookup(&path.to_ascii_uppercase().replace('\\', "/")).unwrap(), e, "{path}");
        if e.is_dir() {
            continue;
        }
        let end = fs.data_offset(e) + u64::from(e.size);
        past_end += usize::from(end > img.source.len());
        // Its first and last bytes read, and a window on it reads the same.
        let n = head.len().min(e.size as usize);
        fs.read_file(e, 0, &mut head[..n]).unwrap();
        fs.read_file(e, u64::from(e.size) - n as u64, &mut tail[..n]).unwrap();
        let src = fs.file_source(e);
        let mut again = [0u8; 4096];
        src.read_at(u64::from(e.size) - n as u64, &mut again[..n]).unwrap();
        assert!(tail[..n] == again[..n], "{path}");
        assert!(fs.read_file(e, u64::from(e.size), &mut head[..1]).is_err());
    }
    println!("xdvdfs: {past_end} files end past the image (trimmed)");
    // Every listing holds names that are unique without their case.
    let mut names: Vec<String> = entries.iter().map(|e| e.0.to_ascii_uppercase()).collect();
    names.sort();
    let before = names.len();
    names.dedup();
    assert_eq!(names.len(), before);

    let xbe = Xbe::read(&fs.file_source(&fs.lookup("default.xbe").unwrap())).unwrap();
    if xbe.title_id() != HALO {
        println!("xdvdfs: not Halo, the reference run's values are not checked");
        return;
    }
    // "disc: CCI image, 4715 MiB in 3471 MiB (1 part)" and "disc: XDVDFS at
    // +0x0, root sector 1714392 (2048 bytes)"
    assert_eq!(fs.partition_offset(), 0);
    assert_eq!((root.sector, root.size), (1_714_392, SECTOR_SIZE));
    if let ImageKind::Cci { parts } = img.kind {
        assert_eq!((parts, img.source.len() >> 20, img.stored_size >> 20), (1, 4715, 3471));
    }
}

#[test]
fn a_large_file_reads_alike_in_one_piece_and_in_many() {
    for img in images() {
        println!("{}", img.path.display());
        a_large_file_reads_alike_in_one_piece_and_in_many_on(img);
    }
}

fn a_large_file_reads_alike_in_one_piece_and_in_many_on(img: DiscImage) {
    let fs = Xdvdfs::open(img.source.clone()).unwrap();
    let mut largest = None;
    fs.walk(|p, e| {
        if !e.is_dir() && largest.as_ref().is_none_or(|(_, l): &(String, _)| e.size > *l) {
            largest = Some((p.to_owned(), e.size));
        }
    })
    .unwrap();
    let (path, _) = largest.expect("a file");
    let e = fs.lookup(&path).unwrap();
    let len = (e.size as usize).min(96 << 20);
    let mut whole = vec![0u8; len];
    let t = std::time::Instant::now();
    fs.read_file(&e, 0, &mut whole).unwrap();
    let secs = t.elapsed().as_secs_f64();
    println!(
        "read: {path}: {} MiB in {:.0} ms ({:.0} MiB/s)",
        len >> 20,
        secs * 1e3,
        (len >> 20) as f64 / secs.max(1e-9)
    );

    // The same bytes from four threads at once, each reading its own odd
    // pieces of the whole.
    std::thread::scope(|s| {
        for t in 0..4usize {
            let (fs, e, whole) = (&fs, &e, &whole);
            s.spawn(move || {
                let mut buf = vec![0u8; 200_000];
                let mut at = t * 12_345 % len;
                for k in 0..400usize {
                    let n = [1usize, 2047, 2048, 2049, 65_536, 199_999][(k + t) % 6].min(len - at);
                    fs.read_file(e, at as u64, &mut buf[..n]).unwrap();
                    assert!(buf[..n] == whole[at..at + n], "{n} bytes at {at}");
                    at = (at + n * 7 + 4097 * k) % len;
                }
            });
        }
    });
}
