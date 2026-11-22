# Pivot plan — Realign Skyber media with Kyber's upstream pipeline

**Status:** Architecture pivot approved for planning; not implemented. Prepared 2026-11-22 on Gate C-1 branch `feat/gate-c1-transport-spike` at `d649ce9`.
**Goal:** Build a two-way Internet calling application that uses Kyber's mature low-latency capture, encoding, media protocol, decoding, synchronization, and playback facilities wherever they are applicable. Skyber should implement peer discovery, identity, invitations, direct/relay connectivity, the Qt user experience, and only the smallest missing media adapters.
**Scope:** Windows and Linux one-to-one camera/microphone calling; preserve independent screen-share support and the later floating-window UI. IPv6, contacts and broad hardware coverage retain their existing milestones.
**Related:** [C-1 ADR](ADR/GATE_C_1_INTERNET_CALLING_MVP.md), [Kyber SDK audit](research/KYBER_SDK_AUDIT.md), [original product plan](PLAN_SKYBER.md), [A2 Windows capture](ADR/A2-1A_WINDOWS_CAPTURE.md), [A2 Ubuntu capture](ADR/A2-1B_UBUNTU_CAPTURE.md), [A2 playback](ADR/A2-1C_PLAYBACK.md).

## Decision and rationale

The A1/A2 media path was deliberately a feasibility prototype. Skyber currently uses genuine Kymux/QUIC media endpoints, but implements separate Windows Media Foundation and screen-capture helpers, Ubuntu GStreamer/PipeWire helpers, a small software-only FFmpeg/x264/Opus codec wrapper (`native/a1/codec.c`), and WinMM/GDI or GStreamer/GTK playback. The proof profile is 320 x 180 at 15 fps and 48 kHz PCM; it does **not** establish Kyber-equivalent capture-to-display latency, synchronization, codec quality, GPU use, or hardware acceleration.

This should **not become the default production media architecture by inertia**. Prefer upstream `kymedia`, `kyavservice`, `avconv` and/or `txproto` as supported by the pinned SDK, upstream Kymux media protocols, and Kyber's optimized VLC player/decoder integration. Reuse the proven existing components, not the entire remote-desktop/controller application. A custom adapter is acceptable only for a demonstrated missing capability (for example, webcam/microphone device selection, platform permission handling, or Qt surface integration); document the gap and seek a minimal upstream-compatible interface.

Preserve existing working A1/A2 fixtures, synthetic codecs, metrics, Gate C-0, ICE/Quinn, signaling, room authentication and WebSocket relay proofs for regression while migration is underway. Do not remove tests, screenshare or Qt goals. Keep transport/signaling/identity development moving independently; **do not put a production media pipeline into C-1d until its upstream-first design is evidenced**.

## 1. Upstream API and behavior audit — first deliverable

Review the exact pinned SDK/tag/revisions (not assumed current-main APIs), checking source examples and the upstream desktop/server configuration. Produce a component matrix containing: upstream component and commit; available public Rust/C/C++ API; supported OS and capture devices; GPU/CPU encoding and decoding; clock/PTS and A/V synchronization; startup/shutdown; permissions; tested examples; and each Skyber adapter that could be retired or kept. Mark evidence as confirmed, inferred, or unresolved.

Focus on:
- `kymedia` / `kyavservice`: actual production grabbers, microphone versus system-audio capture, capture format normalization, encoder selection, frame dropping, timestamps, cancellation and device changes.
- `avconv` versus `txproto`: which backend is production-preferred **at this exact revision**; how to instantiate the media graph with webcam and microphone, whether upstream hardware encoders/decoders work, and what is exposed across the FFI. Do not assume the newest experimental backend is best.
- `kyctl`, `kyvlcplayer` and VLC bindings: low-buffering playback, latest-frame presentation, A/V clock handling, potential embedded Qt/Qt Quick video and audio output, multi-track windows, and GPU texture interoperability. Do not claim a zero-copy path without a measurement.
- `kymux` / `kyproto`: real protocol enumerations, codec-specific constraints, endpoint capabilities, frame size and MTU, keyframe handling and upstream defaults. Read the current upstream server/client selection logic; do **not** call a hypothesis an upstream recommendation.
- Licensing, exact build pins, packaging and compatibility with Windows MSVC + Ubuntu; ensure a replacement does not accidentally introduce incompatible runtime requirements.

Deliver a source-linked `docs/research/KYBER_MEDIA_UPSTREAM_COMPARISON.md` before a production replacement is selected. Record blockers and viable minimal adapters; avoid speculative rewrites.

### Working component direction

