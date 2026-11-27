# Releasing

What the repository's workflows build, and where it ends up.

## Pre-releases: every commit to `main`

A push to `main` runs the checks (`ci.yaml`) and, when they pass, publishes
a GitHub pre-release named by the commit's short hash. Its notes are the
commit's message. Its files:

| File | What |
|---|---|
| `xbemu-<commit>-x86_64-linux-glibc.AppImage` | the standalone app as one file: make it executable and run it |
| `xbemu-<commit>-x86_64-linux-glibc.tar.gz` | the same binary in a folder, with the licenses |
| `xbemu-<commit>-arm64-macos.dmg` | the macOS app for Apple silicon: open it and drag `xbemu.app` to Applications |
| `SHA256SUMS` | the checksums of the above |

Each file carries a build provenance attestation: `gh attestation verify
<file> --repo emuforge/xbemu` checks that this repository's workflow built
it from that commit. `xbemu --version` prints the commit a binary was built
from.

A pre-release is never marked as the latest release. Stable releases, cut
from version tags, are planned and will use the same builds.

## The Linux build

`glibc` in a file's name is the C library the build needs: glibc 2.28 or
newer on x86-64, which every distribution of the last several years has
(Debian 10, Ubuntu 18.10, Fedora 29, RHEL 8, SteamOS 3). Distributions built
on musl (Alpine, Void's musl flavor) need a build of their own, which is
planned and will be named `-linux-musl`.

The binary links only the C and C++ runtimes, `libasound` and `libudev`.
The window system (Wayland or X11) and Vulkan are loaded at run time from
the host, so the host's own GPU driver is used. Nothing else is bundled,
which is why the AppImage is little more than the binary.

It is built in the container of `docker/Dockerfile`, on the oldest system
the build supports, so that nothing newer can be linked by accident;
`tools/package-linux.sh` refuses a binary that asks for a newer glibc.
`tools/docker-build.sh` makes the same files locally, into
`target/docker/dist`.

## The macOS build

`xbemu.app` runs on Apple silicon with macOS 12 or newer. It carries
MoltenVK (Vulkan on Metal) in `Contents/Frameworks`, so nothing else needs
installing; the renderer finds it there before any Vulkan loader on the
system. Started from the Finder or the Dock, the app asks for a disc image
with the system's Open panel; from a terminal it takes the same arguments as
on Linux (`xbemu.app/Contents/MacOS/xbemu game.iso`, or `open -a xbemu
--args game.iso`).

The build runs on a macOS runner, not in a container: Homebrew's
`molten-vk`, the release build with `MACOSX_DEPLOYMENT_TARGET` 12.0, then
`tools/package-macos.sh`, which writes into the app the newer of the binary's
and MoltenVK's minimum macOS versions, refuses a MoltenVK that needs
anything but the system, and makes the disk image. The same script makes a
disk image locally from any release binary.

A pre-release is signed ad hoc, not by a developer: macOS keeps a copy
downloaded from the web from opening until it is allowed once, in System
Settings > Privacy & Security ("Open Anyway"), or with `xattr -dr
com.apple.quarantine /Applications/xbemu.app`. A release signed with a
Developer ID and notarized needs the hardened runtime and
`packaging/macos/xbemu.entitlements`, which lets the CPU backend generate
code.

## Workflows

| Workflow | Runs on | Does |
|---|---|---|
| `ci.yaml` | pull requests, pushes to `main` | formatting, clippy, the tests, the license check, the Linux and macOS builds; on `main`, the pre-release |
| `build-linux.yaml` | called by `ci.yaml` | one Linux target's build and packages, as an artifact; checked by starting it on a bare system at the glibc floor |
| `build-macos.yaml` | called by `ci.yaml` | the macOS build and disk image on a macOS runner, as an artifact; checked by starting the app from the disk image |
| `docs.yaml` | pull requests, pushes to `main` | builds this site and the rustdoc; on `main`, publishes them to <https://docs.xbemu.org> |

The checks and the build run inside the toolchain container
(`tools/in-container.sh`), so a failure in CI is reproduced by the same
command on a developer's machine. CI has no game data and no GPU: the tests
that need either pass there without checking anything
(`titles/README.md`).

### Adding a platform

A platform is a reusable workflow of its own (`build-<platform>.yaml`) that
uploads its files as an artifact named `dist-<platform>-...`, and one more
job in `ci.yaml` calling it. The pre-release job collects every `dist-*`
artifact, so it needs one more line in its `needs` and one more pattern in
its check that nothing is missing. Its packaging goes in
`packaging/<platform>/`.

## One-time setup of the repository

- Pages: in the repository's settings, the source is "GitHub Actions" and
  the custom domain is `docs.xbemu.org` (a DNS `CNAME` record from `docs`
  to `emuforge.github.io`).
- Actions needs no secrets: the workflows use the run's own token.
