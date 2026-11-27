//! The title loader and the boot sequence.
//!
//! As the Xbox kernel loads an XBE, the headers and the preloaded sections
//! get memory and the rest of the image is address space only, until
//! XeLoadSection (ex.rs) brings a section in; XeUnloadSection gives its
//! pages back. Titles with many sections count on that memory (Fable
//! preloads 15 of 107: backing the whole image left it 4 MiB short in
//! gameplay).
//!
//! The XBE's format is `xb-formats`' (`docs/hw/xbe.md`); this module places
//! it in guest memory, points its kernel imports at the kernel, and starts
//! its first thread at the entry point.

use std::path::PathBuf;
use std::sync::Arc;

use xb_cpu::Cpu;
use xb_formats::block::{BlockSource, SubSource};
use xb_formats::xbe::{Encoding, SECTION_PRELOAD, Xbe};
use xb_formats::xdvdfs::Xdvdfs;
use xb_types::{PAGE_SIZE, RAM_DEBUG, RAM_RETAIL, align_up};

use crate::exports::{self, Abi, ORDINAL_MAX};
use crate::kernel::Kernel;

/// The first thread's stack at least (the reference's size).
const MAIN_STACK_SIZE: u32 = 256 * 1024;
/// Header field: the imports from libraries other than the kernel.
const XBE_IMPORTS: usize = 0x15C;
/// XINIT_LIMIT_DEVKIT_MEMORY: the title runs in 64 MiB on a debug kit too.
const INIT_LIMIT_MEMORY: u32 = 4;

const CDROM: &str = "\\Device\\CdRom0";
/// What the kernel knows about the running title, set by the loader before
/// `ex::title_loaded`.
#[derive(Clone, Default)]
pub(crate) struct Title {
    /// The XBE's base address and SizeOfImage.
    pub base: u32,
    pub image_size: u32,
    /// The certificate's title ID.
    pub title_id: u32,
    /// From the XBE certificate: 1 NA, 2 JP, 4 elsewhere (the one chosen),
    /// and every region the disc plays in.
    pub game_region: u32,
    pub game_regions: u32,
    /// XeImageFileName ("" = `\Device\CdRom0\default.xbe`).
    pub image_path: String,
    /// A disc is in the tray.
    pub disc_present: bool,
    /// The running XBE's file (on the disc or a hard disk partition), for
    /// sections loaded later (XeLoadSection): `read_at(file offset, buf)`.
    pub xbe: Option<Arc<dyn BlockSource>>,
    /// The host folder holding the hard disk's partitions; console files
    /// that are not on a partition (the EEPROM image) go next to it.
    pub hdd_root: PathBuf,
}

/// What to boot.
#[derive(Clone, Default)]
pub struct BootOptions {
    /// The disc in the tray as a plain image (`xb_formats::disc::open(..)
    /// .source`), or `None` for an empty tray.
    pub disc: Option<Arc<dyn BlockSource>>,
    /// The host folder holding the hard disk's partitions (`Partition1` ..
    /// under it are created as needed).
    pub hdd_root: PathBuf,
    /// The XBE to boot as an Xbox path; `None`: `\default.xbe` on the disc,
    /// or the one the launch page names.
    pub xbe_path: Option<String>,
    /// The launch data page of the title that relaunched into this one
    /// ([`crate::TitleExit::Relaunch`]).
    pub launch_page: Option<Vec<u8>>,
    /// What the last title kept for this one ([`Kernel::persisted`]).
    pub persisted: Option<Persisted>,
}

/// Memory a title keeps across a quick reboot into another XBE
/// (`MmPersistContiguousMemory`: Direct3D's saved display) and the address
/// it left with `AvSetSavedDataAddress`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Persisted {
    /// The kept ranges: physical address and bytes.
    pub ranges: Vec<(u32, Vec<u8>)>,
    /// `AvSetSavedDataAddress`'s address (0: none).
    pub av_saved: u32,
}

/// The title could not be started.
#[derive(Debug)]
pub struct BootError(pub String);

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BootError {}

