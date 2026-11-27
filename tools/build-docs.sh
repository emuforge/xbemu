#!/usr/bin/env bash
# Builds the documentation site as it is published at docs.xbemu.org, into
# target/site: the pages of docs/ (mdBook, book.toml) with the workspace's
# rustdoc under reference/. Needs mdbook and the build's toolchain
# (docs/development.md); tools/fetch-deps.sh must have run.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
target="${CARGO_TARGET_DIR:-target}"
rm -rf target/site
mdbook build --dest-dir "$root/target/site"
# Private items too: the reference is for people working on the core.
RUSTDOCFLAGS="${RUSTDOCFLAGS:--D warnings}" \
  cargo doc --workspace --no-deps --document-private-items --locked
cp -r "$target/doc" target/site/reference
# rustdoc has no index for a workspace; the core's page is the way in.
cat > target/site/reference/index.html <<'HTML'
<!doctype html>
<meta charset="utf-8">
<meta http-equiv="refresh" content="0; url=xb_core/index.html">
<title>xbemu developer reference</title>
<a href="xb_core/index.html">xbemu developer reference</a>
HTML
echo "site in target/site"
