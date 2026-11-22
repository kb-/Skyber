# Skyber — Product and architecture plan

Status: **Proposal / feasibility-first**  
Date: 2026-11-21  
Repository: https://github.com/kb-/Skyber  
Scope: one-to-one desktop calling; **Kyber (kyber.tech)** media technology, **not** the CRYSTALS-Kyber cryptographic algorithm.

## 1. Vision

Skyber brings back the best Skype-like desktop workflow, while using Kyber for ultra-low-latency, **bidirectional** video, audio, and screen sharing. It should be lightweight, privacy-respecting, responsive, and natural to keep open all day.

The defining interaction is a **resizable, always-on-top, borderless remote-webcam window**. It displays nothing except the actual video in normal use. Call controls appear as an overlay **only while hovering**, without changing the video area's layout or obscuring it when the pointer leaves.

Initial target: Windows 11 desktop; architect for Linux and macOS. One-to-one calls first. Prefer open source. No WebRTC media transport or WebRTC video stack; Kyber is the media basis.

## 2. Recommended stack (provisional until spikes)

| Layer | Preferred choice | Rationale / validation |
| --- | --- | --- |
| Core/application state | **Rust**, Cargo workspace, Tokio where appropriate | Matches Kyber's main language and is suited to asynchronous networking, reliable state machines, and cross-platform work. |
| AV transport, capture, codec | **Kyber SDK / existing Kyber Rust+C components**, retaining its FFmpeg/VLC work | No replacement by WebRTC. Verify exposed APIs and whether bidirectional webcam, mic, and simultaneous screen tracks actually work in the current release. |
| Desktop GUI | **Qt 6 / Qt Quick (QML)** | Well-suited to a polished, multiwindow, frameless native desktop experience. Not a webview. |
| Rust–Qt integration | **CXX-Qt**, thin bridge with bounded ownership/threading | Mature enough to prototype; verify version compatibility and compare with Qt's newer Rust Bridge before committing. Keep interface narrow so binding can change. |
| Video presentation | Direct Kyber/VLC/native video surface, ideally GPU-to-GPU | **Must be proven** in Qt Quick floating window. Avoid mandatory CPU readback/texture upload every frame. Test embedding and render-thread ownership. |
| Local persistence | SQLite with migrations | Contact metadata, preferences, call log and chat history; encrypt or otherwise protect sensitive state appropriately. |
| Signaling, presence, invitations | Small optional self-hostable Rust service | Authentication, invitation exchange, session negotiation, and presence; should never carry cleartext media. |
| Connectivity | Direct QUIC first; investigate ICE/STUN and UDP hole-punching, then relay fallback | Kyber does **not** currently supply turnkey Internet P2P/NAT traversal. ICE can be used solely for connectivity without adopting WebRTC media. QUIC needs a viable direct endpoint and secure identity binding. |
| CI | GitHub Actions: Rust checks, Qt GUI builds, headless/unit tests, manual multimedia lane | Unit coverage by default; expensive real-media/hardware tests in a separate workflow. |

**Why not Tauri/Electron as default?** Skyber is primarily a low-latency native video application with sophisticated floating windows. A webview is not prohibited but would introduce additional video-surface integration and windowing questions. QML + Rust is our first prototype. Tauri, Slint, and egui remain alternatives if an early comparative UI spike changes the evidence.

**Licensing gate:** Kyber's public documentation says the open-source SDK is **AGPLv3**, with commercial licensing offered separately. Verify the exact upstream repositories, component licenses, submodule licenses, Qt redistribution conditions, and implications for Skyber and its optional server before distributing software. Default assumption: Skyber is AGPLv3-compatible open source. Do not claim a proprietary license is permissible without review.

## 3. Non-negotiable product behavior

### 3.1 The remote-webcam floating window

