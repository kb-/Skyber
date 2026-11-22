# Gate A2 — Windows ↔ Ubuntu over a physical LAN

**Status:** Work in progress (2026-11-22). See per-stage evidence; full A2a/A2b acceptance is not claimed.
**Parent:** [Skyber product/architecture plan](PLAN_SKYBER.md), [testing strategy](TESTING.md), [quality policy](QUALITY.md).
**Baseline:** [Gate A1 results](research/GATE_A1_RESULTS.md) and [Kyber SDK audit](research/KYBER_SDK_AUDIT.md), merged to main in PR #1.

## 1. Goal and constraints

Prove an actual bidirectional Kyber-based call across **two separate physical computers** on a genuine local area network:

- **Peer A:** native Windows 11 on the Skyber development computer.
- **Peer B:** Ubuntu 26.04.1 LTS Desktop **guest in VirtualBox on a second physical Windows computer**.
- The second Windows host is a hypervisor and device source; **the peer runs in Ubuntu**, not in the host Windows session.
- The first milestone, **A2a**, uses the VM for functional LAN interoperability, physical-device integration where accessible, and instrumented end-to-end tests.
- The second milestone, **A2b**, validates the remaining native Linux hardware/compositor/GPU behaviors on bare-metal Linux, before claiming that the **full original Gate A2 hardware acceptance** has passed.

The virtualized stage is valuable evidence, not a replacement for a native Linux performance baseline. Do not weaken the original two-physical-host requirement, nor represent VM measurements as bare-metal Linux measurements.

Skyber must continue to use **Kyber (kyber.tech) QUIC/media endpoints**, not WebRTC media. Keep the Rust core, Qt 6/Qt Quick UI, and narrow reviewed native-codec interfaces. Do not build contacts, signaling infrastructure, or the polished full GUI just to make this gate pass.

## 2. Existing evidence and known gaps

Gate A1 already proved two Windows processes on localhost can exchange three distinct synthetic streams in both directions: stereo Opus audio and independent H.264 camera/screen video, including independent screen-track termination. It also proved a Windows MSVC consumer can load the MinGW-built narrow codec DLL. The A1 Qt shell is **not** a real media viewer.

Not yet proved by A1: LAN addressing and reachability; a native Linux Kyber peer; video/audio capture from real devices; speaker playback; capture consent; real screen capture; decoder-to-display integration; audio feedback/AEC; synchronized audio and video; physical-endpoint latency; loss/jitter/reconnect behavior; hardware codec support through the pinned Kyber path; runtime hardening of remote packet input.

Do not reuse A1 performance labels as physical capture-to-display timing. A1's timing has encoder buffering limitations documented in its results.

## 3. Test topology and VirtualBox setup

### 3.1 Physical network

- Two separate computers connected to the **same trusted LAN**, preferably wired Ethernet to control jitter and simplify troubleshooting. Wi-Fi may be tested as a second condition.
- Ubuntu VM adapter 1: **Bridged Adapter** attached to the second computer's active physical network interface, not default NAT. Verify it obtains an address reachable from Peer A. Some Wi-Fi networks and access points implement client isolation or bridging restrictions.
- Keep Windows host and Ubuntu guest firewall policy explicit. Open only the agreed **UDP port(s)** for the development peer on the private LAN/subnet; do not disable firewalls or forward Internet-facing ports.
- Record peer addresses, topology, network media, network driver, VirtualBox version, link speed where available, and any known Wi-Fi isolation.
- Basic reachability tests precede Kyber; ICMP alone is not proof that the selected UDP ports or QUIC handshake work.
- Use peer identity authentication and secure certificate/fingerprint verification. Never turn off TLS verification to make LAN testing easier. Each test peer uses distinct credentials and a test-only trust setup.

### 3.2 Ubuntu VirtualBox guest

