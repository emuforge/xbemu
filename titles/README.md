# The titles stash

Some tests boot a real game. No game data is in this repository or in CI, so
those tests look here for images you link in yourself, from backups of discs
you own. A test whose title is missing prints a note and passes.

## Adding a title

Link (or copy) the image under its key:

```
ln -s '/path/to/Halo - Combat Evolved (USA).cci' titles/halo-ce.cci
```

- The name is `<key>.<extension>`; `.cci`, `.iso` and `.xiso` all work.
- A split CCI is linked part by part: `<key>.1.cci`, `<key>.2.cci`.
- `XBEMU_TITLES_DIR` names another directory to use as the stash.
- Everything here but this file is ignored by git.

Then run the tests as usual: `cargo test --workspace`.

## Titles the tests ask for by key

| Key | Title | Title ID | Used by |
|---|---|---|---|
| `halo-ce` | Halo: Combat Evolved (USA) | `4D530004` | `xb-kernel` `tests/title_boot.rs`, `xb-core` `tests/title_session.rs`, the title's known values in `xb-formats` `tests/game_image.rs` |
| `fuzion-frenzy` | Fuzion Frenzy (USA) | `4D530002` | `xb-core` `tests/title_draws.rs` |

The title ID is checked, so a wrong image under a key fails its test with a
message and does no harm.

Checks that hold for any title (the disc formats, the file system, the
kernel's file calls) run on whatever is in the stash, whatever its key. Use
a short lowercase name for a title that is not in the table.

## Reference logs

`<key>.ref.log`, when present, is a log of another emulator running the same
title. `xb-kernel`'s boot test compares its file opens and first GPU register
accesses with it, and checks less without it.

## For agents

When a task needs one of these tests and the stash lacks the title, the
test's note names the exact link to create. Ask the operator for the path to
their image of that title and for leave to link it; do not search their disks
for game images, and never copy one into the repository.