- Separate independent top-level window; resizable and movable, user-toggleable **always on top**; works without the main Skyber window being open.
- **No border, title bar, shadow gutters, permanent toolbar, padding, or black sidebars**. A frame visually fills the window's client area; OS compositor decorations may be disabled as technically possible.
- Aspect ratio stays correct. Default resizing preserves the stream's aspect ratio; optionally allow free resizing with a clearly chosen **crop-to-fill** or **fit** policy. Never stretch faces. No *unintended* unused space.
- On pointer hover: unobtrusive overlay for mute, hang up, camera, screen-share state, pin/unpin, open chat/main window. Overlay auto-hides on pointer exit or inactivity. It does **not** resize/reflow the image, even temporarily.
- Hover controls and resize handles remain discoverable; support keyboard shortcuts, accessibility labels, drag from the video surface (except interactive controls), configurable minimum size, high-DPI and multi-monitor behavior, and persistence of placement.
- Always-on-top should mean the selected remote webcam stays visible when another app or shared screen is foregrounded; never promise dominance over exclusive fullscreen/system secure desktop.
- Distinguish **remote webcam**, **local self-preview**, and **remote shared screen**. Keep the webcam overlay while screen share is watched fullscreen/borderless.
- Ensure the window is a real native window. Test Windows DWM, borderless resizing hit testing, focus behavior, display scaling, monitor hotplug, and GPU render path.

### 3.2 Calls and media

- Symmetric one-to-one **camera + microphone + screen sharing**: each participant can send/receive both camera and screen simultaneously; screen and webcam are independently enabled.
- Microphone mute, camera off, screen-share on/off, local preview, active source selection, output/speaker and microphone switching without losing the call.
- Optional system-audio share after platform-specific implementation. Do not silently capture desktop audio or microphone.
- Correct echo cancellation, noise suppression, gain control, lip synchronization and audio playback buffering; validate availability rather than assuming Kyber provides AEC.
- Hardware-assisted H.264 for initial compatibility; evaluate HEVC where supported and AV1 later (Kyber currently calls AV1 experimental). Prioritize readable shared text over aggressive low-latency webcam compression.
- Adaptive bitrate, frame pacing, network congestion handling, missing-frame recovery, latency statistics and graceful reconnect.
- No automatic remote keyboard/mouse control: Skyber is calling software, not remote desktop. If remote control ever comes later, require explicit per-session grants.

### 3.3 Chat and contacts

- Searchable contact list with aliases, identity verification, online/available/busy state, favorite contacts and basic blocking.
- Add contact using invite link / QR / verified key or comparable mechanism; explicit mutual acceptance. Initial presence may be opt-in.
- One-to-one text chat **during and outside calls**, with timestamps, delivery status, unread count and persistent local history. Decide how offline delivery works before claiming it is supported.
- Reliable ordered chat messages should be carried on a separate authenticated reliable channel; not on the video frame datagram path. Clearly separate local persistence from optional encrypted offline mailbox.
- Contact and chat UI can be opened independently of the floating video window.
- Privacy defaults: no forced third-party account, no ad/tracking SDKs, minimal metadata, clear indicators for active camera/mic/screen capture, cancel sharing on hang-up.

## 4. Architecture

```text
                    Skyber desktop (each participant)
  ┌───────────────────────────────────────────────────────┐
  │ Native Qt Quick UI                                   │
  │  Contacts / chat / calls / settings                  │
  │  Native frameless webcam overlay                     │
  └─────────────────────────┬─────────────────────────────┘
                            │ thin CXX-Qt API
  ┌─────────────────────────▼─────────────────────────────┐
  │ Rust application core                                │
  │ Identity, contacts, session FSM, media track manager │
  │ SQLite, signaling, device/session permission policy  │
  │ Metrics, recovery and lifecycle                      │
  └──────────────┬──────────────────────┬─────────────────┘
                 │                      │
       Kyber media adapter      Signaling + connectivity
       Capture / encode /       Invite/auth/presence,
       QUIC / decode /          ICE/STUN/UDP reachability,
       direct output            optional forwarding relay
                 │                      │
                 └──────── QUIC media ──┘
                         to other peer
```

