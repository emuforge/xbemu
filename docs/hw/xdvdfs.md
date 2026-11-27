# XDVDFS

The file system of Xbox game discs. Implemented in
`crates/xb-formats/src/xdvdfs.rs`. Sectors are 2048 bytes. All fields are
little-endian.

## Finding the volume

The game partition starts at one of these byte offsets of the image, tried in
order:

| Offset | Image |
|---|---|
| `0` | extracted XISO |
| `0x1830_0000` | full XGD1 dump (redump style) |
| `0x0FD9_0000` | full XGD2 dump |
| `0x0208_0000` | full XGD3 dump |

The volume descriptor is sector 32 of the partition:

| Offset | Size | Field |
|---|---|---|
| `0x000` | 20 | `"MICROSOFT*XBOX*MEDIA"` |
| `0x014` | 4 | root directory sector |
| `0x018` | 4 | root directory size in bytes |
| `0x01C` | 8 | creation time (`FILETIME`) |
| `0x7EC` | 20 | `"MICROSOFT*XBOX*MEDIA"` again |

Both magics must match. Every sector number in the volume counts from the
partition's start.

## Directories

A directory is a table of `size` bytes at its sector. Its entries form a
binary search tree ordered by name (ASCII case-insensitive); the entry at
offset 0 is the root of the tree.

| Offset | Size | Field |
|---|---|---|
| 0 | 2 | left child, table offset / 4 (0: none) |
| 2 | 2 | right child, table offset / 4 (0: none) |
| 4 | 4 | first sector of the data |
| 8 | 4 | size in bytes |
| 12 | 1 | attributes |
| 13 | 1 | name length |
| 14 | n | name, single bytes, no terminator |

Entries are padded to 4 bytes and do not cross a sector; the rest of a sector
is `0xFF`. An empty directory has size 0.

Attributes: `0x01` read-only, `0x02` hidden, `0x04` system, `0x10` directory,
`0x20` archive, `0x80` normal.

## How the reader walks a table

Image tools get the tree wrong in ways the reader must survive, so it does not
binary-search:

1. In-order walk of the tree from offset 0, with an explicit stack (Conker:
   Live & Reloaded's `Audio` directory is 78 entries linked 77 deep). A bit per
   4 bytes of the table marks entries seen, which ends loops.
2. Then a pass over the table as laid out, sector by sector, reporting entries
   the tree did not reach.

An entry is valid when it lies in the table, is not `0xFFFF 0xFFFF` padding
and has a name that is not empty. A lookup compares names byte for byte,
ignoring ASCII case, and takes the first match in that order. Paths accept
either slash; empty components are skipped.

## Limits and edge cases

- A directory table larger than 64 MiB is refused.
- Reads of a file must lie inside its size. Bytes past the image's end read
  as zeros (trimmed images).
- `walk` enters each directory table once and follows nesting 130 deep.
- Names are exposed as strings with each byte as the character of that code
  (Latin-1).
