#pragma once

#include <algorithm>
#include <array>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <fcntl.h>
#include <io.h>

namespace capture {
inline const auto process_start = std::chrono::steady_clock::now();
constexpr int width = 320;
constexpr int height = 180;
constexpr int audio_samples = 960 * 2;
using VideoFrame = std::array<std::uint8_t, width * height * 3 / 2>;

inline bool binary_stdout() {
    return _setmode(_fileno(stdout), _O_BINARY) != -1;
}

inline bool send(const void *data, std::size_t bytes) {
    const auto elapsed = std::chrono::steady_clock::now() - process_start;
    const auto timestamp = static_cast<std::uint64_t>(
        std::chrono::duration_cast<std::chrono::microseconds>(elapsed).count());
    std::array<std::uint8_t, sizeof(timestamp)> header{};
    for (std::size_t i = 0; i < header.size(); ++i) {
        header[i] = static_cast<std::uint8_t>(timestamp >> (i * 8));
    }
    return std::fwrite(header.data(), 1, header.size(), stdout) == header.size()
        && std::fwrite(data, 1, bytes, stdout) == bytes && std::fflush(stdout) == 0;
}

inline std::uint8_t channel(int value) {
    return static_cast<std::uint8_t>(std::clamp(value, 0, 255));
}

// Input rows use a signed stride so the capture adapter can also pass bottom-up images.
inline VideoFrame from_bgra(const std::uint8_t *data, int source_width, int source_height,
                           std::ptrdiff_t stride) {
    VideoFrame frame{};
    for (int y = 0; y < height; ++y) {
        const auto *row = data + (y * source_height / height) * stride;
        for (int x = 0; x < width; ++x) {
            const auto *pixel = row + (x * source_width / width) * 4;
            const int b = pixel[0], g = pixel[1], r = pixel[2];
            frame[y * width + x] = channel(((66*r + 129*g + 25*b + 128) >> 8) + 16);
            if ((x % 2 == 0) && (y % 2 == 0)) {
                const int chroma = y / 2 * (width / 2) + x / 2;
                frame[width * height + chroma] = channel(((-38*r - 74*g + 112*b + 128) >> 8) + 128);
                frame[width * height * 5 / 4 + chroma] = channel(((112*r - 94*g - 18*b + 128) >> 8) + 128);
            }
        }
    }
    return frame;
}
}
