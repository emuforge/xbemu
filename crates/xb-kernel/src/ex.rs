//! Executive, HAL, AV and title services.
//!
//! Ex pool and non-volatile (EEPROM) settings, XBE section loading, object
//! references, DbgPrint, the HAL and AV calls the titles make, and the data
//! exports that tell a title about the console and itself.
//!
//! Differences from the reference:
//!
//! * The console's settings come from [`crate::ConsoleConfig`]: the video
//!   flags and the language are always given, PAL-60 is the `video::PAL60` bit
//!   of the video flags, and no language is "fixed".
//! * The EEPROM image is `eeprom.bin` next to the hard disk's folder, or in
//!   memory only when the kernel has no hard disk folder (tests).
//! * A quick reboot writes no file: the launch data page leaves with
//!   [`TitleExit::Relaunch`].
//! * Not here: the reference app's records next to the image (`eeprom.sync`,
//!   `eeprom.renew`) and the new identity an image from before gets once the
//!   network is on (the kernel knows no network mode); such an image is read
//!   as it is.
//! * The interlocked list and 64-bit helpers, `ObfReferenceObject`,
//!   `ObReferenceObjectByPointer`, `DbgBreakPointWithStatus` and
//!   `KeBugCheckEx` are not in the reference: they follow NT (one CPU, so
//!   plain memory operations).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use xb_types::{CONTIG_WINDOW, PAGE_SIZE, align_up};

use crate::gm::KResult;
use crate::kernel::{
    Args, Kernel, STATUS_INVALID_HANDLE, STATUS_INVALID_PARAMETER, STATUS_NO_MEMORY,
    STATUS_OBJECT_TYPE_MISMATCH, STATUS_SUCCESS,
};
use crate::platform::{DisplayMode, Platform};
use crate::run::{AvPack, Stop, TitleExit, video};
use crate::xc_crypto::{Rc4, Sha1};

const STATUS_BUFFER_TOO_SMALL: u32 = 0xC000_0023;
const STATUS_ACCESS_DENIED: u32 = 0xC000_0022;
const STATUS_OBJECT_NAME_NOT_FOUND: u32 = 0xC000_0034;
const STATUS_DEVICE_DATA_ERROR: u32 = 0xC000_009C;

/// Our header before each Ex pool block: its size, then [`POOL_PAGES`].
const POOL_HDR: u32 = 16;
/// "AGES": a block of whole pages, not in the pool.
const POOL_PAGES: u32 = 0x5345_4741;

const EEPROM_SIZE: usize = 0x100;
const REG_BINARY: u32 = 3;
const REG_DWORD: u32 = 4;
/// The user section, whole.
const XC_MAX_OS: u32 = 0xFF;
/// The factory section, whole.
const XC_MAX_FACTORY: u32 = 0x1FF;
const XC_ENCRYPTED_SECTION: u32 = 0xFFFE;
const XC_MAX_ALL: u32 = 0xFFFF;

/// The launch data page's size.
const LAUNCH_PAGE: u32 = 0x1000;
/// LAUNCH_DATA_PAGE: Header { LaunchDataType, TitleId, `szLaunchPath[520]`,
/// Flags }, then LaunchData at +0x400. XAPI's XLaunchNewImage stores the path
/// as the device directory, ';', the file name
/// (`\Device\CdRom0;morrowind.xbe`).
const LAUNCH_PATH_OFF: usize = 8;
const LAUNCH_PATH_LEN: usize = 520;
/// LaunchDataType LDT_LAUNCH_DASHBOARD: LaunchData (+0x400) is
/// LD_LAUNCH_DASHBOARD { dwReason, dwContext, dwParameter1, dwParameter2 };
/// reason 0 is the main menu.
const LDT_LAUNCH_DASHBOARD: u32 = 1;
const LAUNCH_DATA_OFF: usize = 0x400;
const DASHBOARD_PATH: &str = "\\Device\\Harddisk0\\Partition2\\xboxdash.xbe";

const NT_CURRENT_THREAD: u32 = 0xFFFF_FFFE;
const OB_HEADER: u32 = 0x10;

const AV_QUERY_AV_CAPABILITIES: u32 = 6;
const AV_STANDARD_MASK: u32 = 0x0000_0F00;
const AV_REFRESH_MASK: u32 = 0x00C0_0000;

/// The EEPROM settings by index: (index, offset in the image, size).
///
/// The console's EEPROM (256 bytes) stays the same from boot to boot, like
/// the chip: titles sign saves with keys derived from it (XAPI reads the
/// whole image, XC_MAX_ALL, for console-bound signatures), and a different
/// one each boot makes every save "damaged". Every setting is served from
/// this one image. Layout: encrypted section (hash 0x00, confounder 0x14, HD
/// key 0x1C, game region 0x2C), factory section (checksum 0x30, serial 0x34,
/// MAC 0x40, online key 0x48, AV region 0x58), user section (checksum 0x60,
/// time zone 0x64-0x8F, language 0x90, video 0x94, audio 0x98, parental
/// games 0x9C / password 0xA0 / movies 0xA4, online addresses 0xA8-0xB7, misc
/// 0xB8, DVD region 0xBC).
const NV: [(u32, usize, usize); 28] = [
    (0x00, 0x64, 4),
    (0x01, 0x68, 4),
    (0x02, 0x6C, 4),
    (0x03, 0x78, 4),
    (0x04, 0x7C, 4),
    (0x05, 0x88, 4),
    (0x06, 0x8C, 4),
    (0x07, 0x90, 4),
    (0x08, 0x94, 4),
    (0x09, 0x98, 4),
    (0x0A, 0x9C, 4),
    (0x0B, 0xA0, 4),
    (0x0C, 0xA4, 4),
    (0x0D, 0xA8, 4),
    (0x0E, 0xAC, 4),
    (0x0F, 0xB0, 4),
    (0x10, 0xB4, 4),
    (0x11, 0xB8, 4),
    (0x12, 0xBC, 4),
    (0x100, 0x34, 12),
    (0x101, 0x40, 6),
    (0x102, 0x48, 16),
    (0x103, 0x58, 4),
    (0x104, 0x2C, 4),
    // the sections whole: Splinter Cell Chaos Theory reads the user section
    // (XC_MAX_OS) and, refused, asked again and again
    (XC_MAX_OS, 0x60, 0x60),
    (XC_MAX_FACTORY, 0x30, 0x30),
    (XC_ENCRYPTED_SECTION, 0x00, 0x30),
    (XC_MAX_ALL, 0x00, EEPROM_SIZE),
];

fn nv_find(index: u32) -> Option<(u32, usize, usize)> {
    NV.iter().copied().find(|s| s.0 == index)
}

/// The EEPROM keys by their HMAC-SHA1 midstates (the SHA-1 state after the
/// key's inner and outer block), as the reference has them: MCPX 1.0, 1.1-1.4,
/// 1.6, debug. A new image uses MCPX 1.0's.
const EEPROM_KEYS: [[[u32; 5]; 2]; 4] = [
    [
        [0x7212_7625, 0x3364_72B9, 0xBE60_9BEA, 0xF55E_226B, 0x9995_8DAC],
        [0x7644_1D41, 0x4DE8_2659, 0x2E8E_F85E, 0xB256_FACA, 0xC4FE_2DE8],
    ],
    [
        [0x39B0_6E79, 0xC9BD_25E8, 0xDBC6_B498, 0x40B4_389D, 0x86BB_D7ED],
        [0x9B49_BED3, 0x84B4_30FC, 0x6B87_49CD, 0xEBFE_5FE5, 0xD96E_7393],
    ],
    [
        [0x8058_763A, 0xF97D_4E0E, 0x865A_9762, 0x8A3D_920D, 0x0899_5B2C],
        [0x0107_5307, 0xA2F1_E037, 0x1186_EEEA, 0x88DA_9992, 0x168A_5609],
    ],
    [
        [0x85F9_E51A, 0xE046_13D2, 0x6D86_A50C, 0x77C3_2E3C, 0x4BD7_17A4],
        [0x5D7A_9C6B, 0xE192_2BEB, 0xB82C_CDBC, 0x3137_AB34, 0x486B_52B3],
    ],
];

/// The HD key every console had before they got identities of their own:
/// the saves locked to the console use it.
const LEGACY_HD_KEY: [u8; 16] = *b"0000000001\x00\x50\xF2\x00\x00\x01";

/// The executive exports' state.
pub(crate) struct State {
    /// The image the titles see: the stored one with this run's settings
    /// (the configuration's) over it.
    eeprom: [u8; EEPROM_SIZE],
    /// The stored image (what `eeprom.bin` holds). A title's
    /// ExSaveNonVolatileSetting goes to both; this run's settings (the
    /// regions of the running title) never reach the file.
    eeprom_disk: [u8; EEPROM_SIZE],
    eeprom_ready: bool,
    /// The encrypted section, decrypted.
    sec: [u8; 0x30],
    /// Kept in the clear (an image from before the reference encrypted it).
    sec_plain: bool,
    /// The [`EEPROM_KEYS`] entry it is under.
    sec_key: usize,
    /// AvSetSavedDataAddress's address.
    pub av_saved: u32,
    av_saved_logged: u32,
    /// TV encoder options logged (a bit each).
    av_option_logged: u32,
    /// The last AV mode word a title set (0: none yet).
    pub av_mode: u32,
    smc_version_idx: u32,
    /// The file cache's size in pages, as XSetFileCacheSize last set it (the
    /// XDK's default 64 KiB): read back by XGetFileCacheSize.
    fsc_pages: u32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            eeprom: [0; EEPROM_SIZE],
            eeprom_disk: [0; EEPROM_SIZE],
            eeprom_ready: false,
            sec: [0; 0x30],
            sec_plain: false,
            sec_key: 0,
            av_saved: 0,
            av_saved_logged: 0,
            av_option_logged: 0,
            av_mode: 0,
            smc_version_idx: 0,
            fsc_pages: 16,
        }
    }
}

fn rd32(p: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([p[at], p[at + 1], p[at + 2], p[at + 3]])
}

fn wr32(p: &mut [u8], at: usize, v: u32) {
    p[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn starts_with_ci(s: &[u8], prefix: &[u8]) -> bool {
    s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

// ------------------------------------------------------------------ pool

impl Kernel {
    /// A block of a page or more takes whole pages of its own and gives them
    /// back when freed, as the kernel's pool does (MiAllocatePoolPages): in
    /// the pool, a large temporary buffer grew it for good, and that memory
    /// never went back to the title (JSRF's stage loads ran out of RAM). Its
    /// pages fill a hole, not the top of the highest free run, so a block
    /// freed and asked for again comes back where it was (Psi-Ops keeps a
    /// pointer into its freed text file's block).
    fn ex_pool_alloc(&mut self, size: u32) -> u32 {
        if size > self.gm.ram_size() {
            return 0; // more than there is (and near 4 GiB the sums below wrap)
        }
        if size + POOL_HDR > PAGE_SIZE {
            let len = align_up(size + POOL_HDR, PAGE_SIZE);
            if let Some(pa) = self.gm.alloc_block(len) {
                let p = CONTIG_WINDOW | pa;
                self.gm.kfill(p, 0, len);
                self.gm.kw32(p, size);
                self.gm.kw32(p + 4, POOL_PAGES);
                self.pool.ex = self.pool.ex.wrapping_add(len);
                return p + POOL_HDR;
            }
        }
        let p = self.pool_alloc(size + POOL_HDR);
        if p == 0 {
            return 0;
        }
        self.gm.kw32(p, size);
        self.pool.ex = self.pool.ex.wrapping_add(size + POOL_HDR);
        p + POOL_HDR
    }
}

/// ExAllocatePool(NumberOfBytes)
fn ex_allocate_pool(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from(k.ex_pool_alloc(a[0])))
}

/// ExAllocatePoolWithTag(NumberOfBytes, Tag)
fn ex_allocate_pool_with_tag(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from(k.ex_pool_alloc(a[0])))
}

/// ExFreePool(P)
fn ex_free_pool(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    if a[0] != 0 {
        let hdr = a[0].wrapping_sub(POOL_HDR);
        let size = k.gm.r32(hdr)?.wrapping_add(POOL_HDR);
        if k.gm.r32(hdr + 4)? == POOL_PAGES && hdr >= CONTIG_WINDOW {
            let len = align_up(size, PAGE_SIZE);
            k.gm.w32(hdr + 4, 0)?;
            k.pool.ex = k.pool.ex.wrapping_sub(len);
            k.gm.free_phys(hdr & !CONTIG_WINDOW, len);
        } else {
            k.pool.ex = k.pool.ex.wrapping_sub(size);
            k.pool_free(hdr, size);
        }
    }
    Ok(0)
}

/// ExQueryPoolBlockSize(PoolBlock)
fn ex_query_pool_block_size(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from(k.gm.r32(a[0].wrapping_sub(POOL_HDR))?))
}

// ------------------------------------------------- non-volatile settings

/// The Xbox EEPROM section checksum: ones' complement of the carry-folded sum.
fn eeprom_checksum(p: &[u8]) -> u32 {
    let (mut high, mut low) = (0u32, 0u32);
    for w in p.as_chunks::<4>().0 {
        let v = u32::from_le_bytes(*w);
        let (sum, carry) = low.overflowing_add(v);
        high = high.wrapping_add(u32::from(carry));
        low = sum;
    }
    !high.wrapping_add(low)
}

/// SHA-1 of `data` continued from the state `h` a 64-byte block left (the
/// EEPROM keys are known only as such states).
fn sha1_resumed(h: &[u32; 5], data: &[u8]) -> [u8; 20] {
    let mut b = [0u8; Sha1::BYTES];
    for (o, s) in b.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        o.copy_from_slice(&s.to_le_bytes());
    }
    b[24..32].copy_from_slice(&64u64.to_le_bytes()); // the key's block, done
    let mut s = Sha1::from_bytes(&b);
    s.update(data);
    s.finalize()
}

/// HMAC-SHA1 under the EEPROM key `key` (by its midstates).
fn sec_hmac(key: usize, data: &[u8]) -> [u8; 20] {
    let inner = sha1_resumed(&EEPROM_KEYS[key][0], data);
    sha1_resumed(&EEPROM_KEYS[key][1], &inner)
}

/// Bytes at random, for a new console's identity: the standard library's
/// randomly keyed hasher over the time.
fn random_bytes(buf: &mut [u8]) {
    use std::hash::{BuildHasher, Hasher};
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    for (i, chunk) in buf.chunks_mut(8).enumerate() {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(t);
        h.write_usize(i);
        chunk.copy_from_slice(&h.finish().to_le_bytes()[..chunk.len()]);
    }
}

