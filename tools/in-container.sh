#!/usr/bin/env bash
# Runs a command inside the toolchain container (docker/Dockerfile), with the
# checkout at /src and its own target directory, target/docker. CI runs its
# checks and builds through this, so a failure there is reproduced here:
#   tools/in-container.sh cargo test --workspace --locked
# XBEMU_CONTAINER_ENGINE=podman uses Podman; XBEMU_BUILD_IMAGE names an image
# already built (CI's cache) instead of building docker/Dockerfile. /dev/kvm
# is passed through when the host has one, for the KVM backend's tests.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
engine="${XBEMU_CONTAINER_ENGINE:-docker}"
image="${XBEMU_BUILD_IMAGE:-}"
if [ -z "$image" ]; then
  image="xbemu-build:latest"
  toolchain=$(sed -n 's/^channel = "\(.*\)"/\1/p' "$root/rust-toolchain.toml")
  "$engine" build --build-arg "RUST_TOOLCHAIN=$toolchain" -t "$image" "$root/docker" >&2
fi
kvm=()
if [ -w /dev/kvm ]; then
  kvm=(--device /dev/kvm --group-add "$(stat -c %g /dev/kvm)")
fi
# The commit the build is named by and reports (`xbemu --version`).
commit="${XBEMU_BUILD_COMMIT:-$(git -C "$root" rev-parse --short=7 HEAD 2>/dev/null || echo unknown)}"
exec "$engine" run --rm \
    -u "$(id -u):$(id -g)" "${kvm[@]}" \
    -v "$root:/src" \
    -e HOME=/tmp \
    -e CARGO_HOME=/src/target/docker/cargo-home \
    -e CARGO_TARGET_DIR=/src/target/docker \
    -e XBEMU_DEPS_DIR=/src/target/docker/deps \
    -e XBEMU_BUILD_COMMIT="$commit" \
    "$image" "$@"
