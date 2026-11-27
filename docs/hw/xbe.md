# XBE

The Xbox executable format. Implemented in `crates/xb-formats/src/xbe.rs`.
All fields are little-endian.
"Address" means a virtual address in the loaded image; an address inside the
headers maps to file offset `address - base`.

## Header

The first `headers size` bytes of the file are mapped at `base`.

| Offset | Field |
|---|---|
| `0x000` | magic `"XBEH"` |
| `0x004` | signature (256 bytes) |
| `0x104` | base address (`0x0001_0000`) |
| `0x108` | headers size |
| `0x10C` | image size |
| `0x110` | image header size |
| `0x114` | timestamp |
| `0x118` | certificate address |
| `0x11C` | section count |
| `0x120` | section headers address |
| `0x124` | init flags |
| `0x128` | entry point, XORed |
| `0x12C` | TLS directory address |
| `0x130` | stack commit |
| `0x134` | heap reserve |
| `0x138` | heap commit |
| `0x13C` | PE base address |
| `0x140` | PE image size |
| `0x144` | PE checksum |
| `0x148` | PE timestamp |
| `0x14C` | debug path name address |
| `0x150` | debug file name address |
| `0x154` | debug Unicode file name address |
| `0x158` | kernel thunk table address, XORed |
| `0x15C` | non-kernel import directory address |
| `0x160` | library version count |
| `0x164` | library versions address |
| `0x168` | kernel library version address |
| `0x16C` | XAPI library version address |
| `0x170` | logo bitmap address |
| `0x174` | logo bitmap size |

### Keys

| | Entry point | Kernel thunk |
|---|---|---|
| retail | `0xA8FC_57AB` | `0x5B6D_40B6` |
| debug | `0x9485_9D4B` | `0xEFB1_F152` |

The parser tries retail, then debug, then no key (headers read back from a
loaded image) and takes the first that puts the entry point in
`[base, base + image size)`. The thunk address uses the same choice.

### Checks

Refused: fewer than `0x178` header bytes, a base that is not page aligned or is
below `0x0001_0000`, an image that ends above `0x7FFE_0000`, an entry point
outside the image under every key, a section or library table that is not
inside the headers.

## Certificate

| Offset | Field |
|---|---|
| `0x00` | size |
| `0x04` | timestamp |
| `0x08` | title id |
| `0x0C` | title name, 40 UTF-16 units |
| `0x5C` | 16 alternate title ids |
| `0x9C` | allowed media |
| `0xA0` | game region (1 NA, 2 Japan, 4 rest of world, `0x8000_0000` manufacturing) |
| `0xA4` | game ratings |
| `0xA8` | disc number |
| `0xAC` | version |
| `0xB0` | LAN key (16 bytes) |
| `0xC0` | signature key (16 bytes) |
| `0xD0` | 16 alternate signature keys (not parsed) |

## Section header (`0x38` bytes)

| Offset | Field |
|---|---|
| `0x00` | flags |
| `0x04` | virtual address |
| `0x08` | virtual size |
| `0x0C` | raw (file) offset |
| `0x10` | raw size |
| `0x14` | name address (NUL-terminated) |
| `0x18` | load count |
| `0x1C` | head shared page reference count address |
| `0x20` | tail shared page reference count address |
| `0x24` | SHA-1 digest (20 bytes) |

Flags: `0x01` writable, `0x02` preload, `0x04` executable, `0x08` inserted
file, `0x10` head page read-only, `0x20` tail page read-only.

## Library version (16 bytes)

Name (8 bytes, NUL padded), major, minor, build (`u16` each), flags (`u16`:
bits 0..12 QFE, bits 13..14 approved, bit 15 debug build).

## Loading

The kernel gives memory to the headers and to the sections flagged preload.
The rest of the image is address space only until `XeLoadSection`. Titles count
on that (Fable preloads 15 of 107 sections), so the loader must not back the
whole image.

- The image spans `align_up(max(image size, headers size), 4096)` bytes from
  `base`.
- For each segment (the headers, then sections): copy
  `min(raw size, virtual size)` bytes from the file to the virtual address and
  zero the rest of the virtual size. Sections may share a page with a
  neighbour but not bytes. A section that is not inside the image is refused
  when it is asked for.
- Set the load count of each preloaded section to 1, at
  `section headers address + index * 0x38 + 0x18`.

### Kernel thunk table

A run of `u32` slots at the decoded thunk address, ended by a zero. Each is
`0x8000_0000 | ordinal`. The loader replaces each slot with the address of the
kernel export. The parser refuses a slot without the top bit and a table of
more than 1024 slots. It reads the table either from the loaded image (through
a callback) or from the file, mapping addresses through the section table.

### TLS directory

At the TLS address (inside a section, not the headers), an
`IMAGE_TLS_DIRECTORY32`:

| Offset | Field |
|---|---|
| 0 | raw data start |
| 4 | raw data end |
| 8 | index address |
| 12 | callbacks address |
| 16 | zero fill size |
| 20 | characteristics |

A title without initialized thread-local data has start and end both 0 (Halo).

## Reference values

Halo: Combat Evolved (USA): title `4D530004`, base `0x0001_0000`, image
3732 KiB (3716 KiB backed at boot), 22 of 24 sections preloaded, entry
`0x0001_5CC9`, 134 kernel imports, TLS directory at `0x001D_6A80`.
