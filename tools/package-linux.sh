#!/usr/bin/env bash
# Packages a Linux release binary as the two files a release carries:
#   xbemu-<label>-<arch>-linux-<libc>.tar.gz    the binary and the licenses
#   xbemu-<label>-<arch>-linux-<libc>.AppImage  the same binary as an AppImage
# Nothing is compiled here. Usage:
#   tools/package-linux.sh <binary> <out dir> <label> [libc]
# <label> is the commit's short hash for a pre-release, the version for a
# release. Needs mksquashfs and AppImage's static runtime, at
# $APPIMAGE_RUNTIME or /opt/appimage-runtime-<arch> (docker/Dockerfile).
set -euo pipefail
bin=$(readlink -f "${1:?usage: package-linux.sh <binary> <out dir> <label> [libc]}")
out=$(mkdir -p "${2:?out dir}" && readlink -f "$2")
label=${3:?label}
libc=${4:-glibc}
root=$(cd "$(dirname "$0")/.." && pwd)
arch=$(uname -m)
name="xbemu-${label}-${arch}-linux-${libc}"
runtime=${APPIMAGE_RUNTIME:-/opt/appimage-runtime-${arch}}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The glibc build's floor (docker/Dockerfile): a binary that asks for a newer
# symbol was linked against the wrong system and is not published.
if [ "$libc" = glibc ]; then
  floor=${XBEMU_GLIBC_FLOOR:-2.28}
  need=$(objdump -T "$bin" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -Vu | tail -1)
  if [ "$(printf '%s\n%s\n' "$need" "$floor" | sort -V | tail -1)" != "$floor" ]; then
    echo "package-linux: $bin needs glibc $need, above the floor $floor" >&2
    exit 1
  fi
  echo "package-linux: needs glibc $need (floor $floor)"
fi

# The tarball: one folder, unpacked anywhere.
mkdir "$work/$name"
install -m755 "$bin" "$work/$name/xbemu"
install -m644 "$root/COPYING" "$root/ATTRIBUTION.md" "$root/README.md" "$work/$name/"
tar -C "$work" --owner=0 --group=0 -czf "$out/$name.tar.gz" "$name"

# The AppImage: an AppDir as a SquashFS image behind the runtime that mounts
# it and starts AppRun.
app="$work/AppDir"
mkdir -p "$app/usr/bin" "$app/usr/share/licenses/xbemu"
install -m755 "$bin" "$app/usr/bin/xbemu"
install -m644 "$root/COPYING" "$root/ATTRIBUTION.md" "$app/usr/share/licenses/xbemu/"
install -m644 "$root/packaging/linux/xbemu.desktop" "$app/xbemu.desktop"
install -m644 "$root/packaging/linux/xbemu.png" "$app/xbemu.png"
ln -s xbemu.png "$app/.DirIcon"
ln -s usr/bin/xbemu "$app/AppRun"
mksquashfs "$app" "$work/payload.squashfs" -root-owned -noappend -comp gzip > /dev/null
cat "$runtime" "$work/payload.squashfs" > "$out/$name.AppImage"
chmod 755 "$out/$name.AppImage"

ls -l "$out/$name.tar.gz" "$out/$name.AppImage"
