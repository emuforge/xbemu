# packaging

What a platform's release needs beyond the binary, one folder a platform.

- `linux/`: the AppImage's desktop entry and icon (`xbemu.png` is
  `assets/logo/xbemu-icon.svg` at 256 pixels). `tools/package-linux.sh`
  puts them together; `docs/releasing.md` says what is published.
- `macos/`: the app's `Info.plist` (a template `tools/package-macos.sh`
  fills in), its entitlements for a signed release, and its icon
  (`xbemu.icns`, made from `assets/logo/xbemu-icon.svg` by
  `tools/macos-icon.sh`).
