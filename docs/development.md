# Development

How to build, run and debug xbemu. Linux x86-64 is the only host so far.

## Building

Needed on the host: the Rust toolchain named in `rust-toolchain.toml`,
`clang` and `clang++`, `cmake`, `ninja` or `make`, `python3`, a C++ standard
library, the development packages the window, audio and gamepad crates link
against (`docker/Dockerfile` has the list for Debian), and at run time a
Vulkan loader with a Vulkan 1.1+ driver.

```
tools/fetch-deps.sh                 # once: the renderer's third-party sources into third_party/
cargo build --release -p xbemu
./xbemu path/to/game.cci            # builds if needed, then runs
```

`tools/docker-build.sh` makes the release build inside the image in
`docker/`: the one CI publishes, linked against an old glibc so it runs on
any current distribution, and packaged as a tarball and an AppImage
(`docs/releasing.md`). It needs Docker or Podman and nothing else.

The vendored C (the renderer and the APU) is compiled `-O3` in every
profile; the Rust profiles are in the root `Cargo.toml`. Use `--release`
for anything about speed.

## Running

```
xbemu <disc image> [--data-dir DIR] [--fullscreen] [--log LEVEL]
      [--cpu auto|kvm|translator] [--frames N]
      [--screenshot-dir DIR] [--screenshot-at N]
```

