#!/usr/bin/env bash
# Shared source pins for the native proof codecs.
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

download() {
    local archive="$root/sources/$1" url=$2 checksum=$3
    if [[ ! -f "$archive" ]]; then
        curl --fail --location --retry 3 --output "$archive.part" "$url"
        mv "$archive.part" "$archive"
    fi
    printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check
}
fetch_codec_sources() {
    # GitHub's x264 mirror carries the same immutable object used in Kyber's wrap.
    checkout x264 https://github.com/mirror/x264.git a8b68ebfaa68621b5ac8907610d3335971839d52
    download opus-1.6.1.tar.gz https://downloads.xiph.org/releases/opus/opus-1.6.1.tar.gz \
        6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1
    download FFmpeg-n8.1.tar.gz https://github.com/FFmpeg/FFmpeg/archive/refs/tags/n8.1.tar.gz \
        dd308201bb1239a1b73185f80c6b4121f4efdfa424a009ce544fd00bf736bb2e
}
