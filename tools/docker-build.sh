#!/usr/bin/env bash
# Makes the Linux x86-64 (glibc) release inside the toolchain container: the
# build CI publishes, for hosts without the toolchain too. The binary lands
# in target/docker/release/xbemu and the packaged files (a tarball and an
# AppImage) in target/docker/dist. Usage: tools/docker-build.sh
# The environment is tools/in-container.sh's.
set -euo pipefail
exec "$(dirname "$0")/in-container.sh" bash -euo pipefail -c '
  tools/fetch-deps.sh
  cargo release-build --locked
  rm -rf target/docker/dist
  tools/package-linux.sh target/docker/release/xbemu target/docker/dist "$XBEMU_BUILD_COMMIT"
'
