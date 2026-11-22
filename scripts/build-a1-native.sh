#!/usr/bin/env bash
# Run in Ubuntu WSL2. Build the pinned software-codec subset for Windows x64.
set -euo pipefail

root=$(realpath -m "${1:?usage: bash scripts/build-a1-native.sh BUILD_DIRECTORY}")
target=x86_64-w64-mingw32
prefix="$root/prefix"
jobs=${SKYBER_BUILD_JOBS:-$(nproc)}
mkdir -p "$root/sources" "$prefix"

source scripts/a1-codec-sources.sh
fetch_codec_sources
checkout avconv https://gitlab.com/avconv/avconv.git 5ca80cca50573f212f2b20007b2642d4a7588a43
checkout kymedia https://gitlab.com/kyber/core/kymedia.git b0efdfa251a19274ff1d9bf6324deb6bee10c026

if [[ ! -d "$root/sources/opus-1.6.1" ]]; then
    tar -xf "$root/sources/opus-1.6.1.tar.gz" -C "$root/sources"
fi
if [[ ! -f "$root/sources/FFmpeg-n8.1/.skyber-windows-patches" ]]; then
    tar -xf "$root/sources/FFmpeg-n8.1.tar.gz" -C "$root/sources"
    for change in "$root/sources/kymedia/subprojects/packagefiles/ffmpeg/patches/"*.patch; do
        case "${change##*/}" in
            *videotoolbox*|*visionos*) continue ;;
        esac
        patch --directory="$root/sources/FFmpeg-n8.1" -p1 --fuzz=0 --input="$change"
    done
    touch "$root/sources/FFmpeg-n8.1/.skyber-windows-patches"
fi

mkdir -p "$root/opus" "$root/x264" "$root/ffmpeg" "$root/avconv"
if [[ ! -f "$prefix/lib/libopus.a" ]]; then
    (
        cd "$root/opus"
        "$root/sources/opus-1.6.1/configure" --host="$target" --prefix="$prefix" \
            --disable-shared --enable-static --disable-doc --disable-extra-programs
        make -j "$jobs"
        make install
    )
fi
if [[ ! -f "$prefix/lib/libx264.a" ]]; then
    (
        cd "$root/x264"
        "$root/sources/x264/configure" --host="$target" --cross-prefix="$target-" \
            --prefix="$prefix" --enable-static --disable-cli --disable-opencl
        make -j "$jobs"
        make install
    )
fi
(
    cd "$root/ffmpeg"
    export PKG_CONFIG_LIBDIR="$prefix/lib/pkgconfig"
    "$root/sources/FFmpeg-n8.1/configure" --enable-cross-compile --cross-prefix="$target-" \
        --arch=x86_64 --target-os=mingw32 --prefix="$prefix" --pkg-config="$target-pkg-config" \
        --pkg-config-flags=--static --disable-shared --enable-static --disable-autodetect \
        --disable-doc --disable-programs --disable-network --disable-everything \
        --enable-gpl --enable-version3 --disable-nonfree --enable-libx264 --enable-libopus \
        --enable-encoder=libx264,libopus --enable-decoder=h264,libopus \
        --enable-parser=h264 --enable-filter=sine,testsrc2,smptebars,aresample,aformat,format,null,anull
    make -j "$jobs"
    make install
)
(
    cd "$root/avconv"
    export PKG_CONFIG_LIBDIR="$prefix/lib/pkgconfig"
    "$root/sources/avconv/configure" --enable-cross-compile --cross-prefix="$target-" \
        --arch=x86_64 --target-os=mingw32 --prefix="$prefix" --pkg-config="$target-pkg-config" \
        --pkg-config-flags=--static --disable-shared --enable-static --disable-autodetect \
        --disable-doc --disable-programs --disable-providers --enable-gpl --disable-nonfree
    make -j "$jobs"
    make install
)
printf 'Kyber Windows software libraries installed at %s\n' "$prefix"
