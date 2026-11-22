# Skyber

Skyber is a native one-to-one calling project built around Kyber media and
Qt Quick. Gate A1 is a headless Windows 11 feasibility proof. Two Windows
processes can exchange synthetic Opus audio and independent H.264 camera and
screen tracks over one Kyber QUIC connection.

See the [product plan](docs/PLAN_SKYBER.md), [SDK audit](docs/research/KYBER_SDK_AUDIT.md),
and [Gate A1 results](docs/research/GATE_A1_RESULTS.md).

## Run Gate A1

Open PowerShell at the repository root on Windows 11. The Ubuntu WSL2
distribution needs Rust 1.96.0 with the `x86_64-pc-windows-gnu` target,
MinGW-w64 GCC/G++ 13-posix, the matching pkg-config, GNU Make, NASM, Git, curl,
and patch. Windows needs Python 3.10+ and OpenSSL for the harness.

```powershell
wsl.exe --distribution Ubuntu -- bash -lc 'bash scripts/build-a1-native.sh ~/.cache/skyber-a1-0.29.0'
wsl.exe --distribution Ubuntu -- bash -lc 'bash scripts/build-a1-peer.sh ~/.cache/skyber-a1-0.29.0'
py -3 scripts/test-a1.py target/a1/skyber-a1-peer.exe
```

Use `--audio-only` on the Python command for the short Opus-only test.
The runner creates disposable per-peer credentials and writes test logs to a
timestamped directory under `target/a1/runs`. It asserts decoded media and
track independence, then tests failure and restart paths.

Run the following from an x64 Visual Studio Developer PowerShell after building
the DLL to verify the MSVC side of the codec ABI.

```powershell
cl.exe /nologo /W4 /WX /std:c++20 /EHsc /Fotarget/a1/msvc_probe.obj /Fetarget/a1/skyber-a1-msvc-probe.exe native/a1/msvc_probe.cpp
& .\target\a1\skyber-a1-msvc-probe.exe .\target\a1\skyber_a1_codecs.dll
```

The GitHub Actions Gate A1 workflow is manually dispatched and uses a Windows
runner with the `skyber-a1` label and this toolchain. The same commands run
locally without a runner.

## Fast and desktop checks

```text
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo nextest run --locked --workspace
cargo test --locked --workspace --doc
cargo deny --locked --features skyber_desktop_bridge/qt check
cargo audit
```

For the desktop shell, put the Qt 6.8.3 kit's `bin` directory on `PATH` and
use the corresponding compiler environment. Configure CMake with the Qt kit
root on `CMAKE_PREFIX_PATH`.

On Windows, use a short CMake binary-directory path when building from a
deeply nested worktree. CXX-Qt generates Rust build outputs containing C++
source files under that directory; long generated paths can exceed MSVC's
path limit. A repository-root directory such as `b` is suitable.

```text
cmake -S apps/desktop -B build/desktop -G Ninja -DCMAKE_BUILD_TYPE=Release
cmake --build build/desktop
cmake --build build/desktop --target all_qmllint
python scripts/check-qml-format.py apps/desktop/Main.qml
ctest --test-dir build/desktop --output-on-failure
```

For the native Ubuntu build, two-host Wi-Fi suite, and guest SSH and desktop setup, see [Run Gate A2-0](docs/RUN_GATE_A2_0.md).
