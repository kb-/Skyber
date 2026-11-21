#!/usr/bin/env bash
# Run in Ubuntu WSL2. Build the pinned software-codec subset for Windows x64.
set -euo pipefail

root=$(realpath "${1:?usage: bash scripts/build-a1-native.sh BUILD_DIRECTORY}")
target=x86_64-w64-mingw32
prefix="$root/prefix"
jobs=${SKYBER_BUILD_JOBS:-$(nproc)}
mkdir -p "$root/sources" "$prefix"

checkout() {
    local name=$1 url=$2 revision=$3 directory="$root/sources/$1"
    if [[ ! -d "$directory/.git" ]]; then
        git init -q "$directory"
        git -C "$directory" remote add origin "$url"
    fi
    if ! git -C "$directory" cat-file -e "$revision^{commit}" 2>/dev/null; then
        git -C "$directory" fetch --depth=1 "$url" "$revision"
    fi
    git -C "$directory" checkout --detach "$revision"
    [[ $(git -C "$directory" rev-parse HEAD) == "$revision" ]]
    printf '%s %s\n' "$name" "$revision"
}

checkout avconv https://gitlab.com/avconv/avconv.git 5ca80cca50573f212f2b20007b2642d4a7588a43
checkout kymedia https://gitlab.com/kyber/core/kymedia.git b0efdfa251a19274ff1d9bf6324deb6bee10c026
# GitHub's x264 mirror carries the same immutable object used in Kyber's wrap.
checkout x264 https://github.com/mirror/x264.git a8b68ebfaa68621b5ac8907610d3335971839d52

download() {
    local archive="$root/sources/$1" url=$2 checksum=$3
    if [[ ! -f "$archive" ]]; then
        curl --fail --location --retry 3 --output "$archive.part" "$url"
        mv "$archive.part" "$archive"
    fi
    printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check
}
download opus-1.6.1.tar.gz https://downloads.xiph.org/releases/opus/opus-1.6.1.tar.gz \
    6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1
download FFmpeg-n8.1.tar.gz https://github.com/FFmpeg/FFmpeg/archive/refs/tags/n8.1.tar.gz \
    dd308201bb1239a1b73185f80c6b4121f4efdfa424a009ce544fd00bf736bb2e

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
if [[ ! -f "$prefix/lib/libopus.a" ]]; then (
    cd "$root/opus"
    "$root/sources/opus-1.6.1/configure" --host="$target" --prefix="$prefix" \
        --disable-shared --enable-static --disable-doc --disable-extra-programs
    make -j "$jobs"
    make install
); fi
if [[ ! -f "$prefix/lib/libx264.a" ]]; then (
    cd "$root/x264"
    "$root/sources/x264/configure" --host="$target" --cross-prefix="$target-" \
        --prefix="$prefix" --enable-static --disable-cli --disable-opencl
    make -j "$jobs"
    make install
); fi
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
