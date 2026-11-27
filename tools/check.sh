#!/usr/bin/env bash
# Runs every check CI runs on a change, in CI's order, and stops at the first
# that fails: formatting, clippy, the tests, the license and advisory check,
# and the documentation site with its rustdoc (warnings are errors, so a
# broken doc link fails here as it fails the Docs workflow). Run it before
# every commit:
#   tools/check.sh
# Needs the build's toolchain, cargo-deny and mdbook (docs/development.md);
# tools/fetch-deps.sh must have run. The release packages are not built here:
# tools/docker-build.sh and tools/package-macos.sh make those.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

need() {
  command -v "$1" > /dev/null || { echo "check: $1 is missing; $2" >&2; exit 1; }
}
need cargo-deny "cargo install --locked cargo-deny"
need mdbook "cargo install --locked mdbook"

step() { echo "check: $*" >&2; "$@"; }
step cargo fmt --all --check
step cargo clippy --workspace --all-targets --locked -- -D warnings
step cargo test --workspace --locked
step cargo deny check
step tools/build-docs.sh
echo "check: all passed" >&2
