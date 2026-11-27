//! XBE, the Xbox executable format.
//!
//! The header parser, and the parsing half of loading: address-space checks,
//! the kernel thunk table, the TLS directory, which sections are loaded at
//! boot.
//!
//! Nothing here touches guest memory. [`Xbe`] describes the image; a loader
//! asks for its [`Segment`]s and copies them, then reads the import list with
//! [`Xbe::kernel_imports_with`] from the image it loaded (or with
//! [`Xbe::kernel_imports`] straight from the file) and patches each slot.
//!
//! As the Xbox kernel loads an XBE, the headers and the sections flagged
//! preload get memory and the rest of the image is address space only, until
//! `XeLoadSection` brings a section in. Titles with many sections count on
//! that memory (Fable preloads 15 of 107), so a loader must not back the whole
//! image.

use xb_types::{PAGE_SIZE, RAM_DEBUG, align_up};

use crate::{BlockSource, Error, Result, le16, le32};

/// `"XBEH"`, the first four bytes of the file.
pub const MAGIC: u32 = 0x4845_4258;
/// XOR key of the entry point in a retail XBE.
pub const ENTRY_KEY_RETAIL: u32 = 0xA8FC_57AB;
/// XOR key of the entry point in a debug XBE.
pub const ENTRY_KEY_DEBUG: u32 = 0x9485_9D4B;
/// XOR key of the kernel thunk table address in a retail XBE.
pub const THUNK_KEY_RETAIL: u32 = 0x5B6D_40B6;
/// XOR key of the kernel thunk table address in a debug XBE.
pub const THUNK_KEY_DEBUG: u32 = 0xEFB1_F152;

/// Section flag: the section's pages are writable.
pub const SECTION_WRITABLE: u32 = 0x01;
/// Section flag: the section is loaded with the image, not by `XeLoadSection`.
pub const SECTION_PRELOAD: u32 = 0x02;
/// Section flag: the section holds code.
pub const SECTION_EXECUTABLE: u32 = 0x04;
/// Section flag: the section is a file inserted into the image.
pub const SECTION_INSERTED_FILE: u32 = 0x08;
/// Section flag: the page the section shares at its head is read-only.
pub const SECTION_HEAD_PAGE_READ_ONLY: u32 = 0x10;
/// Section flag: the page the section shares at its tail is read-only.
pub const SECTION_TAIL_PAGE_READ_ONLY: u32 = 0x20;

/// Size of the fixed header: the least a file must hold.
pub const HEADER_MIN: usize = 0x178;
/// Size of a section header.
pub const SECTION_HEADER_SIZE: u32 = 0x38;
/// Offset in a section header of its load count, which a loader sets to 1 for
/// each section it preloads (as `XeLoadSection` counts: an `XeUnloadSection`
/// then frees it).
pub const SECTION_REFCOUNT_OFFSET: u32 = 0x18;
/// Kernel imports accepted at most. The kernel exports fewer than 400
/// ordinals; a table with no end in this many slots is damaged.
pub const KERNEL_IMPORTS_MAX: usize = 1024;

/// Lowest base address of an image: the title's part of the address space
/// starts here (retail and debug XBEs are at `0x0001_0000`).
const BASE_MIN: u32 = 0x0001_0000;
/// End of the title's part of the address space. An image that names the
/// kernel's half would have its headers read over the kernel's own memory.
const USER_END: u64 = 0x7FFE_0000;

// Header field offsets (the header is packed).
const XO_MAGIC: usize = 0x000;
const XO_BASE: usize = 0x104;
const XO_SIZEOF_HEADERS: usize = 0x108;
const XO_SIZEOF_IMAGE: usize = 0x10C;
const XO_SIZEOF_IMAGE_HEADER: usize = 0x110;
const XO_TIMEDATE: usize = 0x114;
const XO_CERT_ADDR: usize = 0x118;
const XO_NSECTIONS: usize = 0x11C;
const XO_SECTIONS_ADDR: usize = 0x120;
const XO_INIT_FLAGS: usize = 0x124;
const XO_ENTRY: usize = 0x128;
const XO_TLS_ADDR: usize = 0x12C;
const XO_STACK_COMMIT: usize = 0x130;
const XO_HEAP_RESERVE: usize = 0x134;
const XO_HEAP_COMMIT: usize = 0x138;
const XO_PE_BASE: usize = 0x13C;
const XO_PE_SIZEOF_IMAGE: usize = 0x140;
const XO_PE_CHECKSUM: usize = 0x144;
const XO_PE_TIMEDATE: usize = 0x148;
const XO_DEBUG_PATHNAME: usize = 0x14C;
const XO_DEBUG_FILENAME: usize = 0x150;
const XO_DEBUG_UNICODE_FILENAME: usize = 0x154;
const XO_KERNEL_THUNK: usize = 0x158;
const XO_IMPORT_DIR: usize = 0x15C;
const XO_NLIBS: usize = 0x160;
const XO_LIBS_ADDR: usize = 0x164;
const XO_KERNEL_LIB_ADDR: usize = 0x168;
const XO_XAPI_LIB_ADDR: usize = 0x16C;
const XO_LOGO_ADDR: usize = 0x170;
const XO_LOGO_SIZE: usize = 0x174;

const LIB_SIZE: u32 = 16;
/// Bytes of the certificate this parser reads: through the signature key.
const CERT_PARSED: u32 = 0xD0;

/// How the entry point and the kernel thunk address are stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// XORed with the retail keys.
    Retail,
    /// XORed with the debug keys.
    Debug,
    /// Stored as they are: headers read back from a loaded image, which the
    /// loader leaves decoded.
    Plain,
}

/// The certificate: what the title is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Certificate {
    /// The certificate's own size field.
    pub size: u32,
    /// When it was made (seconds since 1970).
    pub timestamp: u32,
    /// The title id (publisher code and number, `0x4D53_0004` for Halo).
    pub title_id: u32,
    /// The title's name (up to 40 UTF-16 units in the file).
    pub title_name: String,
    /// Other title ids whose saves the title may open.
    pub alt_title_ids: [u32; 16],
    /// Media the title may run from (bit mask).
    pub allowed_media: u32,
    /// Regions the title may run in (1 North America, 2 Japan, 4 rest of the
    /// world, `0x8000_0000` manufacturing).
    pub game_region: u32,
    /// Age rating.
    pub game_ratings: u32,
    /// Disc number of a multi-disc title.
    pub disc_number: u32,
    /// The title's version.
    pub version: u32,
    /// Key for system-link traffic.
    pub lan_key: [u8; 16],
    /// Key the title's saves are signed with.
    pub signature_key: [u8; 16],
}

