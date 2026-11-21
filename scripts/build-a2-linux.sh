#!/usr/bin/env bash
set -euo pipefail

[[ $(uname -sm) == "Linux x86_64" ]]
root=$(realpath -m "${1:?usage: bash scripts/build-a2-linux.sh BUILD_DIRECTORY [OUTPUT_DIRECTORY]}")
output=$(realpath -m "${2:-target/a2/linux}")
prefix="$root/prefix"
jobs=${SKYBER_BUILD_JOBS:-$(nproc)}
export CC=gcc-13
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$CC"
export CARGO_NET_GIT_FETCH_WITH_CLI=true
export PKG_CONFIG_LIBDIR="$prefix/lib/pkgconfig"
unset PKG_CONFIG_PATH
mkdir -p "$root/sources" "$prefix" "$output" "$root/opus" "$root/x264" "$root/ffmpeg"
source scripts/a1-codec-sources.sh
fetch_codec_sources

tar -xf "$root/sources/opus-1.6.1.tar.gz" -C "$root/sources"
tar -xf "$root/sources/FFmpeg-n8.1.tar.gz" -C "$root/sources"
(
    cd "$root/opus"
    CFLAGS="-O2 -fPIC" "$root/sources/opus-1.6.1/configure" --prefix="$prefix" \
        --disable-shared --enable-static --disable-doc --disable-extra-programs
    make -j "$jobs"
    make install
)
(
    cd "$root/x264"
    "$root/sources/x264/configure" --prefix="$prefix" --enable-pic \
        --enable-static --disable-cli --disable-opencl
    make -j "$jobs"
    make install
)
(
    cd "$root/ffmpeg"
    "$root/sources/FFmpeg-n8.1/configure" --cc="$CC" --prefix="$prefix" \
        --pkg-config-flags=--static --disable-shared --enable-static --enable-pic \
        --disable-autodetect --disable-doc --disable-programs --disable-network --disable-everything \
        --enable-gpl --enable-version3 --disable-nonfree --enable-libx264 --enable-libopus \
        --enable-encoder=libx264,libopus --enable-decoder=h264,libopus --enable-parser=h264
    make -j "$jobs"
    make install
)
flags=$(pkg-config --static --cflags --libs libavcodec libavutil)
read -r -a native_flags <<< "$flags"
"$CC" -std=c11 -O2 -Wall -Wextra -Werror -fPIC -shared -Wl,-z,defs \
    -Wl,--exclude-libs,ALL -o "$output/libskyber_a1_codecs.so" native/a1/codec.c "${native_flags[@]}"
export CARGO_TARGET_DIR="$root/cargo-target"
export SKYBER_A1_NATIVE_DIR="$output"
target=x86_64-unknown-linux-gnu
cargo build --locked --release --target "$target" -p skyber-media-kyber --features a1-native
install -m 755 "$CARGO_TARGET_DIR/$target/release/skyber-a1-peer" "$output/"
(
    git rev-parse HEAD
    rustc -Vv
    "$CC" --version
    nasm -v
    ldd "$output/skyber-a1-peer"
    ldd "$output/libskyber_a1_codecs.so"
    sha256sum "$output/skyber-a1-peer" "$output/libskyber_a1_codecs.so"
) > "$output/build-inventory.txt"
