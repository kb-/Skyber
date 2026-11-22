# Reproducing the Skyber A2 builds and runs

Run the commands from the repository root unless noted. The test peer
uses pinned Kyber QUIC with Opus and H.264. The Rust version is selected
by [rust-toolchain.toml](../rust-toolchain.toml). For the recorded A2-2
run, use commit `99a6960` or a later branch that contains it.

| System | Role in the recorded runs |
| --- | --- |
| Windows 11 x64 | Ran the Windows peer, capture helpers, playback helper, and Python test harness. |
| Ubuntu WSL2 on Windows | Cross-compiled the Windows GNU Rust peer and codec DLL. Also hosted Rust checks and quick Linux rebuilds against the existing Linux codec library. |
| Ubuntu 26.04.1 LTS x64 Desktop in VirtualBox on a second physical PC | Ran the Linux peer and the GNOME, PipeWire, camera, and audio checks. Built the original Linux peer and codec library natively. |

## Windows build

From PowerShell, use WSL2 for the GNU binaries. The native build script
builds the pinned codec dependencies and can be reused for later peer
rebuilds.

```powershell
wsl.exe -d Ubuntu -- bash -lc 'bash scripts/build-a1-native.sh ~/.cache/skyber-a1-0.29.0'
wsl.exe -d Ubuntu -- bash -lc 'bash scripts/build-a1-peer.sh ~/.cache/skyber-a1-0.29.0 target/a2-2/win'
```

From an **x64 Visual Studio Developer PowerShell** with MSVC and the
Windows SDK installed, build the native helpers beside the peer:

```powershell
cl /nologo /W4 /WX /std:c++20 /EHsc /utf-8 /Fotarget/a2-2/win/ /Fetarget/a2-2/win/skyber-windows-capture.exe native/a2/windows_capture_probe.cpp native/a2/windows_screen_capture.cpp mf.lib mfplat.lib mfreadwrite.lib mfuuid.lib ole32.lib d3d11.lib windowsapp.lib user32.lib gdi32.lib
cl /nologo /W4 /WX /std:c++20 /EHsc /Fotarget/a2-2/win/windows_playback.obj /Fetarget/a2-2/win/skyber-windows-playback.exe native/a2/windows_playback.cpp winmm.lib user32.lib gdi32.lib
```

The playback helper was also built with MinGW during A2-2. In the WSL
shell, this command links the MinGW runtime statically so the result
can run on Windows without the build environment:

```bash
x86_64-w64-mingw32-g++ -std=c++17 -O2 -Wall -Wextra -Werror \
  -static -static-libgcc -static-libstdc++ native/a2/windows_playback.cpp \
  -lwinmm -lgdi32 -o target/a2-2/win/skyber-windows-playback.exe
```

The output directory needs the peer, codec DLL, and both helper
executables. Windows harness runs used `uv run --python 3.12` from
PowerShell, with OpenSSL available on `PATH`.

For Rust-only changes, we rebuilt in the WSL shell against the staged
Windows import library and reused the existing DLL and helpers:

```bash
SKYBER_A1_NATIVE_DIR="$PWD/target/a2-2/win" \
  CARGO_TARGET_DIR="$PWD/target/a2-2/wincargo" \
  CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
  cargo build --locked --release --target x86_64-pc-windows-gnu \
  -p skyber-media-kyber --features a1-native
install -m 755 target/a2-2/wincargo/x86_64-pc-windows-gnu/release/skyber-a1-peer.exe \
  target/a2-2/win/skyber-a1-peer.exe
```

## Ubuntu build

Build in the **Ubuntu 26.04.1 LTS guest**. The native script requires GCC
13, Make, NASM, pkg-config, Git, curl, and the repository Rust toolchain.
The live helpers require the distro Python with PyGObject, GStreamer
and its V4L2, PipeWire, and PulseAudio elements, GTK4 introspection,
and an active GNOME Wayland session for screen sharing.

```bash
bash scripts/build-a2-linux.sh ~/.cache/skyber-a2-0.29.0 target/a2-2/linux
ldd target/a2-2/linux/skyber-a1-peer
```

Keep `libskyber_a1_codecs.so`, `skyber-ubuntu-capture`, and
`skyber-ubuntu-playback` next to the Linux peer. The build script installs
the Python helpers with the executable names the peer expects. The
recorded guest runs used system `python3` for the harness and helpers
because it had the installed desktop bindings. For quick peer-only
rebuilds from WSL, we reused the compatible guest codec library and
copied the resulting Linux executable back to the guest. The guest
remained the Linux runtime under test.

The WSL command for that incremental Linux rebuild was:

