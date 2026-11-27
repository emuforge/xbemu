//! Runtime library exports.
//!
//! ANSI_STRING / UNICODE_STRING: { USHORT Length, USHORT MaximumLength,
//! Buffer }, lengths in bytes. TIME_FIELDS: Year, Month, Day, Hour, Minute,
//! Second, Milliseconds, Weekday (CSHORT each). Times are FILETIMEs.
//!
//! The exports from [`register_nt`] on are not in the reference: they follow
//! the NT runtime library the Xbox kernel was built from, with the reference's
//! choices where the two meet (the code page is Latin-1, the case tables are
//! the reference's).
//!
//! Not here: the critical sections (ke.rs), RtlRaiseException and RtlUnwind
//! (seh.rs); RtlRaiseStatus, RtlCaptureContext, RtlCaptureStackBackTrace,
//! RtlGetCallersAddress, RtlWalkFrameChain and the RtlSprintf family, which
//! the reference does not have either.

use xb_types::{CONTIG_WINDOW, PAGE_SIZE};

use crate::exports;
use crate::gm::{Fault, KResult};
use crate::kernel::{Args, Kernel, STATUS_INVALID_PARAMETER, STATUS_NO_MEMORY, STATUS_SUCCESS};
use crate::platform::Platform;

const STATUS_BUFFER_OVERFLOW: u32 = 0x8000_0005;
const STATUS_BUFFER_TOO_SMALL: u32 = 0xC000_0023;
const STATUS_INVALID_PARAMETER_2: u32 = 0xC000_00F0;

/// The header [`str_alloc`] puts before a string's bytes.
const STR_HEADER: u32 = 16;
/// The longest C string measured (the reference measures without a bound; a
/// counted string holds 65535 bytes).
const CSTR_MAX: u32 = 0x0100_0000;

/// A counted string's descriptor.
#[derive(Clone, Copy)]
struct Str {
    len: u16,
    max: u16,
    buf: u32,
}

fn str_get(k: &Kernel, s: u32) -> KResult<Str> {
    Ok(Str { len: k.gm.r16(s)?, max: k.gm.r16(s + 2)?, buf: k.gm.r32(s + 4)? })
}

fn str_set(k: &Kernel, s: u32, len: u16, max: u16, buf: u32) -> KResult<()> {
    k.gm.w16(s, len)?;
    k.gm.w16(s + 2, max)?;
    k.gm.w32(s + 4, buf)
}

/// Kernel pool blocks for strings, freed by RtlFree*String (same header as
/// ExAllocatePool).
fn str_alloc(k: &mut Kernel, size: u32) -> u32 {
    let p = k.pool_alloc(size + STR_HEADER);
    if p == 0 {
        return 0;
    }
    k.gm.kw32(p, size);
    p + STR_HEADER
}

fn str_free(k: &mut Kernel, buf: u32) -> KResult<()> {
    if buf == 0 {
        return Ok(());
    }
    if buf < CONTIG_WINDOW + STR_HEADER {
        // not one of str_alloc's: the pool's free list must not take it
        log::warn!(target: "kernel", "rtl: string buffer {buf:08x} freed, not the kernel's");
        return Ok(());
    }
    let size = k.gm.r32(buf - STR_HEADER)?;
    k.pool_free(buf - STR_HEADER, size.wrapping_add(STR_HEADER));
    Ok(())
}

/// The fault of the first page of `[va, va + len)` that is not in, before
/// anything is changed.
fn probe(k: &Kernel, va: u32, len: u32, write: bool) -> KResult<()> {
    if len == 0 || k.gm.mapped_range(va, len) {
        return Ok(());
    }
    let mut at = va;
    loop {
        if !k.gm.mapped(at) {
            return Err(Fault { va: at, write });
        }
        at = (at & !(PAGE_SIZE - 1)).wrapping_add(PAGE_SIZE);
    }
}

fn wstr(k: &Kernel, va: u32, max_units: u32) -> KResult<Vec<u16>> {
    k.gm.space.read_wstr(va, max_units).map_err(|f| Fault { va: f.va, write: false })
}

/// `units` UTF-16 code units at `va`.
fn read_units(k: &Kernel, va: u32, units: u32) -> KResult<Vec<u16>> {
    let b = k.gm.read_vec(va, units * 2)?;
    Ok(b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect())
}

fn unit_bytes(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|u| u.to_le_bytes()).collect()
}

/// Unicode to the code page, Latin-1: what isn't in it becomes '?'.
fn narrow(c: u16) -> u8 {
    u8::try_from(c).unwrap_or(b'?')
}

/// The simple uppercase of the scripts titles write names in: ASCII,
/// Latin-1, Latin Extended-A, Greek, Cyrillic and the fullwidth forms;
/// everything else is its own uppercase.
pub(crate) fn upcase(c: u16) -> u16 {
    match c {
        0x61..=0x7A => c - 32,
        0xB5 => 0x39C,                      // micro sign: Greek capital mu
        0xE0..=0xFE if c != 0xF7 => c - 32, // Latin-1 letters (not the division sign)
        0xFF => 0x178,
        // Latin Extended-A: case pairs, upper even and lower odd
        0x100..=0x137 | 0x14A..=0x177 => c & !1,
        // upper odd, lower even
        0x139..=0x148 | 0x179..=0x17E => c - u16::from(c & 1 == 0),
        0x17F => 0x53,           // long s
        0x3C2 => 0x3A3,          // final sigma
        0x3B1..=0x3CB => c - 32, // Greek
        0x430..=0x44F => c - 32, // Cyrillic
        0x450..=0x45F => c - 80,
        0xFF41..=0xFF5A => c - 32, // fullwidth a-z
        _ => c,
    }
}

/// The simple lowercase of the same scripts as [`upcase`] (Unicode's
/// mapping; the reference has no downcase).
fn downcase(c: u16) -> u16 {
    match c {
        0x41..=0x5A => c + 32,
        0xC0..=0xDE if c != 0xD7 => c + 32, // not the multiplication sign
        0x130 => 0x69,                      // dotted capital I
        0x100..=0x137 | 0x14A..=0x177 => c | 1,
        0x139..=0x148 | 0x179..=0x17E => c + u16::from(c & 1 != 0),
        0x178 => 0xFF,
        0x391..=0x3A1 | 0x3A3..=0x3AB => c + 32,
        0x410..=0x42F => c + 32,
        0x400..=0x40F => c + 80,
        0xFF21..=0xFF3A => c + 32,
        _ => c,
    }
}

/// RtlInitAnsiString(DestinationString, SourceString)
fn rtl_init_ansi_string(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let len = if a[1] != 0 { k.gm.cstr(a[1], CSTR_MAX)?.len() as u16 } else { 0 };
    str_set(k, a[0], len, if a[1] != 0 { len.wrapping_add(1) } else { 0 }, a[1])?;
    Ok(0)
}

/// RtlInitUnicodeString(DestinationString, SourceString)
fn rtl_init_unicode_string(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    // counted in 32 bits: in 16, a string of 32768 characters never ended
    let n = if a[1] != 0 { wstr(k, a[1], CSTR_MAX)?.len() as u32 } else { 0 };
    let len = (n * 2) as u16;
    str_set(k, a[0], len, if a[1] != 0 { len.wrapping_add(2) } else { 0 }, a[1])?;
    Ok(0)
}

fn ascii_upper(c: u8) -> u8 {
    c.to_ascii_uppercase()
}

fn compare(k: &Kernel, s1: u32, s2: u32, nocase: bool) -> KResult<i32> {
    let (d1, d2) = (str_get(k, s1)?, str_get(k, s2)?);
    for i in 0..u32::from(d1.len.min(d2.len)) {
        let (mut c1, mut c2) = (k.gm.r8(d1.buf.wrapping_add(i))?, k.gm.r8(d2.buf.wrapping_add(i))?);
        if nocase {
            (c1, c2) = (ascii_upper(c1), ascii_upper(c2));
        }
        if c1 != c2 {
            return Ok(i32::from(c1) - i32::from(c2));
        }
    }
    Ok(i32::from(d1.len) - i32::from(d2.len))
}

/// RtlCompareString(String1, String2, CaseInSensitive) -> LONG
fn rtl_compare_string(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from(compare(k, a[0], a[1], a[2] & 0xFF != 0)? as u32))
}

/// RtlEqualString(String1, String2, CaseInSensitive) -> BOOLEAN
fn rtl_equal_string(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let same_len = k.gm.r16(a[0])? == k.gm.r16(a[1])?;
    Ok(u64::from(same_len && compare(k, a[0], a[1], a[2] & 0xFF != 0)? == 0))
}

/// RtlCompareMemoryUlong(Source, Length, Pattern) -> bytes that match
fn rtl_compare_memory_ulong(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let mut n = 0u32;
    while a[1] - n >= 4 && k.gm.r32(a[0].wrapping_add(n))? == a[2] {
        n += 4;
    }
    Ok(u64::from(n))
}

// The memory routines run as guest code (fast.rs); these are the same in
// the kernel, for a kernel configured without the fast paths.

/// Bytes from `va + n` to the end of its page, at most `left`.
fn page_run(va: u32, n: u32, left: u32) -> u32 {
    let at = va.wrapping_add(n);
    left.min(PAGE_SIZE - (at & (PAGE_SIZE - 1)))
}

