# Windows capture boundary for A2-1a

Use Windows Media Foundation for camera and microphone discovery and
sampling. The source reader uses a bounded asynchronous request and a
capture worker owns the source. For screen capture, use the Windows
Graphics Capture picker attached to the Qt top-level window. Keep the
selected window or monitor on its own track and retain the OS capture
indicator. System audio is outside this path.

The pinned Kyber transport already sends independent Opus and H.264
tracks. It accepts encoded packets. The existing codec shim now accepts
caller-owned frames as well as its synthetic sources. The proof profile
accepts 48 kHz stereo floating-point PCM in 20 ms blocks and 320 by 180
I420 video. Platform capture code must convert the negotiated device
format on a worker before handing frames to the codec. This profile
bounds the first integration proof. Device selection, capture lifetime,
and media format will belong to the capture adapter. Qt handles selection
and capture indicators, and Rust owns publication and track state.

Do not widen the FFI to pass COM objects or Qt objects. Use caller-owned
buffers at the codec boundary and bound queues independently for each
track. Device denial or removal ends that source and should surface a
track-specific error. A live source is selected only with explicit user
action.

## Native proof

From an x64 Visual Studio Developer PowerShell at the repository root:

~~~powershell
New-Item -ItemType Directory -Force target/a2-1a
cl /nologo /W4 /WX /std:c++20 /EHsc /utf-8 /Fo"target/a2-1a/" /Fetarget/a2-1a/skyber-windows-capture.exe native/a2/windows_capture_probe.cpp native/a2/windows_screen_capture.cpp mf.lib mfplat.lib mfreadwrite.lib mfuuid.lib ole32.lib d3d11.lib windowsapp.lib user32.lib gdi32.lib
& target/a2-1a/skyber-windows-capture.exe --list camera
& target/a2-1a/skyber-windows-capture.exe --list microphone
~~~

With consent, use `--capture-one camera INDEX` or
`--capture-one microphone INDEX` with the index from the preceding list.
The probe prints the sample byte count. Captured payloads stay in memory.
Run live probes from the Windows console, where device privacy settings
and any other app holding the device may affect access.

On Windows 11, both device lists succeeded and an out-of-range index
was rejected. One approved read produced a 1,382,400-byte camera
sample and a 19,200-byte microphone sample. Separately, native codec
tests encoded and decoded caller-supplied PCM and I420 fixtures in
Ubuntu WSL. The peer invokes the native helper through a local fixed-size frame pipe.
Rust owns the authenticated Kyber session, per-track queues, codec calls,
and subprocess lifetime. The MSVC helper owns Windows API objects, device
format conversion, and the OS screen picker. The MSVC and MinGW toolchains
exchange bytes across the pipe and have no shared object ownership.

Build and stage the Kyber peer in the same directory as the helper:

~~~powershell
wsl.exe --distribution Ubuntu -- bash -lc 'bash scripts/build-a1-peer.sh ~/.cache/skyber-a1-0.29.0 target/a2-1a'
py -3 scripts/test-a2-capture.py target/a2-1a/skyber-a1-peer.exe --camera 0 --microphone 0
$window = Start-Process target/a2-1a/skyber-windows-capture.exe -ArgumentList '--test-window' -PassThru
$env:SKYBER_EXPECTED_SCREEN_TITLE = 'SKYBER TEST'
py -3 scripts/test-a2-capture.py target/a2-1a/skyber-a1-peer.exe --camera 0 --microphone 0 --with-screen
Remove-Item Env:SKYBER_EXPECTED_SCREEN_TITLE
$window.CloseMainWindow()
py -3 scripts/test-a2-capture.py target/a2-1a/skyber-a1-peer.exe --camera 0 --microphone 0 --cancel-screen
~~~

Use the indexes returned by the probe. With screen enabled, select a
the orange window titled SKYBER TEST in the Windows picker. It starts the microphone and
camera after the first selected screen frame. The gate runner verifies
independent screen stop, matching packet counts, decoded media, and
concurrent output on a second Windows process. The helper stops
if the selected window closes or its dimensions change. Scope this
proof to 48 kHz mono or stereo microphone devices and the fixed
320 by 180 software video profile. Qt picker integration and ongoing
in-call track controls belong to subsequent desktop work.

## A2-1a acceptance evidence

The focused Windows 11 loopback acceptance passed on November 21, 2026.
Two independent peers used pinned Kyber over one authenticated QUIC
connection. The publishing peer used the built-in camera and microphone.
The operator selected the disposable window through the OS picker.
Only decoded observations and diagnostics were written to the run logs.

| Case | Run ID | Result |
| --- | --- | --- |
| Camera and microphone | `20261121T211457.057749Z` | Received 49 camera frames and 217 Opus packets. |
| Camera, microphone, and selected window | `20261121T212425.344409Z` | Received 123 camera frames, 517 Opus packets, and 62 screen frames. Counts matched the publisher on every track. |
| Picker cancellation | `20261121T212500.670577Z` | Both peers terminated and the receiver had no media markers. |

The receiving peer observed 3.49 s of overlap across the three live
tracks. Camera and microphone continued at least 6.26 s after the last
screen frame. These are receiver-local intervals. The Windows synthetic
regression passed all ten A1 cases. The results close the focused Windows
capture step. Ubuntu device capture and desktop playback remain A2-1b
and A2-1c work.

[Media Foundation device discovery](https://learn.microsoft.com/en-us/windows/win32/medfound/enumerating-video-capture-devices)
and [source reader](https://learn.microsoft.com/en-us/windows/win32/medfound/processing-media-data-with-the-source-reader).
[Windows Graphics Capture](https://learn.microsoft.com/en-us/windows/uwp/audio-video-camera/screen-capture).

## Windows camera retest (2026-11-22)

A hidden-process LAN run reported `HRESULT 0x80070005` while opening camera
index 0. The documented console probes were then rerun outside the sandbox.
`--list camera` returned `HD Webcam`, and `--capture-one camera 0` captured
a 1,382,400-byte sample in memory. The 60-second Windows A2 camera and
microphone loopback passed: 593 camera frames and 2,988 microphone packets
were sent, received, and decoded, with zero transport packet loss. The access
denial did not reproduce in the documented console workflow. This run did not
indicate a need to change camera Media Foundation behavior.
