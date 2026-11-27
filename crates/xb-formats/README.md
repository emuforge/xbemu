# xb-formats

Layer L0. Disc image and executable formats, parsed from untrusted input.

## Owns

| Module | What |
|---|---|
| `block` | `BlockSource` (random-access reads), `FileSource`, `MemSource`, `SubSource` |
| `cci` | CCI images (one sector per LZ4 block), split sets included, as a `BlockSource` of the plain image |
| `disc` | `disc::open(path)`: CCI (by magic and split naming) or raw ISO/XISO |
| `xdvdfs` | the Xbox DVD file system: volume descriptor, directory trees, lookup, listing, file reads |
| `xbe` | the Xbox executable: header, certificate, sections, libraries, kernel imports, TLS directory, the load layout |

Format references: `docs/hw/cci.md`, `docs/hw/xdvdfs.md`, `docs/hw/xbe.md`.

## Invariants

- `#![forbid(unsafe_code)]`. `unsafe` is not allowed here.
- Every length and offset read from a file is bounds checked. Malformed input
  is an `Error`, never a panic, and never an allocation sized by a field alone
  (directory tables are capped at 64 MiB, XBE headers at the console's RAM, a
  section's bytes are checked against the file before a buffer grows).
- `BlockSource::read_at` is exact and positional. One source serves several
  threads at once with no lock. `read_padded` is the disc-image read: zeros
  past the end, as a trimmed image reads.
- A CCI keeps only its parts' headers in memory. A read is two positional
  reads per 64 blocks (index entries, then the contiguous blocks) and decodes
  whole sectors straight into the caller's buffer. The staging buffer is per
  thread and reused: no allocation per read after a thread's first.
- Nothing is loaded into guest memory. `xbe::Xbe` describes the image; the
  loader copies `Segment`s and patches the import slots.
- Depends only on `xb-types`, `log` and `lz4_flex` (MIT, safe decoder).
- Logs under the `disc` target.

## Tests

```
cargo test -p xb-formats
```

Unit tests build their own fixtures (CCIs with stored and LZ4 blocks, split
sets, XDVDFS trees, XBE files). With a game image they also check the real
thing end to end:

```
ln -s /path/to/image.cci titles/halo-ce.cci     # see titles/README.md
cargo test -p xb-formats -- --nocapture
```

Every image in the titles stash runs the general checks. Known values of
one title are asserted when the image is Halo (`4D530004`).