/// PAL titles that stay at 50 Hz on a console with PAL-60 on (they ask D3D
/// for its default refresh, 50 Hz for PAL) yet pace their frames by the flag
/// as if at 60: the console's PAL-60 is off for them, which changes nothing
/// on screen but their pacing. Halo's PAL release with the flag times its
/// frames by KeTickCount, sleeping 40 ms less 0.83 of the frame's ms, ~41 ms
/// a frame against 40 ms of two vblanks: one frame in 12 to 15 was a vblank
/// late. Without it, it waits for the vblanks: every frame 40.0 ms apart.
fn title_pal50(title_id: u32) -> bool {
    title_id == 0x4D53_0004 // Halo: Combat Evolved
}

/// The video standard of a console the running title is for (the EEPROM's
/// word: XGetVideoStandard reads its second byte): PAL-I for a PAL disc,
/// NTSC-J for a disc for Japan alone, NTSC-M else; 60 Hz but PAL.
fn video_standard(game_region: u32) -> u32 {
    match game_region {
        4 => 0x0080_0300,
        2 => 0x0040_0200,
        _ => 0x0040_0100,
    }
}

impl State {
    /// The decrypted section into the image, encrypted (its hash made anew).
    /// The section, 0x00-0x2F, is RC4 with HMAC-SHA1(EEPROM key, hash) as the
    /// key and checks by its hash, HMAC-SHA1(EEPROM key, confounder..region).
    fn sec_encrypt(&mut self) {
        let hash = sec_hmac(self.sec_key, &self.sec[0x14..0x30]);
        self.sec[..20].copy_from_slice(&hash);
        let seed = sec_hmac(self.sec_key, &self.sec[..20]);
        self.eeprom[..0x30].copy_from_slice(&self.sec);
        Rc4::new(&seed).apply(&mut self.eeprom[0x14..0x30]);
    }

    /// The image's section decrypted into `sec`, under the first key it
    /// checks with; false: none (kept in the clear).
    fn sec_decrypt(&mut self) -> bool {
        for key in 0..EEPROM_KEYS.len() {
            let seed = sec_hmac(key, &self.eeprom[..20]);
            let mut plain = [0u8; 0x1C];
            plain.copy_from_slice(&self.eeprom[0x14..0x30]);
            Rc4::new(&seed).apply(&mut plain);
            if sec_hmac(key, &plain)[..] == self.eeprom[..20] {
                self.sec[..20].copy_from_slice(&self.eeprom[..20]);
                self.sec[0x14..].copy_from_slice(&plain);
                self.sec_key = key;
                return true;
            }
        }
        false
    }

    /// The stored image's two checksums (its encrypted section as it was).
    fn disk_checksums(&mut self) {
        let d = &mut self.eeprom_disk;
        let c = eeprom_checksum(&d[0x34..0x60]);
        wr32(d, 0x30, c);
        let c = eeprom_checksum(&d[0x64..0xC0]);
        wr32(d, 0x60, c);
    }

    fn seal(&mut self) {
        if !self.sec_plain {
            self.sec_encrypt();
        }
        let e = &mut self.eeprom;
        let c = eeprom_checksum(&e[0x34..0x60]);
        wr32(e, 0x30, c);
        let c = eeprom_checksum(&e[0x64..0xC0]);
        wr32(e, 0x60, c);
    }

    /// A console of its own: a serial number, a MAC address (Microsoft's
    /// 00:50:F2 block), an online key and a confounder at random, `hd_key`
    /// as the HD key. Two consoles on a network need different MACs (system
    /// link), Xbox Live services different serials.
    fn identity(&mut self, hd_key: &[u8; 16]) {
        let mut rnd = [0u8; 12];
        random_bytes(&mut rnd);
        for (d, r) in self.eeprom[0x34..0x40].iter_mut().zip(rnd) {
            *d = b'0' + r % 10;
        }
        self.eeprom[0x40..0x43].copy_from_slice(&[0x00, 0x50, 0xF2]);
        random_bytes(&mut self.eeprom[0x43..0x46]);
        random_bytes(&mut self.eeprom[0x48..0x58]); // online key
        random_bytes(&mut self.sec[0x14..0x1C]); // confounder
        self.sec[0x1C..0x2C].copy_from_slice(hd_key);
        self.sec_plain = false;
        self.seal();
    }

    /// A new console's EEPROM: English, stereo, no parental limits, the
    /// region of the first title it runs, an identity of its own but the HD
    /// key all have.
    fn create(&mut self, game_region: u32) {
        self.eeprom = [0; EEPROM_SIZE];
        self.sec = [0; 0x30];
        self.sec_key = 0;
        wr32(&mut self.sec, 0x2C, if game_region != 0 { game_region } else { 1 });
        wr32(&mut self.eeprom, 0x58, video_standard(game_region)); // PAL-I / NTSC-J / NTSC-M
        wr32(&mut self.eeprom, 0x90, 1); // English
        wr32(&mut self.eeprom, 0xBC, if game_region == 4 { 2 } else { 1 }); // DVD region
        self.identity(&LEGACY_HD_KEY);
    }

    /// The game region where a title reads it: the section's own copy when
    /// the image keeps it in the clear.
    fn set_region(&mut self, region: u32) {
        wr32(&mut self.sec, 0x2C, region);
        if self.sec_plain {
            self.eeprom[0x2C..0x30].copy_from_slice(&self.sec[0x2C..0x30]);
        }
    }
}

impl Kernel {
    /// Where the EEPROM image is kept; `None`: in memory only.
    fn eeprom_path(&self) -> Option<PathBuf> {
        let root = &self.title.hdd_root;
        if root.as_os_str().is_empty() {
            return None;
        }
        Some(root.parent().unwrap_or(root).join("eeprom.bin"))
    }

    /// The image, whole or not at all: a new file put in the old one's place.
    fn eeprom_store(&mut self) {
        if !self.ex.eeprom_ready {
            self.ex.eeprom_disk = self.ex.eeprom; // loading: no run's settings on it yet
        }
        let Some(path) = self.eeprom_path() else { return };
        let mut tmp = path.clone().into_os_string();
        tmp.push(".new");
        let tmp = PathBuf::from(tmp);
        let written = (|| {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&self.ex.eeprom_disk)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, &path)
        })();
        if written.is_err() {
            log::warn!(target: "kernel", "ex: can't keep the EEPROM in {}", path.display());
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Readies the EEPROM image: loaded from its file or made new, then this
    /// run's settings over it.
    fn eeprom_load(&mut self) {
        if self.ex.eeprom_ready {
            return;
        }
        let path = self.eeprom_path();
        let shown = path.as_ref().map_or("memory".into(), |p| p.display().to_string());
        let file = path.as_ref().and_then(|p| std::fs::read(p).ok());
        let cfg = self.console.clone();
        let language = if (1..=9).contains(&cfg.language) { cfg.language } else { 0 };
        match file {
            Some(bytes) if bytes.len() >= EEPROM_SIZE => {
                self.ex.eeprom.copy_from_slice(&bytes[..EEPROM_SIZE]);
                self.ex.sec_plain = !self.ex.sec_decrypt();
                if self.ex.sec_plain {
                    let head = self.ex.eeprom;
                    self.ex.sec.copy_from_slice(&head[..0x30]);
                }
            }
            Some(_) => {
                // the file kept as it is: someone's, maybe a real console's
                self.ex.create(self.title.game_region);
                log::warn!(target: "kernel", "ex: {shown} isn't a whole EEPROM: a new one for this run");
            }
            None => {
                self.ex.create(self.title.game_region);
                // set as the configuration has the console
                if language != 0 {
                    wr32(&mut self.ex.eeprom, 0x90, language);
                }
                wr32(&mut self.ex.eeprom, 0x94, cfg.video_flags & 0x001F_0000);
                self.ex.seal();
                self.eeprom_store();
                log::info!(target: "kernel", "ex: new console EEPROM in {shown}");
            }
        }
        self.ex.eeprom_disk = self.ex.eeprom;
        // The regions follow the running title (a library mixes NTSC and PAL
        // discs); the rest stays as stored. A title always sees the same
        // image.
        let region = self.title.game_region;
        let pal = region == 4;
        self.ex.set_region(if region != 0 { region } else { 1 });
        wr32(&mut self.ex.eeprom, 0x58, video_standard(region));
        wr32(&mut self.ex.eeprom, 0xBC, if pal { 2 } else { 1 });
        // So do the video settings: the configuration's (widescreen,
        // 480p...). PAL titles at 60 Hz as a PAL console with PAL-60 on runs
        // them: the flag the console's dashboard sets, which the titles read
        // to offer or take 60 Hz. No other console has it.
        let want_pal60 = cfg.video_flags & video::PAL60 != 0;
        let pal60 = pal && want_pal60 && !title_pal50(self.title.title_id);
        let flags = (cfg.video_flags & !video::PAL60) | if pal60 { video::PAL60 } else { 0 };
        wr32(&mut self.ex.eeprom, 0x94, flags);
        if pal && want_pal60 && !pal60 {
            log::info!(target: "kernel", "ex: PAL-60 off for {:08x}: it stays at 50 Hz and paces itself by the flag",
                self.title.title_id);
        }
        if language != 0 {
            wr32(&mut self.ex.eeprom, 0x90, language);
        }
        // The audio, the configuration's: stereo (0), or Dolby Digital on
        // (XC_AUDIO_FLAGS_ENABLE_AC3), the games mixing six channels, with
        // the analog side at Dolby Surround (XC_AUDIO_FLAGS_SURROUND): the
        // XDK's XGetAudioFlags has AC3 only with SURROUND, and a title that
        // offers its 5.1 options on a SURROUND basic mode saw stereo. Not
        // what an EEPROM kept: one left at surround or mono by another
        // console played that way without a word in the settings.
        wr32(&mut self.ex.eeprom, 0x98, if cfg.surround { 0x0001_0002 } else { 0 });
        // The language stays the console's whatever the disc's folders are
        // called, as in the reference: a title whose disc lacks it finds
        // that out from its own files, as on the console (Dino Crisis 3's
        // European disc names its folders en, fr, ge, it, sp).
        // A disc for North America and Japan both plays as a Japanese
        // console's when the language is Japanese, Korean or Chinese (a
        // console of those markets): Capcom Fighting Evolution's World disc
        // reads the region, not the language, and is Capcom Fighting Jam on
        // a Japanese console. Both regions NTSC, so the video stays.
        let lang_now = rd32(&self.ex.eeprom, 0x90);
        if self.title.game_region == 1
            && self.title.game_regions & 2 != 0
            && matches!(lang_now, 2 | 7 | 8)
        {
            self.title.game_region = 2;
            self.ex.set_region(2);
            log::info!(target: "kernel", "ex: a disc for several regions, the language {lang_now}'s: Japan");
        }
        // And the time zone: its offset from UTC now as the bias (UTC =
        // local time + bias, minutes), without daylight saving rules of its
        // own (no names, dates or standard / daylight biases).
        wr32(&mut self.ex.eeprom, 0x64, cfg.time_zone_minutes_east.wrapping_neg() as u32);
        self.ex.eeprom[0x68..0x70].fill(0);
        self.ex.eeprom[0x78..0x80].fill(0);
        self.ex.eeprom[0x88..0x90].fill(0);
        self.ex.seal();
        self.ex.eeprom_ready = true;
        let e = &self.ex.eeprom;
        log::info!(target: "kernel", "ex: video flags {:08x}, AV pack {}, language {}, time zone bias {} min",
            rd32(e, 0x94), av_pack_number(cfg.av_pack), rd32(e, 0x90), rd32(e, 0x64) as i32);
        log::info!(target: "kernel", "ex: console serial {}, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}{}",
            String::from_utf8_lossy(&e[0x34..0x40]), e[0x40], e[0x41], e[0x42], e[0x43], e[0x44], e[0x45],
            if self.ex.sec_plain { " (an image from before: its section in the clear)" } else { "" });
    }
}

/// ExQueryNonVolatileSetting(ValueIndex, Type, Value, ValueLength, ResultLength)
fn ex_query_non_volatile_setting(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let Some((index, offset, size)) = nv_find(a[0]) else {
        log::info!(target: "kernel", "ex: non-volatile setting 0x{:x} not known", a[0]);
        return Ok(u64::from(STATUS_OBJECT_NAME_NOT_FOUND));
    };
    if a[1] != 0 {
        k.gm.w32(a[1], if size == 4 { REG_DWORD } else { REG_BINARY })?;
    }
    if a[4] != 0 {
        k.gm.w32(a[4], size as u32)?;
    }
    if (a[3] as usize) < size {
        return Ok(u64::from(STATUS_BUFFER_TOO_SMALL));
    }
    k.eeprom_load();
    // the game region is the encrypted section's, decrypted (the kernel's
    // copy); the sections whole are the image's, as the chip has them
    if index == 0x104 {
        k.gm.write(a[2], &k.ex.sec[0x2C..0x30])?;
    } else {
        k.gm.write(a[2], &k.ex.eeprom[offset..offset + size])?;
    }
    Ok(u64::from(STATUS_SUCCESS))
}

/// ExSaveNonVolatileSetting(ValueIndex, Type, Value, ValueLength): the user
/// section (and the factory AV region, which the dashboard may set) only.
fn ex_save_non_volatile_setting(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let Some((index, offset, size)) =
        nv_find(a[0]).filter(|s| s.0 <= 0x103 && (s.0 < 0x100 || s.0 == 0x103))
    else {
        return Ok(u64::from(STATUS_ACCESS_DENIED));
    };
    let n = (a[3] as usize).min(size);
    let src = k.gm.read_vec(a[2], n as u32)?;
    k.eeprom_load();
    for image in [&mut k.ex.eeprom, &mut k.ex.eeprom_disk] {
        image[offset..offset + size].fill(0);
        image[offset..offset + n].copy_from_slice(&src);
    }
    k.ex.seal();
    k.ex.disk_checksums();
    k.eeprom_store();
    log::info!(target: "kernel", "ex: the title saved setting {index:x} ({n} bytes) in the EEPROM");
    Ok(u64::from(STATUS_SUCCESS))
}

// ---------------------------------------------------------- XBE sections
//
// XBE section header: Flags 0x00, VirtualAddress 0x04, VirtualSize 0x08,
// FileAddress 0x0C, FileSize 0x10, SectionName 0x14, SectionReferenceCount
// 0x18; 0x38 bytes.

impl Kernel {
    /// A section header's name (SectionName 0x14), for the logs and the
    /// fixups.
    fn section_name(&self, s: u32) -> String {
        let name = self.gm.r32(s + 0x14).unwrap_or(0);
        if name == 0 {
            return "?".into();
        }
        match self.gm.cstr(name, 64) {
            Ok(n) => String::from_utf8_lossy(&n).into_owned(),
            Err(_) => "?".into(),
        }
    }

    /// An unloaded section's pages go back, except those it shares with the
    /// headers or with a section still loaded (`others`: those sections'
    /// address and size).
    fn section_release(&mut self, va: u32, vsize: u32, hdr_end: u32, others: &[(u32, u32)]) {
        let end = u64::from(va) + u64::from(vsize);
        let mut p = u64::from(va & !(PAGE_SIZE - 1));
        while p < end {
            let shared = p < u64::from(hdr_end)
                || others.iter().any(|&(ova, osize)| {
                    u64::from(ova) < p + u64::from(PAGE_SIZE)
                        && u64::from(ova) + u64::from(osize) > p
                });
            if !shared && self.gm.mapped(p as u32) {
                self.gm.free_at(p as u32, PAGE_SIZE);
            }
            p += u64::from(PAGE_SIZE);
        }
    }
}

