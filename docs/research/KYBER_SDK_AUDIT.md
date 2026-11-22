# Kyber SDK audit for Gate A1

Skyber targets Kyber from kyber.tech. The Gate A1 prototype uses two
authenticated Windows processes and Kyber's native QUIC media transport.

## Source pins and licensing

The [SDK tag `0.29.0`](https://gitlab.com/kyber/core/kysdk/-/tree/0.29.0) resolves
to `ebca32a08f6488ea23750943ded52119efd584d6`. The umbrella repository holds
component Git links. The separate [Kyber product repository](https://gitlab.com/kyber/kyber)
hosts the documentation site.

| Component | Revision or release | A1 role |
| --- | --- | --- |
| kymux | `af10dbb3ea016135c0ec8e01143179ad4bda005b` | Rust QUIC, control, and media endpoints |
| kymedia | `b0efdfa251a19274ff1d9bf6324deb6bee10c026` | Capture and codec reference, build patches |
| kyctl | `92c632f9e889917a34aaa7697fb88b4991054b1c` | Stock controller, client, and player reference |
| kynput | `729244364151dfbb4e17beec3c361fe83349afb5` | SDK inventory |
| kyutil | `1ae6ee42ac1be9a91f0d25089da37e7f1ac3e42a` | SDK inventory |
| avconv | `5ca80cca50573f212f2b20007b2642d4a7588a43` | Native media graph API audit and cross-build |
| FFmpeg | `n8.1` with the kymedia wrap's Windows and common patches | A1 encoder and decoder |
| x264 | `a8b68ebfaa68621b5ac8907610d3335971839d52` | Software H.264 encoder |
| Opus | `1.6.1` | Software Opus encoder and decoder |
| VLC and vlc-rs | `d66cdddbfee408538a45e3e60d4f3b82a2f60b17`, `7cbfc51313b4bb3ab07be51505b7054e4e2c366b` | Future player integration |
| txproto | `7f84b9e76bab936f57f1c184e65fb2118a020f06` | Upstream packaging dependency |

The SDK and component [license files](https://gitlab.com/kyber/core/kysdk/-/blob/0.29.0/COPYING.md)
offer AGPLv3 and commercial terms. The pinned avconv
[license inventory](https://gitlab.com/avconv/avconv/-/blob/5ca80cca50573f212f2b20007b2642d4a7588a43/LICENSE.md)
includes files marked GPL-3.0-or-later AND AGPL-3.0-or-later, plus permissive
exceptions. x264 is GPL-2.0-or-later and Opus is BSD-3-Clause. FFmpeg's A1
configuration enables GPL and version 3 with nonfree components disabled. The
upstream avconv Meson profile enables nonfree components, so the A1 script uses
the standalone configure scripts with a reduced software-only configuration.
The VLC source and plugins have component-specific terms, while the pinned
Rust binding is MIT. Qt's open-source modules have their own GPL and LGPL
conditions. Review the actual packaged binaries, offers, and notices before any
release. Cargo license policy covers the resolved Rust graph.

## Transport and audio on localhost

`kymux` re-exports `kynet` (Quinn), `kyproto` (authenticated protocol, media
and data endpoints), and `kymux-types` (packet types). `kyproto::Connection`
allocates different endpoint ID parities to the initiator and listener, and
each can register outbound audio and video. Individual audio/video endpoints
have separate typed sender and receiver APIs. For A1, each side exchanges its
registered IDs on one Kyber data endpoint and sends codec and media packets
over Kyber reliable media endpoints.

The stock desktop [localhost instructions](https://gitlab.com/kyber/core/kyctl/-/blob/92c632f9e889917a34aaa7697fb88b4991054b1c/README.md#L199)
require `--audio=false --inputs=false`. The README and its introducing commit
`308cc75` give the flags without a rationale. The Windows [audio service](https://gitlab.com/kyber/core/kymedia/-/blob/b0efdfa251a19274ff1d9bf6324deb6bee10c026/kyavservice/src/avconv/audio.rs)
selects the preferred WASAPI loopback capture backend. The older txproto
backend selects the default audio monitor. The [native WASAPI provider](https://gitlab.com/avconv/avconv/-/blob/5ca80cca50573f212f2b20007b2642d4a7588a43/libavgrab/wasapi.c)
sets `AUDCLNT_STREAMFLAGS_LOOPBACK`, and the [VLC player](https://gitlab.com/kyber/core/kyctl/-/blob/92c632f9e889917a34aaa7697fb88b4991054b1c/kyvlcplayer/src/player.rs#L339)
starts ordinary audio playback. Windows [captures the render mix](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording)
for that output endpoint. When capture and playback select the same endpoint,
decoded samples can be captured and sent again as digital feedback. This is a
source-level inference, separate from acoustic echo and physical-device validation.

The public controller API selects desktop displays and a system-audio stream.
It has no selector for a Windows webcam, microphone, or synthetic PCM. The
lower-level native graph exposes packet and frame sources, filters, encoders,
decoders, and sinks. The Rust wrapper exposes the encoder and encoded packet sink,
while its generated header list omits the decoder and data-source APIs. The pinned
VLC binding exposes an audio output callback that the stock player does not wire
into its config.

## A1 implementation choice and boundaries

The Windows build script of `libavconv-sys` resolves FFmpeg symbols through
the txproto DLL, even for a headless software pipeline. A1 uses a small C owner
for the same pinned FFmpeg codec libraries, using `libx264` and `libopus` for
encoding and `h264` and `libopus` for decoding. The Rust peers use Kyber's
audio and video endpoints for all peer-to-peer media. Synthetic PCM originates
in memory and decoded audio stays in memory. The C boundary returns decoded
sample counts and tone observations. The Qt application remains a separate
MSVC target.

The Windows capture API exposes DXGI screen capture. The configurable
GStreamer input at this revision is Linux-specific. Synthetic screen and
camera tracks can demonstrate concurrent transport and independent lifecycle.
Physical webcam plus screen capture, local playback, AEC, GPU texture sharing,
and LAN measurements require later evidence. The upstream packet readers are
also subject to separate allocation and malformed-input review before use
with remote peers. The A1 prototype only accepts authenticated local traffic.

## Native build and Windows ABI results

Ubuntu WSL2 cross-built the software profile with MinGW-w64 GCC 13-posix,
the target pkg-config, NASM, and GNU Make. The two SHA-256-checked archives and
three Git component pins are listed in `scripts/build-a1-native.sh`. The build
installs the pinned FFmpeg libraries, x264, Opus, and avconv in the selected
build prefix. Early configure attempts found that the standalone avconv fork
rejects the FFmpeg `--disable-network` and `--enable-libx264` flags. The working
recipe builds patched FFmpeg first and then avconv. A VideoToolbox patch failed
to apply with zero fuzz, so the Windows recipe selects common and Windows
patches and excludes VideoToolbox and visionOS.

The MinGW codec DLL imports `bcrypt.dll`, `KERNEL32.dll`, and `msvcrt.dll`.
The Windows x64 MSVC probe loaded and unloaded it three times. Each invocation
decoded 3,528 Opus samples per channel from four 960-sample input blocks. The
difference is the Opus decoder's initial 312-sample skip. The null-output check
and missing-export check also passed. The missing-DLL run returned status 1
with Windows error 126. The probe verifies the narrow C ABI and caller-owned
output. It leaves C++ objects, pointers with foreign allocator ownership, and
native codec structs inside their respective modules.

## A2-0 Ubuntu synthetic codec profile

The Ubuntu x86-64 recipe uses the checksum-verified FFmpeg n8.1 release
without the Windows patch set, with the same pinned x264 and Opus and the
same application-owned C ABI. FFmpeg is configured with GPL version 3 and
nonfree components disabled. The reduced static codec libraries are linked
into the staged ELF shared library. The peer uses the existing pinned
kymux source and reliable media endpoints. The Linux software profile
requires a native build and a cross-platform run to establish compatibility.