/// One section header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    /// The section's name (single bytes, kept as Latin-1 characters).
    pub name: String,
    /// `SECTION_*` bits.
    pub flags: u32,
    /// Virtual address it is loaded at.
    pub virtual_addr: u32,
    /// Bytes it takes in memory.
    pub virtual_size: u32,
    /// Offset of its bytes in the file.
    pub raw_addr: u32,
    /// Bytes it has in the file. Memory past them is zero.
    pub raw_size: u32,
    /// Address of the page reference count it shares at its head.
    pub head_shared_ref_addr: u32,
    /// Address of the page reference count it shares at its tail.
    pub tail_shared_ref_addr: u32,
    /// SHA-1 of the section as the linker computed it.
    pub digest: [u8; 20],
}

impl Section {
    /// True when the section is loaded with the image.
    pub fn is_preload(&self) -> bool {
        self.flags & SECTION_PRELOAD != 0
    }

    /// True when `va` lies in the section's memory.
    pub fn contains(&self, va: u32) -> bool {
        va >= self.virtual_addr && va - self.virtual_addr < self.virtual_size
    }

    /// Bytes of the section that come from the file.
    fn file_size(&self) -> u32 {
        self.raw_size.min(self.virtual_size)
    }
}

/// A statically linked library and its version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryVersion {
    /// Name, up to 8 characters (`XAPILIB`, `D3D8`, `DSOUND`, ...).
    pub name: String,
    /// Major version.
    pub major: u16,
    /// Minor version.
    pub minor: u16,
    /// Build number (the XDK's).
    pub build: u16,
    /// QFE number (the flags' low 13 bits).
    pub qfe: u16,
    /// Approval level (the flags' bits 13 and 14).
    pub approved: u8,
    /// A debug build of the library (the flags' top bit).
    pub debug_build: bool,
}

/// The TLS directory (`IMAGE_TLS_DIRECTORY32`), which lies in a section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TlsDirectory {
    /// Start of the initial data of a thread's TLS block.
    pub raw_data_start: u32,
    /// End of that data.
    pub raw_data_end: u32,
    /// Address of the image's TLS index variable.
    pub index_addr: u32,
    /// Address of the callback array, or 0.
    pub callbacks_addr: u32,
    /// Zero bytes that follow the initial data in a thread's block.
    pub zero_fill_size: u32,
    /// Characteristics.
    pub characteristics: u32,
}

/// One slot of the kernel thunk table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelImport {
    /// Address of the slot: where the loader writes the export's address.
    pub slot: u32,
    /// The kernel export's ordinal.
    pub ordinal: u32,
}

/// Which sections a loader asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sections {
    /// The ones loaded at boot.
    Preload,
    /// Every section.
    All,
}

/// A piece of the image a loader places: the headers or one section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// Index of the section, or `None` for the headers.
    pub section: Option<usize>,
    /// Virtual address it goes to.
    pub virtual_addr: u32,
    /// Bytes it takes in memory.
    pub virtual_size: u32,
    /// Offset of its bytes in the file.
    pub file_offset: u32,
    /// Bytes to copy from the file; the rest of `virtual_size` is zero.
    pub file_size: u32,
    /// `SECTION_*` bits. The headers are writable and preloaded: the kernel
    /// keeps section load counts in them.
    pub flags: u32,
}

/// A parsed XBE: the header's fields, the certificate, the section table and
/// the library versions.
#[derive(Clone, Debug)]
pub struct Xbe {
    headers: Vec<u8>,
    /// Address the image is loaded at.
    pub base: u32,
    /// Bytes of headers at the start of the file, mapped at `base`.
    pub headers_size: u32,
    /// Bytes of address space the image takes from `base`.
    pub image_size: u32,
    /// Size of the fixed header as the linker wrote it.
    pub image_header_size: u32,
    /// When the image was linked (seconds since 1970).
    pub timestamp: u32,
    /// Address of the certificate.
    pub certificate_addr: u32,
    /// Address of the section headers.
    pub sections_addr: u32,
    /// Initialization flags.
    pub init_flags: u32,
    /// The entry point, decoded.
    pub entry_point: u32,
    /// Address of the kernel thunk table, decoded.
    pub kernel_thunk_addr: u32,
    /// Which keys decoded the two.
    pub encoding: Encoding,
    /// Address of the TLS directory, or 0.
    pub tls_addr: u32,
    /// Stack bytes committed for the title's first thread.
    pub stack_commit: u32,
    /// Heap bytes reserved (from the PE the XBE was made of).
    pub heap_reserve: u32,
    /// Heap bytes committed.
    pub heap_commit: u32,
    /// Base address of the original PE.
    pub pe_base: u32,
    /// Image size of the original PE.
    pub pe_image_size: u32,
    /// Checksum of the original PE.
    pub pe_checksum: u32,
    /// Link time of the original PE.
    pub pe_timestamp: u32,
    /// Address of the debug path name string.
    pub debug_pathname_addr: u32,
    /// Address of the debug file name string.
    pub debug_filename_addr: u32,
    /// Address of the debug file name as UTF-16.
    pub debug_unicode_filename_addr: u32,
    /// Address of the import directory for libraries other than the kernel
    /// (development builds: `xbdm.dll`), or 0.
    pub import_dir_addr: u32,
    /// Address of the library version table.
    pub libraries_addr: u32,
    /// Address of the kernel's entry in the library version table.
    pub kernel_library_addr: u32,
    /// Address of XAPI's entry in the library version table.
    pub xapi_library_addr: u32,
    /// Address of the logo bitmap.
    pub logo_addr: u32,
    /// Size of the logo bitmap.
    pub logo_size: u32,
    /// The certificate, when the headers hold it.
    pub certificate: Option<Certificate>,
    /// The section headers, in file order.
    pub sections: Vec<Section>,
    /// The library versions.
    pub libraries: Vec<LibraryVersion>,
}

fn bad(what: impl Into<String>) -> Error {
    Error::Xbe(what.into())
}

/// A NUL-terminated string of single bytes at `off`, at most `max` of them.
fn c_string(b: &[u8], off: usize, max: usize) -> String {
    b.get(off..)
        .unwrap_or(&[])
        .iter()
        .take(max)
        .take_while(|&&c| c != 0)
        .map(|&c| char::from(c))
        .collect()
}

