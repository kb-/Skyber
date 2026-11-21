#!/usr/bin/env bash
# Link the proof-only C ABI to the Windows software libraries built in WSL2.
set -euo pipefail

prefix=$(realpath "${1:?usage: bash scripts/build-a1-codecs.sh NATIVE_PREFIX OUTPUT_DIRECTORY}")
output=$(realpath -m "${2:?missing output directory}")
target=x86_64-w64-mingw32
export PKG_CONFIG_LIBDIR="$prefix/lib/pkgconfig"
read -r -a native_flags <<< "$("$target-pkg-config" --static --cflags --libs libavcodec libavutil)"
mkdir -p "$output"
"$target-gcc" -std=c11 -O2 -Wall -Wextra -Werror -DSKYBER_A1_BUILD_DLL \
    -shared -static -o "$output/skyber_a1_codecs.dll" native/a1/codec.c \
    "-Wl,--out-implib,$output/libskyber_a1_codecs.dll.a" "${native_flags[@]}"
"$target-objdump" -p "$output/skyber_a1_codecs.dll" | sed -n '/DLL Name/p'