/// RtlCompareMemory(Source1, Source2, Length) -> the bytes alike from the start
fn rtl_compare_memory(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let (mut x, mut y) = ([0u8; PAGE_SIZE as usize], [0u8; PAGE_SIZE as usize]);
    let mut n = 0u32;
    while n < a[2] {
        // a page of either at a time: nothing past the first difference is read
        let take = page_run(a[0], n, page_run(a[1], n, a[2] - n)) as usize;
        k.gm.read(a[0].wrapping_add(n), &mut x[..take])?;
        k.gm.read(a[1].wrapping_add(n), &mut y[..take])?;
        if let Some(i) = x[..take].iter().zip(&y[..take]).position(|(p, q)| p != q) {
            return Ok(u64::from(n) + i as u64);
        }
        n += take as u32;
    }
    Ok(u64::from(n))
}

/// RtlZeroMemory(Destination, Length)
fn rtl_zero_memory(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    probe(k, a[0], a[1], true)?;
    k.gm.fill(a[0], 0, a[1])?;
    Ok(0)
}

/// RtlFillMemory(Destination, Length, Fill)
fn rtl_fill_memory(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    probe(k, a[0], a[1], true)?;
    k.gm.fill(a[0], a[2] as u8, a[1])?;
    Ok(0)
}

/// The most the block routines hold on the host at a time.
const CHUNK: u32 = 0x1_0000;

/// RtlFillMemoryUlong(Destination, Length, Pattern): Length / 4 words
fn rtl_fill_memory_ulong(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let len = a[1] & !3;
    probe(k, a[0], len, true)?;
    let pattern = a[2].to_le_bytes().repeat((len.min(CHUNK) / 4) as usize);
    let mut n = 0;
    while n < len {
        let take = (len - n).min(CHUNK);
        k.gm.write(a[0].wrapping_add(n), &pattern[..take as usize])?;
        n += take;
    }
    Ok(0)
}

/// RtlMoveMemory(Destination, Source, Length): the ranges may overlap
fn rtl_move_memory(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let (dst, src, len) = (a[0], a[1], a[2]);
    probe(k, src, len, false)?;
    probe(k, dst, len, true)?;
    // forward unless the destination begins inside the source
    let backward = dst.wrapping_sub(src) < len;
    let mut done = 0;
    while done < len {
        let take = (len - done).min(CHUNK);
        let off = if backward { len - done - take } else { done };
        k.gm.copy(dst.wrapping_add(off), src.wrapping_add(off), take)?;
        done += take;
    }
    Ok(0)
}

/// RtlLowerChar(Character): ASCII case only
fn rtl_lower_char(_k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from((a[0] as u8).to_ascii_lowercase()))
}

/// RtlUpperChar(Character): ASCII case only
fn rtl_upper_char(_k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from(ascii_upper(a[0] as u8)))
}

/// RtlUpcaseUnicodeChar(SourceCharacter) -> WCHAR (Midtown Madness 3, on its
/// save names)
fn rtl_upcase_unicode_char(_k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from(upcase(a[0] as u16)))
}

/// RtlUpcaseUnicodeString / RtlDowncaseUnicodeString: the source's whole
/// characters through `case`, into the destination's buffer or one
/// allocated for it.
fn recase_unicode_string(k: &mut Kernel, a: &Args, case: fn(u16) -> u16) -> KResult<u64> {
    let (dst, src) = (str_get(k, a[0])?, str_get(k, a[1])?);
    let n = src.len & !1;
    let text: Vec<u16> = read_units(k, src.buf, u32::from(n / 2))?.into_iter().map(case).collect();
    let (mut buf, mut max) = (dst.buf, dst.max);
    if a[2] & 0xFF != 0 {
        probe(k, a[0], 8, true)?;
        max = n;
        buf = str_alloc(k, u32::from(max));
        if buf == 0 {
            return Ok(u64::from(STATUS_NO_MEMORY));
        }
    } else if max < n {
        return Ok(u64::from(STATUS_BUFFER_OVERFLOW));
    }
    k.gm.write(buf, &unit_bytes(&text))?;
    str_set(k, a[0], n, max, buf)?;
    Ok(u64::from(STATUS_SUCCESS))
}

/// RtlUpcaseUnicodeString(DestinationString, SourceString, AllocateDestinationString) -> NTSTATUS
fn rtl_upcase_unicode_string(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    recase_unicode_string(k, a, upcase)
}

/// RtlUpcaseUnicodeToMultiByteN(MultiByteString, MaxBytesInMultiByteString,
/// BytesInMultiByteString, UnicodeString, BytesInUnicodeString) -> NTSTATUS:
/// the code page is Latin-1 (what isn't in it becomes '?').
fn rtl_upcase_unicode_to_multi_byte_n(
    k: &mut Kernel,
    _p: &mut dyn Platform,
    a: &Args,
) -> KResult<u64> {
    unicode_to_multi_byte(k, a, upcase)
}

fn unicode_to_multi_byte(k: &mut Kernel, a: &Args, case: fn(u16) -> u16) -> KResult<u64> {
    let n = (a[4] / 2).min(a[1]);
    let out: Vec<u8> = read_units(k, a[3], n)?.into_iter().map(|c| narrow(case(c))).collect();
    if a[2] != 0 {
        probe(k, a[2], 4, true)?;
    }
    k.gm.write(a[0], &out)?;
    if a[2] != 0 {
        k.gm.w32(a[2], n)?;
    }
    Ok(u64::from(STATUS_SUCCESS))
}

/// RtlUpperString(DestinationString, SourceString): as much of it as fits,
/// ASCII case
fn rtl_upper_string(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let (dst, src) = (str_get(k, a[0])?, str_get(k, a[1])?);
    let n = src.len.min(dst.max);
    let text: Vec<u8> =
        k.gm.read_vec(src.buf, u32::from(n))?.into_iter().map(ascii_upper).collect();
    k.gm.write(dst.buf, &text)?;
    k.gm.w16(a[0], n)?;
    Ok(0)
}

/// The body of RtlAnsiStringToUnicodeString: `text` widened into the string
/// at `dst`.
fn ansi_to_unicode(k: &mut Kernel, dst_va: u32, text: &[u8], allocate: bool) -> KResult<u32> {
    let dst = str_get(k, dst_va)?;
    let n = text.len() as u32;
    let (mut buf, mut max) = (dst.buf, u32::from(dst.max));
    if allocate {
        if n * 2 + 2 > 0xFFFF {
            // longer than a UNICODE_STRING holds: refused, as the kernel
            // does (the size wrapped to a few bytes and the characters
            // were written past them, over the pool)
            return Ok(STATUS_INVALID_PARAMETER_2);
        }
        probe(k, dst_va, 8, true)?;
        max = n * 2 + 2;
        buf = str_alloc(k, max);
        if buf == 0 {
            return Ok(STATUS_NO_MEMORY);
        }
    } else if max < n * 2 {
        return Ok(STATUS_BUFFER_OVERFLOW);
    }
    let mut wide: Vec<u16> = text.iter().map(|&c| u16::from(c)).collect();
    if max >= n * 2 + 2 {
        wide.push(0);
    }
    k.gm.write(buf, &unit_bytes(&wide))?;
    str_set(k, dst_va, (n * 2) as u16, max as u16, buf)?;
    Ok(STATUS_SUCCESS)
}

/// RtlAnsiStringToUnicodeString(DestinationString, SourceString, AllocateDestinationString)
fn rtl_ansi_string_to_unicode_string(
    k: &mut Kernel,
    _p: &mut dyn Platform,
    a: &Args,
) -> KResult<u64> {
    let src = str_get(k, a[1])?;
    let text = k.gm.read_vec(src.buf, u32::from(src.len))?;
    Ok(u64::from(ansi_to_unicode(k, a[0], &text, a[2] & 0xFF != 0)?))
}

/// RtlUnicodeStringToAnsiString(DestinationString, SourceString, AllocateDestinationString)
fn rtl_unicode_string_to_ansi_string(
    k: &mut Kernel,
    _p: &mut dyn Platform,
    a: &Args,
) -> KResult<u64> {
    let (dst, src) = (str_get(k, a[0])?, str_get(k, a[1])?);
    let n = src.len / 2;
    let mut text: Vec<u8> = read_units(k, src.buf, u32::from(n))?.into_iter().map(narrow).collect();
    let (mut buf, mut max) = (dst.buf, dst.max);
    if a[2] & 0xFF != 0 {
        probe(k, a[0], 8, true)?;
        max = n + 1;
        buf = str_alloc(k, u32::from(max));
        if buf == 0 {
            return Ok(u64::from(STATUS_NO_MEMORY));
        }
    } else if max < n {
        return Ok(u64::from(STATUS_BUFFER_OVERFLOW));
    }
    if max > n {
        text.push(0);
    }
    k.gm.write(buf, &text)?;
    str_set(k, a[0], n, max, buf)?;
    Ok(u64::from(STATUS_SUCCESS))
}

/// RtlFreeAnsiString(AnsiString) / RtlFreeUnicodeString(UnicodeString)
fn rtl_free_string(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let s = str_get(k, a[0])?;
    str_free(k, s.buf)?;
    str_set(k, a[0], 0, 0, 0)?;
    Ok(0)
}

/// RtlNtStatusToDosError(Status)
fn rtl_nt_status_to_dos_error(_k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from(status_to_dos(a[0])))
}