impl Xbe {
    /// Parses the headers of an XBE: `headers` is the start of the file,
    /// ideally its first `headers_size` bytes (what is mapped at the base).
    /// Tables that lie outside the bytes given are an error.
    pub fn parse(headers: &[u8]) -> Result<Self> {
        if headers.len() < HEADER_MIN {
            return Err(bad(format!("{} bytes are too few for the header", headers.len())));
        }
        // Inside the fixed header, which the check above covers.
        let f = |off: usize| le32(headers, off).unwrap_or(0);
        if f(XO_MAGIC) != MAGIC {
            return Err(bad("no XBEH magic"));
        }
        let base = f(XO_BASE);
        let headers_size = f(XO_SIZEOF_HEADERS);
        let image_size = f(XO_SIZEOF_IMAGE);
        if (headers_size as usize) < HEADER_MIN || base % PAGE_SIZE != 0 {
            return Err(bad(format!("bad header (base {base:08x}, headers {headers_size})")));
        }
        let extent = u64::from(image_size.max(headers_size));
        if base < BASE_MIN || u64::from(base) + extent > USER_END {
            return Err(bad(format!(
                "an image of {} KiB at {base:08x} is not in the title's address space",
                extent >> 10
            )));
        }

        // The keys: whichever puts the entry point inside the image.
        let (enc_entry, enc_thunk) = (f(XO_ENTRY), f(XO_KERNEL_THUNK));
        let inside =
            |va: u32| va >= base && u64::from(va) < u64::from(base) + u64::from(image_size);
        let (entry_point, kernel_thunk_addr, encoding) = if inside(enc_entry ^ ENTRY_KEY_RETAIL) {
            (enc_entry ^ ENTRY_KEY_RETAIL, enc_thunk ^ THUNK_KEY_RETAIL, Encoding::Retail)
        } else if inside(enc_entry ^ ENTRY_KEY_DEBUG) {
            (enc_entry ^ ENTRY_KEY_DEBUG, enc_thunk ^ THUNK_KEY_DEBUG, Encoding::Debug)
        } else if inside(enc_entry) {
            (enc_entry, enc_thunk, Encoding::Plain)
        } else {
            return Err(bad("the entry point is outside the image under every key"));
        };

        // The bytes of the headers that are both declared and given.
        let avail = headers.len().min(headers_size as usize);
        let headers = &headers[..avail];
        // Maps the header range [va, va + len) to an offset in `headers`.
        let hva = |va: u32, len: u64| -> Option<usize> {
            let off = va.checked_sub(base)? as usize;
            (off <= avail && len <= (avail - off) as u64).then_some(off)
        };

        let certificate_addr = f(XO_CERT_ADDR);
        let certificate = hva(certificate_addr, u64::from(CERT_PARSED)).and_then(|off| {
            let c = &headers[off..off + CERT_PARSED as usize];
            let name: Vec<u16> =
                (0..40).filter_map(|i| le16(c, 0x0C + i * 2)).take_while(|&u| u != 0).collect();
            let mut alt_title_ids = [0u32; 16];
            for (i, id) in alt_title_ids.iter_mut().enumerate() {
                *id = le32(c, 0x5C + i * 4)?;
            }
            Some(Certificate {
                size: le32(c, 0x00)?,
                timestamp: le32(c, 0x04)?,
                title_id: le32(c, 0x08)?,
                title_name: String::from_utf16_lossy(&name),
                alt_title_ids,
                allowed_media: le32(c, 0x9C)?,
                game_region: le32(c, 0xA0)?,
                game_ratings: le32(c, 0xA4)?,
                disc_number: le32(c, 0xA8)?,
                version: le32(c, 0xAC)?,
                lan_key: c.get(0xB0..0xC0)?.try_into().ok()?,
                signature_key: c.get(0xC0..0xD0)?.try_into().ok()?,
            })
        });

        let (nsections, sections_addr) = (f(XO_NSECTIONS), f(XO_SECTIONS_ADDR));
        let table = hva(sections_addr, u64::from(nsections) * u64::from(SECTION_HEADER_SIZE))
            .ok_or_else(|| {
                bad(format!(
                    "{nsections} section headers at {sections_addr:08x} are outside the headers"
                ))
            })?;
        let mut sections = Vec::with_capacity(nsections as usize);
        let (table, _) = headers[table..].as_chunks::<{ SECTION_HEADER_SIZE as usize }>();
        for s in table.iter().take(nsections as usize) {
            let g = |off: usize| le32(s, off).unwrap_or(0);
            let name = hva(g(0x14), 1).map(|off| c_string(headers, off, 255)).unwrap_or_default();
            let mut digest = [0u8; 20];
            digest.copy_from_slice(&s[0x24..0x38]);
            sections.push(Section {
                name,
                flags: g(0x00),
                virtual_addr: g(0x04),
                virtual_size: g(0x08),
                raw_addr: g(0x0C),
                raw_size: g(0x10),
                head_shared_ref_addr: g(0x1C),
                tail_shared_ref_addr: g(0x20),
                digest,
            });
        }

        let (nlibs, libraries_addr) = (f(XO_NLIBS), f(XO_LIBS_ADDR));
        let table =
            hva(libraries_addr, u64::from(nlibs) * u64::from(LIB_SIZE)).ok_or_else(|| {
                bad(format!(
                    "{nlibs} library versions at {libraries_addr:08x} are outside the headers"
                ))
            })?;
        let mut libraries = Vec::with_capacity(nlibs as usize);
        let (table, _) = headers[table..].as_chunks::<{ LIB_SIZE as usize }>();
        for l in table.iter().take(nlibs as usize) {
            let w = |off: usize| le16(l, off).unwrap_or(0);
            let flags = w(14);
            libraries.push(LibraryVersion {
                name: c_string(l, 0, 8),
                major: w(8),
                minor: w(10),
                build: w(12),
                qfe: flags & 0x1FFF,
                approved: ((flags >> 13) & 3) as u8,
                debug_build: flags & 0x8000 != 0,
            });
        }

        Ok(Self {
            headers: headers.to_vec(),
            base,
            headers_size,
            image_size,
            image_header_size: f(XO_SIZEOF_IMAGE_HEADER),
            timestamp: f(XO_TIMEDATE),
            certificate_addr,
            sections_addr,
            init_flags: f(XO_INIT_FLAGS),
            entry_point,
            kernel_thunk_addr,
            encoding,
            tls_addr: f(XO_TLS_ADDR),
            stack_commit: f(XO_STACK_COMMIT),
            heap_reserve: f(XO_HEAP_RESERVE),
            heap_commit: f(XO_HEAP_COMMIT),
            pe_base: f(XO_PE_BASE),
            pe_image_size: f(XO_PE_SIZEOF_IMAGE),
            pe_checksum: f(XO_PE_CHECKSUM),
            pe_timestamp: f(XO_PE_TIMEDATE),
            debug_pathname_addr: f(XO_DEBUG_PATHNAME),
            debug_filename_addr: f(XO_DEBUG_FILENAME),
            debug_unicode_filename_addr: f(XO_DEBUG_UNICODE_FILENAME),
            import_dir_addr: f(XO_IMPORT_DIR),
            libraries_addr,
            kernel_library_addr: f(XO_KERNEL_LIB_ADDR),
            xapi_library_addr: f(XO_XAPI_LIB_ADDR),
            logo_addr: f(XO_LOGO_ADDR),
            logo_size: f(XO_LOGO_SIZE),
            certificate,
            sections,
            libraries,
        })
    }

