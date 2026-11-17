#!/bin/bash
# Builds a static, portable iztop binary: musl-linked, so it runs on any
# x86-64 Linux regardless of glibc version, and baseline x86-64, so it runs on
# any x86-64 CPU.
#
# A plain `cargo build --release` is fine for use on the machine that built it.
# This script is for a binary to copy to other hosts.
#
#   ./build.sh                  # needs: rustup target add x86_64-unknown-linux-musl
#   IZTOP_BUILD_STD=1 ./build.sh
#                               # also needs: rustup component add rust-src
#
# Some distributions (CachyOS, for one) ship a Rust toolchain whose
# precompiled standard library is built for x86-64-v4 and contains AVX-512
# instructions, whatever -C target-cpu says. A binary built with it dies with
# "Illegal instruction" on CPUs without AVX-512. IZTOP_BUILD_STD=1 rebuilds
# the standard library from source for baseline x86-64, which avoids that. It
# uses the unstable -Z build-std flag, enabled on a stable compiler through
# RUSTC_BOOTSTRAP.
#
# Either way the result is checked for AVX-512 instructions and refused if any
# are found.
set -euo pipefail
cd "$(dirname "$0")"

target=x86_64-unknown-linux-musl
export RUSTFLAGS="${RUSTFLAGS:-} -C target-cpu=x86-64"

if [ "${IZTOP_BUILD_STD:-0}" = 1 ]; then
    RUSTC_BOOTSTRAP=1 cargo build --release --target "$target" \
        -Z build-std=std,panic_abort
else
    cargo build --release --target "$target"
fi

bin="target/$target/release/iztop"
if command -v objdump >/dev/null; then
    # grep -c prints 0 AND exits 1 on no match, so the exit status is discarded.
    n=$(objdump -d "$bin" | grep -c -E '%zmm|vpxorq' || true)
    if [ "$n" != "0" ]; then
        echo "FAIL: $n AVX-512 instructions in $bin; it will not run on CPUs without AVX-512." >&2
        echo "Your toolchain's standard library was built for x86-64-v4. Rebuild with:" >&2
        echo "    IZTOP_BUILD_STD=1 ./build.sh" >&2
        exit 1
    fi
    echo "OK: no AVX-512 instructions"
else
    echo "WARNING: objdump not found (binutils); the AVX-512 check was skipped." >&2
fi
file "$bin" 2>/dev/null || true
sha256sum "$bin"
