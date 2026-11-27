# The toolchain the Linux x86-64 (glibc) release is built with, by CI and by
# tools/docker-build.sh alike.
#
# The base is manylinux_2_28 (AlmaLinux 8): a binary linked here needs glibc
# 2.28 and the libstdc++ of GCC 8 at most, which every current distribution
# has. That is the build's floor; nothing newer may be linked in.
FROM quay.io/pypa/manylinux_2_28_x86_64:latest

# clang and lld: the vendored C is built with clang, as bitcode for the
# link's LTO. alsa-lib and systemd (libudev) are the two libraries the
# binary links; the window system and Vulkan are loaded at run time.
# squashfs-tools and file are the AppImage's.
RUN dnf install -y --setopt=install_weak_deps=False \
        clang lld cmake ninja-build python3 git \
        alsa-lib-devel systemd-devel \
        squashfs-tools file \
    && dnf clean all

# The Rust toolchain rust-toolchain.toml names, for every user the container
# runs as.
ARG RUST_TOOLCHAIN=1.100.0
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal \
            --default-toolchain "${RUST_TOOLCHAIN}" --component rustfmt,clippy \
    && chmod -R a+rwX /opt/rustup /opt/cargo

# AppImage's static type 2 runtime, the head of every AppImage built here.
ARG APPIMAGE_RUNTIME=20251108
RUN curl -fsSL -o /opt/appimage-runtime-x86_64 \
      "https://github.com/AppImage/type2-runtime/releases/download/${APPIMAGE_RUNTIME}/runtime-x86_64" \
    && chmod 644 /opt/appimage-runtime-x86_64

ENV CC=clang CXX=clang++
WORKDIR /src
