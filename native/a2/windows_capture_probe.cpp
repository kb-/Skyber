#include "capture_frames.h"
#include <windows.h>
#include <mfapi.h>
#include <mferror.h>
#include <mfidl.h>
#include <mfreadwrite.h>
#include <wrl.h>

#include <bit>
#include <charconv>
#include <chrono>
#include <condition_variable>
#include <cstring>
#include <limits>
#include <iostream>
#include <memory>
#include <mutex>
#include <string_view>
#include <system_error>

int capture_screen(unsigned duration_ms);
int show_screen_test_window();

namespace {
using Microsoft::WRL::ComPtr;
using Microsoft::WRL::Make;
using Microsoft::WRL::RuntimeClass;
using Microsoft::WRL::RuntimeClassFlags;
using Microsoft::WRL::ClassicCom;

struct ComApartment {
    HRESULT status = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    ~ComApartment() { if (SUCCEEDED(status)) CoUninitialize(); }
};

struct MediaFoundation {
    HRESULT status = MFStartup(MF_VERSION, MFSTARTUP_LITE);
    ~MediaFoundation() { if (SUCCEEDED(status)) MFShutdown(); }
};

struct Devices {
    IMFActivate **items = nullptr;
    UINT32 count = 0;

    ~Devices() {
        for (UINT32 i = 0; i < count; ++i) items[i]->Release();
        CoTaskMemFree(items);
    }
};

struct CoStringDeleter {
    void operator()(wchar_t *value) const { CoTaskMemFree(value); }
};

class SampleReader final : public RuntimeClass<RuntimeClassFlags<ClassicCom>, IMFSourceReaderCallback> {
public:
    STDMETHODIMP OnReadSample(HRESULT status, DWORD, DWORD flags, LONGLONG,
                              IMFSample *sample) noexcept override {
        std::lock_guard lock(mutex_);
        status_ = status;
        if (SUCCEEDED(status_)) {
            if (flags & MF_SOURCE_READERF_ERROR) {
                status_ = E_FAIL;
            } else if (flags & MF_SOURCE_READERF_ENDOFSTREAM) {
                status_ = MF_E_END_OF_STREAM;
            } else {
                sample_ = sample;
                status_ = sample ? sample->GetTotalLength(&bytes_) : S_FALSE;
            }
        }
        done_ = true;
        ready_.notify_one();
        return S_OK;
    }

    STDMETHODIMP OnFlush(DWORD) noexcept override { return S_OK; }
    STDMETHODIMP OnEvent(DWORD, IMFMediaEvent *) noexcept override { return S_OK; }