/// What was booted.
#[derive(Clone, Debug, Default)]
pub struct BootInfo {
    /// The XBE's path as booted.
    pub xbe_path: String,
    /// The certificate's title ID.
    pub title_id: u32,
    /// The certificate's title name.
    pub title_name: String,
    /// The image's base address.
    pub base: u32,
    /// Address space the image spans, in bytes.
    pub image_span: u32,
    /// The entry point.
    pub entry: u32,
    /// Ordinals of the kernel imports, in thunk order.
    pub imports: Vec<u32>,
    /// Names of imported kernel functions this kernel does not implement (a
    /// call to one stops the guest).
    pub unimplemented: Vec<&'static str>,
    /// The first thread's stack: limit and base.
    pub stack: (u32, u32),
}

fn err(s: impl Into<String>) -> BootError {
    BootError(s.into())
}

/// `default.xbe`'s file on the disc.
fn disc_file(disc: &Arc<dyn BlockSource>, rel: &str) -> Option<Arc<dyn BlockSource>> {
    let fs = Xdvdfs::open(Arc::clone(disc)).ok()?;
    let e = fs.lookup(rel).ok().filter(|e| !e.is_dir())?;
    Some(Arc::new(SubSource::new(Arc::clone(disc), fs.data_offset(&e), u64::from(e.size))))
}

/// How much RAM the console should have for the disc's `default.xbe`: a
/// retail console's 64 MiB, or a debug kit's 128 MiB for an XBE signed with
/// the debug keys and not marked for 64. A retail title gets 64 whatever is
/// asked: given 128, Halo's D3D wrote push buffer jumps the GPU can't take.
/// StarCraft Ghost's v1.256 debug build ran out of 64 MiB loading its first
/// mission.
#[must_use]
pub fn ram_size_for(disc: Option<&Arc<dyn BlockSource>>) -> u32 {
    let Some(xbe) =
        disc.and_then(|d| disc_file(d, "default.xbe")).and_then(|f| Xbe::read(&*f).ok())
    else {
        return RAM_RETAIL;
    };
    if xbe.init_flags & INIT_LIMIT_MEMORY == 0 && xbe.encoding == Encoding::Debug {
        log::info!(target: "kernel", "memory: a debug kit's XBE (the debug keys), not marked for 64 MiB: 128 MiB");
        return RAM_DEBUG;
    }
    RAM_RETAIL
}

impl Kernel {
    /// After [`crate::RunOutcome::Exit`] with [`crate::TitleExit::Relaunch`]:
    /// what the title kept for the next one, to pass in its
    /// [`BootOptions::persisted`]. The machine first has the GPU write its
    /// images of those ranges back to RAM (the ranges are
    /// `MmPersistContiguousMemory`'s).
    #[must_use]
    pub fn persisted(&self) -> Persisted {
        Persisted { ranges: self.mm_keep_collect(), av_saved: self.ex.av_saved }
    }

    /// The physical ranges [`Kernel::persisted`] will collect: address and
    /// length.
    #[must_use]
    pub fn persisted_ranges(&self) -> Vec<(u32, u32)> {
        self.mm_kept()
    }

    /// A guest stub for an xbdm.dll export a development build imports (the
    /// debug monitor of a development kit, absent here): each returns its
    /// value and pops its stdcall arguments. Only exports whose import a
    /// title was seen to make: StarCraft: Ghost's 2003 build registers an
    /// "XCAT" command processor (ordinal 30) at its first video.
    fn xbdm_stub(&mut self, ordinal: u32) -> Option<(u32, &'static str)> {
        // (ordinal, argument bytes, return value, name)
        const XBDM: [(u32, u16, u32, &str); 1] =
            [(30, 8, 0x02DA_0000, "DmRegisterCommandProcessor")]; // XBDM_NOERR
        let (_, arg_bytes, ret, name) = XBDM.into_iter().find(|x| x.0 == ordinal)?;
        let va = self.code_alloc(8);
        if va == 0 {
            return None;
        }
        let mut b = vec![0xB8]; // mov eax, ret
        b.extend_from_slice(&ret.to_le_bytes());
        b.push(0xC2); // ret arg_bytes
        b.extend_from_slice(&arg_bytes.to_le_bytes());
        self.gm.kwrite(va, &b);
        Some((va, name))
    }

