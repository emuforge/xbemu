# xbemu (the standalone front end)

A window, gamepads, audio and a command line around `xb-core`. It is
deliberately thin: every setting lives in the core's in-game menu, which the
core draws. The app owns no emulation and depends on no workspace crate but
`xb-core`.

## Usage

```
./xbemu path/to/game.cci [options]          # from a checkout (builds if needed)
cargo run --release -p xbemu -- game.cci    # the same thing
```

| Option | Meaning |
|---|---|
| `--data-dir DIR` | Where the console's hard disk, shader caches and `settings.txt` live. Default: `$XDG_DATA_HOME/xbemu`, or `~/.local/share/xbemu` (`platform.rs` has the other hosts' places). |
| `--fullscreen` | Start in borderless fullscreen. |
| `--log LEVEL` | `off`, `error`, `warn`, `info` (default), `debug`, `trace`. Lines go to stderr as `[seconds] LEVEL target: text`. Library crates (window, audio, gamepad) log one level less than asked below INFO. |
| `--cpu auto\|kvm\|translator` | The CPU backend for this run (`CoreConfig::cpu_override`); the saved setting is not changed. |
| `--frames N` | Exit with status 0 after N presented frames (automated runs). A presented frame is a window refresh, several per frame of the title. |
| `--screenshot-dir DIR` | Where F12's screenshots go. Default: `screenshots/` in the data directory. |
| `--screenshot-at N` | Save a screenshot when N frames were presented (automated runs; `XBEMU_SCRIPT` in `docs/development.md` does it by time). |
| `-h`, `--help` | The usage text. |

Options may also be written `--name=value`. `--` ends the options.

Exit status: 0 a normal end, 1 the core did not boot (or the host has no
display), 2 a usage error, 3 a fatal guest condition. The reason is printed
on stderr.

`--selftest` (hidden, for developers) runs the window, input polling and the
audio stream for five seconds without a core: the window stays blank (on
Wayland it is not shown at all, since nothing is drawn into it), a quiet
1.5 second tone is played through the same ring and device callback the
core's audio takes, and pad state changes are logged.

## Controls

The app's own keys:

| Key | Action |
|---|---|
| F1, Escape | Open or close the in-game menu |
| F11, Alt+Enter | Toggle fullscreen |
| F12 | Save a screenshot (JPEG, the picture with the overlay) |

On a gamepad the Guide button (the Xbox button in the centre of an Xbox
pad, PS or Home on others; `Button::Mode` in `gilrs`, `buttonHome` in
GameController) opens and closes the menu, on any connected pad; on macOS the
app asks the system to leave the button to it. The original controller has no such button, so
the title never sees it. `--log debug` logs each gamepad button as it goes
down with the name it is taken for (on macOS the Guide button alone), and at connection which code a pad's
Guide button has; a pad without one is reported at INFO.

There is no button combo by default. `menu_combo` in `settings.txt` can name
one (a mask of the console's buttons, `0x00c0` for both stick clicks, held
for half a second); `0x0000` is none.

Gamepads (up to four, assigned to ports 1 to 4 in the order they are
connected, hot-pluggable, with rumble where the pad and the host allow it)
are read by one source a host (`pads/`):

- **macOS:** Apple's GameController framework (`pads/macos.rs`), which reads
  every pad macOS's own drivers support (Xbox, DualSense, DualShock 4,
  Switch Pro, MFi) in one layout. An Xbox pad on USB can be read through
  nothing else: its driver shows other interfaces only the raw protocol, no
  buttons. A pad the framework does not support is not read. Rumble goes
  through Core Haptics (`pads/macos/haptics.rs`), one engine for each handle
  where the pad has two; an Xbox pad rumbles over Bluetooth, but macOS does
  not pass rumble to one on USB.
- **Elsewhere:** `gilrs` (`pads/portable.rs`), naming each pad's controls
  from the SDL controller database.

Both answer in `gilrs`'s names, so one mapping (`input.rs`) serves every
host:

