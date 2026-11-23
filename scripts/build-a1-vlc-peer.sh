#!/usr/bin/env bash
# Build the opt-in Windows peer that presents synthetic Kymux video in Kyber VLC.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
output=$(realpath -m "${1:-$repo_root/target/a2-2/win}")
rootfs=$(realpath "${2:-$repo_root/build/kyber-rootfs}")
target=x86_64-pc-windows-gnu
cargo_target_dir=$(realpath -m "$repo_root/target/a2-2/vlc-wincargo")
pkgconfig_dir=$(mktemp -d "$repo_root/target/a2-2/vlc-pkgconfig.XXXXXX")

test -f "$output/libskyber_a1_codecs.dll.a" || {
    echo "Missing Skyber native codec import library in $output" >&2
    exit 1
}
test -f "$rootfs/lib/pkgconfig/libvlc.pc" || {
    echo "Missing pinned Kyber VLC pkg-config metadata in $rootfs" >&2
    exit 1
}
cp -a "$rootfs/lib/pkgconfig/." "$pkgconfig_dir/"
# Kyber's .pc files retain the container installation prefix; point those
# linker paths at this verified local copy of the same pinned rootfs.
for file in "$pkgconfig_dir"/*.pc; do
    sed -i "s|/tmp/skyber-rootfs|$rootfs|g" "$file"
done

export CARGO_TARGET_DIR="$cargo_target_dir"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
export CARGO_NET_GIT_FETCH_WITH_CLI=true
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_LIBDIR="$pkgconfig_dir"
export SKYBER_A1_NATIVE_DIR="$output"

cd "$repo_root"
cargo build --locked --release --target "$target" -p skyber-media-kyber \
    --features a1-native,upstream-vlc-player --bin skyber-a1-peer
install -m 755 "$CARGO_TARGET_DIR/$target/release/skyber-a1-peer.exe" \
    "$output/skyber-a1-vlc-peer.exe"
sha256sum "$output/skyber-a1-vlc-peer.exe"