    /// An Xbox Live title update of a disc XBE: as the Xbox kernel does, the
    /// file of that name in `E:\TDATA\<title ID>\$u` boots instead when it is
    /// the same title's and no older.
    fn title_update(&self, disc: &Arc<dyn BlockSource>, path: &str) -> Option<String> {
        let lower = path.to_ascii_lowercase();
        let rel = if let Some(r) = lower.strip_prefix("\\device\\cdrom0\\") {
            &path[path.len() - r.len()..]
        } else if path.starts_with('\\') && !lower.starts_with("\\device\\") {
            &path[1..]
        } else {
            return None;
        };
        if rel.contains('\\') {
            return None;
        }
        let on_disc = Xbe::read(&*disc_file(disc, rel)?).ok()?;
        let (tid, ver) = (on_disc.title_id(), on_disc.certificate.as_ref()?.version);
        let update = format!("\\Device\\Harddisk0\\Partition1\\TDATA\\{tid:08x}\\$u\\{rel}");
        let src = self.io_host_source(&update)?;
        match Xbe::read(&*src).ok().and_then(|x| x.certificate.clone()) {
            Some(c) if c.title_id == tid && c.version >= ver => {
                log::info!(target: "kernel", "title update: {update} (version {:x}, the disc's {ver:x}) boots for {path}", c.version);
                Some(update)
            }
            other => {
                log::info!(target: "kernel", "title update {update}: not this title's newer version (title {:08x} version {:x}, the disc's {tid:08x} {ver:x}): the disc's boots",
                    other.as_ref().map_or(0, |c| c.title_id), other.as_ref().map_or(0, |c| c.version));
                None
            }
        }
    }