    HRESULT wait(DWORD &size, ComPtr<IMFSample> &sample, std::chrono::steady_clock::time_point deadline) {
        std::unique_lock lock(mutex_);
        if (!ready_.wait_until(lock, deadline, [this] { return done_; })) {
            return HRESULT_FROM_WIN32(ERROR_TIMEOUT);
        }
        size = bytes_;
        sample = std::move(sample_);
        done_ = false;
        return status_;
    }

private:
    std::mutex mutex_;
    std::condition_variable ready_;
    bool done_ = false;
    DWORD bytes_ = 0;
    ComPtr<IMFSample> sample_;
    HRESULT status_ = E_PENDING;
};

int fail(const wchar_t *step, HRESULT status) {
    std::wcerr << step << L" failed (HRESULT 0x" << std::hex
               << static_cast<unsigned long>(status) << L")\n";
    return 1;
}

HRESULT next(IMFSourceReader *reader, SampleReader *callback, DWORD stream,
             ComPtr<IMFSample> &sample, DWORD &bytes) {
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(5);
    while (std::chrono::steady_clock::now() < deadline) {
        const HRESULT pending = reader->ReadSample(stream, 0, nullptr, nullptr, nullptr, nullptr);
        if (FAILED(pending)) return pending;
        const HRESULT status = callback->wait(bytes, sample, deadline);
        if (status != S_FALSE) return status;
    }
    return HRESULT_FROM_WIN32(ERROR_TIMEOUT);
}

HRESULT stream_device(IMFSourceReader *reader, SampleReader *callback, DWORD stream,
                      bool camera, unsigned duration) {
    ComPtr<IMFMediaType> request;
    HRESULT status = MFCreateMediaType(&request);
    if (SUCCEEDED(status)) status = request->SetGUID(MF_MT_MAJOR_TYPE, camera ? MFMediaType_Video : MFMediaType_Audio);
    if (SUCCEEDED(status)) status = request->SetGUID(MF_MT_SUBTYPE, camera ? MFVideoFormat_RGB32 : MFAudioFormat_PCM);
    if (!camera && SUCCEEDED(status)) {
        status = request->SetUINT32(MF_MT_AUDIO_BITS_PER_SAMPLE, 16);
    }
    if (SUCCEEDED(status)) status = reader->SetCurrentMediaType(stream, nullptr, request.Get());
    if (FAILED(status)) return status;
    ComPtr<IMFMediaType> media_type;
    status = reader->GetCurrentMediaType(stream, &media_type);
    if (FAILED(status)) return status;

    UINT32 width = 0, height = 0, channels = 0, rate = 0, bits = 0;
    if (camera) {
        status = MFGetAttributeSize(media_type.Get(), MF_MT_FRAME_SIZE, &width, &height);
        if (FAILED(status) || !width || !height || width > 8192 || height > 8192) return E_INVALIDARG;
    } else {
        status = media_type->GetUINT32(MF_MT_AUDIO_NUM_CHANNELS, &channels);
        if (SUCCEEDED(status)) status = media_type->GetUINT32(MF_MT_AUDIO_SAMPLES_PER_SECOND, &rate);
        if (SUCCEEDED(status)) status = media_type->GetUINT32(MF_MT_AUDIO_BITS_PER_SAMPLE, &bits);
        if (FAILED(status) || (channels != 1 && channels != 2) || bits != 16 || rate != 48000) {
            return MF_E_INVALIDMEDIATYPE;
        }
    }
    if (!capture::binary_stdout()) return E_FAIL;

    std::array<float, capture::audio_samples> audio{};
    std::size_t audio_offset = 0;
    auto start = std::chrono::steady_clock::now();
    auto next_frame = start;
    unsigned emitted = 0;
    do {
        ComPtr<IMFSample> captured;
        DWORD bytes = 0;
        status = next(reader, callback, stream, captured, bytes);
        if (FAILED(status)) return status;
        const auto now = std::chrono::steady_clock::now();
        if (camera && now < next_frame) continue;
        ComPtr<IMFMediaBuffer> buffer;
        status = captured->ConvertToContiguousBuffer(&buffer);
        if (FAILED(status)) return status;
        if (camera) {
            BYTE *data = nullptr;
            DWORD length = 0;
            status = buffer->Lock(&data, nullptr, &length);
            if (FAILED(status)) return status;
            UINT32 packed_stride = width * 4;
            media_type->GetUINT32(MF_MT_DEFAULT_STRIDE, &packed_stride);
            const auto stride = static_cast<std::int64_t>(std::bit_cast<std::int32_t>(packed_stride));
            const auto row_bytes = static_cast<std::int64_t>(width) * 4;
            const auto stride_abs = stride < 0 ? -stride : stride;
            const auto last_row = (height - 1) * stride_abs;
            const bool valid = data && stride_abs >= row_bytes && last_row + row_bytes <= length;
            capture::VideoFrame frame{};
            if (valid) {
                if (stride < 0) data += last_row;
                frame = capture::from_bgra(data, static_cast<int>(width), static_cast<int>(height), stride);
            }
            buffer->Unlock();
            if (!valid || !capture::send(frame.data(), frame.size())) return E_FAIL;
            ++emitted;
            next_frame = now + std::chrono::milliseconds(62);
        } else {
            BYTE *data = nullptr;
            DWORD length = 0;
            status = buffer->Lock(&data, nullptr, &length);
            if (FAILED(status)) return status;
            bool ok = length % (channels * sizeof(std::int16_t)) == 0;
            for (DWORD offset = 0; ok && offset < length; offset += channels * sizeof(std::int16_t)) {
                std::int16_t pcm[2]{};
                std::memcpy(pcm, data + offset, channels * sizeof(std::int16_t));
                audio[audio_offset++] = pcm[0] / 32768.0f;
                audio[audio_offset++] = pcm[channels - 1] / 32768.0f;
                if (audio_offset == audio.size()) {
                    ok = capture::send(audio.data(), sizeof(audio));
                    audio_offset = 0;
                    ++emitted;
                }
            }
            buffer->Unlock();
            if (!ok) return E_FAIL;
        }
    } while (std::chrono::steady_clock::now() - start < std::chrono::milliseconds(duration));
    return emitted ? S_OK : E_FAIL;
}

int sample(IMFActivate *device, DWORD stream, bool camera, unsigned duration) {
    auto callback = Make<SampleReader>();
    if (!callback) return fail(L"Create capture callback", E_OUTOFMEMORY);
    ComPtr<IMFAttributes> attrs;
    HRESULT status = MFCreateAttributes(&attrs, 2);
    if (camera && duration && SUCCEEDED(status)) status = attrs->SetUINT32(MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, TRUE);
    if (SUCCEEDED(status)) status = attrs->SetUnknown(MF_SOURCE_READER_ASYNC_CALLBACK, callback.Get());
    if (FAILED(status)) return fail(L"Configure source reader", status);

    ComPtr<IMFMediaSource> source;
    status = device->ActivateObject(IID_PPV_ARGS(&source));
    if (FAILED(status)) return fail(L"Open selected capture device", status);

    ComPtr<IMFSourceReader> reader;
    status = MFCreateSourceReaderFromMediaSource(source.Get(), attrs.Get(), &reader);
    DWORD bytes = 0;
    if (SUCCEEDED(status)) {
        if (duration) {
            status = stream_device(reader.Get(), callback.Get(), stream, camera, duration);
        } else {
            ComPtr<IMFSample> captured;
            status = next(reader.Get(), callback.Get(), stream, captured, bytes);
        }
        if (status == HRESULT_FROM_WIN32(ERROR_TIMEOUT)) reader->Flush(stream);
    }
    reader.Reset();
    const HRESULT shutdown = device->ShutdownObject();
    if (FAILED(status)) return fail(L"Read device sample", status);
    if (FAILED(shutdown)) return fail(L"Shut down device", shutdown);
    if (duration) return 0;
    if (bytes == 0) return fail(L"Read nonempty sample", E_FAIL);
    std::wcout << L"Captured one device sample (" << bytes << L" bytes)\n";
    return 0;
}
}  // namespace