/// Every status the kernel returns (Windows' mapping).
const DOS_ERRORS: [(u32, u32); 35] = [
    (0x0000_0000, 0),
    (0x0000_0102, 258),
    (0x0000_0103, 997),
    (0x8000_0005, 234),
    (0x8000_0006, 18),
    (0xC000_0001, 31),
    (0xC000_0002, 1),
    (0xC000_0003, 87),
    (0xC000_0008, 6),
    (0xC000_000D, 87),
    (0xC000_000F, 2),
    (0xC000_0010, 1),
    (0xC000_0011, 38),
    (0xC000_0017, 8),
    (0xC000_0018, 487),
    (0xC000_0022, 5),
    (0xC000_0023, 122),
    (0xC000_0024, 6),
    (0xC000_002D, 487),
    (0xC000_0033, 123),
    (0xC000_0034, 2),
    (0xC000_0035, 183),
    (0xC000_003A, 3),
    (0xC000_0043, 32),
    (0xC000_0047, 298),
    (0xC000_0056, 5),
    (0xC000_007F, 112),
    (0xC000_009A, 1450),
    (0xC000_009C, 23),
    (0xC000_009F, 487),
    (0xC000_00A0, 487),
    (0xC000_00A2, 19),
    (0xC000_00BA, 5),
    (0xC000_0101, 145),
    (0xC000_0103, 267),
];

/// The Win32 error of an NT status.
pub(crate) fn status_to_dos(mut status: u32) -> u32 {
    // an HRESULT first, as Windows' (and the Xbox kernel's): a customer
    // code passes as it is, a Win32 error's (HRESULT_FROM_WIN32: XNet's
    // socket errors, WSAEWOULDBLOCK among them) is that error, an NT
    // status's (HRESULT_FROM_NT) is the status's
    if status & 0x2000_0000 != 0 {
        return status;
    }
    if status & 0xFFFF_0000 == 0x8007_0000 {
        return status & 0xFFFF;
    }
    if status & 0xF000_0000 == 0xD000_0000 {
        status &= 0xCFFF_FFFF;
    }
    DOS_ERRORS.iter().find(|m| m.0 == status).map_or(317, |m| m.1) // ERROR_MR_MID_NOT_FOUND
}

// ------------------------------------------------------------------- time

fn leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn year_days(y: i32) -> u64 {
    if leap(y) { 366 } else { 365 }
}

fn month_days(y: i32, m: usize) -> u64 {
    const DAYS: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    DAYS[m] + u64::from(m == 1 && leap(y))
}

/// RtlTimeToTimeFields(Time, TimeFields)
fn rtl_time_to_time_fields(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let ms_total = k.gm.r64(a[0])? / 10_000;
    let mut days = ms_total / 86_400_000;
    let ms_day = (ms_total % 86_400_000) as u32;
    let weekday = (days + 1) % 7; // 1601-01-01 was a Monday
    let mut y = 1601;
    while days >= year_days(y) {
        days -= year_days(y);
        y += 1;
    }
    let mut m = 0;
    while days >= month_days(y, m) {
        days -= month_days(y, m);
        m += 1;
    }
    let tf: [u16; 8] = [
        y as u16,
        m as u16 + 1,
        days as u16 + 1,
        (ms_day / 3_600_000) as u16,
        (ms_day / 60_000 % 60) as u16,
        (ms_day / 1000 % 60) as u16,
        (ms_day % 1000) as u16,
        weekday as u16,
    ];
    k.gm.write(a[1], &unit_bytes(&tf))?;
    Ok(0)
}

/// RtlTimeFieldsToTime(TimeFields, Time) -> BOOLEAN
fn rtl_time_fields_to_time(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let tf: Vec<i32> = read_units(k, a[0], 8)?.into_iter().map(|v| i32::from(v as i16)).collect();
    let (y, mon, day) = (tf[0], tf[1], tf[2]);
    // no such time (the kernel checks every field)
    if y < 1601
        || !(1..=12).contains(&mon)
        || day < 1
        || day as u64 > month_days(y, mon as usize - 1)
        || !(0..=23).contains(&tf[3])
        || !(0..=59).contains(&tf[4])
        || !(0..=59).contains(&tf[5])
        || !(0..=999).contains(&tf[6])
    {
        return Ok(0);
    }
    let mut days: u64 = (1601..y).map(year_days).sum();
    days += (0..mon as usize - 1).map(|m| month_days(y, m)).sum::<u64>();
    days += day as u64 - 1;
    let ms = days * 86_400_000
        + tf[3] as u64 * 3_600_000
        + tf[4] as u64 * 60_000
        + tf[5] as u64 * 1000
        + tf[6] as u64;
    k.gm.w64(a[1], ms * 10_000)?;
    Ok(1)
}

// ----------------------------------- the NT runtime, past the reference

/// The first differing characters' difference, or the lengths' (in bytes).
fn compare_unicode(k: &Kernel, s1: u32, s2: u32, nocase: bool) -> KResult<i32> {
    let (d1, d2) = (str_get(k, s1)?, str_get(k, s2)?);
    for i in 0..u32::from(d1.len.min(d2.len) / 2) {
        let (mut c1, mut c2) =
            (k.gm.r16(d1.buf.wrapping_add(2 * i))?, k.gm.r16(d2.buf.wrapping_add(2 * i))?);
        if nocase {
            (c1, c2) = (upcase(c1), upcase(c2));
        }
        if c1 != c2 {
            return Ok(i32::from(c1) - i32::from(c2));
        }
    }
    Ok(i32::from(d1.len) - i32::from(d2.len))
}

/// RtlCompareUnicodeString(String1, String2, CaseInSensitive) -> LONG
fn rtl_compare_unicode_string(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from(compare_unicode(k, a[0], a[1], a[2] & 0xFF != 0)? as u32))
}

/// RtlEqualUnicodeString(String1, String2, CaseInSensitive) -> BOOLEAN
fn rtl_equal_unicode_string(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let same_len = k.gm.r16(a[0])? == k.gm.r16(a[1])?;
    Ok(u64::from(same_len && compare_unicode(k, a[0], a[1], a[2] & 0xFF != 0)? == 0))
}

/// RtlCopyString(DestinationString, SourceString): as much as fits; no
/// source empties the destination.
fn rtl_copy_string(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    copy_string(k, a, false)
}

/// RtlCopyUnicodeString(DestinationString, SourceString): as much as fits,
/// terminated if there is room; no source empties the destination.
fn rtl_copy_unicode_string(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    copy_string(k, a, true)
}

fn copy_string(k: &mut Kernel, a: &Args, wide: bool) -> KResult<u64> {
    if a[1] == 0 {
        k.gm.w16(a[0], 0)?;
        return Ok(0);
    }
    let (dst, src) = (str_get(k, a[0])?, str_get(k, a[1])?);
    let n = src.len.min(dst.max);
    let mut text = k.gm.read_vec(src.buf, u32::from(n))?;
    if wide && n < dst.max {
        // the kernel stores a whole WCHAR there
        text.extend_from_slice(&[0, 0]);
    }
    k.gm.write(dst.buf, &text)?;
    k.gm.w16(a[0], n)?;
    Ok(0)
}

/// Appends `text` to the counted string at `dst_va`; a wide one is
/// terminated if there is room.
fn append(k: &mut Kernel, dst_va: u32, text: &[u8], wide: bool) -> KResult<u64> {
    if text.is_empty() {
        return Ok(u64::from(STATUS_SUCCESS));
    }
    let dst = str_get(k, dst_va)?;
    let len = u32::from(dst.len) + text.len() as u32;
    if len > u32::from(dst.max) {
        return Ok(u64::from(STATUS_BUFFER_TOO_SMALL));
    }
    let mut text = text.to_vec();
    if wide && len < u32::from(dst.max) {
        text.extend_from_slice(&[0, 0]);
    }
    k.gm.write(dst.buf.wrapping_add(u32::from(dst.len)), &text)?;
    k.gm.w16(dst_va, len as u16)?;
    Ok(u64::from(STATUS_SUCCESS))
}

/// RtlAppendStringToString(Destination, Source) -> NTSTATUS
fn rtl_append_string_to_string(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let text = k.gm.ansi_string(a[1])?;
    append(k, a[0], &text, false)
}

/// RtlAppendUnicodeStringToString(Destination, Source) -> NTSTATUS
fn rtl_append_unicode_string_to_string(
    k: &mut Kernel,
    _p: &mut dyn Platform,
    a: &Args,
) -> KResult<u64> {
    let src = str_get(k, a[1])?;
    let text = k.gm.read_vec(src.buf, u32::from(src.len))?;
    append(k, a[0], &text, true)
}

/// RtlAppendUnicodeToString(Destination, Source) -> NTSTATUS: Source is a
/// NUL-terminated wide string, or NULL for nothing.
fn rtl_append_unicode_to_string(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    if a[1] == 0 {
        return Ok(u64::from(STATUS_SUCCESS));
    }
    // as RtlInitUnicodeString counts it: the length is 16 bits of bytes
    let n = (wstr(k, a[1], CSTR_MAX)?.len() as u32 * 2) as u16;
    let text = k.gm.read_vec(a[1], u32::from(n))?;
    append(k, a[0], &text, true)
}

/// RtlCreateUnicodeString(DestinationString, SourceString) -> BOOLEAN: a
/// pool copy of the NUL-terminated string, for RtlFreeUnicodeString.
fn rtl_create_unicode_string(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let mut text = wstr(k, a[1], CSTR_MAX)?;
    text.push(0);
    let size = text.len() as u32 * 2;
    probe(k, a[0], 8, true)?;
    let buf = str_alloc(k, size);
    if buf == 0 {
        return Ok(0);
    }
    k.gm.kwrite(buf, &unit_bytes(&text));
    str_set(k, a[0], (size - 2) as u16, size as u16, buf)?;
    Ok(1)
}

/// RtlDowncaseUnicodeChar(SourceCharacter) -> WCHAR
fn rtl_downcase_unicode_char(_k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from(downcase(a[0] as u16)))
}