/// XeLoadSection(Section): the first reference backs the section's pages and
/// reads it from the XBE's file.
fn xe_load_section(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let s = a[0];
    let refs = k.gm.r32(s + 0x18)?;
    let (va, vsize) = (k.gm.r32(s + 0x04)?, k.gm.r32(s + 0x08)?);
    let (raw, rsize) = (k.gm.r32(s + 0x0C)?, k.gm.r32(s + 0x10)?);
    let name = k.section_name(s);
    k.gm.w32(s + 0x18, refs.wrapping_add(1))?;
    log::info!(target: "kernel", "ex: load section {:.8} {va:08x}+{vsize:x} (refs {})", name, refs.wrapping_add(1));
    if refs != 0 {
        return Ok(u64::from(STATUS_SUCCESS));
    }
    if !k.mm_back(va, vsize) {
        k.gm.w32(s + 0x18, 0)?;
        return Ok(u64::from(STATUS_NO_MEMORY));
    }
    let mut data = vec![0u8; rsize.min(vsize) as usize];
    let read = k.title.xbe.as_ref().is_some_and(|x| x.read_at(u64::from(raw), &mut data).is_ok());
    // the call runs again after a fault: as not loaded
    let stored = k.gm.fill(va, 0, vsize).and_then(|()| k.gm.write(va, &data));
    if let Err(fault) = stored {
        k.gm.w32(s + 0x18, 0)?;
        return Err(fault);
    }
    if !read {
        return Ok(u64::from(STATUS_DEVICE_DATA_ERROR));
    }
    k.section_fixups(va, vsize, &name);
    k.gm.invalidate_code(va, vsize);
    Ok(u64::from(STATUS_SUCCESS))
}

/// XeUnloadSection(Section): the last reference gives the section's pages
/// back.
fn xe_unload_section(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let s = a[0];
    let refs = k.gm.r32(s + 0x18)?;
    if refs == 0 {
        return Ok(u64::from(STATUS_INVALID_PARAMETER));
    }
    let (va, vsize) = (k.gm.r32(s + 0x04)?, k.gm.r32(s + 0x08)?);
    // what the release needs of the headers, before anything changes
    let mut hdr_end = 0;
    let mut others = Vec::new();
    if refs == 1 {
        let base = k.title.base;
        hdr_end = base.wrapping_add(k.gm.r32(base + 0x108)?);
        let (nsec, secs) = (k.gm.r32(base + 0x11C)?, k.gm.r32(base + 0x120)?);
        for i in 0..nsec {
            let o = secs.wrapping_add(i.wrapping_mul(0x38));
            if o == s || k.gm.r32(o + 0x18)? == 0 {
                continue;
            }
            others.push((k.gm.r32(o + 0x04)?, k.gm.r32(o + 0x08)?));
        }
    }
    k.gm.w32(s + 0x18, refs - 1)?;
    log::info!(target: "kernel", "ex: unload section {:.8} (refs {})", k.section_name(s), refs - 1);
    if refs == 1 {
        k.gm.invalidate_code(va, vsize);
        k.section_release(va, vsize, hdr_end, &others);
    }
    Ok(u64::from(STATUS_SUCCESS))
}

// ---------------------------------------------------------------- objects

/// ObReferenceObjectByHandle(Handle, ObjectType, ReturnedObject)
fn ob_reference_object_by_handle(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let obj = if a[0] == NT_CURRENT_THREAD {
        k.current.map(|t| k.threads[t].ethread)
    } else {
        k.ob_lookup(a[0]).map(|(obj, _)| obj)
    };
    let Some(obj) = obj else {
        return Ok(u64::from(STATUS_INVALID_HANDLE));
    };
    let hdr = obj.wrapping_sub(OB_HEADER);
    if a[1] != 0 && k.gm.r32(hdr + 8)? != a[1] {
        return Ok(u64::from(STATUS_OBJECT_TYPE_MISMATCH));
    }
    let count = k.gm.r32(hdr)?;
    k.gm.w32(a[2], obj)?;
    k.gm.w32(hdr, count.wrapping_add(1))?;
    Ok(u64::from(STATUS_SUCCESS))
}

/// ObReferenceObjectByPointer(Object, ObjectType): as NT's, the object's
/// PointerCount up when it is of the type.
fn ob_reference_object_by_pointer(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let hdr = a[0].wrapping_sub(OB_HEADER);
    if a[1] != 0 && k.gm.r32(hdr + 8)? != a[1] {
        return Ok(u64::from(STATUS_OBJECT_TYPE_MISMATCH));
    }
    let count = k.gm.r32(hdr)?;
    k.gm.w32(hdr, count.wrapping_add(1))?;
    Ok(u64::from(STATUS_SUCCESS))
}

/// ObfReferenceObject(Object), fastcall: PointerCount up.
fn obf_reference_object(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let hdr = a[0].wrapping_sub(OB_HEADER);
    let count = k.gm.r32(hdr)?;
    k.gm.w32(hdr, count.wrapping_add(1))?;
    Ok(0)
}

/// ObfDereferenceObject(Object)
fn obf_dereference_object(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    k.ob_dereference(a[0]);
    Ok(0)
}

// ------------------------------------------------------------------- Dbg

impl Kernel {
    /// The guest's string at `va` (at most `max` bytes), as far as its pages
    /// are mapped: a title's bad pointer ends it rather than faulting.
    fn guest_str(&self, va: u32, max: usize) -> Vec<u8> {
        let mut out = Vec::new();
        while out.len() < max {
            match self.gm.r8(va.wrapping_add(out.len() as u32)) {
                Ok(c) if c != 0 => out.push(c),
                _ => break,
            }
        }
        out
    }
}

/// A printf conversion's flags, width and precision.
#[derive(Default)]
struct Spec {
    left: bool,
    plus: bool,
    space: bool,
    alt: bool,
    zero: bool,
    width: usize,
    precision: Option<usize>,
}

impl Spec {
    fn parse(s: &[u8]) -> Self {
        let mut spec = Self::default();
        let mut i = 0;
        while let Some(c) = s.get(i) {
            match c {
                b'-' => spec.left = true,
                b'+' => spec.plus = true,
                b' ' => spec.space = true,
                b'#' => spec.alt = true,
                b'0' => spec.zero = true,
                _ => break,
            }
            i += 1;
        }
        while let Some(c) = s.get(i).filter(|c| c.is_ascii_digit()) {
            spec.width = (spec.width * 10 + usize::from(c - b'0')).min(512);
            i += 1;
        }
        if s.get(i) == Some(&b'.') {
            i += 1;
            let mut p = 0usize;
            while let Some(c) = s.get(i).filter(|c| c.is_ascii_digit()) {
                p = (p * 10 + usize::from(c - b'0')).min(512);
                i += 1;
            }
            spec.precision = Some(p);
        }
        spec
    }

    /// `body` (after `prefix`, the sign or 0x) padded to the width.
    fn pad(&self, out: &mut Vec<u8>, prefix: &[u8], body: &[u8], zero: bool) {
        let fill = self.width.saturating_sub(prefix.len() + body.len());
        if self.left {
            out.extend_from_slice(prefix);
            out.extend_from_slice(body);
            out.resize(out.len() + fill, b' ');
        } else if zero {
            out.extend_from_slice(prefix);
            out.resize(out.len() + fill, b'0');
            out.extend_from_slice(body);
        } else {
            out.resize(out.len() + fill, b' ');
            out.extend_from_slice(prefix);
            out.extend_from_slice(body);
        }
    }

    /// An integer conversion (`d i u x X`) of `v` as C's printf prints it.
    fn integer(&self, out: &mut Vec<u8>, conv: u8, v: u32) {
        let (prefix, mut digits): (&[u8], String) = match conv {
            b'd' | b'i' => {
                let n = v as i32;
                let sign: &[u8] = if n < 0 {
                    b"-"
                } else if self.plus {
                    b"+"
                } else if self.space {
                    b" "
                } else {
                    b""
                };
                (sign, n.unsigned_abs().to_string())
            }
            b'x' => (if self.alt && v != 0 { b"0x" } else { b"" }, format!("{v:x}")),
            b'X' => (if self.alt && v != 0 { b"0X" } else { b"" }, format!("{v:X}")),
            _ => (b"", v.to_string()),
        };
        if let Some(p) = self.precision {
            if p == 0 && v == 0 {
                digits.clear();
            }
            while digits.len() < p {
                digits.insert(0, '0');
            }
        }
        self.pad(out, prefix, digits.as_bytes(), self.zero && self.precision.is_none());
    }
}

/// DbgPrint(Format, ...): the common printf conversions, arguments from the
/// guest stack.
fn dbg_print(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    const OUT: usize = 512;
    let mut argp = k.esp().wrapping_add(8); // after the return address and Format
    let fmt = k.guest_str(a[0], 511);
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < fmt.len() && out.len() < OUT - 32 {
        let c = fmt[i];
        i += 1;
        if c != b'%' {
            out.push(c);
            continue;
        }
        let mut spec: Vec<u8> = Vec::new();
        while let Some(&f) = fmt.get(i).filter(|f| b"-+ #0123456789.lh".contains(f)) {
            if spec.len() >= 13 {
                break;
            }
            if f != b'l' && f != b'h' {
                spec.push(f);
            }
            i += 1;
        }
        let Some(&conv) = fmt.get(i) else { break };
        i += 1;
        if conv == b'%' {
            out.push(b'%');
            continue;
        }
        let v = k.gm.r32(argp).unwrap_or(0);
        argp = argp.wrapping_add(4);
        let s = Spec::parse(&spec);
        match conv {
            b's' => {
                let mut text = if v != 0 { k.guest_str(v, 255) } else { b"(null)".to_vec() };
                if let Some(p) = s.precision {
                    text.truncate(p);
                }
                s.pad(&mut out, b"", &text, false);
            }
            b'c' => s.pad(&mut out, b"", &[v as u8], false),
            b'd' | b'i' | b'u' | b'x' | b'X' => s.integer(&mut out, conv, v),
            b'p' => out.extend_from_slice(format!("{v:08x}").as_bytes()),
            _ => {
                out.extend_from_slice(b"<%");
                out.extend_from_slice(&spec);
                out.push(conv);
                out.push(b'>');
            }
        }
    }
    out.truncate(OUT - 1);
    while matches!(out.last(), Some(b'\n' | b'\r')) {
        out.pop();
    }
    log::info!(target: "kernel", "dbg: {}", String::from_utf8_lossy(&out));
    Ok(0)
}

/// DbgBreakPoint(): no debugger, so it is told and the title goes on.
fn dbg_break_point(k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    let ret = k.gm.r32(k.esp()).unwrap_or(0);
    log::info!(target: "kernel", "dbg: DbgBreakPoint from {ret:08x}");
    Ok(0)
}

/// DbgBreakPointWithStatus(Status): as DbgBreakPoint, the status told.
fn dbg_break_point_with_status(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let ret = k.gm.r32(k.esp()).unwrap_or(0);
    log::info!(target: "kernel", "dbg: DbgBreakPointWithStatus({:08x}) from {ret:08x}", a[0]);
    Ok(0)
}

// ------------------------------------------------------------- HAL, AV

/// AvGetSavedDataAddress()
fn av_get_saved_data_address(k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    if k.ex.av_saved_logged < 4 {
        k.ex.av_saved_logged += 1;
        log::info!(target: "kernel", "AvGetSavedDataAddress: {:08x}", k.ex.av_saved);
    }
    Ok(u64::from(k.ex.av_saved))
}

/// AvSetSavedDataAddress(Address)
fn av_set_saved_data_address(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    log::info!(target: "kernel", "AvSetSavedDataAddress {:08x}", a[0]);
    k.ex.av_saved = a[0];
    Ok(0)
}

/// The AV pack's number in the logs (the reference's numbering).
fn av_pack_number(pack: AvPack) -> u32 {
    match pack {
        AvPack::Hdtv => 0,
        AvPack::Standard => 1,
        AvPack::SVideo => 2,
        AvPack::Scart => 3,
        AvPack::Vga => 4,
    }
}

/// The AV pack as the TV encoder's capabilities (AV_PACK_*) report it.
fn av_pack_caps(pack: AvPack) -> u32 {
    match pack {
        AvPack::Standard => 1,
        AvPack::Scart => 3,
        AvPack::Vga => 5,
        AvPack::SVideo => 6,
        AvPack::Hdtv => 4, // component
    }
}

/// The AV pack as the system controller's AV pack register (SMC 0x04) reports it.
fn av_pack_smc(pack: AvPack) -> u32 {
    match pack {
        AvPack::Standard => 0x06,
        AvPack::Scart => 0x00,
        AvPack::Vga => 0x02,
        AvPack::SVideo => 0x04,
        AvPack::Hdtv => 0x01, // component
    }
}

/// AvSendTVEncoderOption(RegisterBase, Option, Param, Result)
fn av_send_tv_encoder_option(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let mut r = 0;
    if a[1] == AV_QUERY_AV_CAPABILITIES {
        // The AV pack (the configuration's: component by default), the
        // factory video standard and refresh, and the user's video flags
        // (480p, 720p, widescreen...): D3D lists its display modes from
        // these.
        k.eeprom_load();
        let e = &k.ex.eeprom;
        r = av_pack_caps(k.console.av_pack)
            | (rd32(e, 0x58) & (AV_STANDARD_MASK | AV_REFRESH_MASK))
            | (rd32(e, 0x94) & !(AV_STANDARD_MASK | 0xFF));
    }
    if a[3] != 0 {
        k.gm.w32(a[3], r)?;
    }
    let bit = 1u32 << (a[1] & 31);
    if k.ex.av_option_logged & bit == 0 {
        k.ex.av_option_logged |= bit;
        log::info!(target: "kernel", "av: TV encoder option {} param {:08x} -> {r:08x}", a[1], a[2]);
    }
    Ok(0)
}

/// Whether the 0x30 bytes at `p` have the shape of D3DPRESENT_PARAMETERS: a
/// display mode's size, a back buffer count of 0 - 3, Windowed and
/// EnableAutoDepthStencil 0 or 1, a format, no flag but D3D's own (0x3F1), a
/// refresh of 0, 50 or 60.
fn present_params(p: &[u8]) -> bool {
    let (w, h, fmt, count) = (rd32(p, 0), rd32(p, 4), rd32(p, 8), rd32(p, 12));
    let (windowed, autods, flags, hz) = (rd32(p, 28), rd32(p, 32), rd32(p, 40), rd32(p, 44));
    matches!(w, 640 | 720 | 1280 | 1920)
        && matches!(h, 480 | 576 | 720 | 1080)
        && fmt != 0
        && fmt < 0x80
        && count <= 3
        && windowed <= 1
        && autods <= 1
        && flags & !0x3F1 == 0
        && matches!(hz, 0 | 50 | 60)
}