- **Transport:** each participant runs receiving and publishing functionality. May require two opposing Kyber sessions/roles initially; a unified symmetric session is an *implementation goal*, not a proven SDK feature.
- **Track IDs:** webcam, microphone, screen and optionally system audio have independent start/stop, quality and subscription state. Verify Kyber's simultaneous-stream semantics.
- **Control plane:** authenticated session state and typed messages (invites, SDP-like codec/track negotiation if needed, hangup, mute/share indicators, chat). Do not assume existing Kyber controller metadata API is sufficient.
- **Network:** first LAN/local-address direct tests. For real internet, STUN-assisted candidates and NAT traversal where feasible; explicitly supported relay mode for hard NAT / blocked UDP. QUIC itself does not solve NAT traversal. Preserve call security through any relay, including if the relay is not trusted.
- **Security:** QUIC/TLS protects a direct connection, but **does not automatically give E2EE if a relay terminates QUIC**. Define peer identity verification, key continuity and an end-to-end authenticated encryption design for relayed media before claiming E2EE. Do not roll custom cryptography casually; use audited primitives and protocol review.
- **Surface separation:** Qt UI should not own packet processing or decoding. Media threads send lifecycle/state/texture handles through constrained interfaces, not copied video-frame arrays through general-purpose signals.

## 5. Feasibility gates: prove before scaling up

### Gate A — Kyber interoperability and bidirectional API

**A1 — one Windows 11 machine, loopback (first).**

1. Pin exact Kyber release/commit (current documented release: **0.29.0, 2026-10-09**).
2. Audit upstream: https://gitlab.com/kyber/kyber and https://gitlab.com/kyber/core/kysdk; inspect SDK APIs and examples.
3. Run two independent local participants using `127.0.0.1`/loopback, with separate processes and ports. Establish both directions of Kyber AV using native components; determine if one host can publish and receive without conflicting roles, ports, devices or capture ownership.
4. Use synthetic camera and audio sources for at least one participant where simultaneous opening of a physical device is unreliable. Demonstrate simultaneous **webcam + screen capture** if possible. Record any missing APIs, necessary adapter/fork and upstream contribution opportunities.
5. Trace source/capture/encode/transport/decode/render and note missing echo cancellation. Record latency through the software pipeline; **loopback is not a real LAN latency or acoustic echo-cancellation test**.
6. Deliver a reproducible two-process proof, logs and blockers before building contacts.

**A2 — Windows 11 + Linux on two physical machines over LAN (after A1).**

Detailed execution and acceptance plan: [Gate A2 — two physical PCs with Ubuntu in VirtualBox, then native Linux](PLAN_GATE_A2.md). The VM stage is **A2a**; native Linux hardware/compositor validation is **A2b** and remains required for full original A2 acceptance.

1. Use the Windows 11 development computer as peer A, and a supported Linux desktop (preferably recent Ubuntu) as peer B. Verify exact Linux distribution, desktop session (X11/Wayland) and Kyber capture/codec support first.
2. Run bidirectional video and audio with independent physical capture/playback devices; add simultaneous webcam and screen share where Kyber supports it. Test Qt floating-window rendering on Windows, and preferably Linux too.
3. Measure end-to-end glass-to-glass video latency, audio delay and A/V synchronization across a genuine LAN. Test acoustic echo cancellation, device changes, sustained calls, disconnect and reconnect.
4. Record Linux-specific GPU capture/encode/decode limitations, cross-platform media negotiation, timestamps and transport issues. Deliver a reproducible two-host demo and go/no-go decision.
5. **Windows 7 is not a supported A2 target:** Kyber's Windows notes list Windows 10/11/Server 2022 as tested, and Qt 6 does not support Windows 7. A dedicated legacy compatibility experiment may be considered later but must not determine the core architecture or block development.

**Do not build the contact system before achieving A1 feasibility; validate A2 before promising real-world cross-platform calling.**

### Gate B — borderless floating GPU video