| Modern pad | Xbox |
|---|---|
| South / East / West / North (A B X Y on an Xbox pad) | A / B / X / Y |
| Left shoulder / right shoulder | White / Black |
| Left trigger / right trigger | Left / right trigger (analog) |
| Start / Select | Start / Back |
| Stick clicks, D-pad, sticks | The same |
| Guide | (not the console's) the in-game menu |

The keyboard always drives port 1, merged with the first gamepad (buttons
are OR-ed, pressures take the larger, stick deflections add). Keys are by
position, so the layout is the same on any keyboard language:

| Key | Xbox |
|---|---|
| Arrow keys | D-pad |
| W A S D | Left stick |
| I J K L | Right stick |
| F / H | Left / right stick click |
| Z / X / C / V | A / B / X / Y |
| 1 / 2 | White / Black |
| Q / E | Left / right trigger |
| Enter | Start |
| Backspace | Back |

## Threads

- **Main thread**: the `winit` event loop and the window; it also owns the
  audio stream. It does no emulation. It publishes the window's size and the
  held keys as atomics and raises flags (menu toggle, screenshot, surface
  replaced, stop) that the emulation thread reads once a frame. The menu
  keys only raise the flag; the emulation thread asks the core whether its
  menu is open before toggling, since the pad closes it too.
- **Emulation thread** (`session.rs`): boots the `Core`, calls `Core::frame`
  in a loop, shuts the core down. *Every* core call happens here, so the
  core never crosses a thread and this is the thread its surface is created
  on. It owns the gamepads too: they are polled immediately before each
  frame (a Guide button press toggles the menu there), and rumble is
  forwarded after it.
- **Audio thread** (`cpal`'s): drains the ring the core's audio goes into and
  converts to the device's format. It never blocks or allocates.

Order: the emulation thread boots the core *before* the event loop starts,
so a boot failure is reported without a window opening. The core creates
its surface when it first presents; the emulation thread waits for the
window before its first `frame`.

Shutdown: whatever ends the session (closing the window, Quit in the menu,
a fatal guest condition, `--frames`), the event loop only leaves. The main
thread then sets the stop flag and joins the emulation thread, whose last
act is `Core::shutdown`. The window outlives that by construction: the
surface provider holds it and the core holds the provider. The audio stream
is closed after the join. The menu's Reset shuts down the same way and then
starts the program again (`exec` on Unix), because the renderer cannot be
created twice in one process.

## Audio

`cpal`, at 48 kHz stereo `i16` when the device offers it (then the callback
is `RingSource::fill` and nothing else). Otherwise the nearest configuration
is opened and the callback converts the sample format, the channel count
and, only if the device cannot run at 48 kHz, the rate, with a linear
resampler. On Linux the PulseAudio host (which PipeWire also serves) is
tried before ALSA; it is a Rust protocol client and needs no system library.
With no device at all the core gets a `NullSink` and runs the same, silent.

The device is asked for callbacks of 2048 frames (`audio::PERIOD_FRAMES`,
the reference's size on Linux), which keeps the sound close to the picture
without underruns on a desktop sound server. A device that refuses
the size gets its own. `XBEMU_AUDIO_PERIOD=N` asks for another size (0: the
device's own). At exit the app logs what the device took, how many callbacks
ran short and the delay the device reported.

## Invariants

- No core call outside the emulation thread.
- `Core::shutdown` returns before the window or the audio stream is dropped.
- The audio callback takes no lock and does not allocate.
- `unsafe` is denied crate-wide and allowed in two modules: `surface.rs`,
  which creates the Vulkan surface on the core's instance
  (`SurfaceProvider::create_surface` is an `unsafe fn` and `ash` calls are
  unsafe), and `pads/macos` (with its `haptics` child), whose GameController
  and Core Haptics bindings are all `unsafe` calls. Nothing else may use it.

## Build needs

Linux: ALSA and udev development files (`libasound2-dev`, `libudev-dev` on
Debian) for `cpal` and `gilrs`, and a Vulkan loader at run time.

## Tests

```
cargo test -p xbemu
```

Unit tests cover the pure parts: the command line, key and gamepad mapping,
merging, port assignment, the resampler and format conversion, the choice of
audio configuration. Devices are exercised by `--selftest`.