impl Kernel {
    /// The presentation flags on the frames above the call (their first 4
    /// KiB: the structure, or a pointer to it), as D3D sets the mode from a
    /// title's CreateDevice or Reset with the parameters a local (XIII:
    /// 0x101).
    fn stack_present_flags(&self) -> Option<u32> {
        let esp = self.esp();
        let mut buf = [0u8; 0x30];
        for i in 0..1024u32 {
            let s = esp.wrapping_add(4 * i);
            let Ok(v) = self.gm.r32(s) else { break };
            for at in [v, s] {
                if at & 3 != 0 {
                    continue;
                }
                let Some(pa) = self.gm.phys_of(at) else { continue };
                if self.gm.phys_of(at.wrapping_add(0x30)) == pa.checked_add(0x30)
                    && self.gm.ram().read(pa, &mut buf)
                    && present_params(&buf)
                {
                    return Some(rd32(&buf, 40));
                }
            }
        }
        None
    }

    /// The title's own shape, 16:9 or 4:3, to the machine:
    /// D3DPRESENT_PARAMETERS.Flags' D3DPRESENTFLAG_WIDESCREEN (0x10), which a
    /// title that draws anamorphic 16:9 sets. The AV mode tells it only on a
    /// standard-definition output (its WSS flag); a 480p or HD mode is the
    /// same for both shapes, and D3D sets the mode from no frame that still
    /// holds the parameters. A title keeps them for its Reset (a global in its
    /// data, or on its heap): the structure is found in RAM by its shape. Of
    /// several, the one whose PROGRESSIVE and 10X11PIXELASPECTRATIO flags fit
    /// the AV mode (0: none). Hitman Blood Money at 480p (0x150: widescreen,
    /// progressive, 10x11), Hitman 2 on a standard-definition output (0x130,
    /// the mode's WSS set), Vice City (0x20, no WSS). `stack`: called from
    /// AvSetDisplayMode, whose callers' frames are looked at first.
    pub(crate) fn av_title_scan(&mut self, p: &mut dyn Platform, stack: bool) {
        if let Some(sf) = stack.then(|| self.stack_present_flags()).flatten() {
            let wide = sf & 0x10 != 0;
            p.set_title_widescreen(wide);
            log::info!(target: "kernel", "av: the title's presentation flags {sf:08x} (its call): {}",
                if wide { "16:9" } else { "4:3" });
            return;
        }
        const CHUNK: u32 = 1 << 20;
        let m = self.ex.av_mode;
        let reg = (m >> 16) & 0xFF;
        let (hd, fpar) = (m & 0x8000_0000 != 0, (0x0F..=0x12).contains(&reg));
        let ram = self.gm.ram();
        let ram_size = ram.size();
        let mut best: Option<u32> = None;
        let mut best_score = -1i32;
        let mut buf = vec![0u8; (CHUNK + 0x30) as usize];
        let mut base = 0u32;
        while base < ram_size {
            let n = (ram_size - base).min(CHUNK + 0x30) as usize;
            if ram.read(base, &mut buf[..n]) {
                let mut off = 0usize;
                while off < CHUNK as usize && off + 0x34 <= n {
                    let at = &buf[off..off + 0x30];
                    off += 4;
                    if !matches!(rd32(at, 0), 640 | 720 | 1280 | 1920) || !present_params(at) {
                        continue;
                    }
                    let fl = rd32(at, 40);
                    let score = if m == 0 {
                        0
                    } else {
                        i32::from(hd == (fl & 0x40 != 0)) * 2 + i32::from(fpar == (fl & 0x100 != 0))
                    };
                    if score > best_score {
                        best_score = score;
                        best = Some(fl);
                    }
                }
            }
            base = base.saturating_add(CHUNK);
        }
        match best {
            // (the shape the title gave at an earlier mode stays: XIII and
            // Conker set the mode again later with no parameters left in
            // reach)
            None => {
                log::info!(target: "kernel", "av: the title's presentation parameters not found");
            }
            Some(fl) => {
                let wide = fl & 0x10 != 0;
                p.set_title_widescreen(wide);
                log::info!(target: "kernel", "av: the title's presentation flags {fl:08x}: {}",
                    if wide { "16:9" } else { "4:3" });
            }
        }
    }
}

/// AvSetDisplayMode(RegisterBase, Step, Mode, Format, Pitch, FrameBuffer)
fn av_set_display_mode(k: &mut Kernel, p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    log::info!(target: "kernel", "av: display mode {:08x} format {:08x} pitch {} fb {:08x}", a[2], a[3], a[4], a[5]);
    if a[2] != 0 {
        // AV mode bit 30: the 50 Hz modes (PAL 576-line timings, as
        // 0x58030314 is); the 480-line, 480p and HD modes are 60 Hz.
        let refresh_hz = if a[2] & 0x4000_0000 != 0 { 50 } else { 60 };
        k.ex.av_mode = a[2];
        p.set_display_mode(DisplayMode {
            mode: a[2],
            format: a[3],
            pitch: a[4],
            frame_buffer: a[5],
            refresh_hz,
        });
        k.av_title_scan(p, true);
    } else if a[5] != 0 && !p.title_shape_known() {
        // No mode (the one set before stays), as the reference has it: Dead
        // or Alive 3's D3D passes 0 at every call, its parameters (16:9) on
        // the frames above it all the same. Unscanned, its shape was never
        // found.
        k.av_title_scan(p, true);
    }
    Ok(0)
}

/// HalReadWritePCISpace(BusNumber, SlotNumber, RegisterNumber, Buffer, Length, WritePCISpace)
fn hal_read_write_pci_space(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let write = a[5] & 0xFF != 0;
    log::info!(target: "kernel", "hal: PCI {} bus {} slot {} reg {:02x} len {}",
        if write { "write" } else { "read" }, a[0], a[1], a[2], a[4]);
    if !write {
        k.gm.fill(a[3], 0, a[4])?;
    }
    Ok(0)
}

/// HalRegisterShutdownNotification, HalIsResetOrShutdownPending,
/// HalEnableSecureTrayEject (the tray never opens): nothing, 0.
fn hal_nothing(_k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    Ok(0)
}

/// PhyGetLinkState(Update): the Ethernet link as XNET_ETHERNET_LINK_* flags
/// (active, 100 Mb/s, full duplex), 0 without a network.
fn phy_get_link_state(_k: &mut Kernel, p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    Ok(u64::from(p.phy_link_state()))
}

/// PhyInitialize(ForceReset, Parameter): the PHY negotiates the link again;
/// the NIC driver (the title's XNet) starts once the link-change interrupt
/// comes. Success either way.
fn phy_initialize(_k: &mut Kernel, p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    p.phy_initialize();
    Ok(u64::from(STATUS_SUCCESS))
}

/// The launch page's XBE as a device path: on the disc
/// (`\Device\CdRom0\morrowind.xbe`) or a hard disk partition; `None` when it
/// names neither.
pub(crate) fn launch_xbe(page: &[u8]) -> Option<String> {
    const CD: &[u8] = b"\\Device\\CdRom0";
    const HD: &[u8] = b"\\Device\\Harddisk0\\Partition";
    let raw = page.get(LAUNCH_PATH_OFF..LAUNCH_PATH_OFF + LAUNCH_PATH_LEN)?;
    let p = &raw[..raw.iter().position(|&c| c == 0).unwrap_or(raw.len())];
    let dev = if starts_with_ci(p, CD) {
        CD.len()
    } else if starts_with_ci(p, HD) && p.get(HD.len()).is_some_and(|c| (b'1'..=b'5').contains(c)) {
        HD.len() + 1 // the partition's digit too
    } else {
        return None;
    };
    let mut out = p[..dev].to_vec();
    let rest = &p[dev..];
    if !matches!(rest.first(), Some(b'\\' | b';')) {
        out.push(b'\\');
    }
    out.extend(rest.iter().map(|&c| if c == b';' { b'\\' } else { c }));
    (out.len() > dev + 1).then(|| String::from_utf8_lossy(&out).into_owned())
}

impl Kernel {
    /// A quick reboot to the dashboard (no launch path, or the dashboard's)
    /// that asks for one of its pages, a title's "create an Xbox Live
    /// account": true when the dashboard is there to boot, the page then
    /// naming it. To the main menu, the game closes, as without a dashboard.
    fn dashboard_page(&self, page: &mut [u8], xbe: &mut String) -> bool {
        let to_dash = page[LAUNCH_PATH_OFF] == 0 || xbe.eq_ignore_ascii_case(DASHBOARD_PATH);
        if !to_dash
            || rd32(page, 0) != LDT_LAUNCH_DASHBOARD
            || rd32(page, LAUNCH_DATA_OFF) == 0
            || self.io_host_source(DASHBOARD_PATH).is_none()
        {
            return false;
        }
        let path = b"\\Device\\Harddisk0\\Partition2;xboxdash.xbe";
        page[LAUNCH_PATH_OFF..LAUNCH_PATH_OFF + LAUNCH_PATH_LEN].fill(0);
        page[LAUNCH_PATH_OFF..LAUNCH_PATH_OFF + path.len()].copy_from_slice(path);
        DASHBOARD_PATH.clone_into(xbe);
        true
    }

    /// A contiguous page with `page`'s contents becomes LaunchDataPage, after
    /// `title_loaded` on a relaunch.
    pub(crate) fn launch_page_restore(&mut self, page: &[u8]) {
        let slot = self.data_addr("LaunchDataPage");
        let va = self.mm_contig_alloc(LAUNCH_PAGE);
        if va == 0 {
            log::warn!(target: "kernel", "relaunch: no launch data page for the title");
            return;
        }
        let n = page.len().min(LAUNCH_PAGE as usize);
        self.gm.kfill(va, 0, LAUNCH_PAGE);
        self.gm.kwrite(va, &page[..n]);
        self.gm.kw32(slot, va);
    }
}

/// HalReturnToFirmware(Routine): 2 (ReturnFirmwareQuickReboot) with a launch
/// data page is XLaunchNewImage. The kernel can't reboot in place (its state
/// lives for one run), so the run ends with the page and the machine boots
/// the XBE it names with a new kernel: on the disc, on the hard disk (when
/// it's there), or the dashboard a title asked a page of (`dashboard_page`).
fn hal_return_to_firmware(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let page = k.gm.kr32(k.data_addr("LaunchDataPage"));
    let mut pg = if page != 0 { k.gm.read_vec(page, LAUNCH_PAGE)? } else { Vec::new() };
    if a[0] == 2 && page != 0 {
        let named = launch_xbe(&pg);
        let mut xbe = named.clone().unwrap_or_default();
        let go = k.dashboard_page(&mut pg, &mut xbe)
            || (named.is_some()
                && !xbe.eq_ignore_ascii_case(DASHBOARD_PATH)
                && (starts_with_ci(xbe.as_bytes(), b"\\Device\\CdRom0")
                    || k.io_host_source(&xbe).is_some()));
        if go {
            let why = format!("relaunch into {xbe} (launch type {})", rd32(&pg, 0));
            k.mm_keep_drop(page); // it goes with the exit
            k.stop = Some(Stop::Exit(TitleExit::Relaunch { xbe_path: xbe, launch_page: pg }, why));
            return Ok(0);
        }
    }
    let mut launch = String::new();
    if page != 0 {
        let raw = &pg[LAUNCH_PATH_OFF..LAUNCH_PATH_OFF + LAUNCH_PATH_LEN];
        let n = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        launch = String::from_utf8_lossy(&raw[..n]).into_owned();
    }
    // the title is done (the dashboard, a reboot): the game closes; asking
    // the dashboard for a page of its (an Xbox Live account's creation or
    // recovery) with none installed, the machine can say why
    let dashboard_missing = a[0] == 2
        && page != 0
        && rd32(&pg, 0) == LDT_LAUNCH_DASHBOARD
        && rd32(&pg, LAUNCH_DATA_OFF) != 0
        && k.io_host_source(DASHBOARD_PATH).is_none();
    let why = format!(
        "title returned to the firmware (routine {}{}{launch})",
        a[0],
        if page != 0 { ", launch path " } else { "" }
    );
    k.stop = Some(Stop::Exit(
        TitleExit::Firmware { routine: a[0], launch_path: launch, dashboard_missing },
        why,
    ));
    Ok(0)
}

/// HalInitiateShutdown()
fn hal_initiate_shutdown(k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    k.stop = Some(Stop::Exit(TitleExit::Shutdown, "title shut the console down".into()));
    Ok(0)
}

/// HalReadSMBusValue(Address, Command, ReadWord, DataValue): the system
/// controller's version, tray and AV pack; any other device is the
/// machine's.
fn hal_read_smbus_value(k: &mut Kernel, p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let v = if a[0] & 0xFE == 0x20 {
        match a[1] {
            0x01 => {
                let c = b"P01"[(k.ex.smc_version_idx % 3) as usize];
                k.ex.smc_version_idx = k.ex.smc_version_idx.wrapping_add(1);
                u32::from(c)
            }
            // tray closed, media present or not
            0x03 => {
                if k.title.disc_present {
                    0x60
                } else {
                    0x40
                }
            }
            0x04 => av_pack_smc(k.console.av_pack), // as the TV encoder reports it
            _ => 0,
        }
    } else {
        p.smbus(a[0] as u8, a[1] as u8, a[2] & 0xFF != 0, None)
    };
    if a[3] != 0 {
        k.gm.w32(a[3], v)?;
    }
    Ok(u64::from(STATUS_SUCCESS))
}

/// HalWriteSMBusValue(Address, Command, WriteWord, DataValue): the kernel
/// keeps nothing of it; the machine may.
fn hal_write_smbus_value(_k: &mut Kernel, p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    p.smbus(a[0] as u8, a[1] as u8, a[2] & 0xFF != 0, Some(a[3]));
    Ok(u64::from(STATUS_SUCCESS))
}

/// HalReadSMCTrayState(TrayState, TrayStateChangeCount): the tray closed, a
/// disc in it or none (the dashboard booted alone); it never moves.
fn hal_read_smc_tray_state(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    k.gm.w32(a[0], if k.title.disc_present { 0x60 } else { 0x40 })?;
    if a[1] != 0 {
        k.gm.w32(a[1], 0)?;
    }
    Ok(u64::from(STATUS_SUCCESS))
}

// ------------------------------------------------- caches, bugchecks, DPCs

/// FscGetCacheSize(): the size FscSetCacheSize last set. The cache itself is
/// the host's.
fn fsc_get_cache_size(k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    Ok(u64::from(k.ex.fsc_pages))
}

/// FscSetCacheSize(NumberOfCachePages)
fn fsc_set_cache_size(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    k.ex.fsc_pages = a[0];
    Ok(u64::from(STATUS_SUCCESS))
}

/// KeBugCheck(BugCheckCode)
fn ke_bug_check(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let ret = k.gm.r32(k.esp()).unwrap_or(0);
    k.request_stop(format!("KeBugCheck({:08x}) from {ret:08x}", a[0]));
    Ok(0)
}

