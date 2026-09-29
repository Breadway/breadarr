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

# The runner's /tmp is shared by concurrent jobs, so the checkout must never
# be half-built or deleted under another job (a glob `rm -rf
# /tmp/bread-ecosystem-ci-*` here used to do exactly that):
#   - it is cloned into a private temp dir, marked .ready, then renamed into
#     place, so a job sees either a complete checkout or none;
#   - cloning takes a lock, so two jobs never clone the same rev at once;
#   - checkouts for other revs are only pruned once unused for a day. Every
#     run touches its own, so anything a job is using stays fresh.
CI_TMP="${BREAD_CI_TMP:-/tmp}"
CACHE_DIR="${CI_TMP}/bread-ecosystem-ci-${REV}"
if [ ! -f "${CACHE_DIR}/.ready" ]; then
    if command -v flock >/dev/null 2>&1; then
        exec 8>"${CI_TMP}/bread-ecosystem-ci.lock"
        flock -w 600 8 || { echo "build.sh: timed out waiting for the bread-ecosystem clone lock" >&2; exit 1; }
    fi
    if [ ! -f "${CACHE_DIR}/.ready" ]; then
        CLONE_TMP="$(mktemp -d "${CI_TMP}/bread-ecosystem-ci-clone-XXXXXX")"
        git clone --quiet "${BREAD_ECOSYSTEM_URL:-https://git.breadway.dev/Breadway/bread-ecosystem.git}" "$CLONE_TMP"
        git -C "$CLONE_TMP" checkout --quiet "$REV"
        touch "${CLONE_TMP}/.ready"
        rm -rf "$CACHE_DIR"
        mv "$CLONE_TMP" "$CACHE_DIR"
        find "$CI_TMP" -maxdepth 1 -name 'bread-ecosystem-ci-*' -mmin +1440 -exec rm -rf {} + 2>/dev/null || true
    fi
    exec 8>&-
fi
touch "$CACHE_DIR" 2>/dev/null || true

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
