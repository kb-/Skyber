# Ubuntu capture boundary for A2-1b

Use the existing Rust peer for session state, authenticated Kyber
transport, track endpoints, codec ownership, and capture process
lifetime. The opt-in Ubuntu helper uses system Python and PyGObject to
access GStreamer and the desktop portal. It supplies the same fixed-size
local frame pipe as the Windows helper. Neither helper opens a network
media connection.

The camera chain uses GStreamer V4L2 and normalizes the selected device
to 320 by 180 I420. The microphone chain uses the PipeWire PulseAudio
service with a named ALSA input and normalizes to 48 kHz stereo
float PCM. It rejects monitor sources. The screen path uses the GNOME
Wayland ScreenCast portal and connects only to its restricted PipeWire
remote and selected node. Ask for a single window or monitor and request
no persisted consent. Wait for the first approved screen frame before
starting the other selected devices when screen is enabled. QUIC
keepalives allow the authenticated peer to wait for the picker.

The pinned Kyber `kymedia` revision
`b0efdfa251a19274ff1d9bf6324deb6bee10c026` has a Linux GStreamer
video provider. Its tests exercise a raw `v4l2src` and encoded video
pipelines. The stock provider's configurable video path is in
`kyavservice/src/avconv/video.rs`, and its source list is exposed as
configured displays in `kyavservice/src/avconv/displays.rs`. The staged
Skyber software codec library uses the pinned reduced FFmpeg profile
and the direct kymux peer. It does not build the avconv provider or
expose the GNOME portal session and PipeWire descriptor through the
current Rust boundary. Keeping acquisition in the bounded Ubuntu
helper preserves the existing codec and transport path.

## Run on Ubuntu 26.04.1 LTS

The guest desktop needs Python 3 with PyGObject, GStreamer 1.0 with
the V4L2, PulseAudio, PipeWire, convert, resample, decode, and appsink
elements, plus PipeWire, WirePlumber, and the GNOME desktop portal.
The helper uses the distribution packages. The pinned Rust and native
codec build remains in `scripts/build-a2-linux.sh`. It stages the helper
beside the peer.

Check the attached video device with `v4l2-ctl --list-devices` and
`v4l2-ctl --list-formats-ext -d /dev/video0` when `v4l-utils` is
installed. Inspect audio devices with `wpctl status -n` and select an
`alsa_input.*` entry from Sources. The capture commands read real
devices and require operator consent.

~~~bash
python3 scripts/test-a2-capture.py target/a2/linux/skyber-a1-peer \
  --camera /dev/video0 --microphone SOURCE_NAME
python3 scripts/test-a2-capture.py target/a2/linux/skyber-a1-peer \
  --camera /dev/video0 --microphone SOURCE_NAME --with-screen
python3 scripts/test-a2-capture.py target/a2/linux/skyber-a1-peer \
  --camera /dev/video0 --microphone SOURCE_NAME --cancel-screen
~~~

Run the screen cases from a terminal in the active GNOME Wayland
desktop, or launch the peer through the user's systemd session manager
with its desktop environment. Select a harmless window with updating
content in the positive test. Cancel the OS picker in the negative
test. The receiver checks decoded frames, packet counts, overlapping
capture, and continued camera and audio after screen stop. The run
logs contain observations only.

## A2-1b acceptance evidence

The Ubuntu 26.04.1 LTS Wayland guest passed the focused live-source
loopback checks on November 21, 2026. The Logitech C922 was passed
through to VirtualBox and exposed separate V4L2 device nodes and an
ALSA microphone through PipeWire 1.6.2. GStreamer was 1.28.2. The
test used the x86-64 Linux peer built in Ubuntu WSL and staged in
the VM. Live payloads stayed in memory.

| Case | Run ID | Decoded observations |
| --- | --- | --- |
| USB webcam and microphone | `20261121T213351.883007Z` | 70 camera frames and 220 Opus packets. |
| Webcam, microphone, and selected GNOME window | `20261121T213733.863554Z` | 160 camera frames, 516 Opus packets, and 69 screen frames. Counts matched the publisher. |
| Picker cancellation | `20261121T213840.303926Z` | Both peers terminated and the receiver had no media markers. |

The first screen frame arrived about 22.24 s after the receiving
peer's media setup, beyond the former 10 s QUIC idle timeout.
The three live tracks overlapped for 3.51 s. Camera and audio
continued at least 6.38 s after the last screen frame. All intervals
use the receiver's monotonic clock. The screen source was a disposable
window inside the guest. Live Windows-to-Ubuntu playback, native
Ubuntu hardware, and higher-resolution formats remain later gate
work.

[GStreamer V4L2 source](https://gstreamer.freedesktop.org/documentation/video4linux2/v4l2src.html) ·
[GStreamer PulseAudio source](https://gstreamer.freedesktop.org/documentation/pulseaudio/pulsesrc.html) ·
[ScreenCast portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html)