    /// Reads and parses the headers of the XBE file `file`.
    ///
    /// The headers are loaded into RAM whole, so a size above the most RAM a
    /// console has is refused before anything that large is allocated.
    pub fn read(file: &dyn BlockSource) -> Result<Self> {
        let mut probe = [0u8; HEADER_MIN];
        if file.len() < HEADER_MIN as u64 {
            return Err(bad(format!("a file of {} bytes is too short", file.len())));
        }
        file.read_at(0, &mut probe)?;
        if le32(&probe, XO_MAGIC) != Some(MAGIC) {
            return Err(bad("no XBEH magic"));
        }
        let headers_size = le32(&probe, XO_SIZEOF_HEADERS).unwrap_or(0);
        if (headers_size as usize) < HEADER_MIN
            || u64::from(headers_size) > file.len()
            || headers_size > RAM_DEBUG
        {
            return Err(bad(format!(
                "headers of {headers_size} bytes in a file of {}",
                file.len()
            )));
        }
        let mut headers = vec![0u8; headers_size as usize];
        file.read_at(0, &mut headers)?;
        Self::parse(&headers)
    }

    /// The header bytes the parser was given, at most `headers_size` of them:
    /// what a loader maps at `base`.
    pub fn headers(&self) -> &[u8] {
        &self.headers
    }

    /// The title id, or 0 when the headers hold no certificate.
    pub fn title_id(&self) -> u32 {
        self.certificate.as_ref().map_or(0, |c| c.title_id)
    }

    /// Bytes of address space the image takes from `base`, in whole pages.
    pub fn image_span(&self) -> u32 {
        // At most USER_END: parse checked the unrounded extent.
        align_up(self.image_size.max(self.headers_size), PAGE_SIZE)
    }

    /// Address of section `index`'s load count in the loaded headers.
    pub fn section_refcount_addr(&self, index: usize) -> Option<u32> {
        (index < self.sections.len()).then(|| {
            self.sections_addr
                .wrapping_add(index as u32 * SECTION_HEADER_SIZE)
                .wrapping_add(SECTION_REFCOUNT_OFFSET)
        })
    }