Suggested starting configuration (adapt to the second PC's actual resources):

| Setting | Starting point |
| --- | --- |
| Guest OS | Ubuntu 26.04.1 LTS Desktop, x86-64 |
| VirtualBox | Currently supported 7.x release on Windows host |
| CPU and RAM | 4 virtual CPUs; 8 GB guest RAM when host capacity permits |
| Graphics | VMSVGA, 128 MB video memory, 3D acceleration enabled if stable |
| Network | Bridged Adapter, preferred physical Ethernet |
| Audio | Virtual audio controller with **input and output both enabled** |
| Camera | Host webcam passthrough via Devices → Webcams; optional physical USB passthrough |
| Guest integration | Matching VirtualBox Guest Additions; Extension Pack if webcam passthrough requires it |

Record actual settings and versions rather than relying on these starting values. Review Extension Pack licensing before distributing anything; it is a test-machine dependency, not an application dependency.

**Important virtualization limits:**
- Host webcam passthrough provides real camera images to Ubuntu but device behavior, capture resolution/rate, and latency may differ from native Linux. Enumerate and record negotiated modes; do not assume 720p or 30 fps.
- The Ubuntu guest captures **its own virtual desktop**, **not** the second machine's Windows desktop. This still exercises a real Linux screen-capture implementation.
- The guest typically sees a **virtual graphics device**, even when host-assisted 3D acceleration is enabled. Native access to the physical host GPU's NVENC/NVDEC or VA-API hardware encode/decode stack is **not** established by VirtualBox 3D acceleration. Prefer a verified software-codec path in A2a; document exact acceleration capability rather than presuming it.
- USB headset or dedicated microphone/speakers may offer more direct physical-device semantics than bridged virtual audio. Avoid choosing the same physical endpoint for automatic system-audio loopback capture and playback.
- Wayland and X11 are different capture environments. Test the actual Ubuntu session, desktop portal/PipeWire availability and user capture consent. Switch sessions only as an explicitly documented diagnostic, not a silent compatibility fallback.

Oracle references: [webcam passthrough and Extension Pack](https://docs.oracle.com/en/virtualization/virtualbox/7.2/user/AdvancedTopics.html#webcam-passthrough), [bridged networking](https://docs.oracle.com/en/virtualization/virtualbox/7.2/user/networkingdetails.html), [Guest Additions/3D](https://docs.oracle.com/en/virtualization/virtualbox/7.2/user/guestadditions.html).

## 4. Implementation stages and deliverables

### A2-0 — Linux/Windows native peer and LAN transport (synthetic)

**Deliverable:** reproducible two-host, two-OS Kyber media exchange with the A1 synthetic media, independent of physical device availability.

1. Audit pinned Kyber 0.29.0 components and current build artifacts for **native Ubuntu x86-64**; compile Linux peers and software codecs natively, separately from the Windows MinGW cross-build. Pin source/tool versions and document licenses and binaries.
2. Generalize A1's loopback-only network configuration to explicit LAN peers without accidentally exposing a generally listening production service. Define a clear CLI/config for bind address, remote address, UDP port, role, credentials and profile. Preserve loopback restriction as the default for legacy A1 commands if appropriate. Do not hardcode the second PC's address.
3. Retain a single authenticated Kyber QUIC connection and independently registered audio/camera/screen media endpoints. Confirm the endpoint-ID parity and codec negotiation work across platforms, including case-by-case positive and negative authentication.
4. Add an opt-in LAN harness coordinating two physical machines via documented commands and shared run IDs, exporting comparable structured logs. Do not assume GitHub-hosted CI can run the two-machine LAN test.
5. Reproduce A1's directionality, six concurrent inbound tracks and independent screen stop over the real LAN. Test clean termination and port reuse.

**Exit:** one bidirectional Windows ↔ Ubuntu-VM synthetic call, with media decoded correctly, authenticated connection, no uncontrolled drops/queues, and a repeatable command sequence.

### A2-1 — Real media capture, playback and permission boundaries

**Deliverable:** actual device input and real decoded output on both endpoints, while retaining selectable synthetic sources for diagnosis.

1. Introduce narrow **CaptureSource** / **AudioOutput** / **VideoSink** interfaces; keep device enumeration/capture, codec, Kyber transport and presentation independently replaceable. Do not embed device capture inside the Qt UI or duplicate the media transport.
2. Windows: capture a real webcam and microphone using supported native/SDK interfaces; capture a selected monitor or window and verify it is independent of the webcam track. Keep system-audio share **off by default** and separate from microphone audio.
3. Ubuntu: enumerate V4L2 webcam modes, PipeWire/PulseAudio/ALSA inputs and outputs as available, and X11 or Wayland/portal screen-capture options. Audit actual upstream Kyber native graph/source support before choosing a bridge. If APIs are absent, document a bounded native adapter and source-level reason; do not claim SDK support that was not found.
4. Provide explicit user consent/indicators for camera, microphone and screen capture. Screen sharing must stop on hangup and must never silently capture the Windows host outside Ubuntu.
5. Play received microphone audio through chosen endpoints; show decoded camera and screen video in a minimal native viewer/Qt Quick video sink. The full borderless hover-controls experience belongs to Gate B; a minimal viewer is sufficient here.
6. Test mute/unmute, camera start/stop, screen start/stop, source switching, device disappearance, permission denied, and output changes. Camera and audio must persist when screen sharing stops.
7. Distinguish digital system-audio feedback, acoustic speaker-to-microphone echo and AEC. A headset-based baseline can isolate feedback; test speaker/microphone acoustics separately. Do not claim effective AEC until audible/quantitative tests support it.

**Exit:** both peers capture actual camera/microphone when accessible, share their own desktop, receive audible audio and visible video, with separately controlled tracks. If a VM device/backend is unusable, record a blocked case rather than silently replacing it with synthetic capture.

### A2-2 — Instrumentation and observed latency

**Deliverable:** trustworthy latency and quality measurements on a real LAN.

1. Instrument capture/source timestamps, encode start/end, enqueue, transport send/receive, decode start/end, presentation and audio playback boundaries. Provide frame/packet identity for correct correlation across codec buffering and frame reordering. [Initial peer-boundary timing model](ADR/A2-2_TIMING.md).
2. **Do not subtract uncalibrated Windows and Linux wall clocks.** One-way latency needs synchronized clocks with measured uncertainty, an independently observed external reference, or a correct timebase/clock-correlation method. RTT can be measured from one peer's monotonic clock.
3. Measure **glass-to-glass** video with an external high-speed camera/LED/display reference or equivalent validated technique; distinguish that from internal time stamps. Audio delay and A/V lip sync also require end-to-end observation.
4. Report p50/p95, distributions/sample counts, capture/frame rate, dropped/late frames, audio continuity, CPU/RAM, queue depth, network RTT/jitter/loss, host load and guest scheduling load. Capture both idle and concurrent camera+screen cases.
5. Target a software 720p30 baseline **only if actual VM camera and capture modes support it**. If not, use the best verified VM mode and record an explicit native-Linux requirement for full-resolution performance. Evaluate 1080p options after proof, not as a prerequisite for first success.
6. Benchmark software and any confirmed hardware codec paths separately. Host FFmpeg's NVIDIA smoke test from A1 does **not** establish Kyber GPU integration on Windows or Linux.

**Exit:** reproducible benchmark procedure and logged results with defensible time definitions, virtualization overhead disclosed, and no unsupported glass-to-glass or GPU claims.

### A2-3 — Failure, reliability and security testing

**Deliverable:** repeatable robustness evidence and known limitations.

1. Exercise short calls, 15-minute sustained call, independent repeated toggles, clean hangup/restart, denied/unplugged devices, guest VM suspend/resume, and network interruption/rejoin. Schedule a separate **1-hour** soak as an opt-in/manual gate, not on every PR.
2. Inject **controlled** delay/jitter/loss/burst outage with a documented Linux or Windows impairment mechanism. Explicitly test whether current Kyber **reliable audio/video endpoints** accumulate latency or block delivery through retransmission; investigate bounded queues and appropriate Kyber protocol alternatives based on actual API support.
3. Audit remote packet/config bounds, FFI ownership, malformed codec data, cancellation and all native allocation lifetimes before broadening beyond trusted LAN traffic. Add negative tests and a native sanitizer/fuzz lane when the relevant parsers are exposed.
4. Check incorrect fingerprint/secret, unauthorized peer, malformed track map, invalid codec config, unexpected disconnection, and clean retry. Verify no media is published without explicit intended consent.
5. Record CPU, memory and handle/file-descriptor trend during soak, prevent resource leaks and secret/media dumps. Keep synthetic regression reproducible and isolated from private device recordings.

**Exit:** no crashes or uncontrolled resource/queue growth in the declared scenarios; all failures are logged and categorized, not suppressed.

### A2b — Native Linux acceptance (deferred but still required)

A separate **bare-metal Ubuntu 26.04.1 LTS** test on a physical PC closes gaps masked by VirtualBox:

- Native Linux GPU/renderer and hardware encode/decode path, where supported.
- Native display/compositor behavior, camera/USB/audio stacks and actual resolution/frame-rate capabilities.
- Native Linux desktop capture on the intended X11 and/or Wayland sessions.
- Representative Linux CPU/GPU, glass-to-glass, and A/V latency benchmarks without hypervisor overhead.
- Re-run the essential two-way physical-device and LAN fault matrix.

The A2a milestone can be marked **passed with virtualized limitations**; call the original Gate A2 **fully passed** only when the native Linux cases have been examined and all critical ones passed (or an explicit documented product scope decision narrows platform claims).

## 5. Minimal acceptance matrix

| Scenario | A2a Ubuntu VM | A2b native Linux |
| --- | --- | --- |
| Cross-OS authenticated QUIC across two physical hosts | Required | Recheck |
| Synthetic Opus + separate H.264 camera and screen, both directions | Required | Regression |
| Real camera + microphone capture, both directions | Required where VM exposes supported devices; record blockers | Required |
| Desktop capture from each peer's own OS session | Required with supported VM session | Required |
| Concurrent media, independent screen-stop and restart | Required | Required |
| Audible speaker/headset output and visible decoded video | Required | Required |
| A/V sync, glass-to-glass and echo characterization | Measure/report with VM caveats | Required native baseline |
| Loss/jitter, reconnect, hangup and resource cleanup | Required (controlled) | Recheck essentials |
| Direct native Linux hardware codec path | Not credited | Required only if promised as supported |
| Native GPU Qt video surface and multi-monitor behavior | Exploratory | Gate B/native acceptance as applicable |

Do not equate a pass on a VM virtual device with a pass on bare-metal driver/hardware behavior. **Functional compatibility** and **native performance/hardware capability** are separately reportable.

## 6. Automated test strategy and CI boundaries

- **Each ordinary code PR:** Rust format/check/Clippy/Nextest/doctests, dependency/license policy, relevant Qt/QML tests; no camera, two-host, GPU or long-running calls.
- **Changed media/transport code:** A1 synthetic regression on available Windows setup, Linux synthetic/unit tests, protocol negative cases and targeted FFI safety checks; existing GitHub Gate A1 workflow remains manually dispatched when the self-hosted runner is configured.
- **A2a manual LAN suite:** explicit operator-started orchestration across two physical computers and private test devices. Keep it outside normal hosted CI. Record test suite version, artifacts and both endpoint software inventories.
- **Long soak, impairment, physical devices, fuzzing and GPU:** dedicated opt-in/manual/scheduled lanes; avoid slowing UI-only changes.
- A2 harness should generate **machine-readable JSON results** (run ID, scenario, peer OS/version, source type, codec, tracks, network condition, timestamps, measurements, pass/fail, logs) and a concise Markdown results summary. Do not commit credentials, personal audio/video or identifying raw recordings.
- Add regression cases to fast tests where possible when A2 exposes a bug; distinguish automated guarantees from hardware observation.

## 7. Proposed implementation PR sequence

1. **PR A2-0:** native Linux media build, explicit LAN bind/connect/auth configuration, cross-platform synthetic compatibility, instructions and structured two-host runner. Must not weaken existing A1 loopback safety.
2. **PR A2-1a:** Windows webcam/mic/screen capture, device interfaces and consent; small focused Windows manual acceptance. [Windows capture acceptance evidence](ADR/A2-1A_WINDOWS_CAPTURE.md#a2-1a-acceptance-evidence).
3. **PR A2-1b:** Ubuntu V4L2/audio/screen capture, portal permission handling and limitations; VM-specific manual acceptance. [Ubuntu capture acceptance evidence](ADR/A2-1B_UBUNTU_CAPTURE.md#a2-1b-acceptance-evidence).
4. **PR A2-1c:** real audio playback, minimal decoded camera/screen viewer, independent track lifecycle, error/state surfaces. [Playback verification](ADR/A2-1C_PLAYBACK.md#verification-on-november-21-2026).
5. **PR A2-2:** accurate metrics, glass-to-glass methodology and benchmarks.
6. **PR A2-3:** reliable/unreliable media transport behavior investigation, network impairment, reconnection, negative packet/FFI cases, long stability tests.
7. **A2a report:** results, reproducibility, known virtualization differences and decision on progressing to Gate B/GUI.
8. **A2b acceptance:** native Linux hardware/compositor/GPU checks and updated final gate decision.

Keep the plan's PR ordering flexible if upstream API research makes a different technical dependency order necessary. Avoid merging a giant all-in-one hardware PR.

## 8. Go/no-go and explicit open decisions

**A2a GO:** authenticated cross-platform physical-LAN media functions bidirectionally; independent real capture and screen tracks work with the verified VM devices; decoded video/audio are rendered or played; controlled failure and teardown are reliable; results and limits are honestly measured; no critical architectural blocker. If some VM-specific device or GPU feature is unavailable, label it clearly and decide separately whether it requires native Linux follow-up or blocks platform support.

**A2a NO-GO:** no reproducible Windows↔Linux Kyber transport, inability to accept/decode media cross-platform without abandoning Kyber, systematic unbounded latency/resources, unsafe remote-media boundary, or inability to access any viable real audio/video source on the selected VM with no documented route forward.

**Decisions to record as research/ADRs before implementation:**
- Linux capture backend (V4L2, GStreamer/native avconv graph, PipeWire portal vs X11), pin and license implications.
- Audio input/output and AEC strategy; why system-audio loopback is separate from microphone.
- Codec/packet framing compatibility between native Linux and Windows builds.
- Reliable vs other Kyber media endpoint choices under loss, bounded buffering and recovery.
- Source-correlated timing design across codec delay, clock domains and guest virtualization.
- Whether the first Qt viewer consumes CPU frames or native textures; performance measurements and Gate B transition.
- Ubuntu VM display session and device capabilities; whether a native Linux host becomes necessary earlier.

## 9. Evidence to publish when testing (not yet available)

Create a separate dated report under docs/research/ for the actual Gate A2 test results. Include hardware inventory (both physical machines), second Windows host and VirtualBox versions/settings, Ubuntu kernel/session, camera/mic models and modes, codecs, Kyber pins, source/receiver configurations, LAN adapter, network impairment settings, commands/logs, pass/fail matrix, CPU/RAM/GPU measurements, physical A/V observations, and any blocker issues. Distinguish **observed**, **inferred**, and **planned** claims.

Do not retrofit A1 evidence into the A2 result table. Do not claim A2 completion solely on synthetic Linux media, host FFmpeg hardware smoke, or guest-accelerated Qt rendering.

## 10. Sources and consistency

- [Original Skyber Gate A2 requirements](PLAN_SKYBER.md)
- [Existing test lanes and hardware acceptance](TESTING.md)
- [Gate A1 measured evidence](research/GATE_A1_RESULTS.md)
- [Kyber pinned API/codec constraints](research/KYBER_SDK_AUDIT.md)
- [VirtualBox 7.2 virtual networking](https://docs.oracle.com/en/virtualization/virtualbox/7.2/user/networkingdetails.html)
- [VirtualBox 7.2 webcam passthrough](https://docs.oracle.com/en/virtualization/virtualbox/7.2/user/AdvancedTopics.html)
- [VirtualBox 7.2 Guest Additions / 3D](https://docs.oracle.com/en/virtualization/virtualbox/7.2/user/guestadditions.html)

This document refines **how** Gate A2 will be validated. It does not remove or supersede product requirements in the parent plan, nor authorize an unsupported transport or a claim of native Linux acceptance from a Windows-hosted VM.