- F1, Escape or a gamepad's Guide button (the Xbox button in its centre)
  opens and closes the in-game menu. `--log debug` logs every gamepad button
  as it goes down, with the name it is taken for. Fullscreen is a setting (`fullscreen` in `settings.txt`, the menu's row); F11, Alt+Enter and a double click on the picture switch it, and `--fullscreen` turns it on. F12 saves a screenshot (JPEG)
  into `--screenshot-dir`, by default `screenshots/` in the data directory.
- In the menu the keyboard's Enter chooses the highlighted row and
  Backspace goes back (they are Start and Back in a title, and Start closes
  the menu); the arrows move, and Z, X and C are A, B and X as in a title.
- F2 saves a state to the current slot and F4 loads it; F6 and F7 choose
  the slot (0 to 9). The menu's "Save states" page lists the slots with the
  picture each was saved at (A loads, X saves). `--state N` or `--state
  FILE` starts the title from a savestate instead of from its boot.
- `--cpu` chooses the CPU backend for one run; the menu's "CPU backend" is
  the saved choice. Auto takes KVM when `/dev/kvm` is usable and logs why
  not otherwise.
- `--frames N` ends the run after N window refreshes and `--screenshot-at N`
  saves a picture at the Nth. They count `Core::frame` calls that presented,
  several a title frame (144 a second on a 144 Hz display), not the title's
  frames. For timed automation use `XBEMU_SCRIPT` below.
- Exit status: 0 a normal end, 1 the core did not boot, 2 a usage error,
  3 a fatal guest condition.

### The data directory

`$XDG_DATA_HOME/xbemu` or `~/.local/share/xbemu`, or `--data-dir`:

| Path | What |
|---|---|
| `settings.txt` | the settings, `key = value` lines (`xb_types::settings`); written when the menu changes one and at shutdown. `menu_combo` is a mask of pad buttons that open the menu when held together, `0x0000` for none (the default) |
| `eeprom.bin` | the console's EEPROM, made at the first boot |
| `hdd/PartitionN/` | the hard disk's partitions 1 and 2 as host folders (saves under `Partition1/UDATA/<title id>`) |
| `hdd/cache/<title id>/` | that title's cache partitions (`Partition3` to `Partition5`) and what is kept of their raw sectors |
| `cache/` | the renderer's pipeline and shader caches, per title |
| `screenshots/` | F12's pictures |
| `states/<title id>/` | the savestate slots: `slotN.state`, the picture `slotN.bmp`, and `slotN.disk`, the list of files in `disk/` the slot needs |
| `states/<title id>/disk/` | the cache partitions' files as the slots were saved with them, each once, named by its hash; a file no slot lists is removed at the next save |

Deleting the directory gives a factory-new console. Automated runs should
use their own (`--data-dir target/test-data`).

## Environment variables and test hooks

| Variable | Read by | Effect |
|---|---|---|
| `XBEMU_SCRIPT` | `xb-core` | a scripted session, below |
| `XBEMU_AUDIO_PERIOD` | the app | the audio callback size asked of the device, in frames (default 2048); 0 leaves it to the device |
| `XBEMU_WAIT_LOOPS` | `xb-core` | `off`: the title's wait loops run as guest code instead of being waited out on the host (to compare) |
| `XBEMU_FRAME_HOLD` | `xb-core` | `on` or `off`: the frame-latency hold always or never taken, instead of judged (to compare) |
| `XBEMU_GPU_RAM_COPY` | the renderer | guest RAM copied to the GPU as on a discrete one, not imported (to compare) |
| `XBEMU_TITLES_DIR` | tests | the titles stash, instead of `titles/` (below) |
| `XBEMU_TEST_LOG`, `XBEMU_TEST_SECONDS`, `XBEMU_TEST_HDD` | `xb-kernel`'s `tests/title_boot.rs` | its log level, time limit and a hard disk to start from |
| `XBEMU_DEPS_DIR` | `xb-gpu`'s build | where the third-party sources are, instead of `third_party/` |
| `XBASED_HLE_*`, `XBASED_GPU`, `DEBUG_XBASED_*` | the renderer | the vendored renderer's debug switches (`crates/xb-gpu/README.md`) |
| `NC_APU_*`, `XBASED_DSP_REC`, `XBASED_APU_STATS` | the APU | the vendored APU's (`crates/xb-apu/README.md`) |

### `XBEMU_SCRIPT`

A list of `milliseconds:action` steps separated by commas; the time counts
from the first frame call (about when the window opens).

| Action | Effect |
|---|---|
| `a` `b` `x` `y` `black` `white` `lt` `rt` `start` `back` `up` `down` `left` `right` `lthumb` `rthumb` | the button held on port 1 for 120 ms |
| `menu` | toggles the menu as the app's F1 and Guide button do |
| `combo` | the menu combo (`menu_combo` in `settings.txt`) held for 700 ms; nothing when none is set, which is the default |
| `shot=PATH` | a screenshot of the next presented frame (`.jpg`, else BMP) |
| `plug=N` `unplug=N` | a controller plugged into port N (1 to 4), or pulled |
| `save=PATH` `load=PATH` | a savestate written to the file, or loaded from it; a number in place of the path is a slot, saved and loaded as the keys do |
| `quit` | ends the session as the menu's Quit does |

A screenshot is what the presenter drew, and asking for one makes it draw:
a picture that is not being refreshed on screen still comes out right in
the file. To see what the window shows, capture the window (run under
XWayland with `WAYLAND_DISPLAY` unset and use `import -window <id>`).

Presses take the same path as a real pad's: the menu sees them and the title
does unless the menu hides them. Examples for Halo (the title's own menu is
up about 20 seconds after boot):

```
# Into the campaign: A on Campaign, the profile, the difficulty; pictures on the way.
XBEMU_SCRIPT='21000:a,23500:a,26500:a,29500:a,32500:a,60000:shot=/tmp/s/loading.jpg,75000:shot=/tmp/s/cutscene.jpg,80000:quit' \
  target/release/xbemu game.cci --data-dir /tmp/xbemu-data

# The in-game menu: open, down to "Show FPS", toggle, close.
XBEMU_SCRIPT='26000:menu,27000:shot=/tmp/s/menu.jpg,27200:down,27500:down,27800:down,28100:down,28400:down,28700:down,29000:down,29400:a,30500:b,32500:shot=/tmp/s/hud.jpg,34000:quit' \
  target/release/xbemu game.cci --data-dir /tmp/xbemu-data
```

## Reading a session's log

`--log info` (the default) tells the story: the disc, the CPU backend and
why, the kernel's stage lines (`stage S0 xbe loaded`, `S1 entry`, `S2 first
kernel call`), file opens (`fs: open ...`), the renderer's lines (`gpu:
hle-...`), USB enumeration (`usb: hub: address 1`, `gamepad: address 2`),
and every five seconds the machine's statistics:

```
core: per s: 30.0 flips, 144.0 presents, 698 traps, 2064 cpu exits, 2209 mmio, 33 page faults
  (3 first writes, 30 stale, 0 the title's), 48011 audio frames, 5 APU interrupt acks, 0 mailbox waits,
  61 USB register reads | holds 0 (...) | guest CPU thread 100% of a host core | irq pending 0000,
  guest time 25.0 s
```

- *flips*: frames the title showed. *presents*: window refreshes.
- *traps*: kernel calls plus the kernel's return sentinels. *cpu exits*:
  every return of `Cpu::run` (traps, kicks, halts).
- *mmio*: device register accesses. *page faults*: guest faults handed to
  the machine; *first writes* and *stale* are write tracking's (the tracker
  also counts first writes the kernel makes on the guest's behalf, so that
  figure can exceed the faults); *the title's* were raised as its own
  exceptions.
- *audio frames* must stay at 48,000 while the title runs. *APU interrupt
  acks* are the title's handler clearing causes; *mailbox waits* the
  DirectSound fixup's calls.
- *holds*: waits of the guest CPU for the renderer, and the time kept out of
  guest time. *irq pending*: lines asserted at that moment (bit n: IRQ n).

`--log debug` adds each crate's mechanism and the first 400 register
accesses; `--log trace` every kernel call. A fatal guest condition prints the
kernel's report (reason, registers, recent calls, every thread) at ERROR.

## The reference

xbased 1.2.5 boots the same games and logs the same events, so a stall or a
difference is found by comparing logs, not by guessing. It is not in this
repository; build it from its own source with its diagnostics on. It opens a
window and uses its own data folder, so keep runs short. How its lines map
to ours:

| Reference | xbemu |
|---|---|
| `xbased-native-frontier: stage S0/S1`, `xbased-ktrace:` | `kernel: stage ...`, `kernel: ktrace: ...` |
| `xbased-native-io: open ...` | `fs: open ...` |
| `xbased-native-mmio: nv2a r+000000 -> ... (4)` (first 400) | `core: mmio: nv2a r+000000 -> ... (4)` at DEBUG |
| `xbased-native-usb:` | `usb:` |
| `xbased-apu:` | `apu:` |
| `xbased-hle-*:` | `gpu: hle-*:` |
| `xbased-native-stats: per s: kcalls N ... mmio N` | `core: per s: ... traps ... mmio` |
| `xbased-hle-fps: 30.0 fps` | `gpu: hle-fps: 30.0 fps` |

## Tests and the titles stash

```
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Before a commit, `tools/check.sh` runs everything CI checks, in its order:
those three (formatting as `--check`), `cargo deny check`, and
`tools/build-docs.sh`, whose rustdoc treats warnings as errors. A doc link
to an item that exists on one platform only, such as a `cfg(target_os =
"macos")` module, is broken on the others and fails the docs build: name it
in plain code (`` `macos` ``) rather than as a link. Most tests
need nothing but the toolchain. Some need more and say so in a printed note
when they pass without checking anything:

- **A real title.** No game data is in the repository. Link your own images
  into `titles/` under the keys `titles/README.md` lists
  (`ln -s /path/to/image.cci titles/halo-ce.cci`) and the same `cargo test`
  boots them: `xb-kernel`'s `tests/title_boot.rs`, and `xb-core`'s
  `tests/title_session.rs` and `tests/title_draws.rs`, which also need a
  Vulkan device. None opens a window.
- **KVM.** The KVM backend's tests and the cross-checks between the two CPU
  backends need a usable `/dev/kvm`.
- **Vulkan.** The renderer's headless tests need a Vulkan 1.1 device.

CI has no game data and no GPU, so a change to what those tests cover has
to be run locally before it is done.

## The documentation site

<https://docs.xbemu.org> is this `docs/` folder built with
[mdBook](https://rust-lang.github.io/mdBook/), with the workspace's rustdoc
under `/reference/`. To preview it:

```
cargo install mdbook --locked       # once
mdbook serve --open                 # the pages, rebuilt as they change
tools/build-docs.sh                 # the whole site as CI builds it, into target/site
```

A new page is added to `docs/SUMMARY.md`.
