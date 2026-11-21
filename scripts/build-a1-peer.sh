#!/usr/bin/env bash
# Run from the repository root in Ubuntu WSL2, after build-a1-native.sh.
set -euo pipefail

build=$(realpath "${1:?usage: bash scripts/build-a1-peer.sh BUILD_DIRECTORY [OUTPUT_DIRECTORY]}")
output=$(realpath -m "${2:-target/a1}")
target=x86_64-pc-windows-gnu
bash scripts/build-a1-codecs.sh "$build/prefix" "$output"
export CARGO_TARGET_DIR="$build/cargo-target"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
export CARGO_NET_GIT_FETCH_WITH_CLI=true
export SKYBER_A1_NATIVE_DIR="$output"
cargo build --locked --release --target "$target" -p skyber-media-kyber --features a1-native
install -m 755 "$CARGO_TARGET_DIR/$target/release/skyber-a1-peer.exe" "$output/"
