# Status

What works today and what does not. `ROADMAP.md` says where the project is
going.

xbemu is early. Linux x86-64 is the only platform, and a handful of titles
have been run.

## What works

| Area | State |
|---|---|
| Disc images | CCI (single files and split sets), XISO and raw images; XDVDFS; XBE executables |
| CPU: KVM backend | working; used when the host has `/dev/kvm` |
| CPU: x86-64 translator | working; always available, less tested than KVM |
| Kernel (HLE) | 219 exports implemented, 36 stand-ins, 82 unimplemented, which stop the guest with a clear message (`docs/hw/kernel-exports.md`) |
| GPU | the vendored C renderer on Vulkan, behind a Rust register front end; render scale up to 4x |
| Audio | the vendored C APU, 48 kHz stereo output |
| Input | up to four gamepads; the keyboard plays port 1 |
| In-game menu | Guide button, F1 or Escape; pauses the title; settings are saved |
| Savestates | ten slots a title, with pictures; a title can start from one (`--state`) |
| Disc speed | console speed or fast loading, switchable while a title runs |

## Titles

| Title | State |
|---|---|
| Halo: Combat Evolved | in game at full speed |
| Fuzion Frenzy | in game, close to full speed |
| Tiger Woods PGA Tour 2004 | in game at full speed |
| Halo 2 | reaches its title screen |
| Ninja Gaiden Black | boots, shows no picture |

## Limitations

- Pad input is read while the window is unfocused.
- Widescreen and the CPU backend apply at the next boot; the other settings
  apply while a title runs.
- A title that relaunches into another executable ends the session.
- One title a process: the renderer and the APU cannot be torn down and
  started again.
- `--frames` and `--screenshot-at` count window refreshes, not the title's
  frames.
- A savestate holds the title's cache partitions but not its saved games
  nor the console's stored settings. A state from a build that laid the
  console's memory out differently is refused with a message.
- The guest RAM import into Vulkan is used on Intel integrated GPUs only;
  other GPUs take a copy.
- Audio devices that are not 48 kHz stereo are untested, as is X11 (Wayland
  is what is used day to day).

## Not supported yet

- Platforms other than Linux x86-64 (`ROADMAP.md`).
- A libretro core.
- CHD and extracted-folder images.
- Networking, memory units, the Communicator headset.
- Cheats, mods, uncapped frame rates, CRT filters and FSR upscaling.