    /// Loads the XBE at `path` into guest memory.
    fn xbe_load(
        &mut self,
        disc: Option<&Arc<dyn BlockSource>>,
        path: &str,
    ) -> Result<(Xbe, BootInfo), BootError> {
        let src: Arc<dyn BlockSource> = if let Some(host) = self.io_host_source(path) {
            host
        } else {
            let rel = if path.to_ascii_lowercase().starts_with(&CDROM.to_ascii_lowercase()) {
                &path[CDROM.len()..]
            } else {
                path
            };
            disc.and_then(|d| disc_file(d, rel))
                .ok_or_else(|| err(format!("xbe: {path} not on the disc")))?
        };
        let xbe = Xbe::read(&*src).map_err(|e| err(format!("xbe: {path}: {e}")))?;
        let (base, hdr_size, image_size) = (xbe.base, xbe.headers_size, xbe.image_size);
        if base & (PAGE_SIZE - 1) != 0 || xbe.headers().len() < hdr_size as usize {
            return Err(err(format!("xbe: bad header (base {base:08x}, headers {hdr_size})")));
        }
        // an image lives in the title's part of the address space (retail
        // and debug XBEs are at 0x00010000): one that names the kernel's
        // half would have its headers read over the kernel's own memory
        let extent = image_size.max(hdr_size);
        if base < 0x10000 || u64::from(base) + u64::from(extent) > 0x7FFE_0000 {
            return Err(err(format!(
                "xbe: an image of {} KiB at {base:08x} isn't in the title's address space",
                extent >> 10
            )));
        }
        let span = align_up(extent, PAGE_SIZE);
        if !self.mm_back(base, hdr_size) || self.gm.write(base, xbe.headers()).is_err() {
            return Err(err("xbe: no memory for the headers"));
        }

        let mut loaded = 0;
        for (i, s) in xbe.sections.iter().enumerate() {
            if s.flags & SECTION_PRELOAD == 0 {
                continue; // XeLoadSection brings it in
            }
            if s.virtual_addr < base
                || u64::from(s.virtual_addr) + u64::from(s.virtual_size)
                    > u64::from(base) + u64::from(span)
            {
                return Err(err(format!("xbe: section {} outside the image", s.name)));
            }
            if !self.mm_back(s.virtual_addr, s.virtual_size) {
                return Err(err(format!("xbe: no memory for section {}", s.name)));
            }
            let n = s.raw_size.min(s.virtual_size);
            if n != 0 {
                let mut buf = vec![0u8; n as usize];
                if src.read_at(u64::from(s.raw_addr), &mut buf).is_err()
                    || self.gm.write(s.virtual_addr, &buf).is_err()
                {
                    return Err(err(format!("xbe: read of section {} failed", s.name)));
                }
            }
            // Loaded once, as XeLoadSection counts: an XeUnloadSection frees it.
            if let Some(rc) = xbe.section_refcount_addr(i) {
                let _ = self.gm.w32(rc, 1);
            }
            loaded += 1;
        }
        let backed = self.gm.space.mapped_pages(base, span) * PAGE_SIZE;

        // Kernel thunk table: 0x80000000 | ordinal entries, zero-terminated.
        let mut imports = Vec::new();
        let mut t = xbe.kernel_thunk_addr;
        loop {
            let Ok(v) = self.gm.r32(t) else {
                // not in what was loaded (a damaged header, or a table in a
                // section that isn't preloaded)
                return Err(err(format!(
                    "xbe: the kernel thunk table at {t:08x} isn't in the loaded image"
                )));
            };
            if v == 0 {
                break;
            }
            if v & 0x8000_0000 == 0 {
                return Err(err(format!("xbe: thunk {t:08x} is not an ordinal import ({v:08x})")));
            }
            let ordinal = v & 0x7FFF_FFFF;
            let target = self.resolve(ordinal);
            if target == 0 {
                return Err(err(format!("xbe: import of unknown kernel ordinal {ordinal}")));
            }
            let _ = self.gm.w32(t, target);
            imports.push(ordinal);
            t += 4;
        }

        // A development build's imports from other libraries: {thunks, name}
        // entries, zero-ended, each a table of ordinals like the kernel's.
        // The debug monitor's (xbdm.dll) get stubs where there are some; the
        // rest stay as they are, told now.
        let title_id = xbe.title_id();
        let h = xbe.headers();
        let imp = h
            .get(XBE_IMPORTS..XBE_IMPORTS + 4)
            .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        let mut e = imp;
        while e != 0 {
            let (Ok(thunk), Ok(name_va)) = (self.gm.r32(e), self.gm.r32(e + 4)) else { break };
            if thunk == 0 {
                break;
            }
            let lib: String = (0..31)
                .map_while(|i| self.gm.r16(name_va + 2 * i).ok().filter(|c| *c != 0))
                .map(|c| if c < 0x80 { c as u8 as char } else { '?' })
                .collect();
            let xbdm = lib.eq_ignore_ascii_case("xbdm.dll");
            let mut t = thunk;
            while let Ok(v) = self.gm.r32(t) {
                if v == 0 {
                    break;
                }
                let by_ordinal = v & 0x8000_0000 != 0;
                let stub = if xbdm && by_ordinal { self.xbdm_stub(v & 0x7FFF_FFFF) } else { None };
                if let Some((va, name)) = stub {
                    let _ = self.gm.w32(t, va);
                    log::info!(target: "kernel", "imports: title {title_id:08x} imports {lib} ordinal {} ({name}): a stub, the debug monitor isn't here", v & 0x7FFF_FFFF);
                } else {
                    log::warn!(target: "kernel", "imports: title {title_id:08x} imports {lib} {}{}, not here (a call to it stops the game)",
                        if by_ordinal { "ordinal " } else { "name at " },
                        if by_ordinal { v & 0x7FFF_FFFF } else { v });
                }
                t += 4;
            }
            e += 8;
        }

        let unimplemented: Vec<&'static str> = imports
            .iter()
            .filter_map(|&o| exports::by_ordinal(o))
            .filter(|e| e.abi != Abi::Data && !self.implemented(u32::from(e.ordinal)))
            .map(|e| e.name)
            .collect();
        let tls = xbe.tls_addr;
        log::info!(target: "kernel",
            "stage S0 xbe loaded: title {title_id:08x} base {base:08x} image {} KiB ({} KiB in memory), {loaded}/{} sections, entry {:08x}, {} kernel imports, tls {tls:08x}",
            span >> 10, backed >> 10, xbe.sections.len(), xbe.entry_point, imports.len());
        let list: Vec<String> = imports.iter().map(u32::to_string).collect();
        log::info!(target: "kernel", "imports: title {title_id:08x} {}", list.join(","));
        if !unimplemented.is_empty() {
            // told now, not at the call (that stops the game)
            log::warn!(target: "kernel", "imports: title {title_id:08x} imports {} kernel functions not implemented here: {}",
                unimplemented.len(), unimplemented.join(", "));
        }
        debug_assert!(imports.iter().all(|o| (*o as usize) < ORDINAL_MAX));

        self.title.base = base;
        self.title.image_size = image_size;
        self.title.xbe = Some(src);
        let info = BootInfo {
            xbe_path: path.to_owned(),
            title_id,
            title_name: xbe.certificate.as_ref().map(|c| c.title_name.clone()).unwrap_or_default(),
            base,
            image_span: span,
            entry: xbe.entry_point,
            imports,
            unimplemented,
            stack: (0, 0),
        };
        Ok((xbe, info))
    }

