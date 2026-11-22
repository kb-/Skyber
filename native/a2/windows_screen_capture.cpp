#include "capture_frames.h"

#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <d3d11.h>
#include <dxgi.h>
#include <windows.graphics.directx.direct3d11.interop.h>
#include <shobjidl.h>
#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Graphics.Capture.h>
#include <winrt/Windows.Graphics.DirectX.Direct3D11.h>

#include <atomic>
#include <chrono>
#include <memory>
#include <iostream>

namespace {
using namespace winrt;
using namespace winrt::Windows::Graphics::Capture;
using namespace winrt::Windows::Graphics::DirectX;
using namespace winrt::Windows::Graphics::DirectX::Direct3D11;
using namespace std::chrono_literals;

LRESULT CALLBACK picker_window(HWND window, UINT message, WPARAM wparam, LPARAM lparam) {
    return DefWindowProcW(window, message, wparam, lparam);
}

LRESULT CALLBACK test_window_proc(HWND window, UINT message, WPARAM wparam, LPARAM lparam) {
    switch (message) {
    case WM_CREATE:
        SetTimer(window, 1, 100, nullptr);
        return 0;
    case WM_TIMER:
        InvalidateRect(window, nullptr, FALSE);
        return 0;
    case WM_PAINT: {
        PAINTSTRUCT paint{};
        const auto dc = BeginPaint(window, &paint);
        RECT bounds{};
        GetClientRect(window, &bounds);
        const auto orange = CreateSolidBrush(RGB(255, 91, 26));
        FillRect(dc, &bounds, orange);
        DeleteObject(orange);
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, RGB(0, 0, 0));
        const auto text = std::wstring(L"SKYBER TEST   ") + std::to_wstring(GetTickCount64() / 100);
        DrawTextW(dc, text.c_str(), -1, &bounds, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        EndPaint(window, &paint);
        return 0;
    }
    case WM_DESTROY:
        KillTimer(window, 1);
        PostQuitMessage(0);
        return 0;
    default:
        return DefWindowProcW(window, message, wparam, lparam);
    }
}

struct Window {
    HWND handle = nullptr;
    ~Window() { if (handle && IsWindow(handle)) DestroyWindow(handle); }
};

bool pump_window() {
    MSG message{};
    while (PeekMessageW(&message, nullptr, 0, 0, PM_REMOVE)) {
        if (message.message == WM_QUIT) return false;
        TranslateMessage(&message);
        DispatchMessageW(&message);
    }
    return true;
}

GraphicsCaptureItem choose_item(Window &window) {
    constexpr wchar_t window_class[] = L"SkyberCaptureConsent";
    const auto instance = GetModuleHandleW(nullptr);
    WNDCLASSW cls{};
    cls.lpfnWndProc = picker_window;
    cls.hInstance = instance;
    cls.lpszClassName = window_class;
    if (!RegisterClassW(&cls)) throw_last_error();
    window.handle = CreateWindowExW(0, window_class, L"Skyber capture picker",
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU, CW_USEDEFAULT, CW_USEDEFAULT,
        520, 120, nullptr, nullptr, instance, nullptr);
    if (!window.handle) throw_last_error();
    ShowWindow(window.handle, SW_SHOWNORMAL);
    SetForegroundWindow(window.handle);
    GraphicsCapturePicker picker;
    check_hresult(picker.as<IInitializeWithWindow>()->Initialize(window.handle));
    auto selection = picker.PickSingleItemAsync();
    const auto deadline = std::chrono::steady_clock::now() + 60s;
    while (selection.Status() == winrt::Windows::Foundation::AsyncStatus::Started) {
        if (!pump_window() || !IsWindow(window.handle) || std::chrono::steady_clock::now() >= deadline) {
            selection.Cancel();
            return nullptr;
        }
        Sleep(10);
    }
    if (selection.Status() == winrt::Windows::Foundation::AsyncStatus::Canceled) return nullptr;
    return selection.GetResults();
}

template<typename T>
struct CloseCapture {
    T object;
    ~CloseCapture() { try { object.Close(); } catch (...) {} }
};

int stream(GraphicsCaptureItem const &item, unsigned duration_ms, const wchar_t *&step) {
    com_ptr<ID3D11Device> device;
    com_ptr<ID3D11DeviceContext> context;
    step = L"create Direct3D device";
    check_hresult(D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0, D3D11_SDK_VERSION,
        device.put(), nullptr, context.put()));
    com_ptr<::IInspectable> inspectable;
    step = L"create WinRT device";
    check_hresult(CreateDirect3D11DeviceFromDXGIDevice(device.as<IDXGIDevice>().get(), inspectable.put()));
    const auto d3d = inspectable.as<IDirect3DDevice>();
    step = L"read selected source size";
    const auto size = item.Size();
    if (size.Width <= 0 || size.Height <= 0 || size.Width > 8192 || size.Height > 8192) {
        throw hresult_invalid_argument();
    }
    step = L"create frame pool";
    CloseCapture pool{Direct3D11CaptureFramePool::CreateFreeThreaded(
        d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)};
    step = L"create capture session";
    CloseCapture session{pool.object.CreateCaptureSession(item)};
    const auto closed = std::make_shared<std::atomic<bool>>(false);
    auto closed_handler = item.Closed(auto_revoke, [closed](auto const &, auto const &) { *closed = true; });
    step = L"start capture session";
    session.object.StartCapture();
    if (!capture::binary_stdout()) return 1;
    com_ptr<ID3D11Texture2D> staging;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(duration_ms);
    unsigned emitted = 0;
    while (!closed->load() && std::chrono::steady_clock::now() < deadline) {
        if (!pump_window()) break;
        step = L"get capture frame";
        auto frame = pool.object.TryGetNextFrame();
        if (!frame) {
            Sleep(10);
            continue;
        }
        const auto content = frame.ContentSize();
        if (content.Width != size.Width || content.Height != size.Height) {
            std::cerr << "Selected screen source changed size\n";
            return 1;
        }
        step = L"access capture surface";
        auto access = frame.Surface().as<::Windows::Graphics::DirectX::Direct3D11::IDirect3DDxgiInterfaceAccess>();
        com_ptr<ID3D11Texture2D> texture;
        check_hresult(access->GetInterface(__uuidof(ID3D11Texture2D), texture.put_void()));
        if (!staging) {
            D3D11_TEXTURE2D_DESC desc{};
            texture->GetDesc(&desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
            desc.MiscFlags = 0;
            step = L"create staging texture";
            check_hresult(device->CreateTexture2D(&desc, nullptr, staging.put()));
        }
        context->CopyResource(staging.get(), texture.get());
        D3D11_MAPPED_SUBRESOURCE mapped{};
        step = L"map capture frame";
        check_hresult(context->Map(staging.get(), 0, D3D11_MAP_READ, 0, &mapped));
        if (!mapped.pData || mapped.RowPitch < static_cast<UINT>(size.Width) * 4) {
            context->Unmap(staging.get(), 0);
            return 1;
        }
        const auto packed = capture::from_bgra(static_cast<const std::uint8_t *>(mapped.pData),
                                               size.Width, size.Height, mapped.RowPitch);
        context->Unmap(staging.get(), 0);
        if (!capture::send(packed.data(), packed.size())) return 1;
        ++emitted;
        Sleep(62);
    }
    return emitted ? 0 : 1;
}
}  // namespace