| Stage | A1/A2 proof | Intended production direction / decision to verify |
| --- | --- | --- |
| Camera/mic/screen acquisition | Independent Windows and Ubuntu helpers | Reuse Kyber capture components where supported; add the narrowest missing device/portal adapter |
| Frame normalization, timestamps, synchronization | Fixed-size pipes and local clocks | Prefer Kyber media graph and clock model; measure drift |
| Video/audio encoding | Custom software FFmpeg shim | Prefer Kyber-configured pipeline, platform hardware encoding when supported, software fallback |
| Media transport | Upstream Kymux with fixed `Reliable` modes | Keep upstream Kymux; expose supported audio/video protocol selection |
| Decode/presentation/audio output | Custom FFmpeg + WinMM/GDI/GTK | Prefer Kyber's optimized decoder/player; adapt its output to Qt only where needed |
| Invitation, peer authentication, NAT and relay | Skyber C-1 components | Keep Skyber-specific design, with end-to-end authenticated QUIC |

## 2. Internet-call media protocols: selectable until evidence determines defaults

Kyber already supports multiple audio/video transport behaviors. Skyber currently forces `AudioProtocol::Reliable` and `VideoProtocol::Reliable` in `crates/skyber-media-kyber/src/transport.rs`; this was a test baseline, not a production latency choice.

**Policy:** Use the most applicable **upstream-supported** protocol for live Internet conversations, and make the supported alternatives user-selectable while we gather evidence. Do not permanently select a winner before comparison. Retain the setting if network conditions or user priorities justify more than one mode.

1. Audit the exact pinned `kyproto` variants and source examples first. Candidate audio modes include unreliable, unreliable-with-FEC and reliable *only if the pinned API implements and supports them*; candidate video modes include `GopStream`, unreliable variants and reliable *only if available*. Verify method signatures, required codec metadata, packet size limits, FEC behavior, keyframe and intra-refresh requirements and compatibility with the selected network backend.
2. **Provisional Internet-call experiment profile** (not an official Kyber recommendation): prioritize Kyber's supported low-latency/unreliable audio mode for microphone; test its FEC variant if supported and network loss makes it useful; prioritize Kyber's available GOP-aware low-latency video mode for webcam, with properly configured keyframes and intra-refresh disabled when required. Keep reliable audio/video as explicit baselines; examine screen share separately because text fidelity and motion have different priorities. Revise after the upstream audit, not by guessing protocol names.
3. Provide an **Advanced → Media transport** setting in the desktop app: independent microphone, webcam and screen-share modes, using the upstream protocol names and plain-language descriptions. Show only implemented, compatible options; expose a clear default such as `Auto (upstream-informed)` and a deliberate manual override. Remember user preferences locally but keep room credentials and secrets separate. Show the **negotiated effective** protocol for each track in diagnostics.
4. Negotiate each track's mode and required codec settings between both authenticated peers **before** creating Kyber media endpoints. Reject unsupported or mismatched selections clearly; never silently substitute reliable mode. Test endpoint creation and receiver interpretation in both roles.
5. For the initial implementation, apply changed protocol selections on the **next call** or a clearly signaled track restart. Live switching is optional until safe per-track renegotiation, new endpoint lifetime, keyframe recovery and synchronized playback have been proven. Do not imply an in-call switch happened when it did not.
6. Keep control, identity and room state reliably delivered; the media-mode setting must not change the authenticated signaling or raw QUIC bootstrap. Direct UDP and WSS-relayed QUIC require their own performance profiles, and a specific mode can be unavailable on one path.

### Comparative evidence / mode-selection decision

Use deterministic generated media and timestamped repeatable network impairment (no heavy models). Include at least: clean LAN, direct Internet/mobile 4G, controlled 0.5/1/3% loss, varying jitter/RTT/bandwidth, UDP-blocked WSS fallback, short calls, and sustained 15-minute tests. Do not confuse transport packet retransmissions with lost media frames.

Collect per mode/path/platform: captured sample/frame coverage, source and sink gaps, decoded and presented frames, audio glitches and underruns, lip-sync/drift, p50/p95/p99 latency where clocks permit, externally measured glass-to-glass/acoustic latency where feasible, jitter, freezes, keyframe recovery, CPU/GPU, memory and bandwidth, setup and reconnect performance. Use existing A2 instrumentation; label application-only timings and unknowns honestly. Compare with the same capture, codec and presentation pipeline, bitrate and source. Record upstream-configured protocol and keyframe settings with each result.

Pick defaults by **low perceived delay and continuity under realistic impairment**, with intelligible audio and acceptable video quality as constraints, not by raw packet delivery percentage alone. Publish the decision, remaining environment-specific preferences and the exact tested configurations. If no mode dominates, retain Auto plus meaningful manual options; do not force convergence.

## 3. Upstream-first media proof before product integration

Implement the smallest non-Qt, non-Internet proof that runs **one real media source through Kyber's own capture/codec path**, Kymux media transport and preferably Kyber's decoder/player. Preserve the current A2 software shim as comparison/control only. Compare CPU/GPU, latency, capture device behavior, resource lifetime, memory/queue depth, video quality, audio continuity and synchronization. Repeat on Windows and native Linux when a working device environment is available; Ubuntu VirtualBox C922 physical microphone failures remain an environment limitation, not a requirement to fix.