/// RtlDowncaseUnicodeString(DestinationString, SourceString, AllocateDestinationString) -> NTSTATUS
fn rtl_downcase_unicode_string(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    recase_unicode_string(k, a, downcase)
}

/// RtlUnicodeToMultiByteN(MultiByteString, MaxBytesInMultiByteString,
/// BytesInMultiByteString, UnicodeString, BytesInUnicodeString) -> NTSTATUS:
/// as many characters as fit, the count through the optional pointer.
fn rtl_unicode_to_multi_byte_n(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    unicode_to_multi_byte(k, a, |c| c)
}

/// RtlMultiByteToUnicodeN(UnicodeString, MaxBytesInUnicodeString,
/// BytesInUnicodeString, MultiByteString, BytesInMultiByteString) -> NTSTATUS
fn rtl_multi_byte_to_unicode_n(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let n = (a[1] / 2).min(a[4]);
    let wide: Vec<u16> = k.gm.read_vec(a[3], n)?.into_iter().map(u16::from).collect();
    if a[2] != 0 {
        probe(k, a[2], 4, true)?;
    }
    k.gm.write(a[0], &unit_bytes(&wide))?;
    if a[2] != 0 {
        k.gm.w32(a[2], n * 2)?;
    }
    Ok(u64::from(STATUS_SUCCESS))
}

/// RtlMultiByteToUnicodeSize(BytesInUnicodeString, MultiByteString,
/// BytesInMultiByteString) -> NTSTATUS: a byte is a character.
fn rtl_multi_byte_to_unicode_size(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    k.gm.w32(a[0], a[2].wrapping_mul(2))?;
    Ok(u64::from(STATUS_SUCCESS))
}

/// RtlUnicodeToMultiByteSize(BytesInMultiByteString, UnicodeString,
/// BytesInUnicodeString) -> NTSTATUS: a character is a byte.
fn rtl_unicode_to_multi_byte_size(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    k.gm.w32(a[0], a[2] / 2)?;
    Ok(u64::from(STATUS_SUCCESS))
}

/// The number `text` begins with, as RtlCharToInteger and
/// RtlUnicodeStringToInteger read one: blanks, a sign, with no base given a
/// 0x / 0o / 0b prefix (decimal without), then digits of the base up to the
/// first character that is not one. Overflow wraps. `None`: no such base.
fn parse_integer(text: &[u16], base: u32, blank: fn(u16) -> bool) -> Option<u32> {
    let mut s = text;
    while let [c, rest @ ..] = s
        && blank(*c)
    {
        s = rest;
    }
    let mut negative = false;
    if let [c @ (0x2B | 0x2D), rest @ ..] = s {
        negative = *c == 0x2D;
        s = rest;
    }
    let base = match base {
        0 => match s {
            [0x30, 0x78, rest @ ..] => {
                s = rest;
                16
            }
            [0x30, 0x6F, rest @ ..] => {
                s = rest;
                8
            }
            [0x30, 0x62, rest @ ..] => {
                s = rest;
                2
            }
            _ => 10,
        },
        2 | 8 | 10 | 16 => base,
        _ => return None,
    };
    let mut value = 0u32;
    for &c in s {
        let Some(digit) = char::from_u32(u32::from(c)).and_then(|c| c.to_digit(16)) else { break };
        if digit >= base {
            break;
        }
        value = value.wrapping_mul(base).wrapping_add(digit);
    }
    Some(if negative { value.wrapping_neg() } else { value })
}

/// RtlCharToInteger(String, Base, Value) -> NTSTATUS: Base 0 reads the
/// prefix; 2, 8, 10 and 16 are the others.
fn rtl_char_to_integer(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let text: Vec<u16> = k.gm.cstr(a[0], CSTR_MAX)?.into_iter().map(u16::from).collect();
    // the kernel's characters are signed: one past 0x7F is under the space too
    let Some(v) = parse_integer(&text, a[1], |c| c <= 0x20 || c >= 0x80) else {
        return Ok(u64::from(STATUS_INVALID_PARAMETER));
    };
    k.gm.w32(a[2], v)?;
    Ok(u64::from(STATUS_SUCCESS))
}

/// RtlUnicodeStringToInteger(String, Base, Value) -> NTSTATUS
fn rtl_unicode_string_to_integer(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let s = str_get(k, a[0])?;
    let text = read_units(k, s.buf, u32::from(s.len / 2))?;
    let Some(v) = parse_integer(&text, a[1], |c| c <= 0x20) else {
        return Ok(u64::from(STATUS_INVALID_PARAMETER));
    };
    k.gm.w32(a[2], v)?;
    Ok(u64::from(STATUS_SUCCESS))
}

/// The digits RtlIntegerToChar writes: `value` in `base` (0 is 10), padded
/// with zeros to `-length` characters for a negative `length`. Without a
/// terminator. The kernel's 32-character scratch bounds the padding.
fn integer_digits(mut value: u32, base: u32, length: i32) -> Result<Vec<u8>, u32> {
    let base = match base {
        0 => 10,
        2 | 8 | 10 | 16 => base,
        _ => return Err(STATUS_INVALID_PARAMETER),
    };
    let mut digits = Vec::with_capacity(32);
    loop {
        digits.push(b"0123456789ABCDEF"[(value % base) as usize]);
        value /= base;
        if value == 0 {
            break;
        }
    }
    let room = length.unsigned_abs() as usize;
    if length < 0 {
        while digits.len() < room.min(32) {
            digits.push(b'0');
        }
    }
    if digits.len() > room {
        return Err(STATUS_BUFFER_OVERFLOW);
    }
    digits.reverse();
    Ok(digits)
}

/// RtlIntegerToChar(Value, Base, OutputLength, String) -> NTSTATUS: the
/// digits, terminated if there is room; a negative OutputLength pads with
/// zeros to that many characters.
fn rtl_integer_to_char(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let length = a[2] as i32;
    let mut digits = match integer_digits(a[0], a[1], length) {
        Ok(d) => d,
        Err(status) => return Ok(u64::from(status)),
    };
    if digits.len() < length.unsigned_abs() as usize {
        digits.push(0);
    }
    k.gm.write(a[3], &digits)?;
    Ok(u64::from(STATUS_SUCCESS))
}

/// RtlIntegerToUnicodeString(Value, Base, String) -> NTSTATUS: through 16
/// characters, as the kernel's (a binary number longer than that overflows).
fn rtl_integer_to_unicode_string(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let digits = match integer_digits(a[0], a[1], 16) {
        Ok(d) => d,
        Err(status) => return Ok(u64::from(status)),
    };
    Ok(u64::from(ansi_to_unicode(k, a[2], &digits, false)?))
}

/// RtlExtendedIntegerMultiply(Multiplicand, Multiplier) -> LARGE_INTEGER:
/// a signed 64-bit by a signed 32-bit.
fn rtl_extended_integer_multiply(_k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let x = (u64::from(a[0]) | u64::from(a[1]) << 32) as i64;
    Ok(x.wrapping_mul(i64::from(a[2] as i32)) as u64)
}

/// RtlExtendedLargeIntegerDivide(Dividend, Divisor, Remainder) ->
/// LARGE_INTEGER: an unsigned 64-bit by an unsigned 32-bit, the remainder
/// through the optional pointer. The kernel raises
/// STATUS_INTEGER_DIVIDE_BY_ZERO on a zero divisor; that stops the guest
/// here.
fn rtl_extended_large_integer_divide(
    k: &mut Kernel,
    _p: &mut dyn Platform,
    a: &Args,
) -> KResult<u64> {
    let x = u64::from(a[0]) | u64::from(a[1]) << 32;
    let d = u64::from(a[2]);
    if d == 0 {
        k.request_stop("RtlExtendedLargeIntegerDivide by zero".into());
        return Ok(0);
    }
    if a[3] != 0 {
        k.gm.w32(a[3], (x % d) as u32)?;
    }
    Ok(x / d)
}

/// RtlExtendedMagicDivide(Dividend, MagicDivisor, ShiftCount) ->
/// LARGE_INTEGER: division by a constant as its reciprocal: the high 64
/// bits of |Dividend| * MagicDivisor, shifted right, with the dividend's
/// sign.
fn rtl_extended_magic_divide(_k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let x = (u64::from(a[0]) | u64::from(a[1]) << 32) as i64;
    let magic = u64::from(a[2]) | u64::from(a[3]) << 32;
    let high = ((u128::from(x.unsigned_abs()) * u128::from(magic)) >> 64) as u64;
    let q = high.checked_shr(a[4] & 0xFF).unwrap_or(0);
    Ok(if x < 0 { q.wrapping_neg() } else { q })
}

/// RtlUlongByteSwap(Source) -> ULONG (fastcall)
fn rtl_ulong_byte_swap(_k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from(a[0].swap_bytes()))
}

/// RtlUshortByteSwap(Source) -> USHORT (fastcall)
fn rtl_ushort_byte_swap(_k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from((a[0] as u16).swap_bytes()))
}

/// RtlMapGenericMask(AccessMask, GenericMapping): the mask's generic bits
/// become the mapping's GenericRead, Write, Execute and All.
fn rtl_map_generic_mask(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let mut mask = k.gm.r32(a[0])?;
    for (i, generic) in
        [0x8000_0000u32, 0x4000_0000, 0x2000_0000, 0x1000_0000].into_iter().enumerate()
    {
        if mask & generic != 0 {
            mask |= k.gm.r32(a[1] + 4 * i as u32)?;
        }
    }
    k.gm.w32(a[0], mask & 0x0FFF_FFFF)?;
    Ok(0)
}

