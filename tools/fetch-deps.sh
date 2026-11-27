#!/bin/bash
# Fetches the third-party sources the vendored C needs, each at the exact
# revision xbased 1.2.5 is tested with (its tools/fetch_deps.sh), into
# third_party/ at the repository root, or into the directory given:
#   tools/fetch-deps.sh [deps dir]
# The build reads them from XBEMU_DEPS_DIR if set, else third_party/.
# Nothing here is committed. Needs git and network access.
set -euo pipefail
REPO=$(cd "$(dirname "$0")/.." && pwd)
DEPS=${1:-${XBEMU_DEPS_DIR:-$REPO/third_party}}
mkdir -p "$DEPS"

# fetch <directory under the deps dir> <url> <commit>
fetch() {
  local dir="$DEPS/$1" url=$2 rev=$3
  if [ -d "$dir/.git" ] && [ "$(git -C "$dir" rev-parse HEAD)" = "$rev" ]; then
    echo "$1: up to date"
    return
  fi
  echo "$1: $url @ ${rev:0:12}"
  rm -rf "$dir"
  mkdir -p "$dir"
  git -C "$dir" init -q
  git -C "$dir" remote add origin "$url"
  git -C "$dir" fetch -q --depth 1 origin "$rev"
  git -C "$dir" -c advice.detachedHead=false checkout -q FETCH_HEAD
}

# The GPU renderer (vendor/xbased-hle, built by crates/xb-gpu/build.rs).
fetch Vulkan-Headers https://github.com/KhronosGroup/Vulkan-Headers.git               9a0f3099c8a9607a7c0f3127d8abfdc19a93e8c5
fetch volk           https://github.com/zeux/volk.git                                 0b17a763ba5643e32da1b2152f8140461b3b7345
fetch glslang        https://github.com/KhronosGroup/glslang.git                      b5782e52ee2f7b3e40bb9c80d15b47016e008bc9
fetch spirv_reflect  https://github.com/KhronosGroup/SPIRV-Reflect.git                ef913b3ab3da1becca3cf46b15a10667c67bebe5
fetch xxhash         https://github.com/Cyan4973/xxHash.git                           e626a72bc2321cd320e953a0ccf1584cad60f363
fetch root/subprojects/nv2a_vsh_cpu https://github.com/xemu-project/nv2a_vsh_cpu.git  1115255708c10c4841b65dcd2223262e7a316598

echo "deps ready in $DEPS"
