# CCI disc images

A CCI is an Xbox disc image (an XISO or a full dump) compressed one 2048-byte
sector at a time with LZ4. Repackinator and XboxToolkit write it and Cerbios'
loader reads it. Implemented in `crates/xb-formats/src/cci.rs`. All fields
are little-endian.

## File layout

| Part | Contents |
|---|---|
| header | 32 bytes at offset 0 |
| blocks | one per sector of the image, in order, contiguous |
| index | one `u32` per block, plus one more |

### Header

| Offset | Size | Field | Accepted |
|---|---|---|---|
| 0 | 4 | magic | `"CCIM"` |
| 4 | 4 | header size | 32 |
| 8 | 8 | uncompressed size in bytes | not 0 |
| 16 | 8 | offset of the index | 32 ..= file size |
| 24 | 4 | block size | 2048 |
| 28 | 1 | version | 1 |
| 29 | 1 | index shift | 0 ..= 4 (XboxToolkit writes 2) |
| 30 | 2 | reserved | |

The image has `ceil(size / 2048)` blocks; at most `0x3FFF_FFFF` are accepted.
The index must have room for `blocks + 1` entries inside the file.

### Index entry

```
bit 31      set: the block is LZ4 compressed
bits 0..30  the block's file offset >> shift
```

Entry `i + 1` gives where block `i` ends, so the last entry is where the
blocks end (the index's own offset). A reader rejects a run of entries where
an offset is below 32, above the index offset, below its predecessor, or more
than `2048 + 1 + 16` bytes above it.

### Block

- **Stored** (bit 31 clear): the sector's 2048 bytes.
- **LZ4** (bit 31 set): one byte `p`, then a raw LZ4 block (no frame) that
  must decode to exactly 2048 bytes, then `p` bytes of padding that align the
  next block to `1 << shift`.

## Split sets

A set is named `Name.1.cci`, `Name.2.cci`, ... (one or two digits, no leading
zero; up to 9 parts are read). Each part is a complete CCI with its own header
and index, holding the sectors that follow the previous part's.

Writers cut a part at `0xFF00_0000` bytes (16 MiB short of FATX's 4 GiB file
limit): the blocks of every part but the last end at the first block's end past
that offset. So a part whose index offset is in
`[0xFF00_0000, 0xFF00_0000 + 2065)` is "filled to the cut" and another part
follows it. Images in one file do not end there.

Rules the reader applies:

- A full part must hold whole sectors.
- A full first part must be named `Name.1.cci`; the next parts are opened
  beside it until one is not full. A missing part fails the open.
- `Name.2.cci` is a later part only if `Name.1.cci` exists and is full.
  Otherwise it is an image of its own ("Game v1.2.cci").
- `disc::open` on a later part opens the set from its first part.

## Reading

Nothing is kept in memory but each part's header. For a run of up to 64
blocks inside one part: one positional read of the index entries, one of the
blocks' stored bytes, then each block is decoded. A whole sector goes straight
into the caller's buffer; a partial first or last sector goes through a
2048-byte stack buffer. The stored bytes land in a per-thread buffer that
grows to at most about 130 KiB and is reused.

Reads past the image's end: `read_at` is an error, `read_padded` gives zeros.