/// A string of a debugging export, for the log.
fn log_text(k: &Kernel, va: u32) -> String {
    if va == 0 {
        return String::new();
    }
    match k.gm.cstr(va, 256) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(_) => format!("<{va:08x}>"),
    }
}

/// RtlAssert(FailedAssertion, FileName, LineNumber, Message): the kernel
/// asks its debugger what to do; with none, the title goes on.
fn rtl_assert(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    log::warn!(target: "kernel", "rtl: assertion failed: {} {} at {}:{} | frames{}",
        log_text(k, a[3]), log_text(k, a[0]), log_text(k, a[1]), a[2], k.frame_chain());
    Ok(0)
}

/// RtlRip(ApiName, Expression, Message): the kernel prints it and breaks
/// into its debugger; with none, the title goes on.
fn rtl_rip(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    log::warn!(target: "kernel", "rtl: RIP in {}: {} {} | frames{}",
        log_text(k, a[0]), log_text(k, a[1]), log_text(k, a[2]), k.frame_chain());
    Ok(0)
}

/// The export `name` as it is installed, run for this call.
fn forward(k: &mut Kernel, p: &mut dyn Platform, a: &Args, name: &str) -> KResult<u64> {
    let f = exports::by_name(name).and_then(|e| k.impls[usize::from(e.ordinal)]);
    match f {
        Some(f) => f(k, p, a),
        None => {
            k.request_stop(format!("{name} is not implemented"));
            Ok(0)
        }
    }
}

/// RtlEnterCriticalSectionAndRegion(CriticalSection): KeEnterCriticalRegion
/// then RtlEnterCriticalSection. The region is nothing here (ke.rs: APCs are
/// delivered only in alertable waits).
fn rtl_enter_critical_section_and_region(
    k: &mut Kernel,
    p: &mut dyn Platform,
    a: &Args,
) -> KResult<u64> {
    forward(k, p, a, "RtlEnterCriticalSection")
}

/// RtlLeaveCriticalSectionAndRegion(CriticalSection)
fn rtl_leave_critical_section_and_region(
    k: &mut Kernel,
    p: &mut dyn Platform,
    a: &Args,
) -> KResult<u64> {
    forward(k, p, a, "RtlLeaveCriticalSection")
}

/// The exports the reference has.
pub(crate) fn register(k: &mut Kernel) {
    k.imp("RtlInitAnsiString", rtl_init_ansi_string);
    k.imp("RtlInitUnicodeString", rtl_init_unicode_string);
    k.imp("RtlCompareString", rtl_compare_string);
    k.imp("RtlEqualString", rtl_equal_string);
    k.imp("RtlCompareMemoryUlong", rtl_compare_memory_ulong);
    k.imp("RtlCompareMemory", rtl_compare_memory);
    k.imp("RtlZeroMemory", rtl_zero_memory);
    k.imp("RtlFillMemory", rtl_fill_memory);
    k.imp("RtlFillMemoryUlong", rtl_fill_memory_ulong);
    k.imp("RtlMoveMemory", rtl_move_memory);
    k.imp("RtlLowerChar", rtl_lower_char);
    k.imp("RtlUpperChar", rtl_upper_char);
    k.imp("RtlUpcaseUnicodeChar", rtl_upcase_unicode_char);
    k.imp("RtlUpcaseUnicodeString", rtl_upcase_unicode_string);
    k.imp("RtlUpcaseUnicodeToMultiByteN", rtl_upcase_unicode_to_multi_byte_n);
    k.imp("RtlUpperString", rtl_upper_string);
    k.imp("RtlAnsiStringToUnicodeString", rtl_ansi_string_to_unicode_string);
    k.imp("RtlUnicodeStringToAnsiString", rtl_unicode_string_to_ansi_string);
    k.imp("RtlFreeAnsiString", rtl_free_string);
    k.imp("RtlFreeUnicodeString", rtl_free_string);
    k.imp("RtlNtStatusToDosError", rtl_nt_status_to_dos_error);
    k.imp("RtlTimeToTimeFields", rtl_time_to_time_fields);
    k.imp("RtlTimeFieldsToTime", rtl_time_fields_to_time);
    register_nt(k);
}