/// KeBugCheckEx(BugCheckCode, Parameter1..4)
fn ke_bug_check_ex(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let ret = k.gm.r32(k.esp()).unwrap_or(0);
    k.request_stop(format!(
        "KeBugCheckEx({:08x}, {:08x}, {:08x}, {:08x}, {:08x}) from {ret:08x}",
        a[0], a[1], a[2], a[3], a[4]
    ));
    Ok(0)
}

/// KeStallExecutionProcessor(MicroSeconds): the processor spins that long, as
/// the console's does, and nothing else of the title runs meanwhile. It
/// returned at once: a driver's wait for its device (look, stall 10 us, a
/// few hundred times) was over before a device that takes real time here
/// could answer, and the time a title measures around a stall didn't pass. A
/// tenth of a second at most (its callers ask for microseconds).
fn ke_stall_execution_processor(_k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let until = Instant::now() + Duration::from_micros(u64::from(a[0].min(100_000)));
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        if left > Duration::from_micros(500) {
            // the long ones mostly asleep: the host's other threads run
            std::thread::sleep(left - Duration::from_micros(200));
        } else {
            std::hint::spin_loop();
        }
    }
    Ok(0)
}

/// KeInitializeDpc(Dpc, DeferredRoutine, DeferredContext). KDPC: Type 0x00,
/// Inserted 0x02, DpcListEntry 0x04, DeferredRoutine 0x0C, DeferredContext
/// 0x10, SystemArgument1 0x14, SystemArgument2 0x18.
fn ke_initialize_dpc(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    k.gm.fill(a[0], 0, 0x1C)?;
    k.gm.w8(a[0], 19)?; // DpcObject
    k.gm.w32(a[0] + 0x0C, a[1])?;
    k.gm.w32(a[0] + 0x10, a[2])?;
    Ok(0)
}

/// KeSaveFloatingPointState / KeRestoreFloatingPointState: guest FPU state
/// stays put.
fn ke_floating_point_state(_k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    Ok(u64::from(STATUS_SUCCESS))
}

// -------------------------------------------------- interlocked helpers
//
// Not in the reference. One CPU and no preemption inside a kernel call, so each is
// the plain memory operation NT's does under its lock (or with interrupts
// off); the Xbox's take no spin lock argument where the table has none.

/// ExInterlockedAddLargeInteger(Addend, Increment (8 bytes), Lock): the
/// 64-bit sum stored; the value before it returned in EDX:EAX.
fn ex_interlocked_add_large_integer(
    k: &mut Kernel,
    _p: &mut dyn Platform,
    a: &Args,
) -> KResult<u64> {
    let old = k.gm.r64(a[0])?;
    let inc = u64::from(a[1]) | u64::from(a[2]) << 32;
    k.gm.w64(a[0], old.wrapping_add(inc))?;
    Ok(old)
}

/// ExInterlockedAddLargeStatistic(Addend, Increment), fastcall: a 32-bit
/// increment to a 64-bit counter.
fn ex_interlocked_add_large_statistic(
    k: &mut Kernel,
    _p: &mut dyn Platform,
    a: &Args,
) -> KResult<u64> {
    let old = k.gm.r64(a[0])?;
    k.gm.w64(a[0], old.wrapping_add(u64::from(a[1])))?;
    Ok(0)
}

/// ExInterlockedCompareExchange64(Destination, Exchange, Comparand),
/// fastcall, all pointers: `*Destination` becomes `*Exchange` when it equals
/// `*Comparand`; its value before returned in EDX:EAX.
fn ex_interlocked_compare_exchange64(
    k: &mut Kernel,
    _p: &mut dyn Platform,
    a: &Args,
) -> KResult<u64> {
    let old = k.gm.r64(a[0])?;
    let (exchange, comparand) = (k.gm.r64(a[1])?, k.gm.r64(a[2])?);
    if old == comparand {
        k.gm.w64(a[0], exchange)?;
    }
    Ok(old)
}

/// ExfInterlockedInsertHeadList(ListHead, ListEntry), fastcall: the entry
/// first in the doubly linked list; the entry that was first, or NULL for a
/// list that was empty.
fn exf_interlocked_insert_head_list(
    k: &mut Kernel,
    _p: &mut dyn Platform,
    a: &Args,
) -> KResult<u64> {
    let (head, entry) = (a[0], a[1]);
    let first = k.gm.r32(head)?;
    k.gm.r32(first.wrapping_add(4))?; // both stores below can be made
    k.gm.w32(entry, first)?;
    k.gm.w32(entry.wrapping_add(4), head)?;
    k.gm.w32(first.wrapping_add(4), entry)?;
    k.gm.w32(head, entry)?;
    Ok(u64::from(if first == head { 0 } else { first }))
}

/// ExfInterlockedInsertTailList(ListHead, ListEntry), fastcall: the entry
/// last in the list; the entry that was last, or NULL for a list that was
/// empty.
fn exf_interlocked_insert_tail_list(
    k: &mut Kernel,
    _p: &mut dyn Platform,
    a: &Args,
) -> KResult<u64> {
    let (head, entry) = (a[0], a[1]);
    let last = k.gm.r32(head.wrapping_add(4))?;
    k.gm.r32(last)?;
    k.gm.w32(entry, head)?;
    k.gm.w32(entry.wrapping_add(4), last)?;
    k.gm.w32(last, entry)?;
    k.gm.w32(head.wrapping_add(4), entry)?;
    Ok(u64::from(if last == head { 0 } else { last }))
}

/// ExfInterlockedRemoveHeadList(ListHead), fastcall: the first entry out of
/// the list and returned; NULL for an empty list.
fn exf_interlocked_remove_head_list(
    k: &mut Kernel,
    _p: &mut dyn Platform,
    a: &Args,
) -> KResult<u64> {
    let head = a[0];
    let first = k.gm.r32(head)?;
    if first == head {
        return Ok(0);
    }
    let next = k.gm.r32(first)?;
    k.gm.r32(next.wrapping_add(4))?;
    k.gm.w32(next.wrapping_add(4), head)?;
    k.gm.w32(head, next)?;
    Ok(u64::from(first))
}

/// InterlockedPushEntrySList(ListHead, ListEntry), fastcall. SLIST_HEADER:
/// Next 0x00, Depth 0x04 (16 bits), Sequence 0x06. The entry first, the
/// depth and the sequence up; the entry that was first.
fn interlocked_push_entry_slist(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let (head, entry) = (a[0], a[1]);
    let first = k.gm.r32(head)?;
    let counts = k.gm.r32(head.wrapping_add(4))?;
    k.gm.w32(entry, first)?;
    k.gm.w32(head.wrapping_add(4), counts.wrapping_add(0x0001_0001))?;
    k.gm.w32(head, entry)?;
    Ok(u64::from(first))
}

/// InterlockedPopEntrySList(ListHead), fastcall: the first entry out and
/// returned, the depth down; NULL for an empty list.
fn interlocked_pop_entry_slist(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let head = a[0];
    let first = k.gm.r32(head)?;
    if first == 0 {
        return Ok(0);
    }
    let counts = k.gm.r32(head.wrapping_add(4))?;
    let next = k.gm.r32(first)?;
    k.gm.w32(head.wrapping_add(4), counts.wrapping_sub(1))?;
    k.gm.w32(head, next)?;
    Ok(u64::from(first))
}

/// InterlockedFlushSList(ListHead), fastcall: every entry out, the depth 0;
/// the entry that was first.
fn interlocked_flush_slist(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let head = a[0];
    let first = k.gm.r32(head)?;
    if first == 0 {
        return Ok(0);
    }
    let counts = k.gm.r32(head.wrapping_add(4))?;
    k.gm.w32(head.wrapping_add(4), counts & 0xFFFF_0000)?;
    k.gm.w32(head, 0)?;
    Ok(u64::from(first))
}

// ----------------------------------------------------- title information

impl Kernel {
    /// After the XBE is loaded and `self.title` is filled in: what the kernel
    /// exposes about the title: the regions from its certificate,
    /// XeImageFileName, the EEPROM and the hardware data exports, the HD key.
    pub(crate) fn title_loaded(&mut self) {
        let base = self.title.base;
        self.mm_note_image(base, self.title.image_size);

        // Certificate (header 0x118): TitleId at 0x08, GameRegion at 0xA0;
        // in the headers that were loaded, or no region told (a damaged
        // header's address).
        let cert = self.gm.r32(base.wrapping_add(0x118)).unwrap_or(0);
        let region = self.gm.r32(cert.wrapping_add(0xA0)).unwrap_or(0);
        self.title.game_regions = region;
        self.title.game_region = if region & 1 != 0 {
            1
        } else if region & 2 != 0 {
            2
        } else if region & 4 != 0 {
            4
        } else {
            1
        };
        self.title.title_id = if self.gm.mapped_range(cert.wrapping_add(0xA0), 4) {
            self.gm.r32(cert.wrapping_add(0x08)).unwrap_or(0)
        } else {
            0
        };
        // the configuration's region, when it names one, over the disc's
        if let Some(r) = self.console.region.filter(|r| matches!(r, 1 | 2 | 4)) {
            self.title.game_region = r;
        }

        // XeImageFileName: ANSI_STRING { Length, MaximumLength, Buffer }.
        let path = if self.title.image_path.is_empty() {
            "\\Device\\CdRom0\\default.xbe".to_owned()
        } else {
            self.title.image_path.clone()
        };
        let len = path.len() as u32 + 1;
        let buf = self.pool_alloc(len);
        if buf != 0 {
            self.gm.kwrite(buf, path.as_bytes());
            self.gm.kw8(buf + len - 1, 0);
            let s = self.data_addr("XeImageFileName");
            self.gm.kw16(s, (len - 1) as u16);
            self.gm.kw16(s + 2, len as u16);
            self.gm.kw32(s + 4, buf);
        }
        // An XBE on the hard disk: D: is the folder it was launched from, as
        // the launch path's directory part becomes D: on the Xbox (the
        // dashboard's: C:'s root). A disc's stays the disc's root, and so
        // does a title update's (E:\TDATA\<title>\$u, booted for the disc's
        // XBE).
        if starts_with_ci(path.as_bytes(), b"\\Device\\Harddisk0")
            && !path.contains("\\$u\\")
            && let Some(slash) = path.rfind('\\').filter(|&i| i != 0)
        {
            let dir = path[..slash].to_owned();
            self.io_set_launch_dir(&dir);
        }
        self.set_hd_key();
    }

    /// XboxHDKey, the key saves locked to the console are signed with, and
    /// what else of the console's the data exports tell.
    fn set_hd_key(&mut self) {
        self.eeprom_load();
        let key: [u8; 16] = if self.ex.sec_plain {
            self.ex.eeprom[0x36..0x46].try_into().unwrap_or_default()
        } else {
            self.ex.sec[0x1C..0x2C].try_into().unwrap_or_default()
        };
        self.gm.kwrite(self.data_addr("XboxHDKey"), &key);
        // The AV pack the system controller reported at boot, which XAPI's
        // XGetAVPack returns (and XGetVideoFlags keeps the HD modes by):
        // left 0, SCART, X-Men Legends never took 720p and drew 4:3 with
        // widescreen on.
        self.gm.kw32(self.data_addr("HalBootSMCVideoMode"), av_pack_smc(self.console.av_pack));
        // The hard disk's model and serial number (STRINGs, as its IDENTIFY
        // data has them: 40 and 20 characters, blank-padded), the serial
        // made from the console's so it stays the same on every run. Left
        // empty, their buffer was 0: Xbox Music Mixer copies the serial's 20
        // bytes as it starts and faulted at address 0.
        let serial = String::from_utf8_lossy(&self.ex.eeprom[0x34..0x40]).into_owned();
        let disk = [
            ("HalDiskModelNumber", format!("{:<40}", "XBASED HDD")),
            ("HalDiskSerialNumber", format!("{:<20}", format!("XB{serial}"))),
        ];
        for (name, text) in disk {
            let s = self.data_addr(name);
            let bytes = text.as_bytes();
            self.gm.kw16(s, bytes.len() as u16);
            self.gm.kw16(s + 2, bytes.len() as u16);
            self.gm.kw32(s + 4, s + 8);
            self.gm.kwrite(s + 8, bytes);
        }
    }
}

pub(crate) fn register(k: &mut Kernel) {
    k.imp("ExAllocatePool", ex_allocate_pool);
    k.imp("ExAllocatePoolWithTag", ex_allocate_pool_with_tag);
    k.imp("ExFreePool", ex_free_pool);
    k.imp("ExQueryPoolBlockSize", ex_query_pool_block_size);
    k.imp("ExQueryNonVolatileSetting", ex_query_non_volatile_setting);
    k.imp("ExSaveNonVolatileSetting", ex_save_non_volatile_setting);
    k.imp("XeLoadSection", xe_load_section);
    k.imp("XeUnloadSection", xe_unload_section);
    k.imp("ObReferenceObjectByHandle", ob_reference_object_by_handle);
    k.imp("ObReferenceObjectByPointer", ob_reference_object_by_pointer);
    k.imp("ObfReferenceObject", obf_reference_object);
    k.imp("ObfDereferenceObject", obf_dereference_object);
    k.imp("DbgPrint", dbg_print);
    k.imp("DbgBreakPoint", dbg_break_point);
    k.imp("DbgBreakPointWithStatus", dbg_break_point_with_status);
    k.imp("AvGetSavedDataAddress", av_get_saved_data_address);
    k.imp("AvSetSavedDataAddress", av_set_saved_data_address);
    k.imp("AvSendTVEncoderOption", av_send_tv_encoder_option);
    k.imp("AvSetDisplayMode", av_set_display_mode);
    k.imp("HalReadWritePCISpace", hal_read_write_pci_space);
    k.imp("HalRegisterShutdownNotification", hal_nothing);
    k.imp("HalIsResetOrShutdownPending", hal_nothing);
    k.imp("HalReturnToFirmware", hal_return_to_firmware);
    k.imp("HalInitiateShutdown", hal_initiate_shutdown);
    k.imp("HalReadSMBusValue", hal_read_smbus_value);
    k.imp("HalWriteSMBusValue", hal_write_smbus_value);
    k.imp("HalReadSMCTrayState", hal_read_smc_tray_state);
    k.imp("HalEnableSecureTrayEject", hal_nothing); // the tray never opens
    k.imp("FscGetCacheSize", fsc_get_cache_size);
    k.imp("FscSetCacheSize", fsc_set_cache_size);
    k.imp("PhyGetLinkState", phy_get_link_state);
    k.imp("PhyInitialize", phy_initialize);
    k.imp("KeBugCheck", ke_bug_check);
    k.imp("KeBugCheckEx", ke_bug_check_ex);
    k.imp("KeStallExecutionProcessor", ke_stall_execution_processor);
    k.imp("KeSaveFloatingPointState", ke_floating_point_state);
    k.imp("KeRestoreFloatingPointState", ke_floating_point_state);
    k.imp("KeInitializeDpc", ke_initialize_dpc);
    k.imp("ExInterlockedAddLargeInteger", ex_interlocked_add_large_integer);
    k.imp("ExInterlockedAddLargeStatistic", ex_interlocked_add_large_statistic);
    k.imp("ExInterlockedCompareExchange64", ex_interlocked_compare_exchange64);
    k.imp("ExfInterlockedInsertHeadList", exf_interlocked_insert_head_list);
    k.imp("ExfInterlockedInsertTailList", exf_interlocked_insert_tail_list);
    k.imp("ExfInterlockedRemoveHeadList", exf_interlocked_remove_head_list);
    k.imp("InterlockedPushEntrySList", interlocked_push_entry_slist);
    k.imp("InterlockedPopEntrySList", interlocked_pop_entry_slist);
    k.imp("InterlockedFlushSList", interlocked_flush_slist);
}

