# Playback boundary for A2-1c

The opt-in gate peer decodes received packets through the existing C
codec shim. The shim copies stereo S16LE PCM or packed I420 into
caller-owned memory. Rust owns the decoder, bounded per-track queues,
and playback process lifetime. The existing headless decode path
continues to report samples and frame fingerprints.

The platform playback helpers have no network or capture input. On
Windows the helper uses WinMM for PCM output and Win32 for separate
aspect-preserving camera and screen windows. On Ubuntu 26.04.1 LTS it
uses the existing PipeWire PulseAudio service for PCM and converts
I420 with GStreamer for separate GTK4 windows. These are gate viewers.
The production Qt Quick presentation and GPU surface proof remain Gate B
work.

Each viewer follows the lifetime of its own track. End of a track
closes its corresponding window. Closing a viewer discards subsequent
presentation frames for that track while decoding can continue. The
audio helper uses the current default output or an explicitly selected
one and attenuates the test signal to 20 percent amplitude. The caller
opts in with `--playback`. The microphone is never opened by playback.

## Build and run

On Windows, run from an x64 Visual Studio Developer PowerShell:

~~~powershell
New-Item -ItemType Directory -Force target/a2-1c
cl /nologo /W4 /WX /std:c++20 /EHsc /Fotarget/a2-1c/windows_playback.obj /Fetarget/a2-1c/skyber-windows-playback.exe native/a2/windows_playback.cpp winmm.lib user32.lib gdi32.lib
wsl.exe --distribution Ubuntu -- bash -lc 'bash scripts/build-a1-peer.sh ~/.cache/skyber-a1-0.29.0 target/a2-1c'
& target/a2-1c/skyber-windows-playback.exe --list-audio
py -3 scripts/test-a2-playback.py target/a2-1c/skyber-a1-peer.exe --audio-output INDEX
~~~

On Ubuntu, the helper additionally needs the GTK4 introspection bindings
and the GStreamer `appsrc`, `appsink`, `videoconvert`, `volume`, and
`pulsesink` elements. Build with `scripts/build-a2-linux.sh` and run from
the active GNOME session:

~~~bash
wpctl status -n
python3 scripts/test-a2-playback.py target/a2/linux/skyber-a1-peer \
  --audio-output SINK_NAME
~~~

The output argument can be omitted to use the system default. The test
publishes a low-volume tone and two distinguishable synthetic video
patterns over one authenticated Kyber connection. Screen ends first.
The test verifies that camera and audio continue, and checks the raw
byte count on every playback pipe plus completion from each helper.
Audible sound at a physical speaker and interactive viewing are
separate observations.

## Verification on November 21, 2026

Two loopback peers ran on each target. Each receiver passed 180 decoded
camera frames, 105 screen frames, and 575,688 stereo sample frames to its
playback helpers. Windows used the Realtek speakers, and Ubuntu used
the VirtualBox analog stereo output. The audio helpers reported completed
playback on both platforms. The operator confirmed hearing the tone on
each physical PC.

| Check | Evidence |
| --- | --- |
| Windows synthetic playback | `20261121T220626.167522Z`. Both distinct video patterns were visible in the native viewers. The screen viewer closed before the camera viewer. |
| Windows viewer dismissal | `20261121T220829.700078Z`. Closing the screen window early preserved decoded tracks and complete camera and audio playback. |
| Ubuntu 26.04.1 LTS Wayland synthetic playback | `20261121T221002.798853Z`. The GTK viewers reported 164 camera and 87 screen updates. The latest-frame slot can replace a frame before the GUI paints it. |

Test screenshots and process logs stayed under `target/a2-1c`. The
Windows A1 matrix and MSVC codec ABI probe passed. The native codec
tests verified packed I420 and PCM output. These observations establish
decoded output and per-track viewer lifetime.

## Cross-host live verification on November 21, 2026

The Windows 11 PC (`192.168.1.30`) and Ubuntu 26.04.1 LTS VirtualBox
guest (`192.168.1.36`) exchanged live media over Wi-Fi. Ubuntu initiated
each authenticated Kyber connection and Windows listened. Both capture
directions delivered live microphone and camera packets, and the
receiving platform reported the matching decoded playback bytes.

| Call | Live source and decoded output |
| --- | --- |
| Windows to Ubuntu | 291 microphone packets and 59 camera packets sent and received. Ubuntu decoded and passed 1,116,192 audio bytes and 5,097,600 camera bytes to playback. |
| Ubuntu to Windows | Logitech C922 and microphone sent 293 microphone packets and 92 camera packets. Windows received and decoded every packet, with 1,123,872 audio bytes and 7,948,800 camera bytes passed to its playback helpers. |
| Windows screen to Ubuntu | The operator selected the disposable `SKYBER TEST` window. All 58 screen, 515 microphone, and 118 camera packets arrived and decoded. Ubuntu presented 58 screen updates. Camera and microphone overlapped the screen for 3.18 seconds, then continued for 6.44 and 6.32 seconds after its last frame. |

The Windows screen helper was configured to reject any selected title
other than `SKYBER TEST`. No camera, microphone, or screen payload was
written to the result directory. The structured results are under
`target/a2-1c/live/20261121T221703.082189Z`.

In this bridged Wi-Fi topology, a separate Windows-originated UDP
datagram to a temporary Ubuntu listener was not observed within 12
seconds. Ubuntu's UFW service was inactive. The tested calls therefore
used Ubuntu as the QUIC connection initiator while media flowed in both
directions. This probe does not establish the cause; determine whether
host-initiated sessions are required when validating bare-metal Linux
or a different Wi-Fi bridge before treating this as a platform limit.
