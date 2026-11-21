# Gate A1 results

Run on November 21, 2026 on Windows 11 23H2 build 22631, x64, using the pinned
[Kyber SDK and native build](KYBER_SDK_AUDIT.md). The build used Ubuntu WSL2,
Rust 1.96.0, MinGW-w64 GCC 13-posix, and the Windows GNU target. The desktop
and ABI host used x64 MSVC. Reproduction commands are in the [README](../../README.md).

## Required software proof

`py -3 scripts/test-a1.py target/a1/skyber-a1-peer.exe` passed all ten cases
twice. It used two Windows processes on `127.0.0.1`, a single native Kyber
QUIC connection, a pinned test certificate, and authenticated track exchange.

| Scenario | Observed result |
| --- | --- |
| Audio in both directions | 5.781 s of overlapping decoded audio. Each peer received 290 Opus packets and decoded 278,088 samples per channel. Measured tones were 440.15 Hz and 660.05 Hz. |
| A→B and B→A independently | Both runs decoded the expected remote audio and camera stream. |
| Camera, screen and audio in both directions | 5.326 s of overlapping decoded media. Each peer decoded 325 Opus blocks, 97 camera frames, and 81 screen frames. Camera and audio continued for at least 0.5 s after screen stopped. |
| Failure paths | Wrong token, wrong certificate, mismatched track map, missing peer, and peer termination during streaming all failed within the harness deadline. |
| Repeat | A new run on the same port succeeded after termination. |

The concurrent run used H.264 at 320×180, 15 fps, and stereo Opus at 48 kHz in
20 ms blocks. Its six received tracks measured p50 0.515–0.570 ms and p95
0.734–0.921 ms from the software source call through the receiver reading the
decoded frame or PCM samples. This is same-host software timing with no physical
capture or presentation. Windows reported 31.25 ms of process CPU time per peer
for the concurrent run. The software profile used no GPU codec.

The decoder captured PCM in memory. The Opus logs reported the initial skip and
one encoder frame waiting at the finite stream boundary. The proof treats that
boundary as a live media stop. Production gapless audio and A/V synchronization
still require dedicated validation.

## Toolchain, ABI, and GPU

- `scripts/build-a1-native.sh` and `scripts/build-a1-peer.sh` completed in WSL2.
  The MSVC probe decoded 3,528 Opus samples per channel on each of three loads
  of the GNU-built DLL. The missing-DLL, missing-export, and null-output checks
  returned the expected failures. The build and ABI details are in the SDK audit.
- A separate FFmpeg 7.0.1 smoke test encoded and decoded 60 synthetic H.264
  frames at 640×360 and 30 fps with `h264_nvenc` and `h264_cuvid` on the
  GeForce RTX 3070 Laptop GPU, driver 591.44. This verifies the host's NVIDIA
  acceleration. The pinned Kyber hardware media path remains unverified.
- The stock desktop loopback flags were investigated in source. The headless
  harness does not launch the stock desktop client or open physical media devices.

The NVIDIA check used these commands from Windows PowerShell.

```powershell
ffmpeg -hide_banner -v info -y -f lavfi -i "testsrc2=size=640x360:rate=30" -frames:v 60 -an -c:v h264_nvenc -f h264 target/a1/nvidia-smoke.h264
ffmpeg -hide_banner -v info -hwaccel cuda -c:v h264_cuvid -i target/a1/nvidia-smoke.h264 -an -f null NUL
```

## Fast checks

| Command | Local result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed on Windows and Ubuntu WSL2 |
| `cargo check --locked --workspace --all-targets` | Passed on Windows and Ubuntu WSL2 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | Passed on Windows and Ubuntu WSL2 |
| `cargo nextest run --locked --workspace` | Two tests passed on each host |
| `cargo test --locked --workspace --doc` | All crates passed. The workspace has zero doctests. |
| `cargo clippy --locked --target x86_64-pc-windows-gnu -p skyber-media-kyber --features a1-native --all-targets -- -D warnings` | Passed in WSL2 |
| `cargo deny --locked --features skyber_desktop_bridge/qt check` | Passed with duplicate-version warnings and one documented advisory exception |
| `cargo audit` | Exit 0. Reported the unmaintained `rustls-pemfile` advisory RUSTSEC-2025-0134. |
| Windows Qt 6.8.3 configure and build, `all_qmllint`, `qmlformat` comparison, and `ctest` | Passed. The offscreen shell smoke test exercised the CXX-Qt bridge. |
| `actionlint -no-color` | Both workflow files passed static validation. |

The Rust dependency policy exception is scoped to the pinned Kyber dependency.
The A1 harness uses that parser with locally generated PEM files. The exception
can be removed when upstream migrates to `rustls-pki-types`.

The GitHub workflows await their first dispatched run. Linux Qt runtime,
physical media, actual Windows webcam plus screen capture, and the GPU video
surface need separate validation. Separate clang-format and clang-tidy lanes
are outside the initial setup. The native codec and ABI probe were compiled
with warnings as errors.