Test real microphone (not Kyber's default Windows system-audio loopback), webcam, independent screen share, and receiving audio so that capture of speaker output does not cause digital feedback. Keep device consent and source-specific mute/on/off behavior. Verify hardware acceleration by observing actual encoder/decoder usage and fallback, not just by requesting a codec name. Preserve audio/video stream independence.

If a needed public SDK boundary is absent, make a narrow Skyber-owned adapter using upstream-compatible APIs and document precisely which functions are unavailable. Prefer upstream feature requests to wholesale custom media engine copies. Do not patch or fork Kymux for its parser issue: the previously approved ownership/exposure decision in the C-1 ADR remains unchanged.

## 4. Kyber-native WebSocket versus Skyber's encrypted relay

Separately determine whether Kyber 0.29's native WebSocket transport can be used for **two mutually authenticated peers with an opaque relay that cannot read media**. The native feature is documented for Kyber client-to-server fallback; its existence alone does not imply it satisfies Skyber's two-peer end-to-end encryption model.

Compare authentication boundary, confidentiality at the relay, transport framing, metadata, reconnect, overhead, latency and implementation effort against the existing QUIC-in-WSS relay proof. Prefer upstream native transport only if the Skyber room identities and peer-to-peer E2EE invariants are preserved. Do not replace a working relay merely because both designs use WebSocket. Keep this assessment separate from media protocol selection.

## 5. Delivery increments and dependencies

- **P0 — Audit and decision table:** `KYBER_MEDIA_UPSTREAM_COMPARISON.md`, exact upstream commits/API capabilities, production-preferred backend and protocol list; explicitly identify unsupported microphone/webcam/output integration. Existing network/identity tasks continue in parallel.
- **P1 — Upstream media reference proof:** working real source → Kyber encode → Kyber protocol → Kyber decode/playback; verified hardware/software behavior; retain current A2 proof and tests.
- **P2 — Configurable per-track media protocols:** Rust typed configuration, verified mode negotiation on both peers, basic Qt advanced setting, explicit effective-mode diagnostics, compatibility/error tests and automated impairment comparisons. Default provisional until benchmark evidence.
- **P3 — Production media adapter and Qt integration:** platform device selection and capture consent, selected upstream pipeline, separate webcam/screen windows, microphone/camera toggles, mute, A/V timing and robust teardown; no untested substitution of the old fixed-size proof profile.
- **P4 — Real-network evaluation and decision:** same configuration under direct and relayed paths, controlled degradation, 15-minute continuity, real-device Windows testing, synthetic Ubuntu VM and later native Linux; choose documented defaults or retain selectable modes.
- **P5 — Controlled release readiness:** cross-platform packaging/performance, supported-network and hardware limits, Kyber compatibility pin, codec licenses, upstream decoder security dependency status, and explicit general-public release go/no-go.

### Gates / no-regression rules

- Do **not** freeze a custom audio/video pipeline as the product implementation until the upstream comparison and reference proof are complete.
- Do **not** remove or regress C-0/C-1 signaling, direct ICE, authenticated relay, Qt invitation/call controls, camera, microphone, independent screen share, or synthetic/hardware test paths. Completed transport work remains valuable.
- Do **not** replace Kymux, Kyber codecs, or optimized media semantics with bespoke algorithms without measurable evidence and a recorded rationale. Do **not** claim that reusing FFmpeg alone equals reusing Kyber's full pipeline.
- Do **not** expose Internet-facing Kyber media to arbitrary/unvetted peers while the pinned upstream decoder allocation risk is unresolved. Retain the ADR's narrowly controlled trial policy and future private upstream report; this pivot is not a parser patch.
- Keep quick deterministic unit/integration tests separate from device/soak/Internet tests; continue CI checks and reproducible Windows/Linux builds.
- Plan completion means an architecture and phased implementation decision, **not** Gate C-1 completion. Gate C-1 still needs working Qt call flow, direct/relay acceptance and documented platform/IPv6 limitations.

## Primary sources to check and reference in the audit

- Pinned Skyber SDK and audit: `docs/research/KYBER_SDK_AUDIT.md`; Kymux `af10dbb3ea016135c0ec8e01143179ad4bda005b`; Kyber SDK tag `0.29.0`.
- Kyber architecture: https://docs.kyber.tech/docs/Architecture/General/
- Kyber server/media configuration: https://docs.kyber.tech/docs/GettingStarted/KyberServerConfig/
- Kyber 0.18 audio protocol selection: https://docs.kyber.tech/blog/kyber-0-18-0/
- Kyber 0.28 GOP stream / intra-refresh compatibility: https://kyber.tech/article/v-028/
- Kyber 0.29 native WebSocket and GStreamer/media improvements: https://kyber.tech/article/v-029/
- Kyber technology principles: https://kyber.tech/technology/

Document where these product-level claims differ from verified interfaces available to Skyber at its **pinned** commits.