int main(int argc, char **argv) {
    const auto help = [] {
        std::cerr << "Usage: skyber-windows-capture --list <camera|microphone>\n"
                     "       skyber-windows-capture --capture-one <camera|microphone> <index>\n"
                     "       skyber-windows-capture --stream <camera|microphone> <index> <ms>\n"
                     "       skyber-windows-capture --screen <ms>\n";
        return 2;
    };
    if (argc == 2 && std::string_view(argv[1]) == "--test-window") return show_screen_test_window();
    if (argc < 3) return help();
    const std::string_view command = argv[1];
    const auto parse = [](std::string_view text, UINT32 &value) {
        const auto [end, error] = std::from_chars(text.data(), text.data() + text.size(), value);
        return error == std::errc{} && end == text.data() + text.size();
    };
    UINT32 duration = 0;
    if (command == "--screen" && argc == 3) {
        return parse(argv[2], duration) && duration >= 500 && duration <= 900000
            ? capture_screen(duration) : help();
    }
    const std::string_view kind = argv[2];
    const bool listing = command == "--list" && argc == 3;
    const bool stream = command == "--stream" && argc == 5;
    const bool capture = (command == "--capture-one" && argc == 4) || stream;
    if (stream && (!parse(argv[4], duration) || duration < 500 || duration > 900000)) return help();
    if ((!listing && !capture) || (kind != "camera" && kind != "microphone")) return help();

    UINT32 index = 0;
    if (capture && !parse(argv[3], index)) return help();
    ComApartment apartment;
    if (FAILED(apartment.status)) return fail(L"Initialize COM", apartment.status);
    MediaFoundation foundation;
    if (FAILED(foundation.status)) return fail(L"Initialize Media Foundation", foundation.status);
    ComPtr<IMFAttributes> attrs;
    HRESULT status = MFCreateAttributes(&attrs, 1);
    const bool camera = kind == "camera";
    if (SUCCEEDED(status)) {
        status = attrs->SetGUID(MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, camera
            ? MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID
            : MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_AUDCAP_GUID);
    }
    Devices devices;
    if (SUCCEEDED(status)) status = MFEnumDeviceSources(attrs.Get(), &devices.items, &devices.count);
    if (FAILED(status)) return fail(L"Enumerate capture devices", status);
    if (capture) {
        if (index >= devices.count) return fail(L"Select listed device index", E_INVALIDARG);
        return sample(devices.items[index], camera
            ? static_cast<DWORD>(MF_SOURCE_READER_FIRST_VIDEO_STREAM)
            : static_cast<DWORD>(MF_SOURCE_READER_FIRST_AUDIO_STREAM), camera, duration);
    }
    for (UINT32 i = 0; i < devices.count; ++i) {
        wchar_t *name = nullptr;
        status = devices.items[i]->GetAllocatedString(MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &name, nullptr);
        const std::unique_ptr<wchar_t, CoStringDeleter> owned_name(name);
        if (FAILED(status)) return fail(L"Read device name", status);
        std::wcout << i << L"  " << owned_name.get() << L'\n';
    }
    if (!devices.count) std::wcout << L"No capture devices found\n";
    return 0;
}
