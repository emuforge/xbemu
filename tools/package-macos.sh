#!/usr/bin/env bash
# Packages a macOS release binary as the file a release carries:
#   xbemu-<label>-<arch>-macos.dmg    xbemu.app on a disk image, beside a
#                                     link to /Applications
# The app carries MoltenVK (Vulkan on Metal), so nothing else is needed on
# the Mac. Nothing is compiled here. Usage:
#   tools/package-macos.sh <binary> <out dir> <label> [libMoltenVK.dylib]
# <label> is the commit's short hash for a pre-release, the version for a
# release. MoltenVK defaults to Homebrew's (`brew install molten-vk`).
# Runs on macOS: needs otool, codesign and hdiutil.
set -euo pipefail
usage="usage: package-macos.sh <binary> <out dir> <label> [libMoltenVK.dylib]"
bin=${1:?$usage}
out=${2:?$usage}
label=${3:?$usage}
brew_prefix=$(brew --prefix 2>/dev/null || echo /opt/homebrew)
moltenvk=${4:-$brew_prefix/opt/molten-vk/lib/libMoltenVK.dylib}
root=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)
arch=$(lipo -archs "$bin")
name="xbemu-${label}-${arch// /-}-macos"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

[ -f "$moltenvk" ] || { echo "package-macos: no MoltenVK at $moltenvk" >&2; exit 1; }
moltenvk_license=$(dirname "$(dirname "$moltenvk")")/LICENSE
[ -f "$moltenvk_license" ] || { echo "package-macos: no MoltenVK license beside $moltenvk" >&2; exit 1; }

# Both have to run, so the app asks for the newer of their minimum macOS
# versions.
minos() { otool -l "$1" | awk '/LC_BUILD_VERSION/ { b = 1 } b && $1 == "minos" { print $2; exit }'; }
minimum=$(printf '%s\n%s\n' "$(minos "$bin")" "$(minos "$moltenvk")" | sort -V | tail -1)
echo "package-macos: needs macOS ${minimum}"

# MoltenVK has to need nothing but the system's.
if otool -L "$moltenvk" | tail -n +3 | grep -vqE '^\s+(/System/Library/|/usr/lib/)'; then
  echo "package-macos: $moltenvk links libraries outside the system:" >&2
  otool -L "$moltenvk" >&2
  exit 1
fi

# The bundle. The renderer finds MoltenVK in Contents/Frameworks
# (crates/xb-gpu/src/os.rs, vulkan_libraries).
app="$work/stage/xbemu.app/Contents"
mkdir -p "$app/MacOS" "$app/Frameworks" "$app/Resources/licenses"
install -m755 "$bin" "$app/MacOS/xbemu"
install -m644 "$moltenvk" "$app/Frameworks/libMoltenVK.dylib"
install -m644 "$root/packaging/macos/xbemu.icns" "$app/Resources/xbemu.icns"
install -m644 "$root/COPYING" "$root/ATTRIBUTION.md" "$root/README.md" "$app/Resources/licenses/"
install -m644 "$moltenvk_license" "$app/Resources/licenses/MoltenVK-LICENSE"
sed -e "s/@VERSION@/${version}/" -e "s/@LABEL@/${label}/" -e "s/@MINIMUM_MACOS@/${minimum}/" \
  "$root/packaging/macos/Info.plist" > "$app/Info.plist"
plutil -lint "$app/Info.plist" > /dev/null

# An ad-hoc signature, inside out: Apple silicon runs no unsigned code. A
# Developer ID signature and notarization, with the hardened runtime and
# packaging/macos/xbemu.entitlements, replace it for a release.
codesign --force --sign - --timestamp=none "$app/Frameworks/libMoltenVK.dylib"
codesign --force --sign - --timestamp=none --entitlements "$root/packaging/macos/xbemu.entitlements" \
  "$work/stage/xbemu.app"
codesign --verify --deep --strict "$work/stage/xbemu.app"

# The disk image: the app and a link to drag it onto.
ln -s /Applications "$work/stage/Applications"
rm -f "$out/$name.dmg"
hdiutil create -quiet -volname "xbemu ${label}" -srcfolder "$work/stage" -fs HFS+ -format UDZO \
  "$out/$name.dmg"
ls -l "$out/$name.dmg"