xb_types::state_fields!(State {
    eeprom,
    eeprom_disk,
    eeprom_ready,
    sec,
    sec_plain,
    sec_key,
    av_saved,
    av_mode,
    smc_version_idx,
    fsc_pages
});

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use xb_formats::block::MemSource;

    use super::*;
    use crate::kernel::ObType;
    use crate::testing::{TestKernel, kernel, kernel_with};
    use crate::xc_crypto::sha1;

    /// ExQueryNonVolatileSetting into a buffer of `len` bytes: the status,
    /// the type, the result length and the buffer.
    fn query(t: &mut TestKernel, index: u32, len: u32) -> (u32, u32, u32, Vec<u8>) {
        let out = t.alloc(0x200);
        let (ty, rl) = (out + 0x180, out + 0x184);
        let st = t.call32("ExQueryNonVolatileSetting", &[index, ty, out, len, rl]);
        let (ty, rl) = (t.k.gm.r32(ty).unwrap(), t.k.gm.r32(rl).unwrap());
        (st, ty, rl, t.k.gm.read_vec(out, len).unwrap())
    }

    fn image(t: &mut TestKernel) -> Vec<u8> {
        let (st, _, _, img) = query(t, XC_MAX_ALL, 0x100);
        assert_eq!(st, STATUS_SUCCESS);
        img
    }

    fn exit(t: &TestKernel) -> (TitleExit, String) {
        match t.k.stop.clone() {
            Some(Stop::Exit(e, why)) => (e, why),
            other => panic!("no title exit: {:?}", other.is_some()),
        }
    }

    #[test]
    fn a_pool_block_reports_its_size_and_its_bytes_come_back_when_freed() {
        let mut t = kernel();
        let before = t.k.pool.ex;
        let p = t.call32("ExAllocatePool", &[100]);
        assert!(p >= CONTIG_WINDOW);
        assert_eq!(t.call32("ExQueryPoolBlockSize", &[p]), 100);
        assert_eq!(t.k.pool.ex, before + 100 + POOL_HDR);
        t.k.gm.fill(p, 0xAA, 100).unwrap();
        let q = t.call32("ExAllocatePoolWithTag", &[40, 0x6761_5474]);
        assert!(q != 0 && q != p);
        t.call("ExFreePool", &[p]);
        t.call("ExFreePool", &[q]);
        assert_eq!(t.k.pool.ex, before);
        t.call("ExFreePool", &[0]);
        assert_eq!(t.k.pool.ex, before);
    }

    #[test]
    fn a_large_pool_block_takes_whole_pages_and_gives_them_back() {
        let mut t = kernel();
        let (free, ex) = (t.k.gm.phys.free_bytes(), t.k.pool.ex);
        let p = t.call32("ExAllocatePool", &[0x3000]);
        assert_eq!(p & (PAGE_SIZE - 1), POOL_HDR);
        assert_eq!(t.call32("ExQueryPoolBlockSize", &[p]), 0x3000);
        assert_eq!(t.k.gm.phys.free_bytes(), free - 0x4000);
        assert_eq!(t.k.pool.ex, ex + 0x4000);
        t.call("ExFreePool", &[p]);
        assert_eq!(t.k.gm.phys.free_bytes(), free);
        assert_eq!(t.k.pool.ex, ex);
        // more than there is
        assert_eq!(t.call32("ExAllocatePool", &[0xFFFF_FFF8]), 0);
    }

    #[test]
    fn every_known_setting_is_served_with_its_type_and_length() {
        let mut t = kernel();
        for (index, _, size) in NV {
            let (st, ty, rl, _) = query(&mut t, index, size as u32);
            assert_eq!(st, STATUS_SUCCESS, "setting {index:x}");
            assert_eq!(ty, if size == 4 { REG_DWORD } else { REG_BINARY }, "setting {index:x}");
            assert_eq!(rl, size as u32);
            let (st, _, rl, _) = query(&mut t, index, size as u32 - 1);
            assert_eq!(st, STATUS_BUFFER_TOO_SMALL);
            assert_eq!(rl, size as u32);
        }
        assert_eq!(query(&mut t, 0x55, 4).0, STATUS_OBJECT_NAME_NOT_FOUND);
    }

    #[test]
    fn the_image_checks_by_its_checksums_and_its_encrypted_section_by_its_hash() {
        let mut t = kernel();
        let img = image(&mut t);
        assert_eq!(rd32(&img, 0x30), eeprom_checksum(&img[0x34..0x60]));
        assert_eq!(rd32(&img, 0x60), eeprom_checksum(&img[0x64..0xC0]));
        assert!(img[0x34..0x40].iter().all(u8::is_ascii_digit));
        assert_eq!(img[0x40..0x43], [0x00, 0x50, 0xF2]);
        // as another kernel reading the chip would decrypt it
        let mut s = State::default();
        s.eeprom.copy_from_slice(&img);
        assert!(s.sec_decrypt());
        assert_eq!(s.sec_key, 0);
        assert_eq!(s.sec[0x1C..0x2C], LEGACY_HD_KEY);
        let (_, _, _, region) = query(&mut t, 0x104, 4);
        assert_eq!(region, s.sec[0x2C..0x30]);
        // the sections whole are the image's
        assert_eq!(query(&mut t, XC_MAX_OS, 0x60).3, img[0x60..0xC0]);
        assert_eq!(query(&mut t, XC_MAX_FACTORY, 0x30).3, img[0x30..0x60]);
        assert_eq!(query(&mut t, XC_ENCRYPTED_SECTION, 0x30).3, img[..0x30]);
        // a damaged section checks under no key
        s.eeprom[0x20] ^= 1;
        assert!(!s.sec_decrypt());
    }

    #[test]
    fn a_resumed_sha1_agrees_with_the_hash_of_the_whole_message() {
        let block = [0x36u8; 64];
        let mut s = Sha1::new();
        s.update(&block);
        let bytes = s.to_bytes();
        let mut h = [0u32; 5];
        for (h, c) in h.iter_mut().zip(bytes.as_chunks::<4>().0) {
            *h = u32::from_le_bytes(*c);
        }
        let mut whole = block.to_vec();
        whole.extend_from_slice(b"the confounder and the keys");
        assert_eq!(sha1_resumed(&h, b"the confounder and the keys"), sha1(&whole));
    }

    #[test]
    fn a_saved_setting_is_what_the_next_query_returns() {
        let mut t = kernel();
        let v = t.alloc(16);
        t.k.gm.w32(v, 3).unwrap();
        assert_eq!(t.call32("ExSaveNonVolatileSetting", &[0x07, REG_DWORD, v, 4]), STATUS_SUCCESS);
        assert_eq!(query(&mut t, 0x07, 4).3, [3, 0, 0, 0]);
        let img = image(&mut t);
        assert_eq!(rd32(&img, 0x60), eeprom_checksum(&img[0x64..0xC0]));
        let d = t.k.ex.eeprom_disk;
        assert_eq!(rd32(&d, 0x90), 3);
        assert_eq!(rd32(&d, 0x60), eeprom_checksum(&d[0x64..0xC0]));
        // a short value is zero-extended; the factory section is not the title's
        t.k.gm.w32(v, 0xFFFF_FFFF).unwrap();
        assert_eq!(t.call32("ExSaveNonVolatileSetting", &[0x0A, REG_DWORD, v, 2]), STATUS_SUCCESS);
        assert_eq!(query(&mut t, 0x0A, 4).3, [0xFF, 0xFF, 0, 0]);
        for index in [0x100, 0x101, 0x102, 0x104, XC_MAX_FACTORY, XC_MAX_ALL, 0x55] {
            assert_eq!(
                t.call32("ExSaveNonVolatileSetting", &[index, REG_BINARY, v, 4]),
                STATUS_ACCESS_DENIED
            );
        }
        assert_eq!(t.call32("ExSaveNonVolatileSetting", &[0x103, REG_DWORD, v, 4]), STATUS_SUCCESS);
        // the user section whole, as it was read
        let user = t.alloc(0x60);
        let mut section = query(&mut t, XC_MAX_OS, 0x60).3;
        section[0x30] = 6; // the language, at 0x90 of the image
        t.k.gm.write(user, &section).unwrap();
        assert_eq!(
            t.call32("ExSaveNonVolatileSetting", &[XC_MAX_OS, REG_BINARY, user, 0x60]),
            STATUS_SUCCESS
        );
        assert_eq!(query(&mut t, 0x07, 4).3, [6, 0, 0, 0]);
        let img = image(&mut t);
        assert_eq!(rd32(&img, 0x60), eeprom_checksum(&img[0x64..0xC0]));
    }

    #[test]
    fn the_configuration_and_the_title_set_the_runs_settings() {
        let mut t = kernel_with(|c| {
            c.console.video_flags = video::WIDESCREEN | video::PAL60;
            c.console.language = 4;
            c.console.time_zone_minutes_east = 60;
            c.console.surround = true;
        });
        t.k.title.game_region = 4;
        t.k.title.game_regions = 4;
        let img = image(&mut t);
        assert_eq!(rd32(&img, 0x94), video::WIDESCREEN | video::PAL60);
        assert_eq!(rd32(&img, 0x90), 4);
        assert_eq!(rd32(&img, 0x64) as i32, -60);
        assert_eq!(rd32(&img, 0x98), 0x0001_0002);
        assert_eq!(rd32(&img, 0x58), 0x0080_0300);
        assert_eq!(rd32(&img, 0xBC), 2);
        assert_eq!(query(&mut t, 0x104, 4).3, [4, 0, 0, 0]);
        // the stored image keeps the console's own settings, not the run's
        assert_eq!(rd32(&t.k.ex.eeprom_disk, 0x94), video::WIDESCREEN);
        assert_eq!(rd32(&t.k.ex.eeprom_disk, 0x98), 0);

        // an NTSC title has no PAL-60, nor has Halo's PAL release
        let mut t = kernel_with(|c| c.console.video_flags = video::HD_480P | video::PAL60);
        t.k.title.game_region = 1;
        assert_eq!(rd32(&image(&mut t), 0x94), video::HD_480P);
        let mut t = kernel_with(|c| c.console.video_flags = video::HD_480P | video::PAL60);
        t.k.title.game_region = 4;
        t.k.title.title_id = 0x4D53_0004;
        assert_eq!(rd32(&image(&mut t), 0x94), video::HD_480P);
    }

    #[test]
    fn a_disc_for_america_and_japan_is_a_japanese_consoles_with_a_japanese_language() {
        let mut t = kernel_with(|c| c.console.language = 2);
        t.k.title.game_region = 1;
        t.k.title.game_regions = 3;
        let img = image(&mut t);
        assert_eq!(t.k.title.game_region, 2);
        assert_eq!(query(&mut t, 0x104, 4).3, [2, 0, 0, 0]);
        assert_eq!(rd32(&img, 0x58), 0x0040_0100); // both NTSC: the video stays
    }

    #[test]
    fn the_console_keeps_its_identity_in_a_file_next_to_the_hard_disk() {
        let dir = std::env::temp_dir().join(format!("xb-kernel-ex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("hdd")).unwrap();
        let mut t = kernel();
        t.k.title.hdd_root = dir.join("hdd");
        let first = image(&mut t);
        let stored = std::fs::read(dir.join("eeprom.bin")).unwrap();
        assert_eq!(stored.len(), EEPROM_SIZE);
        assert_eq!(stored[0x34..0x58], first[0x34..0x58]);
        // a title's save reaches the file
        let v = t.alloc(16);
        t.k.gm.w32(v, 5).unwrap();
        t.call32("ExSaveNonVolatileSetting", &[0x0B, REG_DWORD, v, 4]);
        // the next boot is the same console, with what the title saved
        let mut t = kernel();
        t.k.title.hdd_root = dir.join("hdd");
        let second = image(&mut t);
        assert_eq!(second[..0x60], first[..0x60]);
        assert_eq!(rd32(&second, 0xA0), 5);
        // and a file that isn't an EEPROM stays as it is
        std::fs::write(dir.join("eeprom.bin"), b"short").unwrap();
        let mut t = kernel();
        t.k.title.hdd_root = dir.join("hdd");
        let third = image(&mut t);
        assert_eq!(rd32(&third, 0x30), eeprom_checksum(&third[0x34..0x60]));
        assert_eq!(std::fs::read(dir.join("eeprom.bin")).unwrap(), b"short");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An XBE header at a scratch page with two sections that share a page,
    /// and the file they load from.
    fn sections(t: &mut TestKernel) -> (u32, u32, Vec<u8>) {
        let base = t.alloc(0x1000);
        let g = &t.k.gm;
        g.w32(base + 0x108, 0x1000).unwrap();
        g.w32(base + 0x11C, 2).unwrap();
        g.w32(base + 0x120, base + 0x200).unwrap();
        g.write(base + 0x300, b"FIRST\0").unwrap();
        let (a, b) = (base + 0x200, base + 0x238);
        for (s, va, vsize, raw, rsize) in [
            (a, 0x0200_0000u32, 0x1800u32, 0x100u32, 0x1000u32),
            (b, 0x0200_1800, 0x800, 0x1100, 0x100),
        ] {
            g.w32(s + 0x04, va).unwrap();
            g.w32(s + 0x08, vsize).unwrap();
            g.w32(s + 0x0C, raw).unwrap();
            g.w32(s + 0x10, rsize).unwrap();
        }
        g.w32(a + 0x14, base + 0x300).unwrap();
        t.k.title.base = base;
        let file: Vec<u8> = (0..0x1200u32).map(|i| (i % 251) as u8 + 1).collect();
        t.k.title.xbe = Some(Arc::new(MemSource(file.clone())));
        (a, b, file)
    }

    #[test]
    fn a_section_loads_from_the_xbe_on_its_first_reference_and_goes_with_its_last() {
        let mut t = kernel();
        let (a, b, file) = sections(&mut t);
        assert!(!t.k.gm.mapped(0x0200_0000));
        assert_eq!(t.call32("XeLoadSection", &[a]), STATUS_SUCCESS);
        assert_eq!(t.k.gm.r32(a + 0x18), Ok(1));
        assert_eq!(t.k.gm.read_vec(0x0200_0000, 0x1000).unwrap(), file[0x100..0x1100]);
        assert_eq!(t.k.gm.read_vec(0x0200_1000, 0x800).unwrap(), vec![0; 0x800]);
        // loaded once: a second reference only counts
        t.k.gm.w8(0x0200_0000, 0xEE).unwrap();
        assert_eq!(t.call32("XeLoadSection", &[a]), STATUS_SUCCESS);
        assert_eq!(t.k.gm.r32(a + 0x18), Ok(2));
        assert_eq!(t.k.gm.r8(0x0200_0000), Ok(0xEE));
        assert_eq!(t.call32("XeLoadSection", &[b]), STATUS_SUCCESS);
        assert_eq!(t.k.gm.read_vec(0x0200_1800, 0x100).unwrap(), file[0x1100..0x1200]);

        assert_eq!(t.call32("XeUnloadSection", &[a]), STATUS_SUCCESS);
        assert!(t.k.gm.mapped(0x0200_0000));
        assert_eq!(t.call32("XeUnloadSection", &[a]), STATUS_SUCCESS);
        // its own page goes; the one it shares with the loaded section stays
        assert!(!t.k.gm.mapped(0x0200_0000));
        assert!(t.k.gm.mapped(0x0200_1000));
        assert_eq!(t.k.gm.read_vec(0x0200_1800, 0x100).unwrap(), file[0x1100..0x1200]);
        assert_eq!(t.call32("XeUnloadSection", &[a]), STATUS_INVALID_PARAMETER);
        assert_eq!(t.call32("XeUnloadSection", &[b]), STATUS_SUCCESS);
        assert!(!t.k.gm.mapped(0x0200_1000));
    }

    #[test]
    fn a_section_the_file_cannot_give_is_a_device_error() {
        let mut t = kernel();
        let (a, _, file) = sections(&mut t);
        t.k.title.xbe = Some(Arc::new(MemSource(file[..0x800].to_vec())));
        assert_eq!(t.call32("XeLoadSection", &[a]), STATUS_DEVICE_DATA_ERROR);
        t.k.title.xbe = None;
        t.k.gm.w32(a + 0x18, 0).unwrap();
        assert_eq!(t.call32("XeLoadSection", &[a]), STATUS_DEVICE_DATA_ERROR);
    }

    #[test]
    fn a_handle_gives_its_object_with_a_reference_more() {
        let mut t = kernel();
        let obj = t.k.ob_create(ObType::Event, 0x10);
        let h = t.k.ob_handle(obj, ObType::Event);
        let out = t.alloc(8);
        let event = t.k.type_va[ObType::Event as usize];
        let sem = t.k.type_va[ObType::Semaphore as usize];
        assert_eq!(t.call32("ObReferenceObjectByHandle", &[h, event, out]), STATUS_SUCCESS);
        assert_eq!(t.k.gm.r32(out), Ok(obj));
        assert_eq!(t.k.gm.r32(obj - OB_HEADER), Ok(2));
        assert_eq!(
            t.call32("ObReferenceObjectByHandle", &[h, sem, out]),
            STATUS_OBJECT_TYPE_MISMATCH
        );
        assert_eq!(t.call32("ObReferenceObjectByHandle", &[0x7770, 0, out]), STATUS_INVALID_HANDLE);
        t.call("ObfReferenceObject", &[obj]);
        assert_eq!(t.k.gm.r32(obj - OB_HEADER), Ok(3));
        assert_eq!(t.call32("ObReferenceObjectByPointer", &[obj, event]), STATUS_SUCCESS);
        assert_eq!(
            t.call32("ObReferenceObjectByPointer", &[obj, sem]),
            STATUS_OBJECT_TYPE_MISMATCH
        );
        for left in [3, 2, 1] {
            t.call("ObfDereferenceObject", &[obj]);
            assert_eq!(t.k.gm.r32(obj - OB_HEADER), Ok(left));
        }
        // NtCurrentThread is the running thread
        let me = t.k.threads[t.k.current.unwrap()].ethread;
        assert_eq!(t.call32("ObReferenceObjectByHandle", &[NT_CURRENT_THREAD, 0, out]), 0);
        assert_eq!(t.k.gm.r32(out), Ok(me));
    }

    fn printed(spec: &str, conv: u8, v: u32) -> String {
        let mut out = Vec::new();
        Spec::parse(spec.as_bytes()).integer(&mut out, conv, v);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn integer_conversions_print_as_printf_does() {
        assert_eq!(printed("", b'd', (-42i32) as u32), "-42");
        assert_eq!(printed("05", b'd', (-42i32) as u32), "-0042");
        assert_eq!(printed("+", b'i', 7), "+7");
        assert_eq!(printed("-6", b'x', 0xAB), "ab    ");
        assert_eq!(printed("#", b'X', 255), "0XFF");
        assert_eq!(printed("08", b'x', 0xBEEF), "0000beef");
        assert_eq!(printed("6.3", b'u', 7), "   007");
        assert_eq!(printed("", b'u', 0xFFFF_FFFF), "4294967295");
        assert_eq!(printed(".0", b'd', 0), "");
    }

    #[test]
    fn dbg_print_returns_to_its_caller_whatever_its_arguments_point_at() {
        let mut t = kernel();
        let f = t.alloc(64);
        t.k.gm.write(f, b"%s is %5d%% of %08lx%c\n\0").unwrap();
        assert_eq!(t.call32("DbgPrint", &[f, 0xDEAD_0000, 50, 0x1234, 0x21]), 0);
        assert!(t.returned());
        assert_eq!(t.call32("DbgPrint", &[0xDEAD_0000]), 0);
        assert!(t.returned());
        t.call("DbgBreakPoint", &[]);
        t.call("DbgBreakPointWithStatus", &[5]);
        assert!(t.returned() && t.stop_reason().is_none());
    }

    #[test]
    fn the_saved_av_data_address_is_the_one_last_set() {
        let mut t = kernel();
        assert_eq!(t.call32("AvGetSavedDataAddress", &[]), 0);
        t.call("AvSetSavedDataAddress", &[0x83F0_0000]);
        assert_eq!(t.call32("AvGetSavedDataAddress", &[]), 0x83F0_0000);
    }

    #[test]
    fn the_tv_encoder_reports_the_av_pack_the_standard_and_the_video_flags() {
        for (pack, caps, smc) in [
            (AvPack::Standard, 1, 0x06),
            (AvPack::Scart, 3, 0x00),
            (AvPack::Hdtv, 4, 0x01),
            (AvPack::Vga, 5, 0x02),
            (AvPack::SVideo, 6, 0x04),
        ] {
            let mut t = kernel_with(|c| {
                c.console.av_pack = pack;
                c.console.video_flags = video::HD_480P | video::WIDESCREEN;
            });
            let out = t.alloc(8);
            t.call("AvSendTVEncoderOption", &[0xFD00_0000, AV_QUERY_AV_CAPABILITIES, 0, out]);
            // NTSC-M at 60 Hz for a title without a region
            assert_eq!(
                t.k.gm.r32(out),
                Ok(caps | 0x0040_0100 | video::HD_480P | video::WIDESCREEN),
                "{pack:?}"
            );
            // the system controller's register agrees with the pack
            assert_eq!(t.call32("HalReadSMBusValue", &[0x20, 0x04, 0, out]), STATUS_SUCCESS);
            assert_eq!(t.k.gm.r32(out), Ok(smc));
            // any other option answers 0, and a null result is fine
            t.k.gm.w32(out, 0xFFFF_FFFF).unwrap();
            t.call("AvSendTVEncoderOption", &[0xFD00_0000, 2, 1, out]);
            assert_eq!(t.k.gm.r32(out), Ok(0));
            t.call("AvSendTVEncoderOption", &[0xFD00_0000, AV_QUERY_AV_CAPABILITIES, 0, 0]);
            assert!(t.returned());
        }
    }

    #[test]
    fn a_display_mode_reaches_the_machine_with_its_refresh_rate() {
        let mut t = kernel();
        t.call("AvSetDisplayMode", &[0xFD00_0000, 0, 0, 0x12, 2560, 0x03C0_0000]);
        assert!(t.p.modes.is_empty());
        t.call("AvSetDisplayMode", &[0xFD00_0000, 0, 0x5803_0314, 0x12, 2880, 0x03C0_0000]);
        t.call("AvSetDisplayMode", &[0xFD00_0000, 0, 0x8801_0101, 0x12, 2560, 0x03B0_0000]);
        assert_eq!(
            t.p.modes,
            [
                DisplayMode {
                    mode: 0x5803_0314,
                    format: 0x12,
                    pitch: 2880,
                    frame_buffer: 0x03C0_0000,
                    refresh_hz: 50
                },
                DisplayMode {
                    mode: 0x8801_0101,
                    format: 0x12,
                    pitch: 2560,
                    frame_buffer: 0x03B0_0000,
                    refresh_hz: 60
                },
            ]
        );
        assert_eq!(t.k.ex.av_mode, 0x8801_0101);
        // no presentation parameters anywhere: the shape is not told
        assert!(t.p.widescreen.is_empty());
    }

    /// D3DPRESENT_PARAMETERS for 640x480 with `flags`, at `va`.
    fn put_present_params(t: &TestKernel, va: u32, flags: u32) {
        for (off, v) in [(0, 640), (4, 480), (8, 0x12), (12, 1), (32, 1), (40, flags), (44, 60)] {
            t.k.gm.w32(va + off, v).unwrap();
        }
    }

    #[test]
    fn the_titles_shape_is_found_in_its_presentation_parameters() {
        // in its memory
        let mut t = kernel();
        let pp = t.alloc(0x100);
        put_present_params(&t, pp + 0x40, 0x10);
        t.call("AvSetDisplayMode", &[0, 0, 0x2002_0000, 0x12, 2560, 0]);
        assert_eq!(t.p.widescreen, [true]);
        // of two, the one whose flags fit the mode: progressive for an HD mode
        let mut t = kernel();
        let pp = t.alloc(0x100);
        put_present_params(&t, pp, 0x10);
        put_present_params(&t, pp + 0x80, 0x40);
        t.call("AvSetDisplayMode", &[0, 0, 0x8801_0101, 0x12, 2560, 0]);
        assert_eq!(t.p.widescreen, [false]);
        // a local of the caller's, by a pointer on its stack, comes first
        let mut t = kernel();
        let pp = t.alloc(0x100);
        put_present_params(&t, pp, 0x10);
        put_present_params(&t, pp + 0x80, 0);
        let esp = t.k.esp();
        t.k.gm.w32(esp - 4, pp).unwrap();
        t.k.set_esp(esp - 4);
        t.call("AvSetDisplayMode", &[0, 0, 0x2002_0000, 0x12, 2560, 0]);
        assert_eq!(t.p.widescreen, [true]);
        // something that only starts like one is none
        let mut t = kernel();
        let pp = t.alloc(0x100);
        put_present_params(&t, pp, 0x10);
        t.k.gm.w32(pp + 44, 75).unwrap();
        t.call("AvSetDisplayMode", &[0, 0, 0x2002_0000, 0x12, 2560, 0]);
        assert!(t.p.widescreen.is_empty());
    }

    #[test]
    fn a_call_with_no_mode_but_a_frame_buffer_looks_for_a_shape_not_yet_known() {
        let mut t = kernel();
        let pp = t.alloc(0x100);
        put_present_params(&t, pp, 0x10);
        // Neither a mode nor a frame buffer: nothing to go by.
        t.call("AvSetDisplayMode", &[0, 0, 0, 0x12, 2560, 0]);
        assert!(t.p.widescreen.is_empty());
        t.call("AvSetDisplayMode", &[0, 0, 0, 0x12, 2560, 0x03C0_0000]);
        assert_eq!(t.p.widescreen, [true]);
        assert!(t.p.modes.is_empty(), "no mode was set");
        // Known, it is not looked for again without a mode.
        put_present_params(&t, pp, 0);
        t.call("AvSetDisplayMode", &[0, 0, 0, 0x12, 2560, 0x03C0_0000]);
        assert_eq!(t.p.widescreen, [true]);
        // A mode's own scan is the same one.
        t.call("AvSetDisplayMode", &[0, 0, 0x2002_0000, 0x12, 2560, 0x03C0_0000]);
        assert_eq!(t.p.widescreen, [true, false]);
    }

    #[test]
    fn a_large_pool_block_freed_comes_back_at_its_address_past_smaller_ones() {
        let mut t = kernel();
        // A hole of five pages between blocks that stay.
        let hole = t.call32("ExAllocatePool", &[0x4800]);
        let stays = t.call32("ExAllocatePool", &[0x1800]);
        assert!(hole != 0 && stays != 0);
        t.call("ExFreePool", &[hole]);
        // The title frees a large block, makes smaller ones and asks for one
        // of the first's size again.
        let text = t.call32("ExAllocatePool", &[0x1E_0000]);
        assert_ne!(text, 0);
        t.call("ExFreePool", &[text]);
        let small = [0x3800, 0x1800].map(|size| t.call32("ExAllocatePool", &[size]));
        assert!(small.iter().all(|&p| p != 0 && p != text));
        assert_eq!(t.call32("ExAllocatePool", &[0x1E_0000]), text);
    }

    #[test]
    fn the_launch_pages_path_becomes_a_device_path() {
        let page = |path: &[u8]| {
            let mut p = vec![0u8; 0x1000];
            p[LAUNCH_PATH_OFF..LAUNCH_PATH_OFF + path.len()].copy_from_slice(path);
            p
        };
        assert_eq!(
            launch_xbe(&page(b"\\Device\\CdRom0;morrowind.xbe")).as_deref(),
            Some("\\Device\\CdRom0\\morrowind.xbe")
        );
        assert_eq!(
            launch_xbe(&page(b"\\device\\harddisk0\\partition2\\apps;x.xbe")).as_deref(),
            Some("\\device\\harddisk0\\partition2\\apps\\x.xbe")
        );
        assert_eq!(
            launch_xbe(&page(b"\\Device\\CdRom0\\dir\\a.xbe")).as_deref(),
            Some("\\Device\\CdRom0\\dir\\a.xbe")
        );
        assert_eq!(launch_xbe(&page(b"\\Device\\Harddisk0\\Partition7;x.xbe")), None);
        assert_eq!(launch_xbe(&page(b"\\Device\\CdRom0")), None);
        assert_eq!(launch_xbe(&page(b"D:\\default.xbe")), None);
        assert_eq!(launch_xbe(&page(b"")), None);
        assert_eq!(launch_xbe(&[0u8; 16]), None);
    }

    /// A launch data page of the type, with the path and the first word of
    /// its data, as LaunchDataPage.
    fn launch_page(t: &mut TestKernel, ty: u32, path: &[u8], data: u32) -> u32 {
        let page = t.alloc(0x1000);
        t.k.gm.w32(page, ty).unwrap();
        t.k.gm.write(page + LAUNCH_PATH_OFF as u32, path).unwrap();
        t.k.gm.w32(page + LAUNCH_DATA_OFF as u32, data).unwrap();
        t.k.gm.kw32(t.k.data_addr("LaunchDataPage"), page);
        page
    }

    #[test]
    fn a_quick_reboot_into_an_xbe_on_the_disc_is_a_relaunch_with_its_page() {
        let mut t = kernel();
        let page = launch_page(&mut t, 0xFFFF_FFFF, b"\\Device\\CdRom0;second.xbe", 0x1234);
        t.call("HalReturnToFirmware", &[2]);
        let (e, why) = exit(&t);
        let TitleExit::Relaunch { xbe_path, launch_page } = e else { panic!("{e:?}") };
        assert_eq!(xbe_path, "\\Device\\CdRom0\\second.xbe");
        assert_eq!(launch_page, t.k.gm.read_vec(page, 0x1000).unwrap());
        assert_eq!(why, "relaunch into \\Device\\CdRom0\\second.xbe (launch type 4294967295)");
    }

    #[test]
    fn returning_to_the_firmware_any_other_way_ends_the_title() {
        // no launch data page
        let mut t = kernel();
        t.call("HalReturnToFirmware", &[1]);
        let (e, why) = exit(&t);
        assert_eq!(
            e,
            TitleExit::Firmware {
                routine: 1,
                launch_path: String::new(),
                dashboard_missing: false
            }
        );
        assert_eq!(why, "title returned to the firmware (routine 1)");
        // an XBE that is on no hard disk
        let mut t = kernel();
        launch_page(&mut t, 0xFFFF_FFFF, b"\\Device\\Harddisk0\\Partition1;gone.xbe", 0);
        t.call("HalReturnToFirmware", &[2]);
        let (e, why) = exit(&t);
        assert_eq!(
            e,
            TitleExit::Firmware {
                routine: 2,
                launch_path: "\\Device\\Harddisk0\\Partition1;gone.xbe".into(),
                dashboard_missing: false
            }
        );
        assert_eq!(
            why,
            "title returned to the firmware (routine 2, launch path \\Device\\Harddisk0\\Partition1;gone.xbe)"
        );
        // a page of the dashboard's, with no dashboard installed
        let mut t = kernel();
        launch_page(&mut t, LDT_LAUNCH_DASHBOARD, b"", 3);
        t.call("HalReturnToFirmware", &[2]);
        assert_eq!(
            exit(&t).0,
            TitleExit::Firmware { routine: 2, launch_path: String::new(), dashboard_missing: true }
        );
        // the dashboard's main menu: the game closes
        let mut t = kernel();
        launch_page(&mut t, LDT_LAUNCH_DASHBOARD, b"", 0);
        t.call("HalReturnToFirmware", &[2]);
        assert_eq!(
            exit(&t).0,
            TitleExit::Firmware {
                routine: 2,
                launch_path: String::new(),
                dashboard_missing: false
            }
        );
        // a shutdown
        let mut t = kernel();
        t.call("HalInitiateShutdown", &[]);
        assert_eq!(exit(&t), (TitleExit::Shutdown, "title shut the console down".into()));
    }

    #[test]
    fn the_system_controller_answers_its_version_the_tray_and_nothing_else() {
        let mut t = kernel();
        let out = t.alloc(8);
        let mut version = Vec::new();
        for _ in 0..4 {
            t.call("HalReadSMBusValue", &[0x21, 0x01, 0, out]);
            version.push(t.k.gm.r32(out).unwrap() as u8);
        }
        assert_eq!(version, b"P01P");
        t.call("HalReadSMBusValue", &[0x20, 0x03, 0, out]);
        assert_eq!(t.k.gm.r32(out), Ok(0x40));
        t.k.title.disc_present = true;
        t.call("HalReadSMBusValue", &[0x20, 0x03, 0, out]);
        assert_eq!(t.k.gm.r32(out), Ok(0x60));
        t.k.gm.w32(out + 4, 9).unwrap();
        assert_eq!(t.call32("HalReadSMCTrayState", &[out, out + 4]), STATUS_SUCCESS);
        assert_eq!((t.k.gm.r32(out), t.k.gm.r32(out + 4)), (Ok(0x60), Ok(0)));
        t.k.gm.w32(out, 9).unwrap();
        t.call("HalReadSMBusValue", &[0x20, 0x0F, 0, out]);
        assert_eq!(t.k.gm.r32(out), Ok(0));
        assert_eq!(t.call32("HalWriteSMBusValue", &[0x20, 0x08, 0, 1]), STATUS_SUCCESS);
        assert_eq!(t.call32("HalIsResetOrShutdownPending", &[]), 0);
        t.call("HalRegisterShutdownNotification", &[out, 1]);
        t.call("HalEnableSecureTrayEject", &[]);
        assert!(t.returned());
    }

    #[test]
    fn pci_configuration_space_reads_as_zeros() {
        let mut t = kernel();
        let buf = t.alloc(16);
        t.k.gm.fill(buf, 0xFF, 16).unwrap();
        t.call("HalReadWritePCISpace", &[0, 0x10, 0x10, buf, 4, 0]);
        assert_eq!(t.k.gm.read_vec(buf, 8).unwrap(), [0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]);
        t.call("HalReadWritePCISpace", &[0, 0x10, 0x10, buf + 4, 4, 1]);
        assert_eq!(t.k.gm.r32(buf + 4), Ok(0xFFFF_FFFF));
    }

    #[test]
    fn the_network_phy_is_the_machines() {
        let mut t = kernel();
        t.p.link_state = 0x0B;
        assert_eq!(t.call32("PhyGetLinkState", &[0]), 0x0B);
        assert_eq!(t.call32("PhyInitialize", &[0, 0]), STATUS_SUCCESS);
        assert_eq!(t.p.phy_inits, 1);
    }

    #[test]
    fn the_file_cache_size_reads_back_as_set() {
        let mut t = kernel();
        assert_eq!(t.call32("FscGetCacheSize", &[]), 16);
        assert_eq!(t.call32("FscSetCacheSize", &[64]), STATUS_SUCCESS);
        assert_eq!(t.call32("FscGetCacheSize", &[]), 64);
    }

    #[test]
    fn a_bugcheck_stops_the_guest_with_its_code() {
        let mut t = kernel();
        t.call("KeBugCheck", &[0xC000_021A]);
        assert!(t.stop_reason().unwrap().starts_with("KeBugCheck(c000021a) from "));
        assert!(matches!(t.k.stop, Some(Stop::Fatal(_))));
        let mut t = kernel();
        t.call("KeBugCheckEx", &[0x50, 1, 2, 3, 4]);
        assert!(t.stop_reason().unwrap().starts_with("KeBugCheckEx(00000050, 00000001, "));
    }

    #[test]
    fn a_stall_takes_the_time_asked_and_the_guests_clock_sees_it() {
        let mut t = kernel();
        t.k.clock.pause_end(); // as inside Kernel::run: the guest's time goes on
        let (host, guest) = (Instant::now(), t.k.now());
        t.call("KeStallExecutionProcessor", &[3000]);
        assert!(host.elapsed() >= Duration::from_micros(3000));
        assert!(t.k.now() - guest >= 3_000_000);
        // a tenth of a second at most
        let host = Instant::now();
        t.call("KeStallExecutionProcessor", &[0xFFFF_FFFF]);
        assert!(host.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_dpc_is_initialized_with_its_routine_and_context() {
        let mut t = kernel();
        let d = t.alloc(0x40);
        t.k.gm.fill(d, 0xFF, 0x20).unwrap();
        t.call("KeInitializeDpc", &[d, 0x0001_2000, 0x0BAD_F00D]);
        assert_eq!(t.k.gm.r32(d), Ok(19));
        assert_eq!(t.k.gm.r32(d + 0x0C), Ok(0x0001_2000));
        assert_eq!(t.k.gm.r32(d + 0x10), Ok(0x0BAD_F00D));
        assert_eq!(t.k.gm.r64(d + 0x14), Ok(0));
        assert_eq!(t.k.gm.r32(d + 0x1C), Ok(0xFFFF_FFFF));
        assert_eq!(t.call32("KeSaveFloatingPointState", &[d]), STATUS_SUCCESS);
        assert_eq!(t.call32("KeRestoreFloatingPointState", &[d]), STATUS_SUCCESS);
    }

    #[test]
    fn the_interlocked_64_bit_helpers_return_the_value_before() {
        let mut t = kernel();
        let m = t.alloc(0x40);
        t.k.gm.w64(m, 0x0000_0001_FFFF_FFFF).unwrap();
        assert_eq!(t.call("ExInterlockedAddLargeInteger", &[m, 2, 1, 0]), 0x0000_0001_FFFF_FFFF);
        assert_eq!(t.k.gm.r64(m), Ok(0x0000_0003_0000_0001));
        t.call("ExInterlockedAddLargeStatistic", &[m, 0xFFFF_FFFF]);
        assert_eq!(t.k.gm.r64(m), Ok(0x0000_0004_0000_0000));
        t.k.gm.w64(m + 8, 0x1111_2222_3333_4444).unwrap(); // exchange
        t.k.gm.w64(m + 16, 5).unwrap(); // comparand: not the value
        assert_eq!(
            t.call("ExInterlockedCompareExchange64", &[m, m + 8, m + 16]),
            0x0000_0004_0000_0000
        );
        assert_eq!(t.k.gm.r64(m), Ok(0x0000_0004_0000_0000));
        t.k.gm.w64(m + 16, 0x0000_0004_0000_0000).unwrap();
        assert_eq!(
            t.call("ExInterlockedCompareExchange64", &[m, m + 8, m + 16]),
            0x0000_0004_0000_0000
        );
        assert_eq!(t.k.gm.r64(m), Ok(0x1111_2222_3333_4444));
    }

    #[test]
    fn the_interlocked_list_helpers_keep_a_doubly_linked_list() {
        let mut t = kernel();
        let m = t.alloc(0x100);
        let (head, a, b, c) = (m, m + 0x10, m + 0x20, m + 0x30);
        t.k.gm.w32(head, head).unwrap();
        t.k.gm.w32(head + 4, head).unwrap();
        assert_eq!(t.call32("ExfInterlockedRemoveHeadList", &[head]), 0);
        assert_eq!(t.call32("ExfInterlockedInsertHeadList", &[head, b]), 0);
        assert_eq!(t.call32("ExfInterlockedInsertHeadList", &[head, a]), b);
        assert_eq!(t.call32("ExfInterlockedInsertTailList", &[head, c]), b);
        // head <-> a <-> b <-> c <-> head
        let links = |t: &TestKernel, e: u32| (t.k.gm.r32(e).unwrap(), t.k.gm.r32(e + 4).unwrap());
        assert_eq!(links(&t, head), (a, c));
        assert_eq!(links(&t, a), (b, head));
        assert_eq!(links(&t, b), (c, a));
        assert_eq!(links(&t, c), (head, b));
        for e in [a, b, c, 0] {
            assert_eq!(t.call32("ExfInterlockedRemoveHeadList", &[head]), e);
        }
        assert_eq!(links(&t, head), (head, head));
        assert_eq!(t.call32("ExfInterlockedInsertTailList", &[head, a]), 0);
        assert_eq!(links(&t, head), (a, a));
    }

    #[test]
    fn the_interlocked_slist_is_last_in_first_out_and_counts_its_depth() {
        let mut t = kernel();
        let m = t.alloc(0x100);
        let (head, a, b) = (m, m + 0x10, m + 0x20);
        let depth = |t: &TestKernel| t.k.gm.r16(head + 4).unwrap();
        assert_eq!(t.call32("InterlockedPopEntrySList", &[head]), 0);
        assert_eq!(t.call32("InterlockedPushEntrySList", &[head, a]), 0);
        assert_eq!(t.call32("InterlockedPushEntrySList", &[head, b]), a);
        assert_eq!(depth(&t), 2);
        assert_eq!(t.k.gm.r32(b), Ok(a));
        assert_eq!(t.call32("InterlockedPopEntrySList", &[head]), b);
        assert_eq!(depth(&t), 1);
        assert_eq!(t.call32("InterlockedPushEntrySList", &[head, b]), a);
        assert_eq!(t.call32("InterlockedFlushSList", &[head]), b);
        assert_eq!((t.k.gm.r32(head), depth(&t)), (Ok(0), 0));
        assert_eq!(t.call32("InterlockedFlushSList", &[head]), 0);
        assert_eq!(t.call32("InterlockedPopEntrySList", &[head]), 0);
    }

    #[test]
    fn a_loaded_title_is_told_in_the_data_exports() {
        let mut t = kernel();
        let base = t.alloc(0x1000);
        let cert = base + 0x400;
        t.k.gm.w32(base + 0x118, cert).unwrap();
        t.k.gm.w32(cert + 0x08, 0x4541_0042).unwrap();
        t.k.gm.w32(cert + 0xA0, 4).unwrap();
        t.k.title.base = base;
        t.k.title.image_size = 0x1000;
        t.k.title_loaded();
        assert_eq!(
            (t.k.title.title_id, t.k.title.game_region, t.k.title.game_regions),
            (0x4541_0042, 4, 4)
        );
        let name = t.k.gm.ansi_string(t.k.data_addr("XeImageFileName")).unwrap();
        assert_eq!(name, b"\\Device\\CdRom0\\default.xbe");
        let mut key = [0u8; 16];
        t.k.gm.read(t.k.data_addr("XboxHDKey"), &mut key).unwrap();
        assert_eq!(key, LEGACY_HD_KEY);
        assert_eq!(t.k.gm.r32(t.k.data_addr("HalBootSMCVideoMode")), Ok(1));
        let model = t.k.gm.ansi_string(t.k.data_addr("HalDiskModelNumber")).unwrap();
        assert_eq!(model.len(), 40);
        assert!(model.starts_with(b"XBASED HDD "));
        let serial = t.k.gm.ansi_string(t.k.data_addr("HalDiskSerialNumber")).unwrap();
        assert_eq!(serial.len(), 20);
        assert_eq!(serial[..2], *b"XB");
        assert_eq!(serial[2..14], t.k.ex.eeprom[0x34..0x40]);
        // the console is the title's: a PAL one
        assert_eq!(rd32(&t.k.ex.eeprom, 0x58), 0x0080_0300);

        // a header whose certificate is nowhere tells no region: North America
        let mut t = kernel();
        let base = t.alloc(0x1000);
        t.k.gm.w32(base + 0x118, 0x7000_0000).unwrap();
        t.k.title.base = base;
        t.k.title.image_path = "\\Device\\Harddisk0\\Partition2\\xboxdash.xbe".into();
        t.k.title_loaded();
        assert_eq!((t.k.title.title_id, t.k.title.game_region, t.k.title.game_regions), (0, 1, 0));
        let name = t.k.gm.ansi_string(t.k.data_addr("XeImageFileName")).unwrap();
        assert_eq!(name, b"\\Device\\Harddisk0\\Partition2\\xboxdash.xbe");
    }
}
