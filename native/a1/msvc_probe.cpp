#define WIN32_LEAN_AND_MEAN
#include <windows.h>

#include "codec.h"

#include <bit>
#include <cstdio>
#include <filesystem>

int wmain(int argc, wchar_t** argv) {
    if (argc != 2) {
        std::fputs("Usage: skyber-a1-msvc-probe PATH_TO_DLL\n", stderr);
        return 2;
    }
    std::error_code error;
    const auto path = std::filesystem::absolute(argv[1], error);
    if (error) {
        std::fprintf(stderr, "Could not resolve DLL path (%d)\n", error.value());
        return 2;
    }
    for (int attempt = 0; attempt < 3; ++attempt) {
        const HMODULE library = LoadLibraryExW(
            path.c_str(), nullptr, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32);
        if (!library) {
            std::fprintf(stderr, "LoadLibraryExW failed (%lu)\n", GetLastError());
            return 1;
        }
        using Probe = decltype(&skyber_a1_codec_probe_v1);
        // GetProcAddress returns the address declared by codec.h for this export.
        const auto probe = std::bit_cast<Probe>(GetProcAddress(library, "skyber_a1_codec_probe_v1"));
        if (!probe || GetProcAddress(library, "skyber_a1_missing_export")) {
            std::fputs("Unexpected codec probe exports\n", stderr);
            FreeLibrary(library);
            return 1;
        }
        uint32_t samples = 0;
        const int32_t status = probe(&samples);
        const int32_t invalid_status = probe(nullptr);
        const BOOL unloaded = FreeLibrary(library);
        if (status != 0 || invalid_status >= 0 || samples < 2880 || samples > 3840 || !unloaded) {
            std::fprintf(stderr, "Opus probe failed (%d, %u samples)\n", status, samples);
            return 1;
        }
        std::printf("MSVC load %d decoded %u Opus samples and unloaded\n", attempt + 1, samples);
    }
}
