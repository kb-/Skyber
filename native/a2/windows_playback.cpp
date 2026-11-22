#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <mmsystem.h>

#include <algorithm>
#include <array>
#include <charconv>
#include <chrono>
#include <cstdint>
#include <iostream>
#include <string_view>
#include <system_error>

namespace {
constexpr int width = 320;
constexpr int height = 180;
constexpr unsigned i420_size = width * height * 3 / 2;

int play_audio(HANDLE input, UINT device) {
    const auto helper_start = std::chrono::steady_clock::now();
    WAVEFORMATEX format{WAVE_FORMAT_PCM, 2, 48000, 192000, 4, 16, 0};
    HWAVEOUT output = nullptr;
    const MMRESULT opened = waveOutOpen(&output, device, &format, 0, 0, CALLBACK_NULL);
    if (opened != MMSYSERR_NOERROR) {
        std::cerr << "Could not open Windows audio output (" << opened << ")\n";
        return 1;
    }
    struct Block {
        std::array<std::int16_t, 1920> samples{};
        WAVEHDR header{};
        bool prepared = false;
    };
    std::array<Block, 4> blocks;
    struct CloseAudio {
        HWAVEOUT output;
        std::array<Block, 4> &blocks;
        ~CloseAudio() {
            waveOutReset(output);
            for (auto &block : blocks) {
                if (block.prepared) waveOutUnprepareHeader(output, &block.header, sizeof(block.header));
            }
            waveOutClose(output);
        }
    } cleanup{output, blocks};
    const auto release = [output](Block &block) {
        if (!block.prepared) return true;
        const auto start = GetTickCount64();
        for (;;) {
            const auto status = waveOutUnprepareHeader(output, &block.header, sizeof(block.header));
            if (status == MMSYSERR_NOERROR) {
                block.prepared = false;
                return true;
            }
            if (status != WAVERR_STILLPLAYING || GetTickCount64() - start >= 2000) return false;
            Sleep(2);
        }
    };

    unsigned delivered = 0;
    for (std::size_t index = 0;; index = (index + 1) % blocks.size()) {
        auto &block = blocks[index];
        if (!release(block)) return 1;
        auto *bytes = reinterpret_cast<char *>(block.samples.data());
        DWORD used = 0;
        while (used < sizeof(block.samples)) {
            DWORD received = 0;
            if (!ReadFile(input, bytes + used, static_cast<DWORD>(sizeof(block.samples)) - used, &received, nullptr)) {
                if (GetLastError() == ERROR_BROKEN_PIPE) break;
                return 1;
            }
            if (!received) break;
            used += received;
        }
        if (!used) break;
        if (used % format.nBlockAlign) return 1;
        for (DWORD i = 0; i < used / sizeof(block.samples[0]); ++i) {
            block.samples[i] = static_cast<std::int16_t>(block.samples[i] / 5);
        }
        block.header = {};
        block.header.lpData = bytes;
        block.header.dwBufferLength = used;
        if (waveOutPrepareHeader(output, &block.header, sizeof(block.header)) != MMSYSERR_NOERROR) return 1;
        block.prepared = true;
        if (waveOutWrite(output, &block.header, sizeof(block.header)) != MMSYSERR_NOERROR) return 1;
        delivered += used / format.nBlockAlign;
    }
    for (auto &block : blocks) {
        if (!release(block)) return 1;
    }
    const auto helper_timestamp_us = std::chrono::duration_cast<std::chrono::microseconds>(
        std::chrono::steady_clock::now() - helper_start).count();
    std::cerr << "SKYBER_AUDIO_DRAINED " << delivered << " " << helper_timestamp_us << '\n';
    std::cerr << "Played " << delivered << " stereo sample frames\n";
    return delivered ? 0 : 1;
}

struct Viewer {
    std::array<std::uint8_t, width * height * 4> image{};
    unsigned rendered = 0;
    unsigned superseded = 0;
    std::uint64_t packet_id = 0;
    bool awaiting_paint = false;
    std::chrono::steady_clock::time_point helper_start = std::chrono::steady_clock::now();
};

std::uint8_t channel(int component) {
    return static_cast<std::uint8_t>(std::clamp(component, 0, 255));
}

void update(Viewer &viewer, const std::array<std::uint8_t, i420_size> &frame,
            std::uint64_t packet_id) {
    if (viewer.awaiting_paint) ++viewer.superseded;
    for (int row = 0; row < height; ++row) {
        for (int col = 0; col < width; ++col) {
            const int pixel = row * width + col;
            const int uv = row / 2 * (width / 2) + col / 2;
            const int y = std::max(0, frame[pixel] - 16);
            const int u = frame[width * height + uv] - 128;
            const int v = frame[width * height * 5 / 4 + uv] - 128;
            viewer.image[pixel * 4] = channel((298*y + 516*u + 128) >> 8);
            viewer.image[pixel * 4 + 1] = channel((298*y - 100*u - 208*v + 128) >> 8);
            viewer.image[pixel * 4 + 2] = channel((298*y + 409*v + 128) >> 8);
        }
    }
    viewer.packet_id = packet_id;
    viewer.awaiting_paint = true;
    ++viewer.rendered;
}

LRESULT CALLBACK window_proc(HWND window, UINT message, WPARAM wparam, LPARAM lparam) {
    if (message == WM_NCCREATE) {
        const auto *create = reinterpret_cast<const CREATESTRUCTW *>(lparam);
        SetWindowLongPtrW(window, GWLP_USERDATA, reinterpret_cast<LONG_PTR>(create->lpCreateParams));
    } else if (message == WM_DESTROY) {
        PostQuitMessage(0);
        return 0;
    } else if (message == WM_PAINT) {
        auto *viewer = reinterpret_cast<Viewer *>(GetWindowLongPtrW(window, GWLP_USERDATA));
        PAINTSTRUCT paint{};
        const HDC dc = BeginPaint(window, &paint);
        RECT rect{};
        GetClientRect(window, &rect);
        FillRect(dc, &rect, static_cast<HBRUSH>(GetStockObject(BLACK_BRUSH)));
        if (viewer && viewer->rendered) {
            const auto display_width = std::min(rect.right, rect.bottom * width / height);
            const auto display_height = display_width * height / width;
            BITMAPINFO bitmap{};
            bitmap.bmiHeader.biSize = sizeof(BITMAPINFOHEADER);
            bitmap.bmiHeader.biWidth = width;
            bitmap.bmiHeader.biHeight = -height;
            bitmap.bmiHeader.biPlanes = 1;
            bitmap.bmiHeader.biBitCount = 32;
            StretchDIBits(dc, (rect.right - display_width) / 2, (rect.bottom - display_height) / 2,
                display_width, display_height, 0, 0, width, height, viewer->image.data(),
                &bitmap, DIB_RGB_COLORS, SRCCOPY);
        }
        EndPaint(window, &paint);
        if (viewer && viewer->awaiting_paint) {
            const auto helper_timestamp_us = std::chrono::duration_cast<std::chrono::microseconds>(
                std::chrono::steady_clock::now() - viewer->helper_start).count();
            std::cerr << "SKYBER_PRESENT_VIDEO " << viewer->packet_id << " "
                      << helper_timestamp_us << '\n';
            viewer->awaiting_paint = false;
        }
        return 0;
    }
    return DefWindowProcW(window, message, wparam, lparam);
}

int play_video(HANDLE input, bool screen) {
    const auto instance = GetModuleHandleW(nullptr);
    WNDCLASSW cls{};
    cls.hInstance = instance;
    cls.lpfnWndProc = window_proc;
    cls.hCursor = LoadCursor(nullptr, IDC_ARROW);
    cls.lpszClassName = L"SkyberGateViewer";
    if (!RegisterClassW(&cls)) return 1;
    Viewer viewer;
    const auto window = CreateWindowExW(0, cls.lpszClassName,
        screen ? L"Skyber remote screen" : L"Skyber remote camera",
        WS_OVERLAPPEDWINDOW, screen ? 480 : 40, 100, 440, 300,
        nullptr, nullptr, instance, &viewer);
    if (!window) return 1;
    ShowWindow(window, SW_SHOWNOACTIVATE);
    std::array<std::uint8_t, i420_size> frame{};
    std::array<std::uint8_t, sizeof(std::uint64_t)> packet_id_bytes{};
    std::uint64_t packet_id = 0;
    DWORD packet_id_used = 0;
    DWORD used = 0;
    while (IsWindow(window)) {
        MSG message{};
        while (PeekMessageW(&message, nullptr, 0, 0, PM_REMOVE)) {
            if (message.message == WM_QUIT) break;
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        if (!IsWindow(window)) break;
        DWORD available = 0;
        if (!PeekNamedPipe(input, nullptr, 0, nullptr, &available, nullptr)) {
            const DWORD error = GetLastError();
            DestroyWindow(window);
            if (error != ERROR_BROKEN_PIPE || packet_id_used != 0 || used != 0 || !viewer.rendered) return 1;
            break;
        }
        if (available) {
            DWORD received = 0;
            auto *destination = packet_id_used < packet_id_bytes.size()
                ? packet_id_bytes.data() + packet_id_used : frame.data() + used;
            const auto remaining = packet_id_used < packet_id_bytes.size()
                ? static_cast<DWORD>(packet_id_bytes.size()) - packet_id_used
                : static_cast<DWORD>(frame.size()) - used;
            const auto request = std::min(available, remaining);
            if (!ReadFile(input, destination, request, &received, nullptr) || !received) {
                DestroyWindow(window);
                return 1;
            }
            if (packet_id_used < packet_id_bytes.size()) {
                packet_id_used += received;
                if (packet_id_used == packet_id_bytes.size()) {
                    packet_id = 0;
                    for (std::size_t i = 0; i < packet_id_bytes.size(); ++i) {
                        packet_id |= static_cast<std::uint64_t>(packet_id_bytes[i]) << (i * 8);
                    }
                }
            } else {
                used += received;
            }
            if (used == frame.size()) {
                used = 0;
                update(viewer, frame, packet_id);
                InvalidateRect(window, nullptr, FALSE);
                UpdateWindow(window);
                packet_id_used = 0;
            }
        } else {
            MsgWaitForMultipleObjects(0, nullptr, FALSE, 8, QS_ALLINPUT);
        }
    }
    std::cerr << "Presented " << viewer.rendered << " "
              << (screen ? "screen" : "camera") << " frames\n";
    std::cerr << "SKYBER_VIDEO_SUPERSEDED " << (screen ? "screen" : "camera")
              << " " << viewer.superseded << '\n';
    return 0;
}
}  // namespace

int main(int argc, char **argv) {
    if (argc < 2 || argc > 3) return 2;
    const std::string_view track = argv[1];
    if (track == "--list-audio" && argc == 2) {
        for (UINT index = 0; index < waveOutGetNumDevs(); ++index) {
            WAVEOUTCAPSW caps{};
            if (waveOutGetDevCapsW(index, &caps, sizeof(caps)) == MMSYSERR_NOERROR) {
                std::wcout << index << L"  " << caps.szPname << L'\n';
            }
        }
        return 0;
    }
    const HANDLE input = GetStdHandle(STD_INPUT_HANDLE);
    if (!input || input == INVALID_HANDLE_VALUE) return 2;
    if (track == "microphone") {
        UINT device = WAVE_MAPPER;
        if (argc == 3) {
            const std::string_view text = argv[2];
            const auto [end, error] = std::from_chars(text.data(), text.data() + text.size(), device);
            if (error != std::errc{} || end != text.data() + text.size() || device >= waveOutGetNumDevs()) {
                std::cerr << "Invalid Windows audio output index\n";
                return 2;
            }
        }
        return play_audio(input, device);
    }
    if (argc == 2 && (track == "camera" || track == "screen")) return play_video(input, track == "screen");
    return 2;
}