    /// Boots a title: mounts the disc and the hard disk, loads the XBE
    /// (`\default.xbe`, the one `opts` names, or the one a launch page
    /// names; an installed title update of a disc XBE instead of it),
    /// applies the library fixups and readies the first thread at the entry
    /// point. [`Kernel::run`] then runs it.
    ///
    /// Call once, on a kernel fresh from [`Kernel::new`], with the same
    /// `cpu`.
    ///
    /// # Errors
    ///
    /// The XBE is missing, damaged or does not fit, or RAM ran out.
    pub fn boot(&mut self, cpu: &mut dyn Cpu, opts: BootOptions) -> Result<BootInfo, BootError> {
        if self.current.is_some() {
            return Err(err("a title is already loaded"));
        }
        self.title.hdd_root.clone_from(&opts.hdd_root);
        self.title.disc_present = opts.disc.is_some();
        self.io_mount(opts.disc.clone(), &opts.hdd_root).map_err(BootError)?;
        // the last title's kept memory (D3D's saved display), where it was,
        // before anything else takes those pages
        if let Some(p) = &opts.persisted {
            self.mm_keep_restore(&p.ranges);
            self.ex.av_saved = p.av_saved;
        }

        // A relaunch (XLaunchNewImage): the XBE the launch data page names,
        // and the page for the title.
        let mut xbe_path = "\\default.xbe".to_owned();
        let mut page = opts.launch_page;
        if let Some(p) = &opts.xbe_path {
            xbe_path.clone_from(p);
            self.title.image_path.clone_from(p);
        } else if let Some(pg) = &page {
            if let Some(p) = crate::ex::launch_xbe(pg) {
                log::info!(target: "kernel", "relaunch: {p} (launch type {})",
                    u32::from_le_bytes([pg[0], pg[1], pg[2], pg[3]]));
                xbe_path.clone_from(&p);
                self.title.image_path = p;
            } else {
                page = None;
                log::info!(target: "kernel", "relaunch: no launch data page, booting {xbe_path}");
            }
        }
        // a disc XBE with a title update installed: the update
        if let Some(d) = &opts.disc
            && let Some(update) = self.title_update(d, &xbe_path)
        {
            xbe_path.clone_from(&update);
            self.title.image_path = update;
        }

        let (xbe, mut info) = self.xbe_load(opts.disc.as_ref(), &xbe_path)?;
        self.mm_note_image(info.base, info.image_span);
        self.title_loaded();
        // The cache partitions are this title's from here on.
        if let Some(disk) = self.io_fs().hard_disk() {
            disk.set_cache_title(info.title_id);
        }
        if let Some(pg) = &page {
            self.launch_page_restore(pg);
        }
        let sections: Vec<(String, u32, u32)> =
            xbe.sections.iter().map(|s| (s.name.clone(), s.virtual_addr, s.virtual_size)).collect();
        self.title_fixups(&sections);
        // the titles that need the drives' own speed, by id or by the
        // certificate's title name (another edition, another id)
        self.io_title_timing(info.title_id, &info.title_name);

        let stack = MAIN_STACK_SIZE.max(align_up(xbe.stack_commit, PAGE_SIZE));
        let (limit, base) = self
            .boot_thread(info.entry, stack)
            .ok_or_else(|| err("no memory for the title's first thread"))?;
        info.stack = (limit, base);
        log::info!(target: "kernel", "stage S1 entry {:08x}, stack {limit:08x}-{base:08x}, fs {:08x}",
            info.entry, self.kpcr_va);
        self.gm.flush(cpu);
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::TRAP_VA;
    use crate::testing::MockCpu;
    use xb_formats::block::MemSource;
    use xb_formats::xbe::{ENTRY_KEY_RETAIL, THUNK_KEY_RETAIL};
    use xb_mem::Ram;
    use xb_types::irq::IrqHub;

    /// A minimal XBE: headers of one page, one preloaded section holding the
    /// kernel thunk table and some code, one section that is not preloaded.
    fn synthetic_xbe() -> Vec<u8> {
        const BASE: u32 = 0x0001_0000;
        let mut f = vec![0u8; 0x3000];
        let put = |f: &mut Vec<u8>, off: usize, v: u32| {
            f[off..off + 4].copy_from_slice(&v.to_le_bytes());
        };
        f[0..4].copy_from_slice(b"XBEH");
        put(&mut f, 0x104, BASE);
        put(&mut f, 0x108, 0x1000); // SizeOfHeaders
        put(&mut f, 0x10C, 0x4000); // SizeOfImage
        put(&mut f, 0x110, 0x178); // SizeOfImageHeader
        put(&mut f, 0x118, BASE + 0x180); // certificate
        put(&mut f, 0x11C, 2); // sections
        put(&mut f, 0x120, BASE + 0x400);
        put(&mut f, 0x128, (BASE + 0x1010) ^ ENTRY_KEY_RETAIL);
        put(&mut f, 0x130, 0x8000); // stack commit
        put(&mut f, 0x158, (BASE + 0x1000) ^ THUNK_KEY_RETAIL);
        put(&mut f, 0x164, BASE + 0x600); // library versions (none)
        // certificate
        put(&mut f, 0x180, 0x1D0);
        put(&mut f, 0x188, 0x4D53_0004);
        for (i, c) in "Test".encode_utf16().enumerate() {
            f[0x18C + 2 * i..0x18E + 2 * i].copy_from_slice(&c.to_le_bytes());
        }
        put(&mut f, 0x180 + 0xA0, 1); // region
        // section 0: preload, at +0x1000, 0x1000 bytes from file 0x1000
        let s0 = 0x400;
        put(&mut f, s0, SECTION_PRELOAD | 4);
        put(&mut f, s0 + 4, BASE + 0x1000);
        put(&mut f, s0 + 8, 0x1000);
        put(&mut f, s0 + 12, 0x1000);
        put(&mut f, s0 + 16, 0x800);
        put(&mut f, s0 + 20, BASE + 0x500);
        put(&mut f, s0 + 28, BASE + 0x480);
        put(&mut f, s0 + 32, BASE + 0x484);
        // section 1: not preloaded, at +0x2000
        let s1 = 0x438;
        put(&mut f, s1, 4);
        put(&mut f, s1 + 4, BASE + 0x2000);
        put(&mut f, s1 + 8, 0x1000);
        put(&mut f, s1 + 12, 0x2000);
        put(&mut f, s1 + 16, 0x1000);
        put(&mut f, s1 + 20, BASE + 0x508);
        put(&mut f, s1 + 28, BASE + 0x484);
        put(&mut f, s1 + 32, BASE + 0x488);
        f[0x500..0x505].copy_from_slice(b".text");
        f[0x508..0x50D].copy_from_slice(b".late");
        // thunks: PsCreateSystemThreadEx (255), KeTickCount (156), end
        put(&mut f, 0x1000, 0x8000_0000 | 255);
        put(&mut f, 0x1004, 0x8000_0000 | 156);
        put(&mut f, 0x1008, 0);
        f[0x1010] = 0xC3;
        f[0x2000] = 0xAA;
        f
    }

    /// An XDVDFS image holding `default.xbe` at the root.
    fn disc_with(xbe: &[u8]) -> Arc<dyn BlockSource> {
        const SECTOR: usize = 2048;
        let xbe_sector = 40usize;
        let mut img = vec![0u8; (xbe_sector + xbe.len().div_ceil(SECTOR) + 1) * SECTOR];
        let vd = 32 * SECTOR;
        img[vd..vd + 20].copy_from_slice(b"MICROSOFT*XBOX*MEDIA");
        img[vd + 20..vd + 24].copy_from_slice(&33u32.to_le_bytes()); // root sector
        img[vd + 24..vd + 28].copy_from_slice(&(SECTOR as u32).to_le_bytes());
        img[vd + 0x7EC..vd + 0x800].copy_from_slice(b"MICROSOFT*XBOX*MEDIA");
        let d = 33 * SECTOR;
        img[d..d + SECTOR].fill(0xFF);
        img[d..d + 4].copy_from_slice(&[0, 0, 0, 0]); // no left/right
        img[d + 4..d + 8].copy_from_slice(&(xbe_sector as u32).to_le_bytes());
        img[d + 8..d + 12].copy_from_slice(&(xbe.len() as u32).to_le_bytes());
        img[d + 12] = 0x20;
        img[d + 13] = 11;
        img[d + 14..d + 25].copy_from_slice(b"default.xbe");
        img[xbe_sector * SECTOR..xbe_sector * SECTOR + xbe.len()].copy_from_slice(xbe);
        Arc::new(MemSource(img))
    }

    #[test]
    fn a_synthetic_xbe_boots_with_its_imports_resolved_and_its_first_thread_ready() {
        let disc = disc_with(&synthetic_xbe());
        assert_eq!(ram_size_for(Some(&disc)), RAM_RETAIL);
        let ram = Ram::new(RAM_RETAIL).unwrap();
        let mut cpu = MockCpu::default();
        let mut k = Kernel::new(KernelConfig::new(ram, Arc::new(IrqHub::new())), &mut cpu).unwrap();
        let info = k
            .boot(&mut cpu, BootOptions { disc: Some(disc), ..BootOptions::default() })
            .expect("boot");
        assert_eq!(info.title_id, 0x4D53_0004);
        assert_eq!(info.title_name, "Test");
        assert_eq!(info.imports, [255, 156]);
        assert_eq!(info.entry, 0x0001_1010);
        assert!(info.unimplemented.is_empty());
        // the function import resolves into the trap range, the data import
        // to the variable
        assert_eq!(k.gm.r32(0x0001_1000).unwrap(), TRAP_VA + 255);
        assert_eq!(k.gm.r32(0x0001_1004).unwrap(), k.data_addr("KeTickCount"));
        // the preloaded section is in memory with a load count of 1, its
        // tail zero; the other one is address space only
        assert_eq!(k.gm.r8(0x0001_1010).unwrap(), 0xC3);
        assert_eq!(k.gm.r8(0x0001_1900).unwrap(), 0);
        assert_eq!(k.gm.r32(0x0001_0400 + 0x18).unwrap(), 1);
        assert!(!k.gm.mapped(0x0001_2000));
        assert_eq!(k.gm.r32(0x0001_0438 + 0x18).unwrap(), 0);
        // the first thread: at the entry point, its stack mapped, FS at the KPCR
        assert_eq!(k.eip(), info.entry);
        assert_eq!(k.frame.regs.fs_base, k.kpcr_va);
        assert!(info.stack.1 - info.stack.0 >= MAIN_STACK_SIZE);
        assert!(k.gm.mapped(k.esp()));
        assert_eq!(k.gm.r32(k.esp()).unwrap(), Kernel::trap(crate::kernel::ORD_THREAD_RETURN));
        // the CPU got the trap range and the image's mappings
        assert_eq!(cpu.trap, (TRAP_VA, crate::kernel::TRAP_LEN));
        assert!(
            cpu.maps.iter().any(
                |m| matches!(m, crate::testing::MapCall::Map(va, _, _, _) if *va == 0x0001_0000)
            )
        );
        assert!(k.boot(&mut cpu, BootOptions::default()).is_err(), "a second boot is refused");
    }

    #[test]
    fn a_missing_xbe_is_an_error_and_an_empty_tray_keeps_retail_ram() {
        let ram = Ram::new(RAM_RETAIL).unwrap();
        let mut cpu = MockCpu::default();
        let mut k = Kernel::new(KernelConfig::new(ram, Arc::new(IrqHub::new())), &mut cpu).unwrap();
        assert!(k.boot(&mut cpu, BootOptions::default()).is_err());
        assert_eq!(ram_size_for(None), RAM_RETAIL);
    }

    use crate::kernel::KernelConfig;
}
