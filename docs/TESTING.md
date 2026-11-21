# Testing strategy

**Status:** Gate A1 software tests and CI lanes exist. See the [run instructions](../README.md) and [Gate A1 results](research/GATE_A1_RESULTS.md). **See also:** [QUALITY.md](QUALITY.md), [CODING_STYLE.md](CODING_STYLE.md), [PLAN_SKYBER.md](PLAN_SKYBER.md).

## Principle: fast feedback, realistic validation

Each subsystem exposes narrow, mockable interfaces (media capture, media sink, clock, connectivity, identity, chat store, Qt state adapter). Tests must be deterministic without camera, microphone, GPU or remote services unless explicitly assigned to a hardware tier. No real media, AI model or large fuzz workload in the default fast suite.

### Test organization

| Type | Purpose | Frequency |
| --- | --- | --- |
| Unit | Rust state machine, track subscription, layout math, storage migrations, protocol serialization | Every PR |
| Property-based | Call state invariants, mute/share lifecycle, identity/key continuity where appropriate, message ordering | PR with bounded cases, extended nightly |
| Synthetic integration | Two local peers; media sources and sinks; timeouts, multiple tracks, reconnect, cancellation | A1 adapter changes + manual |
| Qt component | Hover and overlay visibility, controls, resizing, focus, visibility, correct state presentation | GUI PR |
| Visual/desktop | Borderless webcam, no permanently wasted space, aspect/crop policy, always-on-top, multi-DPI, multimonitor, overlay without reflow | GUI milestone/manual and where stable automate |
| Negative/security | Invalid packets, malformed chat/control messages, unauthorized signaling, secret redaction | PR/scheduled by scope |
| Native safety | `unsafe` audits, CXX-Qt/native boundaries, lifetime and threading checks under sanitizers where supported | Boundary changes / scheduled |
| A2 hardware | Two physical machines, codec/capture/playback and speaker/mic acoustics | Manual milestone/release |
| Performance/soak | A/V sync, glass-to-glass latency, stability under jitter, GPU load, heat, reconnections | Dedicated lane, never default PR |

### Gate A1 — one Windows 11 host

- Run two **independent processes** (peers A and B) on loopback `127.0.0.1`, with separate ports, identities and data directories.
- At least one participant uses synthetic video and audio. Prefer controllable frame/audio timestamps and counters so correctness and latency are measurable.
- Test A→B and B→A independently and simultaneously, webcam + audio + screen tracks when the SDK supports them.
- Test cancel/hangup while encoding, rejoin, device unavailable, missing/corrupt packet handling, timeout and resource cleanup.
- Record CPU/GPU usage and pipeline timing, but **do not label loopback timing as real LAN or glass-to-glass**.
- Document SDK gaps instead of silently implementing substitute transport.

### Gate A2 — Windows 11 ↔ Linux

- Two separate physical hosts on the same LAN; peer A Windows 11, peer B a supported Linux distribution (Ubuntu 24.04 LTS is a starting point, subject to Kyber device support).
- Verify Linux display/session dependencies such as X11 vs Wayland and PipeWire/portal behavior for screen capture.
- Use distinct physical camera/microphone/speaker devices; verify simultaneous capture and playback, echo cancellation and independent screen shares.
- Test hardware encode/decode support on each host, 720p30 baseline, screen text readability, 1080p options where available.
- Measure actual end-to-end video **glass-to-glass** separately from internal pipeline timestamps; report p50/p95, sample size, codec, resolution, frames/s, transport type, hardware and network conditions.
- Test at least one extended call, sleep/wake or network reconnect when feasible, and note limitations rather than claiming universal internet compatibility.
- Windows 7 is not in the supported A2 matrix.

## Floating-window acceptance tests

1. Remote video consumes the entire client region in the selected crop-to-fill mode; fit mode may use necessary letterboxing, but no accidental borders, fixed paddings or controls.
2. Mouse outside window or after idle delay: no visible toolbar, frame or controls. Hover reveals *overlay*, never resizes video.
3. Resize preserves aspect by default; if cropping is selected, aspect still correct; no image stretching.
4. Independent always-on-top behavior even when main window is minimized; shared screen does not hide remote webcam in regular desktop windows.
5. Alt-Tab, taskbar, fullscreen application, display scaling, monitor hotplug, move/resize, keyboard access and drag hit-testing behave predictably. Document platform compositor limitations.
6. Frame handling must not block Qt's UI thread, leak texture handles, or accumulate unbounded backpressure.

## Simulated network scenarios

Build a deterministic transport fault injector: configurable packet delay, jitter, loss, duplication/reordering, burst outage and connection reset. Validate bounded queues and reconnection without retained inactive cameras/microphones. Integration tests should inject clocks and randomness where needed; no arbitrary sleeps as correctness assertions.

## Test hygiene

- Prefer interface fakes to hardware mocking deep inside upstream Kyber.
- Synthetic fixtures must be small, reproducible, safe to publish, and licensed. No real contacts, conversations, credentials or recorded identifiable participants.
- Every observed regression gets a minimal automated test whenever feasible.
- GUI tests require controlled window geometry/display scaling and must distinguish assertions supported by a headless runner from real-compositor tests.
- Tests must clean up sockets, processes, devices, temporary files and threads even on failure.
- Profile/benchmark workloads separately; avoid performance thresholds on shared hosted runners without noise controls.

## Running tests after scaffolding

~~~bash
cargo nextest run --workspace
cargo test --workspace --doc
cargo llvm-cov nextest --workspace --lcov --output-path coverage.lcov
cmake --build build --target all_qmllint
~~~

`cargo-nextest` does not run doctests; they require the separate command. Hardware scenarios should be accessible through explicit CI workflow dispatch or a clearly documented harness, **not** triggered by ordinary unit-test commands.

**Acceptance evidence:** test suite name, host OS/arch, Kyber version, Qt/Rust versions, media source/device, codec, network path, timestamped measurements, failure logs, and a linked issue for any skipped or flaky test.
