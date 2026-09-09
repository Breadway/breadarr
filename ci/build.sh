#!/usr/bin/env bash
# Delegates to bread-ecosystem's shared CI build image/script, pinned to
# the commit in ci/bread-ecosystem.rev, not `main`. bread-ecosystem's CI
# files now affect every product's release pipeline, so bumping the pin
# is a deliberate act instead of silent drift (see the bread-theme test
# that broke here for exactly that reason, before it was pinned by rev).
#
# Usage: ci/build.sh cargo build --release --locked
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REV="$(cat "${ROOT}/ci/bread-ecosystem.rev")"

CACHE_DIR="/tmp/bread-ecosystem-ci-${REV}"
# Only create/replace this product's own pin directory. A glob rm of
# /tmp/bread-ecosystem-ci-* races other products' mktemp clones used by
# release-index regen.
if [ ! -d "$CACHE_DIR/.git" ]; then
    rm -rf "$CACHE_DIR"
    git clone https://git.breadway.dev/Breadway/bread-ecosystem.git "$CACHE_DIR"
    git -C "$CACHE_DIR" checkout --quiet "$REV"
fi

# hestia (breadarrd's actual deployment target) runs Ubuntu 24.04 (glibc
# 2.39). The shared CI image is Arch, which tracks a much newer glibc
# (currently 2.43): a binary linked normally there refuses to start on
# hestia (glibc symbol versioning is forward-only). Rather than patch the
# shared image (would affect every other product's build), pull down a
# real, complete glibc + matching gcc-libs (for libstdc++) from the Arch
# Linux Archive and link against those via --sysroot, so the binary only
# requires symbol versions that actually exist on hestia.
#
# cargo-zigbuild (targeting an older glibc via zig's cross-linker) was
# tried first and doesn't work here: zig only maintains a version database
# for glibc's own symbols, not libstdc++'s, and onnxruntime, statically
# linked in by the `ort` crate, needs a real, complete libstdc++, not
# zig's minimal stand-in. A real archived glibc+gcc-libs package pair,
# linked via --sysroot, sidesteps that entirely.
GLIBC_VER="2.39-4"
GCC_LIBS_VER="13.2.1-6"
# Pinned to the archive.archlinux.org packages fetched above (sha256 of
# the two .pkg.tar.zst files, not the extracted trees).
GLIBC_SHA256="c6aef7065e0d53d700cc5ff65d4db775cb84e2f4e3912a609223ed72fb1799d4"
GCC_LIBS_SHA256="edbb8c4772b8852fe102853a84d197253127418f50cca475feda9bbaa842a378"
OLD_GLIBC_CACHE="/tmp/bread-ci-old-glibc-${GLIBC_VER}"
if [ ! -d "${OLD_GLIBC_CACHE}/usr/lib" ]; then
    rm -rf "${OLD_GLIBC_CACHE}"
    mkdir -p "${OLD_GLIBC_CACHE}"
    curl -sfL -o /tmp/old-glibc.pkg.tar.zst \
        "https://archive.archlinux.org/packages/g/glibc/glibc-${GLIBC_VER}-x86_64.pkg.tar.zst"
    curl -sfL -o /tmp/old-gcc-libs.pkg.tar.zst \
        "https://archive.archlinux.org/packages/g/gcc-libs/gcc-libs-${GCC_LIBS_VER}-x86_64.pkg.tar.zst"
    echo "${GLIBC_SHA256}  /tmp/old-glibc.pkg.tar.zst" | sha256sum -c -
    echo "${GCC_LIBS_SHA256}  /tmp/old-gcc-libs.pkg.tar.zst" | sha256sum -c -
    tar --zstd -xf /tmp/old-glibc.pkg.tar.zst -C "${OLD_GLIBC_CACHE}"
    tar --zstd -xf /tmp/old-gcc-libs.pkg.tar.zst -C "${OLD_GLIBC_CACHE}"
    rm -f /tmp/old-glibc.pkg.tar.zst /tmp/old-gcc-libs.pkg.tar.zst
fi
# The shared build script only bind-mounts $ROOT (as /workspace) into the
# container, not arbitrary host paths, so the cached sysroot has to be
# copied under $ROOT to be visible there. The download above is cached
# persistently in /tmp across runs; this copy is just a fast local `cp`.
rm -rf "${ROOT}/.ci-old-glibc"
cp -a "${OLD_GLIBC_CACHE}" "${ROOT}/.ci-old-glibc"

bash "${CACHE_DIR}/ci/build.sh" breadarr "$ROOT" sh -c '
    RUSTFLAGS="-C link-arg=--sysroot=/workspace/.ci-old-glibc -C link-arg=-L/workspace/.ci-old-glibc/usr/lib" \
    exec "$@"
' sh "$@"
