# xbemu

xbemu is an original Xbox emulator core written in Rust. It runs games and
homebrew straight from a disc image: there is no BIOS to supply and no
dashboard to boot, because the console's kernel is reimplemented on the host.

These pages are the project's documentation, built from the `docs/` folder
of [the repository](https://github.com/emuforge/xbemu) at its `main` branch.

- **[Status](status.md)**: what works today and what does not.
- **[Development](development.md)**: building, running, the data directory,
  tests and the hooks for automated runs.
- **[Releasing](releasing.md)**: what is built from each commit and where
  to get it.
- **[Architecture](architecture.md)**: how the core is put together.
- **Hardware and formats**: reference pages for the parts of the console
  and the file formats the core implements.
- **[Developer reference](reference/index.html)**: the rustdoc of every
  crate, private items included.

xbemu is derived from xbased, hakuX and xemu and is licensed under the GNU
General Public License, version 3 or later. No game, BIOS or other console
data is included with it.