```bash
SKYBER_A1_NATIVE_DIR="$PWD/target/a2-2/linux" \
  CARGO_TARGET_DIR="$PWD/target/a2-2/linuxcargo" \
  cargo build --locked --release --target x86_64-unknown-linux-gnu \
  -p skyber-media-kyber --features a1-native
```

We copied the resulting `target/a2-2/linuxcargo/x86_64-unknown-linux-gnu/release/skyber-a1-peer`
over SSH to the guest's build folder. When the Python helpers changed,
we also copied the scripts and set their executable bits on the guest.

Once the native Linux codec is available locally, the WSL checks can be
run with:

```bash
export SKYBER_A1_NATIVE_DIR="$PWD/target/a2-2/linux"
export LD_LIBRARY_PATH="$SKYBER_A1_NATIVE_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
cargo fmt --all -- --check
cargo clippy --locked -p skyber-media-kyber --features a1-native --all-targets -- -D warnings
cargo test --locked -p skyber-media-kyber --features a1-native
```

## Run order

1. **Synthetic loopback.** Run the Windows and Ubuntu harnesses before
   trying the LAN. Keep the generated reports under `target/`.

   Windows:
   ```powershell
   uv run --python 3.12 scripts/test-a1.py target/a2-2/win/skyber-a1-peer.exe
   ```

   Ubuntu:
   ```bash
   python3 scripts/test-a2.py local target/a2-2/linux/skyber-a1-peer
   ```

2. **Synthetic LAN.** Follow [the A2-0 runbook](RUN_GATE_A2_0.md) to
   prepare an authenticated run, open a scoped UDP listener, and run
   the six-case matrix. Start the Windows listener before the Ubuntu
   initiator. The test connection carries media in either direction.
3. **Live loopback and playback.** Discover Windows device indices with
   `skyber-windows-capture.exe --list camera`, `--list microphone`, and
   `skyber-windows-playback.exe --list-audio`. On Ubuntu, use
   `wpctl status -n` for audio names and select the camera's V4L2 device.
   Run the live capture harness and then the playback harness.

   Windows, using index 0 as an example:
   ```powershell
   uv run --python 3.12 scripts/test-a2-capture.py target/a2-2/win/skyber-a1-peer.exe --camera 0 --microphone 0
   uv run --python 3.12 scripts/test-a2-playback.py target/a2-2/win/skyber-a1-peer.exe
   ```

   Ubuntu, from the unlocked test desktop:
   ```bash
   python3 scripts/test-a2-capture.py target/a2-2/linux/skyber-a1-peer \
     --camera /dev/video0 --microphone SOURCE_NAME
   python3 scripts/test-a2-playback.py target/a2-2/linux/skyber-a1-peer
   ```

   Add `--with-screen` for a consented test window or `--cancel-screen`
   to exercise cancellation. For the Windows capture helper, set
   `SKYBER_EXPECTED_SCREEN_TITLE=SKYBER TEST` in the runner environment
   and keep its `--test-window` open before triggering the picker.
4. **Live VPN and 4G.** We first confirmed the split tunnel on Wi-Fi
   using a self-hosted OpenVPN server in the guest. The Windows peer
   listened on the private tunnel address and Ubuntu initiated the
   Kyber call. We ran synthetic media, one live direction at a time,
   and a 15-minute synthetic rehearsal. Then Windows moved to 4G,
   connected to the same VPN through the router's forwarded UDP port,
   and repeated the synthetic smoke run before the long live call.
   Skyber UDP, SSH, and RDP stayed on private interfaces.

   Run the live peers from their active desktop sessions. For the
   bidirectional call, use authenticated run directories with
   `profile: "av_with_screen"`, `direction: "both"`, and each peer's
   local capture device settings. Start the listener before the
   initiator:

   ```powershell
   & .\target\a2-2\win\skyber-a1-peer.exe --lan --capture --playback $listenerRun
   ```

   ```bash
   ./target/a2-2/linux/skyber-a1-peer --lan --capture --playback "$initiator_run"
   ```

   For the sustained call set `"sustained": true` and
   `"duration_ms": 900000` on both peers. Set
   `"wait_for_remote_screen": true` on the listener only and allow
   picker time with `"capture_wait_ms": 63000`. Select one disposable
   moving test window in Ubuntu first, then the Windows test window
   when its picker opens. A visible moving Ubuntu window also proved
   useful in the standalone portal capture check. Keep the peers
   connected until their JSON reports finish writing.

Compare each sender's packet IDs against the receiver's and check
decoded frame counts, video paint events, audio drain, and audio sample
continuity separately. The 4G run matched all sent IDs, but microphone
capture had gaps. See [A2-2 timing and results](ADR/A2-2_TIMING.md).
Keep private run configuration, VPN keys and profiles, and raw logs
local. Remove the temporary port forward and firewall rules after use.