int capture_screen(unsigned duration_ms) {
    const wchar_t *step = L"initialize picker";
    try {
        winrt::init_apartment(winrt::apartment_type::single_threaded);
        const struct Apartment {
            ~Apartment() { winrt::uninit_apartment(); }
        } apartment;
        if (!winrt::Windows::Graphics::Capture::GraphicsCaptureSession::IsSupported()) {
            std::cerr << "Windows Graphics Capture is unavailable\n";
            return 1;
        }
        Window owner;
        step = L"pick a window or display";
        const auto item = choose_item(owner);
        if (!item) {
            std::cerr << "Screen selection cancelled\n";
            return 1;
        }
        wchar_t expected[256]{};
        const DWORD expected_length = GetEnvironmentVariableW(L"SKYBER_EXPECTED_SCREEN_TITLE", expected, 256);
        if (expected_length && (expected_length >= 256 || item.DisplayName() != expected)) {
            std::cerr << "Selection was not the disposable test window\n";
            return 1;
        }
        ShowWindow(owner.handle, SW_HIDE);
        return stream(item, duration_ms, step);
    } catch (winrt::hresult_error const &error) {
        std::wcerr << L"Screen capture could not " << step << L": " << error.message().c_str() << L'\n';
        return 1;
    }
}


int show_screen_test_window() {
    const auto instance = GetModuleHandleW(nullptr);
    WNDCLASSW cls{};
    cls.lpfnWndProc = test_window_proc;
    cls.hInstance = instance;
    cls.lpszClassName = L"SkyberCaptureTestWindow";
    if (!RegisterClassW(&cls)) return 1;
    Window window{CreateWindowExW(WS_EX_APPWINDOW, cls.lpszClassName, L"SKYBER TEST",
        WS_OVERLAPPEDWINDOW | WS_VISIBLE, 500, 320, 520, 300,
        nullptr, nullptr, instance, nullptr)};
    if (!window.handle) return 1;
    ShowWindow(window.handle, SW_SHOWNORMAL);
    SetForegroundWindow(window.handle);
    MSG message{};
    BOOL state;
    while ((state = GetMessageW(&message, nullptr, 0, 0)) > 0) {
        TranslateMessage(&message);
        DispatchMessageW(&message);
    }
    return state < 0 ? 1 : 0;
}