1. Qt6/QML frameless topmost window: native drag, edge/corner resize, hover-only controls, multi-DPI and fullscreen interactions.
2. Put real decoded Kyber video into the window, try a native VLC embedding path and GPU texture-sharing path. Measure CPU/GPU overhead, end-to-end latency, stability across resize and DPI changes.
3. Define explicit fallback only after profiling; avoid promising zero-copy until verified. Choose Rust/Qt binding after benchmark.
4. Test preserve-ratio resizing, optional crop-to-fill, no title/border/padding, no visible controls at idle, and correct focus when remote screen is displayed.

### Gate C — real-network calls

1. Establish secure peer identity and invitation signaling. Attempt direct IP/IPv6 connectivity, then STUN candidate discovery / UDP hole punching / ICE-like path selection as needed.
2. Evaluate a TURN-style UDP relay, raw encrypted UDP forwarding, or purpose-built Kyber-aware relay; do **not** assume WebRTC TURN integration is plug-and-play.
3. Make relay security and authentication explicit; test residential NAT, symmetric NAT, UDP-blocked workplace networks, and disconnect/reconnect.
4. If a reliable P2P/relay solution is not possible without significant Kyber changes, decide whether to maintain an adapter/upstream contribution or explicitly narrow Skyber's supported environments. Do not silently swap transports.

## 6. Iterative delivery plan

| Increment | Deliverable | Exit criteria |
| --- | --- | --- |
| **0 — Discovery and proof** | Upstream audit; licenses; two-way Kyber LAN call proof; test camera+screen concurrency; borderless video window proof. | Findings documented, feasibility gates A+B either pass or clear blockers enumerated; reproducible demo. |
| **1 — LAN calling MVP** | One-to-one AV calls, device selection, remote webcam floating window, mic/cam toggle and hangup, basic diagnostics. | Both directions work for sustained calls, no windows/padding in idle overlay, stable teardown/reconnect. |
| **2 — Screen-share MVP** | Screen/window selection, shared-screen independent window/view, remote webcam stays floating; simultaneous camera+screen. | Each peer can independently share, stop and switch; predictable consent and latency. |
| **3 — Contacts and live chat** | Invite/contact management, searchable contacts, text chat, SQLite history, read/unread and identity state. | Tests cover contact acceptance, blocking, message ordering, reconnect and no-content leaks. |
| **4 — Internet connectivity** | Rendezvous/signaling, direct connection attempts, relay fallback, secure identity and presence. | Real residential/corporate topology matrix; no unjustified E2EE claims. |
| **5 — Polish and cross-platform** | Echo cancellation/audio quality, adaptive QoS, release packaging/signatures, macOS integration and broader Linux compatibility. | Cross-platform acceptance matrix; measured performance and documented limitations. |
| **Later** | Offline encrypted messaging, files, group calling, mobile, richer screen/audio controls. | Only after one-to-one experience is stable. |

Increments 2 and 3 may be developed in parallel **after** the transport/window spike, while Internet functionality should be prototyped early to avoid a late architectural dead end.

## 7. Performance and acceptance

Instrument on-screen and exportable metrics:
- Capture-to-encode, packet serialization, network RTT/jitter/loss, decode-to-present, audio delay, A/V sync, power, CPU/GPU usage, codec/frame rate, and connection/relay path.
- Ground truth glass-to-glass latency using LED/display high-speed-camera or equivalent externally timed experiment. Do not infer glass-to-glass from Kyber's internal latency alone.
- Provisional performance objectives, **not promises**: smooth 720p30 on ordinary hardware and 1080p30/60 with hardware acceleration; aim for video p95 glass-to-glass below ~80 ms in favorable LAN conditions, with actual targets revised after baseline measurements. WAN latency depends on propagation and conditions.
- Reliability scenarios: toggling camera/mic 100 times, concurrent webcam+screen, 1-hour call, rapid hangup/reconnect, sleep/wake, network changes, hardware device unplug, DPI/monitor changes.
- Floating UI snapshot acceptance: video fills client rect; no visible chrome at idle; overlay only on hover; no relayout; topmost toggle persists; resizing never stretches; no dead areas; keyboard/assistive access.
- **Testing lanes:** fast Rust unit/property/integration checks and UI state tests on each PR; separate opt-in hardware-media and network-impairment test suite; nightly or release candidate end-to-end device loop. Do not require video/codec hardware tests for chat-only edits.