    /// The section named `name`, compared exactly.
    pub fn find_section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }

    /// The section whose memory holds `va`.
    pub fn section_at(&self, va: u32) -> Option<&Section> {
        self.sections.iter().find(|s| s.contains(va))
    }

    /// The library named `name`, compared exactly.
    pub fn find_library(&self, name: &str) -> Option<&LibraryVersion> {
        self.libraries.iter().find(|l| l.name == name)
    }

    /// The headers as a segment.
    pub fn header_segment(&self) -> Segment {
        Segment {
            section: None,
            virtual_addr: self.base,
            virtual_size: self.headers_size,
            file_offset: 0,
            file_size: self.headers_size,
            flags: SECTION_WRITABLE | SECTION_PRELOAD,
        }
    }

    /// Section `index` as a segment. A section that is not wholly inside the
    /// image is an error.
    pub fn section_segment(&self, index: usize) -> Result<Segment> {
        let s = self.sections.get(index).ok_or_else(|| bad(format!("no section {index}")))?;
        let end = u64::from(self.base) + u64::from(self.image_span());
        if s.virtual_addr < self.base || u64::from(s.virtual_addr) + u64::from(s.virtual_size) > end
        {
            return Err(bad(format!("section {} is outside the image", s.name)));
        }
        Ok(Segment {
            section: Some(index),
            virtual_addr: s.virtual_addr,
            virtual_size: s.virtual_size,
            file_offset: s.raw_addr,
            file_size: s.file_size(),
            flags: s.flags,
        })
    }

    /// The image laid out for a loader: the headers, then the sections asked
    /// for, in file order. Each names where it goes, how much memory it takes
    /// and where its bytes are in the file ([`Self::read_segment`] fetches
    /// them). Segments may share a page with a neighbour but not bytes.
    pub fn segments(&self, which: Sections) -> Result<Vec<Segment>> {
        let mut out = vec![self.header_segment()];
        for (i, s) in self.sections.iter().enumerate() {
            if which == Sections::All || s.is_preload() {
                out.push(self.section_segment(i)?);
            }
        }
        Ok(out)
    }

    /// Reads a segment's bytes from `file` straight into the memory a loader
    /// gives it. `dst` is the segment's memory, `virtual_size` bytes: the
    /// file's bytes go to its start and the rest is zeroed.
    pub fn read_segment(
        &self,
        file: &dyn BlockSource,
        segment: &Segment,
        dst: &mut [u8],
    ) -> Result<()> {
        if dst.len() as u64 != u64::from(segment.virtual_size)
            || segment.file_size > segment.virtual_size
        {
            return Err(bad("a segment's memory is not its virtual size"));
        }
        let (data, zero) = dst.split_at_mut(segment.file_size as usize);
        file.read_at(u64::from(segment.file_offset), data)?;
        zero.fill(0);
        Ok(())
    }

    /// Calls `sink` with each segment asked for and its bytes from the file
    /// (`file_size` of them; the loader zeroes the rest of `virtual_size`).
    /// Stops at the first error, the sink's included.
    pub fn for_each_segment(
        &self,
        file: &dyn BlockSource,
        which: Sections,
        mut sink: impl FnMut(&Segment, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let mut buf = Vec::new();
        for seg in self.segments(which)? {
            // Checked before the buffer grows to a size the file only claims.
            crate::block::check_range(
                u64::from(seg.file_offset),
                seg.file_size as usize,
                file.len(),
            )?;
            buf.resize(seg.file_size as usize, 0);
            file.read_at(u64::from(seg.file_offset), &mut buf)?;
            sink(&seg, &buf)?;
        }
        Ok(())
    }

    /// True when `[va, va + len)` lies in the headers or in one section's
    /// memory.
    pub fn is_mapped(&self, va: u32, len: u32) -> bool {
        let within = |start: u32, size: u32| {
            va >= start && u64::from(va - start) + u64::from(len) <= u64::from(size)
        };
        within(self.base, self.headers_size)
            || self.sections.iter().any(|s| within(s.virtual_addr, s.virtual_size))
    }

    /// Reads the image's bytes at `va` as they are once every section is
    /// loaded, from the file: header and section bytes where the range meets
    /// them, zeros elsewhere (a section's memory past its file bytes, gaps).
    pub fn read_image(&self, file: &dyn BlockSource, va: u32, buf: &mut [u8]) -> Result<()> {
        buf.fill(0);
        let (lo, hi) = (u64::from(va), u64::from(va) + buf.len() as u64);
        let pieces = std::iter::once((self.base, self.headers_size, 0u32))
            .chain(self.sections.iter().map(|s| (s.virtual_addr, s.file_size(), s.raw_addr)));
        for (start, size, file_off) in pieces {
            let (a, b) = (lo.max(u64::from(start)), hi.min(u64::from(start) + u64::from(size)));
            if a < b {
                let dst = &mut buf[(a - lo) as usize..(b - lo) as usize];
                file.read_at(u64::from(file_off) + (a - u64::from(start)), dst)?;
            }
        }
        Ok(())
    }

    /// Reads the kernel import list through `read`, which fills a buffer with
    /// the loaded image's bytes at a virtual address (an error when nothing is
    /// mapped there).
    ///
    /// The thunk table is a run of `0x8000_0000 | ordinal` slots ended by a
    /// zero. A slot that is not an ordinal import, or no end within
    /// [`KERNEL_IMPORTS_MAX`] slots, is an error.
    pub fn kernel_imports_with(
        &self,
        mut read: impl FnMut(u32, &mut [u8]) -> Result<()>,
    ) -> Result<Vec<KernelImport>> {
        let mut imports = Vec::new();
        let mut slot = self.kernel_thunk_addr;
        loop {
            let mut raw = [0u8; 4];
            read(slot, &mut raw)?;
            let v = u32::from_le_bytes(raw);
            if v == 0 {
                return Ok(imports);
            }
            if v & 0x8000_0000 == 0 {
                return Err(bad(format!("thunk {slot:08x} is not an ordinal import ({v:08x})")));
            }
            if imports.len() == KERNEL_IMPORTS_MAX {
                return Err(bad("the kernel thunk table has no end"));
            }
            imports.push(KernelImport { slot, ordinal: v & 0x7FFF_FFFF });
            slot = slot
                .checked_add(4)
                .ok_or_else(|| bad("the kernel thunk table runs off the address space"))?;
        }
    }

    /// Reads the kernel import list from the file. The table must lie in the
    /// headers or a section.
    pub fn kernel_imports(&self, file: &dyn BlockSource) -> Result<Vec<KernelImport>> {
        // The table at its longest, read once.
        let start = self.kernel_thunk_addr;
        let mut window = vec![0u8; (KERNEL_IMPORTS_MAX + 1) * 4];
        self.read_image(file, start, &mut window)?;
        self.kernel_imports_with(|va, buf| {
            if !self.is_mapped(va, buf.len() as u32) {
                return Err(bad(format!("the kernel thunk table at {va:08x} is not in the image")));
            }
            let off = (va - start) as usize;
            let src = window
                .get(off..off + buf.len())
                .ok_or_else(|| bad("the kernel thunk table has no end"))?;
            buf.copy_from_slice(src);
            Ok(())
        })
    }

    /// Reads the TLS directory through `read` (see
    /// [`Self::kernel_imports_with`]). `None` when the image has none.
    pub fn tls_directory_with(
        &self,
        mut read: impl FnMut(u32, &mut [u8]) -> Result<()>,
    ) -> Result<Option<TlsDirectory>> {
        if self.tls_addr == 0 {
            return Ok(None);
        }
        let mut raw = [0u8; 24];
        read(self.tls_addr, &mut raw)?;
        let f = |off: usize| le32(&raw, off).unwrap_or(0);
        Ok(Some(TlsDirectory {
            raw_data_start: f(0),
            raw_data_end: f(4),
            index_addr: f(8),
            callbacks_addr: f(12),
            zero_fill_size: f(16),
            characteristics: f(20),
        }))
    }

    /// Reads the TLS directory from the file. It must lie in the headers or a
    /// section.
    pub fn tls_directory(&self, file: &dyn BlockSource) -> Result<Option<TlsDirectory>> {
        self.tls_directory_with(|va, buf| {
            if !self.is_mapped(va, buf.len() as u32) {
                return Err(bad(format!("the TLS directory at {va:08x} is not in the image")));
            }
            self.read_image(file, va, buf)
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::MemSource;
    use crate::testutil::Lcg;

    pub(crate) const BASE: u32 = 0x0001_0000;
    pub(crate) const ENTRY: u32 = 0x0001_1234;
    pub(crate) const THUNK: u32 = 0x0001_3040;
    pub(crate) const TLS: u32 = 0x0001_3400;
    pub(crate) const ORDINALS: [u32; 7] = [294, 277, 1, 129, 49, 378, 16];
    const HEADERS: u32 = 0x1000;
    const IMAGE: u32 = 0x9000;

    fn put32(b: &mut [u8], off: usize, v: u32) {
        b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// (name, flags, va, vsize, raw offset, raw size)
    const SECTIONS: [(&str, u32, u32, u32, u32, u32); 4] = [
        (".text", SECTION_PRELOAD | SECTION_EXECUTABLE, 0x0001_1000, 0x1F00, 0x1000, 0x1F00),
        // Shares its first page with .text's last.
        (".rdata", SECTION_PRELOAD, 0x0001_2F00, 0x0900, 0x3000, 0x0900),
        // Memory past its file bytes.
        (".data", SECTION_PRELOAD | SECTION_WRITABLE, 0x0001_4000, 0x3000, 0x4000, 0x0800),
        // Loaded on demand; a name longer than eight characters.
        ("$$XTIMAGE", SECTION_INSERTED_FILE, 0x0001_7000, 0x1800, 0x5000, 0x1800),
    ];

    /// A whole XBE file with the keys of `encoding`.
    pub(crate) fn build(encoding: Encoding) -> Vec<u8> {
        let mut rng = Lcg(5);
        let mut f: Vec<u8> = (0..0x6800).map(|_| rng.next() as u8).collect();
        f[..HEADERS as usize].fill(0);
        put32(&mut f, XO_MAGIC, MAGIC);
        put32(&mut f, XO_BASE, BASE);
        put32(&mut f, XO_SIZEOF_HEADERS, HEADERS);
        put32(&mut f, XO_SIZEOF_IMAGE, IMAGE);
        put32(&mut f, XO_SIZEOF_IMAGE_HEADER, HEADER_MIN as u32);
        put32(&mut f, XO_TIMEDATE, 0x3B9A_CA00);
        let (ek, tk) = match encoding {
            Encoding::Retail => (ENTRY_KEY_RETAIL, THUNK_KEY_RETAIL),
            Encoding::Debug => (ENTRY_KEY_DEBUG, THUNK_KEY_DEBUG),
            Encoding::Plain => (0, 0),
        };
        put32(&mut f, XO_ENTRY, ENTRY ^ ek);
        put32(&mut f, XO_KERNEL_THUNK, THUNK ^ tk);
        put32(&mut f, XO_TLS_ADDR, TLS);
        put32(&mut f, XO_STACK_COMMIT, 0x10000);
        put32(&mut f, XO_IMPORT_DIR, 0);

        // Certificate at 0x178.
        let cert = 0x178usize;
        put32(&mut f, XO_CERT_ADDR, BASE + cert as u32);
        put32(&mut f, cert, 0x1D0);
        put32(&mut f, cert + 0x04, 0x3B9A_C9FF);
        put32(&mut f, cert + 0x08, 0x5445_0001);
        for (i, u) in "Tëst Title".encode_utf16().enumerate() {
            f[cert + 0x0C + i * 2..][..2].copy_from_slice(&u.to_le_bytes());
        }
        put32(&mut f, cert + 0x5C, 0x5445_0002);
        put32(&mut f, cert + 0x9C, 0x0000_0002);
        put32(&mut f, cert + 0xA0, 0x0000_0007);
        put32(&mut f, cert + 0xA4, 3);
        put32(&mut f, cert + 0xA8, 1);
        put32(&mut f, cert + 0xAC, 9);
        for i in 0..16 {
            f[cert + 0xB0 + i] = 0x10 + i as u8;
            f[cert + 0xC0 + i] = 0x80 + i as u8;
        }

        // Section headers at 0x400, their names at 0x600.
        let (table, mut names) = (0x400usize, 0x600usize);
        put32(&mut f, XO_NSECTIONS, SECTIONS.len() as u32);
        put32(&mut f, XO_SECTIONS_ADDR, BASE + table as u32);
        for (i, (name, flags, va, vsize, raw, raw_size)) in SECTIONS.into_iter().enumerate() {
            let s = table + i * SECTION_HEADER_SIZE as usize;
            put32(&mut f, s, flags);
            put32(&mut f, s + 0x04, va);
            put32(&mut f, s + 0x08, vsize);
            put32(&mut f, s + 0x0C, raw);
            put32(&mut f, s + 0x10, raw_size);
            put32(&mut f, s + 0x14, BASE + names as u32);
            f[s + 0x24..s + 0x38].fill(i as u8 + 1);
            f[names..names + name.len()].copy_from_slice(name.as_bytes());
            names += name.len() + 1;
        }

        // Library versions at 0x700.
        let libs = 0x700usize;
        put32(&mut f, XO_NLIBS, 2);
        put32(&mut f, XO_LIBS_ADDR, BASE + libs as u32);
        put32(&mut f, XO_KERNEL_LIB_ADDR, BASE + libs as u32);
        for (i, (name, build, flags)) in
            [("XAPILIB\0", 5849u16, 0x4001u16), ("D3D8LTCG", 5849, 0x8000)].into_iter().enumerate()
        {
            let l = libs + i * 16;
            f[l..l + 8].copy_from_slice(name.as_bytes());
            f[l + 8..l + 10].copy_from_slice(&1u16.to_le_bytes());
            f[l + 12..l + 14].copy_from_slice(&build.to_le_bytes());
            f[l + 14..l + 16].copy_from_slice(&flags.to_le_bytes());
        }

        // In .rdata (file 0x3000 = va 0x12F00): the thunk table and the TLS
        // directory.
        let at = |va: u32| (va - 0x0001_2F00 + 0x3000) as usize;
        for (i, o) in ORDINALS.into_iter().enumerate() {
            put32(&mut f, at(THUNK) + i * 4, 0x8000_0000 | o);
        }
        put32(&mut f, at(THUNK) + ORDINALS.len() * 4, 0);
        for (i, v) in [0x0001_4000u32, 0x0001_4100, 0x0001_4200, 0, 0x40, 0].into_iter().enumerate()
        {
            put32(&mut f, at(TLS) + i * 4, v);
        }
        f
    }

    /// The image as a loader would place it: memory from `BASE`, filled by
    /// the segments.
    fn load(xbe: &Xbe, file: &MemSource, which: Sections) -> Vec<u8> {
        let mut mem = vec![0u8; xbe.image_span() as usize];
        for seg in xbe.segments(which).unwrap() {
            let at = (seg.virtual_addr - xbe.base) as usize;
            xbe.read_segment(file, &seg, &mut mem[at..at + seg.virtual_size as usize]).unwrap();
        }
        mem
    }

    #[test]
    fn the_header_fields_come_back_as_they_were_written() {
        let file = build(Encoding::Retail);
        let xbe = Xbe::read(&MemSource(file.clone())).unwrap();
        assert_eq!((xbe.base, xbe.headers_size, xbe.image_size), (BASE, HEADERS, IMAGE));
        assert_eq!(xbe.image_span(), IMAGE);
        assert_eq!(xbe.headers(), &file[..HEADERS as usize]);
        assert_eq!((xbe.tls_addr, xbe.stack_commit, xbe.timestamp), (TLS, 0x10000, 0x3B9A_CA00));
        let c = xbe.certificate.as_ref().unwrap();
        assert_eq!((c.size, c.title_id, xbe.title_id()), (0x1D0, 0x5445_0001, 0x5445_0001));
        assert_eq!(c.title_name, "Tëst Title");
        assert_eq!((c.alt_title_ids[0], c.alt_title_ids[1]), (0x5445_0002, 0));
        assert_eq!(
            (c.allowed_media, c.game_region, c.game_ratings, c.disc_number, c.version),
            (2, 7, 3, 1, 9)
        );
        assert_eq!(
            (c.lan_key[0], c.lan_key[15], c.signature_key[0], c.signature_key[15]),
            (0x10, 0x1F, 0x80, 0x8F)
        );
        assert_eq!(xbe.sections.len(), SECTIONS.len());
        for (i, (s, want)) in xbe.sections.iter().zip(SECTIONS).enumerate() {
            assert_eq!(
                (s.name.as_str(), s.flags, s.virtual_addr, s.virtual_size, s.raw_addr, s.raw_size),
                want
            );
            assert_eq!(s.digest, [i as u8 + 1; 20]);
            assert_eq!(xbe.find_section(want.0), Some(s));
            assert_eq!(xbe.section_at(want.2 + want.3 - 1), Some(s));
            assert_eq!(xbe.section_refcount_addr(i), Some(BASE + 0x400 + i as u32 * 0x38 + 0x18));
        }
        assert_eq!(xbe.section_at(BASE), None);
        assert_eq!(xbe.section_refcount_addr(SECTIONS.len()), None);
        let x = xbe.find_library("XAPILIB").unwrap();
        assert_eq!((x.major, x.build, x.qfe, x.approved, x.debug_build), (1, 5849, 1, 2, false));
        assert!(xbe.find_library("D3D8LTCG").unwrap().debug_build);
        assert!(xbe.find_library("D3D8").is_none());
    }

    #[test]
    fn retail_debug_and_plain_headers_decode_to_the_same_addresses() {
        for encoding in [Encoding::Retail, Encoding::Debug, Encoding::Plain] {
            let xbe = Xbe::parse(&build(encoding)).unwrap();
            assert_eq!(
                (xbe.encoding, xbe.entry_point, xbe.kernel_thunk_addr),
                (encoding, ENTRY, THUNK)
            );
        }
    }

    #[test]
    fn the_layout_places_headers_and_preload_sections_and_zeroes_past_file_bytes() {
        let file = MemSource(build(Encoding::Retail));
        let xbe = Xbe::read(&file).unwrap();
        let segs = xbe.segments(Sections::Preload).unwrap();
        assert_eq!(segs.len(), 4);
        assert_eq!(segs[0], xbe.header_segment());
        assert_eq!(
            segs.iter().map(|s| s.section).collect::<Vec<_>>(),
            [None, Some(0), Some(1), Some(2)]
        );
        assert_eq!(xbe.segments(Sections::All).unwrap().len(), 5);
        let mem = load(&xbe, &file, Sections::Preload);
        assert_eq!(&mem[..HEADERS as usize], xbe.headers());
        for (name, flags, va, vsize, raw, raw_size) in SECTIONS {
            let got = &mem[(va - BASE) as usize..][..vsize as usize];
            if flags & SECTION_PRELOAD == 0 {
                assert!(got.iter().all(|&b| b == 0), "{name} is not loaded");
                continue;
            }
            assert!(
                got[..raw_size as usize] == file.0[raw as usize..][..raw_size as usize],
                "{name}"
            );
            assert!(got[raw_size as usize..].iter().all(|&b| b == 0), "{name}");
        }
        // The callback form delivers the same bytes.
        let mut mem2 = vec![0u8; mem.len()];
        xbe.for_each_segment(&file, Sections::Preload, |seg, bytes| {
            assert_eq!(bytes.len(), seg.file_size as usize);
            mem2[(seg.virtual_addr - BASE) as usize..][..bytes.len()].copy_from_slice(bytes);
            Ok(())
        })
        .unwrap();
        assert!(mem == mem2);
        // A sink's error stops the walk.
        let mut calls = 0;
        let r = xbe.for_each_segment(&file, Sections::All, |_, _| {
            calls += 1;
            Err(Error::NotFound)
        });
        assert!(matches!(r, Err(Error::NotFound)) && calls == 1);
    }

    #[test]
    fn the_file_view_of_the_image_agrees_with_the_loaded_image() {
        let file = MemSource(build(Encoding::Retail));
        let xbe = Xbe::read(&file).unwrap();
        let mem = load(&xbe, &file, Sections::All);
        let mut rng = Lcg(8);
        for _ in 0..500 {
            let off = (rng.next() as usize * 3) % mem.len();
            let len = (1 + rng.next() as usize % 9000).min(mem.len() - off);
            let mut buf = vec![0xAA; len];
            xbe.read_image(&file, BASE + off as u32, &mut buf).unwrap();
            assert!(buf == mem[off..off + len], "{len} bytes at +{off:#x}");
        }
        // Outside the image: zeros, without a wrap at the address space's end.
        let mut buf = [0xAAu8; 64];
        xbe.read_image(&file, 0xFFFF_FFF0, &mut buf).unwrap();
        assert_eq!(buf, [0; 64]);
    }

    #[test]
    fn the_import_list_read_from_the_file_agrees_with_the_loaded_image() {
        let file = MemSource(build(Encoding::Debug));
        let xbe = Xbe::read(&file).unwrap();
        let from_file = xbe.kernel_imports(&file).unwrap();
        let mem = load(&xbe, &file, Sections::Preload);
        let from_mem = xbe
            .kernel_imports_with(|va, buf| {
                let off = (va - BASE) as usize;
                buf.copy_from_slice(mem.get(off..off + buf.len()).ok_or(Error::NotFound)?);
                Ok(())
            })
            .unwrap();
        assert_eq!(from_file, from_mem);
        assert_eq!(from_file.iter().map(|i| i.ordinal).collect::<Vec<_>>(), ORDINALS);
        for (i, imp) in from_file.iter().enumerate() {
            assert_eq!(imp.slot, THUNK + i as u32 * 4);
        }
        let tls = xbe.tls_directory(&file).unwrap().unwrap();
        assert_eq!(
            tls,
            TlsDirectory {
                raw_data_start: 0x0001_4000,
                raw_data_end: 0x0001_4100,
                index_addr: 0x0001_4200,
                callbacks_addr: 0,
                zero_fill_size: 0x40,
                characteristics: 0
            }
        );
    }

    #[test]
    fn a_thunk_table_that_is_damaged_or_outside_the_image_is_an_error() {
        let good = build(Encoding::Retail);
        let at = (THUNK - 0x0001_2F00 + 0x3000) as usize;
        // A slot that is not an ordinal.
        let mut f = good.clone();
        put32(&mut f, at + 8, 0x0001_2345);
        assert!(matches!(
            Xbe::parse(&f).unwrap().kernel_imports(&MemSource(f)),
            Err(Error::Xbe(_))
        ));
        // No end before the section's.
        let mut f = good.clone();
        f[at..0x3900].fill(0x81);
        assert!(matches!(
            Xbe::parse(&f).unwrap().kernel_imports(&MemSource(f)),
            Err(Error::Xbe(_))
        ));
        // No end at all.
        let xbe = Xbe::parse(&good).unwrap();
        let r = xbe.kernel_imports_with(|_, buf| {
            buf.copy_from_slice(&0x8000_0001u32.to_le_bytes());
            Ok(())
        });
        assert!(matches!(r, Err(Error::Xbe(_))));
        // A table and a TLS directory in no section.
        for va in [0x0000_8000u32, 0x0001_8800, 0x7000_0000, 0xFFFF_FFFC] {
            let mut f = good.clone();
            put32(&mut f, XO_KERNEL_THUNK, va ^ THUNK_KEY_RETAIL);
            put32(&mut f, XO_TLS_ADDR, va);
            let xbe = Xbe::parse(&f).unwrap();
            assert!(
                matches!(xbe.kernel_imports(&MemSource(f.clone())), Err(Error::Xbe(_))),
                "{va:08x}"
            );
            assert!(matches!(xbe.tls_directory(&MemSource(f)), Err(Error::Xbe(_))), "{va:08x}");
        }
        // No TLS directory at all.
        let mut f = good.clone();
        put32(&mut f, XO_TLS_ADDR, 0);
        assert_eq!(Xbe::parse(&f).unwrap().tls_directory(&MemSource(f)).unwrap(), None);
    }

    #[test]
    fn headers_the_console_could_not_load_are_refused() {
        let good = build(Encoding::Retail);
        let cases: [(usize, u32); 12] = [
            (XO_MAGIC, 0x4845_4259),
            (XO_BASE, BASE + 0x800),
            (XO_BASE, 0x8000),
            (XO_BASE, 0x7FFE_0000),
            (XO_BASE, 0x8001_0000),
            (XO_SIZEOF_IMAGE, 0x7FFF_0000),
            (XO_SIZEOF_HEADERS, 0x100),
            (XO_ENTRY, 0x0002_0000 ^ ENTRY_KEY_RETAIL),
            (XO_NSECTIONS, 0x8000_0000),
            (XO_NSECTIONS, 80),
            (XO_SECTIONS_ADDR, 0x8000),
            (XO_LIBS_ADDR, BASE + HEADERS - 8),
        ];
        for (off, v) in cases {
            let mut f = good.clone();
            put32(&mut f, off, v);
            assert!(matches!(Xbe::parse(&f), Err(Error::Xbe(_))), "field {off:#x} = {v:#x}");
            assert!(Xbe::read(&MemSource(f)).is_err());
        }
        // Headers larger than the file.
        let mut f = good.clone();
        put32(&mut f, XO_SIZEOF_HEADERS, 0x7000);
        assert!(matches!(Xbe::read(&MemSource(f)), Err(Error::Xbe(_))));
        assert!(Xbe::read(&MemSource(Vec::new())).is_err());
    }

    #[test]
    fn a_section_outside_the_image_or_the_file_fails_when_it_is_asked_for() {
        let good = build(Encoding::Retail);
        let sec = |i: usize| 0x400 + i * 0x38;
        // The on-demand section outside the image: the boot layout is sound.
        let mut f = good.clone();
        put32(&mut f, sec(3) + 4, 0x0002_0000);
        let xbe = Xbe::parse(&f).unwrap();
        assert_eq!(xbe.segments(Sections::Preload).unwrap().len(), 4);
        assert!(matches!(xbe.segments(Sections::All), Err(Error::Xbe(_))));
        assert!(matches!(xbe.section_segment(3), Err(Error::Xbe(_))));
        assert!(matches!(xbe.section_segment(4), Err(Error::Xbe(_))));
        for (field, v) in [(4usize, 0x8000u32), (4, 0xFFFF_F000), (8, 0xFFFF_FFFF), (8, 0x8001)] {
            let mut f = good.clone();
            put32(&mut f, sec(1) + field, v);
            assert!(matches!(
                Xbe::parse(&f).unwrap().segments(Sections::Preload),
                Err(Error::Xbe(_))
            ));
        }
        // File bytes the file does not have.
        let mut f = good.clone();
        put32(&mut f, sec(0) + 0x0C, 0xFFFF_FF00);
        let xbe = Xbe::parse(&f).unwrap();
        let file = MemSource(f);
        let seg = xbe.section_segment(0).unwrap();
        let mut mem = vec![0u8; seg.virtual_size as usize];
        assert!(matches!(xbe.read_segment(&file, &seg, &mut mem), Err(Error::OutOfRange { .. })));
        assert!(matches!(xbe.read_segment(&file, &seg, &mut mem[1..]), Err(Error::Xbe(_))));
        assert!(matches!(
            xbe.for_each_segment(&file, Sections::Preload, |_, _| Ok(())),
            Err(Error::OutOfRange { .. })
        ));
        let mut buf = [0u8; 16];
        assert!(xbe.read_image(&file, 0x0001_1000, &mut buf).is_err());
    }

    #[test]
    fn truncated_and_damaged_headers_never_panic() {
        let good = build(Encoding::Retail);
        for len in (0..0x800).chain([0xFFF, 0x1000, 0x1001]) {
            if let Ok(xbe) = Xbe::parse(&good[..len]) {
                assert!(xbe.headers().len() <= len);
            }
            let _ = Xbe::read(&MemSource(good[..len].to_vec()));
        }
        let mut rng = Lcg(77);
        for _ in 0..3000 {
            let mut f = good.clone();
            for _ in 0..6 {
                // Mostly in the fixed header and the tables.
                let at = if rng.next().is_multiple_of(4) {
                    rng.next() as usize % f.len()
                } else {
                    0x100 + rng.next() as usize % 0x700
                };
                f[at] = rng.next() as u8;
            }
            let file = MemSource(f);
            let Ok(xbe) = Xbe::read(&file) else { continue };
            let _ = xbe.kernel_imports(&file);
            let _ = xbe.tls_directory(&file);
            let _ = xbe.segments(Sections::All);
            let _ = xbe.for_each_segment(&file, Sections::Preload, |_, _| Ok(()));
            let mut buf = [0u8; 300];
            let _ = xbe.read_image(&file, xbe.kernel_thunk_addr, &mut buf);
        }
    }
}