/// The exports the reference leaves out, from the NT runtime library.
fn register_nt(k: &mut Kernel) {
    k.imp("RtlCompareUnicodeString", rtl_compare_unicode_string);
    k.imp("RtlEqualUnicodeString", rtl_equal_unicode_string);
    k.imp("RtlCopyString", rtl_copy_string);
    k.imp("RtlCopyUnicodeString", rtl_copy_unicode_string);
    k.imp("RtlAppendStringToString", rtl_append_string_to_string);
    k.imp("RtlAppendUnicodeStringToString", rtl_append_unicode_string_to_string);
    k.imp("RtlAppendUnicodeToString", rtl_append_unicode_to_string);
    k.imp("RtlCreateUnicodeString", rtl_create_unicode_string);
    k.imp("RtlDowncaseUnicodeChar", rtl_downcase_unicode_char);
    k.imp("RtlDowncaseUnicodeString", rtl_downcase_unicode_string);
    k.imp("RtlUnicodeToMultiByteN", rtl_unicode_to_multi_byte_n);
    k.imp("RtlMultiByteToUnicodeN", rtl_multi_byte_to_unicode_n);
    k.imp("RtlMultiByteToUnicodeSize", rtl_multi_byte_to_unicode_size);
    k.imp("RtlUnicodeToMultiByteSize", rtl_unicode_to_multi_byte_size);
    k.imp("RtlCharToInteger", rtl_char_to_integer);
    k.imp("RtlUnicodeStringToInteger", rtl_unicode_string_to_integer);
    k.imp("RtlIntegerToChar", rtl_integer_to_char);
    k.imp("RtlIntegerToUnicodeString", rtl_integer_to_unicode_string);
    k.imp("RtlExtendedIntegerMultiply", rtl_extended_integer_multiply);
    k.imp("RtlExtendedLargeIntegerDivide", rtl_extended_large_integer_divide);
    k.imp("RtlExtendedMagicDivide", rtl_extended_magic_divide);
    k.imp("RtlUlongByteSwap", rtl_ulong_byte_swap);
    k.imp("RtlUshortByteSwap", rtl_ushort_byte_swap);
    k.imp("RtlMapGenericMask", rtl_map_generic_mask);
    k.imp("RtlAssert", rtl_assert);
    k.imp("RtlRip", rtl_rip);
    k.imp("RtlEnterCriticalSectionAndRegion", rtl_enter_critical_section_and_region);
    k.imp("RtlLeaveCriticalSectionAndRegion", rtl_leave_critical_section_and_region);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{self, TestKernel};

    fn put(t: &mut TestKernel, data: &[u8]) -> u32 {
        let va = t.alloc(data.len() as u32);
        t.k.gm.write(va, data).unwrap();
        va
    }

    fn wide(s: &str) -> Vec<u8> {
        unit_bytes(&s.encode_utf16().collect::<Vec<_>>())
    }

    /// A counted string over `bytes` with `room` bytes of buffer.
    fn counted(t: &mut TestKernel, bytes: &[u8], room: u16) -> u32 {
        let buf = t.alloc(u32::from(room).max(bytes.len() as u32));
        t.k.gm.write(buf, bytes).unwrap();
        let s = t.alloc(8);
        str_set(&t.k, s, bytes.len() as u16, room, buf).unwrap();
        s
    }

    fn ansi(t: &mut TestKernel, s: &[u8]) -> u32 {
        counted(t, s, s.len() as u16)
    }

    fn uni(t: &mut TestKernel, s: &str) -> u32 {
        let b = wide(s);
        counted(t, &b, b.len() as u16)
    }

    fn text(t: &TestKernel, s: u32) -> Vec<u8> {
        let d = str_get(&t.k, s).unwrap();
        t.k.gm.read_vec(d.buf, u32::from(d.len)).unwrap()
    }

    fn fields(t: &TestKernel, s: u32) -> (u16, u16, u32) {
        let d = str_get(&t.k, s).unwrap();
        (d.len, d.max, d.buf)
    }

    #[test]
    fn init_strings_count_up_to_the_terminator() {
        let mut t = testing::kernel();
        let (s, a, w) = (t.alloc(8), put(&mut t, b"default.xbe\0"), put(&mut t, &wide("save\0")));
        t.call("RtlInitAnsiString", &[s, a]);
        assert_eq!(fields(&t, s), (11, 12, a));
        t.call("RtlInitUnicodeString", &[s, w]);
        assert_eq!(fields(&t, s), (8, 10, w));
        t.call("RtlInitAnsiString", &[s, 0]);
        assert_eq!(fields(&t, s), (0, 0, 0));
    }

    #[test]
    fn strings_compare_by_bytes_then_length_and_fold_case_on_request() {
        let mut t = testing::kernel();
        let (abc, upper, abd, ab) =
            (ansi(&mut t, b"abc"), ansi(&mut t, b"ABC"), ansi(&mut t, b"abd"), ansi(&mut t, b"ab"));
        assert_eq!(t.call32("RtlCompareString", &[abc, abc, 0]), 0);
        assert_eq!(t.call32("RtlCompareString", &[abc, abd, 0]) as i32, -1);
        assert_eq!(t.call32("RtlCompareString", &[abc, ab, 0]) as i32, 1);
        assert_eq!(t.call32("RtlCompareString", &[abc, upper, 0]) as i32, 32);
        assert_eq!(t.call32("RtlCompareString", &[abc, upper, 1]), 0);
        assert_eq!(t.call32("RtlEqualString", &[abc, upper, 0]), 0);
        assert_eq!(t.call32("RtlEqualString", &[abc, upper, 1]), 1);
        assert_eq!(t.call32("RtlEqualString", &[abc, ab, 1]), 0);

        let (w1, w2, w3) = (uni(&mut t, "Größe"), uni(&mut t, "GRÖSSE"), uni(&mut t, "grÖße"));
        assert_eq!(t.call32("RtlEqualUnicodeString", &[w1, w3, 1]), 1);
        assert_eq!(t.call32("RtlEqualUnicodeString", &[w1, w3, 0]), 0);
        assert_eq!(t.call32("RtlEqualUnicodeString", &[w1, w2, 1]), 0);
        assert_eq!(t.call32("RtlCompareUnicodeString", &[w1, w3, 1]), 0);
        assert_eq!(
            t.call32("RtlCompareUnicodeString", &[w1, w3, 0]) as i32,
            'G' as i32 - 'g' as i32
        );
        assert_eq!(t.call32("RtlCompareUnicodeString", &[w2, w1, 1]) as i32, 'S' as i32 - 0xDF);
    }

    /// The memory routines with the guest-code fast paths off.
    fn slow() -> TestKernel {
        testing::kernel_with(|c| c.fast_paths = false)
    }

    #[test]
    fn the_memory_routines_fill_move_and_compare() {
        let mut t = slow();
        let m = t.alloc(0x3000);
        t.call("RtlFillMemory", &[m, 0x2800, 0x1AB]);
        assert!(t.k.gm.read_vec(m, 0x2800).unwrap().iter().all(|&b| b == 0xAB));
        assert_eq!(t.k.gm.r8(m + 0x2800).unwrap(), 0);
        t.call("RtlZeroMemory", &[m + 1, 0x27FE]);
        assert_eq!(t.k.gm.r8(m).unwrap(), 0xAB);
        assert!(t.k.gm.read_vec(m + 1, 0x27FE).unwrap().iter().all(|&b| b == 0));
        assert_eq!(t.k.gm.r8(m + 0x27FF).unwrap(), 0xAB);

        t.call("RtlFillMemoryUlong", &[m, 0x1007, 0x1122_3344]);
        assert_eq!(t.k.gm.r32(m + 0x1000).unwrap(), 0x1122_3344);
        assert_eq!(t.k.gm.r32(m + 0x1004).unwrap(), 0);
        assert_eq!(t.call32("RtlCompareMemoryUlong", &[m, 0x2000, 0x1122_3344]), 0x1004);
        assert_eq!(t.call32("RtlCompareMemoryUlong", &[m, 0x1003, 0x1122_3344]), 0x1000);

        let data: Vec<u8> = (0..0x1800u32).map(|i| (i ^ (i >> 8)) as u8).collect();
        t.k.gm.write(m, &data).unwrap();
        t.call("RtlMoveMemory", &[m + 0x1800, m, 0x1800]);
        assert_eq!(t.call32("RtlCompareMemory", &[m, m + 0x1800, 0x1800]), 0x1800);
        t.k.gm.w8(m + 0x1800 + 0x1234, !data[0x1234]).unwrap();
        assert_eq!(t.call32("RtlCompareMemory", &[m, m + 0x1800, 0x1800]), 0x1234);
        assert_eq!(t.call32("RtlCompareMemory", &[m, m + 0x1800, 0]), 0);
    }

    #[test]
    fn move_memory_copies_overlapping_ranges_either_way() {
        let mut t = slow();
        let len = 3 * CHUNK + 0x123;
        let m = t.alloc(len + 2 * CHUNK);
        let data: Vec<u8> = (0..len).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
        for shift in [1u32, 0x777, CHUNK + 5] {
            t.k.gm.write(m, &data).unwrap();
            t.call("RtlMoveMemory", &[m + shift, m, len]);
            assert!(t.k.gm.read_vec(m + shift, len).unwrap() == data, "up by {shift:x}");
            t.call("RtlMoveMemory", &[m, m + shift, len]);
            assert!(t.k.gm.read_vec(m, len).unwrap() == data, "down by {shift:x}");
        }
    }

    #[test]
    fn a_fill_that_runs_off_its_pages_faults_before_it_writes() {
        let mut t = slow();
        let m = t.alloc(0x1000);
        t.call("RtlFillMemory", &[m + 0x800, 0x1000, 0xFF]);
        assert!(!t.returned());
        assert!(t.k.gm.read_vec(m, 0x1000).unwrap().iter().all(|&b| b == 0));
    }

    #[test]
    fn the_character_exports_fold_ascii_and_the_unicode_ones_the_c_table() {
        let mut t = slow();
        assert_eq!(t.call32("RtlUpperChar", &[u32::from(b'q')]), u32::from(b'Q'));
        assert_eq!(t.call32("RtlUpperChar", &[0xE9]), 0xE9);
        assert_eq!(t.call32("RtlLowerChar", &[0x100 | u32::from(b'Q')]), u32::from(b'q'));
        assert_eq!(t.call32("RtlLowerChar", &[u32::from(b'[')]), u32::from(b'['));
        assert_eq!(t.call32("RtlUpcaseUnicodeChar", &[0xE9]), 0xC9);
        assert_eq!(t.call32("RtlUpcaseUnicodeChar", &[0x44F]), 0x42F);
        assert_eq!(t.call32("RtlDowncaseUnicodeChar", &[0x42F]), 0x44F);
        assert_eq!(t.call32("RtlDowncaseUnicodeChar", &[0xD7]), 0xD7);
    }

    #[test]
    fn the_case_tables_agree_with_unicode_and_with_each_other() {
        // where the reference's table differs from Unicode's simple uppercase:
        // the dotless i, which it pairs with the dotted capital
        let c_only = [0x131u16];
        for c in 0..=0xFFFFu16 {
            let ch = char::from_u32(u32::from(c));
            let (up, down) = (upcase(c), downcase(c));
            if up != c && !c_only.contains(&c) {
                let mut u = ch.unwrap().to_uppercase();
                assert_eq!((u.next(), u.next()), (char::from_u32(u32::from(up)), None), "{c:04x}");
            }
            if down != c {
                let mut l = ch.unwrap().to_lowercase();
                let first = l.next();
                // U+0130 lowers to i and a combining dot in full; i in simple
                assert_eq!(first, char::from_u32(u32::from(down)), "{c:04x}");
                assert!(l.next().is_none() || c == 0x130);
                assert_eq!(upcase(down), if c == 0x130 { 0x49 } else { c }, "{c:04x}");
            }
        }
    }

    #[test]
    fn upcase_unicode_string_allocates_or_fills_and_free_gives_the_pool_back() {
        let mut t = testing::kernel();
        let (src, dst) = (uni(&mut t, "straße.sav"), t.alloc(8));
        let free0 = t.k.pool.free_bytes();
        assert_eq!(t.call32("RtlUpcaseUnicodeString", &[dst, src, 1]), 0);
        assert_eq!(text(&t, dst), wide("STRAßE.SAV"));
        let (len, max, buf) = fields(&t, dst);
        assert_eq!((len, max), (20, 20));
        assert!(buf >= CONTIG_WINDOW && t.k.pool.free_bytes() < free0);
        t.call("RtlFreeUnicodeString", &[dst]);
        assert_eq!(fields(&t, dst), (0, 0, 0));
        assert_eq!(t.k.pool.free_bytes(), free0);

        let small = counted(&mut t, &[], 18);
        assert_eq!(t.call32("RtlUpcaseUnicodeString", &[small, src, 0]), STATUS_BUFFER_OVERFLOW);
        let fits = counted(&mut t, &[], 20);
        assert_eq!(t.call32("RtlDowncaseUnicodeString", &[fits, dst, 0]), 0);
        assert_eq!(text(&t, fits), b"");
        t.call("RtlUpcaseUnicodeString", &[dst, src, 1]);
        assert_eq!(t.call32("RtlDowncaseUnicodeString", &[fits, dst, 0]), 0);
        assert_eq!(text(&t, fits), wide("straße.sav"));
    }

    #[test]
    fn ansi_and_unicode_strings_convert_both_ways() {
        let mut t = testing::kernel();
        let (src, w, back) = (ansi(&mut t, b"T:\\save\xE9"), t.alloc(8), t.alloc(8));
        assert_eq!(t.call32("RtlAnsiStringToUnicodeString", &[w, src, 1]), 0);
        let (len, max, buf) = fields(&t, w);
        assert_eq!((len, max), (16, 18));
        assert_eq!(t.k.gm.read_vec(buf, 18).unwrap(), wide("T:\\save\u{E9}\0"));
        assert_eq!(t.call32("RtlUnicodeStringToAnsiString", &[back, w, 1]), 0);
        assert_eq!(text(&t, back), b"T:\\save\xE9");
        assert_eq!(fields(&t, back).1, 9);
        assert_eq!(t.k.gm.r8(fields(&t, back).2 + 8).unwrap(), 0);
        t.call("RtlFreeAnsiString", &[back]);
        t.call("RtlFreeUnicodeString", &[w]);

        // into the caller's buffers: what fits, terminated when there is room
        let exact = counted(&mut t, &[], 16);
        assert_eq!(t.call32("RtlAnsiStringToUnicodeString", &[exact, src, 0]), 0);
        assert_eq!(text(&t, exact), wide("T:\\save\u{E9}"));
        let short = counted(&mut t, &[], 14);
        assert_eq!(
            t.call32("RtlAnsiStringToUnicodeString", &[short, src, 0]),
            STATUS_BUFFER_OVERFLOW
        );
        let greek = uni(&mut t, "a\u{3A9}b");
        let three = counted(&mut t, &[], 3);
        assert_eq!(t.call32("RtlUnicodeStringToAnsiString", &[three, greek, 0]), 0);
        assert_eq!(text(&t, three), b"a?b");
        let tiny = counted(&mut t, &[], 2);
        assert_eq!(
            t.call32("RtlUnicodeStringToAnsiString", &[tiny, greek, 0]),
            STATUS_BUFFER_OVERFLOW
        );
    }

    #[test]
    fn a_string_too_long_for_a_unicode_string_is_refused() {
        let mut t = testing::kernel();
        let (src, dst) = (counted(&mut t, &vec![b'x'; 0x7FFF], 0x7FFF), t.alloc(8));
        assert_eq!(
            t.call32("RtlAnsiStringToUnicodeString", &[dst, src, 1]),
            STATUS_INVALID_PARAMETER_2
        );
        assert_eq!(fields(&t, dst), (0, 0, 0));
    }

    #[test]
    fn freeing_a_buffer_that_is_not_the_pools_leaves_the_pool_alone() {
        let mut t = testing::kernel();
        let s = ansi(&mut t, b"mine");
        let free0 = t.k.pool.free_bytes();
        t.call("RtlFreeAnsiString", &[s]);
        assert_eq!(fields(&t, s), (0, 0, 0));
        assert_eq!(t.k.pool.free_bytes(), free0);
    }

    #[test]
    fn the_n_conversions_convert_what_fits_and_report_the_bytes() {
        let mut t = testing::kernel();
        let (w, out, count) = (put(&mut t, &wide("x\u{444}y\u{E9}")), t.alloc(16), t.alloc(4));
        assert_eq!(t.call32("RtlUpcaseUnicodeToMultiByteN", &[out, 16, count, w, 8]), 0);
        assert_eq!(t.k.gm.read_vec(out, 5).unwrap(), b"X?Y\xC9\0");
        assert_eq!(t.k.gm.r32(count).unwrap(), 4);
        assert_eq!(t.call32("RtlUnicodeToMultiByteN", &[out, 3, count, w, 8]), 0);
        assert_eq!(t.k.gm.read_vec(out, 4).unwrap(), b"x?y\xC9");
        assert_eq!(t.k.gm.r32(count).unwrap(), 3);
        assert_eq!(t.call32("RtlUnicodeToMultiByteN", &[out, 16, 0, w, 8]), 0);
        assert_eq!(t.k.gm.read_vec(out, 4).unwrap(), b"x?y\xE9");

        let (m, wout) = (put(&mut t, b"ab\xE9"), t.alloc(16));
        assert_eq!(t.call32("RtlMultiByteToUnicodeN", &[wout, 5, count, m, 3]), 0);
        assert_eq!(t.k.gm.read_vec(wout, 6).unwrap(), wide("ab\0"));
        assert_eq!(t.k.gm.r32(count).unwrap(), 4);
        assert_eq!(t.call32("RtlMultiByteToUnicodeN", &[wout, 16, count, m, 3]), 0);
        assert_eq!(t.k.gm.read_vec(wout, 6).unwrap(), wide("ab\u{E9}"));
        assert_eq!(t.call32("RtlMultiByteToUnicodeSize", &[count, m, 3]), 0);
        assert_eq!(t.k.gm.r32(count).unwrap(), 6);
        assert_eq!(t.call32("RtlUnicodeToMultiByteSize", &[count, w, 8]), 0);
        assert_eq!(t.k.gm.r32(count).unwrap(), 4);
    }

    #[test]
    fn upper_string_and_the_copies_take_what_fits() {
        let mut t = testing::kernel();
        let (src, dst) = (ansi(&mut t, b"d:\\media"), counted(&mut t, b"", 5));
        t.call("RtlUpperString", &[dst, src]);
        assert_eq!(text(&t, dst), b"D:\\ME");
        t.call("RtlCopyString", &[dst, src]);
        assert_eq!(text(&t, dst), b"d:\\me");
        t.call("RtlCopyString", &[dst, 0]);
        assert_eq!(fields(&t, dst).0, 0);

        let (wsrc, wdst) = (uni(&mut t, "abc"), counted(&mut t, &[0xEE; 8], 8));
        t.k.gm.w16(wdst, 0).unwrap();
        t.call("RtlCopyUnicodeString", &[wdst, wsrc]);
        assert_eq!(text(&t, wdst), wide("abc"));
        assert_eq!(t.k.gm.r16(fields(&t, wdst).2 + 6).unwrap(), 0);
        let exact = counted(&mut t, &[], 4);
        t.call("RtlCopyUnicodeString", &[exact, wsrc]);
        assert_eq!(text(&t, exact), wide("ab"));
    }

    #[test]
    fn the_appends_extend_a_string_or_report_it_too_small() {
        let mut t = testing::kernel();
        let (dst, more) = (counted(&mut t, b"D:\\", 8), ansi(&mut t, b"media"));
        assert_eq!(t.call32("RtlAppendStringToString", &[dst, more]), 0);
        assert_eq!(text(&t, dst), b"D:\\media");
        assert_eq!(t.call32("RtlAppendStringToString", &[dst, more]), STATUS_BUFFER_TOO_SMALL);
        assert_eq!(text(&t, dst), b"D:\\media");

        let mut room = wide("ab");
        room.extend_from_slice(&[0xEE; 10]);
        let wdst = counted(&mut t, &room, 12);
        t.k.gm.w16(wdst, 4).unwrap();
        let (cd, tail) = (uni(&mut t, "cd"), put(&mut t, &wide("ef\0")));
        assert_eq!(t.call32("RtlAppendUnicodeStringToString", &[wdst, cd]), 0);
        assert_eq!(text(&t, wdst), wide("abcd"));
        assert_eq!(t.k.gm.r16(fields(&t, wdst).2 + 8).unwrap(), 0, "terminated: there is room");
        assert_eq!(t.call32("RtlAppendUnicodeToString", &[wdst, tail]), 0);
        assert_eq!(text(&t, wdst), wide("abcdef"));
        assert_eq!(t.call32("RtlAppendUnicodeToString", &[wdst, tail]), STATUS_BUFFER_TOO_SMALL);
        assert_eq!(t.call32("RtlAppendUnicodeToString", &[wdst, 0]), 0);
        assert_eq!(text(&t, wdst), wide("abcdef"));
    }

    #[test]
    fn create_unicode_string_makes_a_pool_copy_that_free_returns() {
        let mut t = testing::kernel();
        let (src, dst) = (put(&mut t, &wide("Halo\0")), t.alloc(8));
        let free0 = t.k.pool.free_bytes();
        assert_eq!(t.call32("RtlCreateUnicodeString", &[dst, src]), 1);
        let (len, max, buf) = fields(&t, dst);
        assert_eq!((len, max), (8, 10));
        assert_eq!(t.k.gm.read_vec(buf, 10).unwrap(), wide("Halo\0"));
        t.call("RtlFreeUnicodeString", &[dst]);
        assert_eq!(t.k.pool.free_bytes(), free0);
    }

    #[test]
    fn status_to_dos_maps_statuses_and_unwraps_hresults() {
        assert_eq!(status_to_dos(0), 0);
        assert_eq!(status_to_dos(0xC000_0034), 2);
        assert_eq!(status_to_dos(0xC000_0022), 5);
        assert_eq!(status_to_dos(0x0000_0103), 997);
        assert_eq!(status_to_dos(0xD000_0034), 2, "HRESULT_FROM_NT");
        assert_eq!(status_to_dos(0x8007_2733), 10035, "HRESULT_FROM_WIN32(WSAEWOULDBLOCK)");
        assert_eq!(status_to_dos(0xA000_0001), 0xA000_0001, "a customer code");
        assert_eq!(status_to_dos(0xC0FF_FFFF), 317);
        let mut t = testing::kernel();
        assert_eq!(t.call32("RtlNtStatusToDosError", &[0xC000_000F]), 2);
    }

    #[test]
    fn time_fields_and_times_convert_both_ways() {
        let mut t = testing::kernel();
        let (time, tf) = (t.alloc(8), t.alloc(16));
        // 2001-11-15 (a Thursday) 12:34:56.789, the Xbox's launch day
        let known: [u16; 8] = [2001, 11, 15, 12, 34, 56, 789, 4];
        t.k.gm.write(tf, &unit_bytes(&known)).unwrap();
        assert_eq!(t.call32("RtlTimeFieldsToTime", &[tf, time]), 1);
        let ft = t.k.gm.r64(time).unwrap();
        // days from 1601 to 1970 are 134774; 2001-11-15 is day 11641 of the Unix era
        let ms = (134_774 + 11_641) * 86_400_000u64 + ((12 * 60 + 34) * 60 + 56) * 1000 + 789;
        assert_eq!(ft, ms * 10_000);
        t.k.gm.fill(tf, 0, 16).unwrap();
        t.call("RtlTimeToTimeFields", &[time, tf]);
        assert_eq!(read_units(&t.k, tf, 8).unwrap(), known);

        // round trips across leap days and the eras' ends
        for want in [
            [1601u16, 1, 1, 0, 0, 0, 0, 1],
            [2000, 2, 29, 23, 59, 59, 999, 2],
            [2004, 12, 31, 0, 0, 0, 0, 5],
            [2100, 3, 1, 6, 7, 8, 9, 1],
        ] {
            t.k.gm.write(tf, &unit_bytes(&want)).unwrap();
            assert_eq!(t.call32("RtlTimeFieldsToTime", &[tf, time]), 1, "{want:?}");
            t.call("RtlTimeToTimeFields", &[time, tf]);
            assert_eq!(read_units(&t.k, tf, 8).unwrap(), want);
        }
        for bad in [
            [1600u16, 1, 1, 0, 0, 0, 0, 0],
            [2001, 2, 29, 0, 0, 0, 0, 0],
            [2001, 13, 1, 0, 0, 0, 0, 0],
            [2001, 0, 1, 0, 0, 0, 0, 0],
            [2001, 1, 1, 24, 0, 0, 0, 0],
            [2001, 1, 1, 0, 60, 0, 0, 0],
            [2001, 1, 1, 0, 0, 0, 1000, 0],
        ] {
            t.k.gm.write(tf, &unit_bytes(&bad)).unwrap();
            assert_eq!(t.call32("RtlTimeFieldsToTime", &[tf, time]), 0, "{bad:?}");
        }
    }

    #[test]
    fn integers_parse_with_sign_prefix_and_base() {
        let mut t = testing::kernel();
        let out = t.alloc(4);
        for (s, base, want) in [
            ("1234", 0u32, Some(1234u32)),
            ("  -42xyz", 0, Some(-42i32 as u32)),
            ("+7", 10, Some(7)),
            ("0x1fG", 0, Some(0x1F)),
            ("0b101", 0, Some(5)),
            ("0o17", 0, Some(15)),
            ("FF", 16, Some(255)),
            ("0x10", 16, Some(0)),
            ("129", 8, Some(10)),
            ("", 0, Some(0)),
            ("4294967297", 10, Some(1)),
            ("12", 7, None),
        ] {
            let c = put(&mut t, format!("{s}\0").as_bytes());
            t.k.gm.w32(out, 0xDEAD_BEEF).unwrap();
            let status = t.call32("RtlCharToInteger", &[c, base, out]);
            let u = uni(&mut t, s);
            match want {
                Some(v) => {
                    assert_eq!((status, t.k.gm.r32(out).unwrap()), (0, v), "{s:?} base {base}");
                    assert_eq!(t.call32("RtlUnicodeStringToInteger", &[u, base, out]), 0);
                    assert_eq!(t.k.gm.r32(out).unwrap(), v, "wide {s:?} base {base}");
                }
                None => {
                    assert_eq!(status, STATUS_INVALID_PARAMETER);
                    assert_eq!(
                        t.call32("RtlUnicodeStringToInteger", &[u, base, out]),
                        STATUS_INVALID_PARAMETER
                    );
                    assert_eq!(t.k.gm.r32(out).unwrap(), 0xDEAD_BEEF);
                }
            }
        }
    }

    #[test]
    fn integers_print_in_a_base_and_parse_back() {
        let mut t = testing::kernel();
        let (buf, out) = (t.alloc(64), t.alloc(4));
        for (v, base) in
            [(0u32, 10u32), (255, 16), (0xDEAD_BEEF, 16), (4_294_967_295, 0), (9, 2), (511, 8)]
        {
            t.k.gm.fill(buf, 0xEE, 64).unwrap();
            assert_eq!(t.call32("RtlIntegerToChar", &[v, base, 40, buf]), 0);
            assert_eq!(
                t.call32("RtlCharToInteger", &[buf, if base == 0 { 10 } else { base }, out]),
                0
            );
            assert_eq!(t.k.gm.r32(out).unwrap(), v);
        }
        assert_eq!(t.call32("RtlIntegerToChar", &[0xBEEF, 16, 40, buf]), 0);
        assert_eq!(t.k.gm.cstr(buf, 64).unwrap(), b"BEEF");
        // exactly the room: no terminator
        t.k.gm.fill(buf, 0xEE, 64).unwrap();
        assert_eq!(t.call32("RtlIntegerToChar", &[1234, 10, 4, buf]), 0);
        assert_eq!(t.k.gm.read_vec(buf, 5).unwrap(), b"1234\xEE");
        assert_eq!(t.call32("RtlIntegerToChar", &[12345, 10, 4, buf]), STATUS_BUFFER_OVERFLOW);
        assert_eq!(t.call32("RtlIntegerToChar", &[1, 3, 4, buf]), STATUS_INVALID_PARAMETER);
        // a negative length pads with zeros
        assert_eq!(t.call32("RtlIntegerToChar", &[42, 10, -6i32 as u32, buf]), 0);
        assert_eq!(t.k.gm.read_vec(buf, 7).unwrap(), b"000042\xEE");

        let s = counted(&mut t, &[], 20);
        assert_eq!(t.call32("RtlIntegerToUnicodeString", &[0x1F, 16, s]), 0);
        assert_eq!(text(&t, s), wide("1F"));
        assert_eq!(t.call32("RtlUnicodeStringToInteger", &[s, 16, out]), 0);
        assert_eq!(t.k.gm.r32(out).unwrap(), 0x1F);
        assert_eq!(
            t.call32("RtlIntegerToUnicodeString", &[0x1_0000, 2, s]),
            STATUS_BUFFER_OVERFLOW
        );
    }

    fn halves(v: u64) -> [u32; 2] {
        [v as u32, (v >> 32) as u32]
    }

    #[test]
    fn the_large_integer_helpers_agree_with_64_bit_arithmetic() {
        let mut t = testing::kernel();
        let rem = t.alloc(4);
        for (x, y) in [
            (0x0123_4567_89AB_CDEFi64, 1000i32),
            (-5_000_000_000, 7),
            (5_000_000_000, -7),
            (i64::MAX, 2),
            (-1, -1),
        ] {
            let [lo, hi] = halves(x as u64);
            assert_eq!(
                t.call("RtlExtendedIntegerMultiply", &[lo, hi, y as u32]),
                x.wrapping_mul(i64::from(y)) as u64
            );
            let d = y.unsigned_abs();
            assert_eq!(
                t.call("RtlExtendedLargeIntegerDivide", &[lo, hi, d, rem]),
                x as u64 / u64::from(d)
            );
            assert_eq!(u64::from(t.k.gm.r32(rem).unwrap()), x as u64 % u64::from(d));
            assert_eq!(
                t.call("RtlExtendedLargeIntegerDivide", &[lo, hi, d, 0]),
                x as u64 / u64::from(d)
            );
        }
        // the kernel's own constants: x / 10000 as a multiply by
        // 0xD1B71758E219652C and a shift by 13
        let [mlo, mhi] = halves(0xD1B7_1758_E219_652C);
        for x in [0i64, 9999, 10_000, 126_000_000_000_000_000, -126_000_000_000_012_345] {
            let [lo, hi] = halves(x as u64);
            assert_eq!(
                t.call("RtlExtendedMagicDivide", &[lo, hi, mlo, mhi, 13]) as i64,
                x / 10_000,
                "{x}"
            );
        }
        assert!(t.stop_reason().is_none());
        t.call("RtlExtendedLargeIntegerDivide", &[1, 0, 0, 0]);
        assert!(t.stop_reason().is_some_and(|r| r.contains("by zero")));
    }

    #[test]
    fn the_byte_swaps_reverse_their_register_argument() {
        let mut t = testing::kernel();
        assert_eq!(t.call32("RtlUlongByteSwap", &[0x1122_3344]), 0x4433_2211);
        assert_eq!(t.call32("RtlUshortByteSwap", &[0xFFFF_1122]), 0x2211);
        assert!(t.returned());
    }

    #[test]
    fn map_generic_mask_replaces_the_generic_bits_with_the_mappings() {
        let mut t = testing::kernel();
        let mapping: Vec<u8> = [0x0012_0089u32, 0x0012_0116, 0x0012_00A0, 0x001F_01FF]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let (mask, map) = (t.alloc(4), put(&mut t, &mapping));
        t.k.gm.w32(mask, 0xC000_0001).unwrap();
        t.call("RtlMapGenericMask", &[mask, map]);
        assert_eq!(t.k.gm.r32(mask).unwrap(), 0x0012_0089 | 0x0012_0116 | 1);
        t.k.gm.w32(mask, 0x1000_0000).unwrap();
        t.call("RtlMapGenericMask", &[mask, map]);
        assert_eq!(t.k.gm.r32(mask).unwrap(), 0x001F_01FF);
        t.k.gm.w32(mask, 0x0000_0002).unwrap();
        t.call("RtlMapGenericMask", &[mask, map]);
        assert_eq!(t.k.gm.r32(mask).unwrap(), 2);
    }

    #[test]
    fn the_debugging_exports_return_to_the_title() {
        let mut t = testing::kernel();
        let (expr, file) = (put(&mut t, b"p != NULL\0"), put(&mut t, b"d3d.cpp\0"));
        t.call("RtlAssert", &[expr, file, 42, 0]);
        assert!(t.returned());
        t.call("RtlRip", &[file, expr, 0x7000_0000]);
        assert!(t.returned() && t.stop_reason().is_none());
    }

    #[test]
    fn the_region_forms_take_and_release_a_critical_section() {
        let mut t = testing::kernel();
        let cs = t.alloc(0x1C);
        t.call("RtlInitializeCriticalSection", &[cs]);
        t.call("RtlEnterCriticalSectionAndRegion", &[cs]);
        assert!(t.returned());
        assert_eq!(t.call32("RtlTryEnterCriticalSection", &[cs]), 1);
        assert_eq!(t.k.gm.r32(cs + 0x14).unwrap(), 2, "entered twice");
        t.call("RtlLeaveCriticalSectionAndRegion", &[cs]);
        t.call("RtlLeaveCriticalSectionAndRegion", &[cs]);
        assert_eq!(t.k.gm.r32(cs + 0x10).unwrap() as i32, -1, "free again");
        assert_eq!(t.k.gm.r32(cs + 0x18).unwrap(), 0);
    }
}
