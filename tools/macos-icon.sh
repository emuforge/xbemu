#!/usr/bin/env bash
# Makes the macOS app icon, packaging/macos/xbemu.icns, from the logo
# (assets/logo/xbemu-icon.svg). Run on a Mac when the logo changes; the
# result is committed, as packaging/linux/xbemu.png is.
#   tools/macos-icon.sh
# The logo is drawn at 824 of 1024 pixels, centered: the margin a macOS app
# icon keeps so it sits with the others in the Dock and the Finder. Needs
# only what macOS has: swift (AppKit draws SVG), sips and iconutil.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cat > "$work/render.swift" <<'EOF'
// render <in.svg> <out.png> <canvas> <art>: the image drawn at art x art
// pixels in the middle of a transparent canvas x canvas.
import AppKit
let a = CommandLine.arguments
guard a.count == 5, let canvas = Int(a[3]), let art = Int(a[4]),
      let img = NSImage(contentsOfFile: a[1]) else {
    FileHandle.standardError.write("usage: render <in.svg> <out.png> <canvas> <art>\n".data(using: .utf8)!)
    exit(2)
}
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: canvas, pixelsHigh: canvas,
    bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
    colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
rep.size = NSSize(width: canvas, height: canvas)
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
NSGraphicsContext.current?.imageInterpolation = .high
let at = (canvas - art) / 2
img.draw(in: NSRect(x: at, y: at, width: art, height: art))
NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: a[2]))
EOF

set="$work/xbemu.iconset"
mkdir "$set"
swift "$work/render.swift" "$root/assets/logo/xbemu-icon.svg" "$work/1024.png" 1024 824
# The sizes an iconset holds: 16 to 512 points, each at 1x and 2x.
for pt in 16 32 128 256 512; do
  sips -z "$pt" "$pt" "$work/1024.png" --out "$set/icon_${pt}x${pt}.png" > /dev/null
  px=$((pt * 2))
  sips -z "$px" "$px" "$work/1024.png" --out "$set/icon_${pt}x${pt}@2x.png" > /dev/null
done
mkdir -p "$root/packaging/macos"
iconutil -c icns "$set" -o "$root/packaging/macos/xbemu.icns"
ls -l "$root/packaging/macos/xbemu.icns"