## 8. Repository proposal

```text
Skyber/
  README.md
  docs/
    PLAN_SKYBER.md
    ADR/
    research/
      KYBER_SDK_AUDIT.md
      BENCHMARKS.md
  crates/
    skyber-core/
    skyber-media-kyber/
    skyber-signaling/
    skyber-connectivity/
    skyber-storage/
    skyber-desktop-bridge/
  apps/
    desktop/          # QML, Qt Quick, CXX-Qt/CMake bridge
  services/
    rendezvous/       # opt-in self-hostable signaling/presence
  tests/
    scenarios/
    hardware/
  .github/workflows/
```

Use feature-gated fake media sources and reproducible transport test harnesses rather than tightly coupling UI tests to real cameras. No dependency on proprietary cloud services for LAN calling. Keep optional rendezvous backend independently deployable.

## 9. Open architectural decisions

1. Is a symmetric Skyber call represented by **two Kyber connections** or can the current SDK provide truly bidirectional multiplexed streams on one QUIC connection?
2. What is the best **native video surface** path for Qt Quick on Windows, followed by Linux/macOS? Does current Kyber/VLC API expose usable GPU texture handles and synchronization?
3. How to implement usable **acoustic echo cancellation** across devices; what is already in the Kyber capture/playback path?
4. How to deploy **internet connectivity** (ICE-like approach versus direct IPv6 + relay), and can it remain fully end-to-end encrypted when relayed?
5. Should relay/signaling be community-hosted, user-hosted, or both? What is the privacy and metadata policy?
6. Does the current Kyber component licensing permit our intended distribution and installer packaging? Must resolve before releases.
7. For chat while contacts are offline: deferred until a defined offline encrypted mailbox/protocol exists, or intentionally included in a later increment?
8. Evaluate official Qt Bridge for Rust against CXX-Qt when stable; no rewrite of the core application logic.

## 10. Sources checked (2026-11-21)

- Kyber FAQ (Rust, QUIC and P2P limitations): https://docs.kyber.tech/docs/Help/FAQ/
- Kyber architecture (client/server, bidirectional I/O intent): https://docs.kyber.tech/docs/Architecture/General/
- Kyber release 0.29.0: https://docs.kyber.tech/blog/
- Kyber open-source license statement: https://kyber.tech/xr/
- Kyber upstream GitLab: https://gitlab.com/kyber/kyber
- Kyber Rust SDK: https://gitlab.com/kyber/core/kysdk
- CXX-Qt bindings: https://github.com/KDAB/cxx-qt
- Qt Bridge for Rust beta (2026): https://www.qt.io/blog/qt-bridge-for-rust-0.3-cxx-qt-compatibility-and-soundness

All recommendations are provisional until Gates A–C. Do not represent proposed functionality, concurrency or P2P behavior as already implemented by the Kyber SDK.

- Windows platform support: https://docs.kyber.tech/docs/GettingStarted/Kyber-installation-notes-windows/
- Linux platform installation: https://docs.kyber.tech/docs/GettingStarted/Kyber-installation-notes-linux/
- Qt 6 Windows supported configurations: https://doc.qt.io/qt-6/windows.html

## 11. Engineering practices and agent guidance

The implementation shall follow:
- [Code quality and toolchain policy](QUALITY.md) — Rust/QML linting, dependency/license controls, CI tiers, code coverage, PR requirements.
- [Testing strategy](TESTING.md) — deterministic local A1 versus physical Windows 11–Linux A2 and specialized multimedia tests.
- [Coding style](CODING_STYLE.md) — Rust, Qt Quick/QML, C++ FFI, threading, security, and performance conventions.
- [AGENTS.md](../AGENTS.md) — the single authoritative coding-agent instruction file.

These documents are **proposed policy**, not claims that the associated build files, binaries or CI checks have been implemented.
