# Plan — Investigate long-call audio continuity without 4G

**Status:** Investigation plan, not a completed fix. Prepared against Skyber A2-2 commit 99a6960 (2026-11-22).
**Scope:** Windows 11 + Ubuntu 26.04.1 LTS in VirtualBox, on one host or two separate PCs over the existing home LAN. **No cellular connection, OpenVPN, public IP, or Internet access is required to reproduce or fix the defect.**
**Related:** [A2-2 timing evidence](ADR/A2-2_TIMING.md), [A2 development plan](PLAN_GATE_A2.md), [test policy](TESTING.md), [native capture adapters](ADR/A2-1A_WINDOWS_CAPTURE.md) and [Ubuntu capture adapter](ADR/A2-1B_UBUNTU_CAPTURE.md).

## 1. Problem statement and what the existing evidence proves

A 900-second bidirectional live call with camera, microphone, and screen in each direction was run with Windows on 4G and Ubuntu on its previous connection, connected through a self-hosted OpenVPN tunnel. The run reportedly had approximately 885 seconds of six-track overlap. All **published** reliable media packet IDs matched the receivers, but microphones did not produce near-continuous audio.

| Producer → consumer | Emitted/received Opus packets | Expected PCM frames in a 900 s uninterrupted 48 kHz source | Output sample frames drained | Output equivalent |
| --- | ---: | ---: | ---: | ---: |
| Windows → Ubuntu | 36,554 | 43,200,000 | 35,091,528 | 731.07 s |
| Ubuntu → Windows | 14,115 | 43,200,000 | 13,550,088 | 282.29 s |

For 20 ms packets at 48 kHz, 960 **sample frames per channel** are contained in each encoded Opus block. The observed outputs are *exactly* 36,554 × 960 − 312 and 14,115 × 960 − 312 sample frames. The 312-frame difference is consistent with the existing Opus decoder's initial priming/skip. This strongly indicates that decoder/playback output accounts for published audio packets; it **does not prove** that microphone capture continuously supplied blocks, or that transport backpressure did not throttle production upstream.

A full 900-second expected-duration comparator is only a rough initial reference. Permission dialogs, screen picker ordering, source startup and requested stop times differ. The precise regression **must compute expected audio from each source's independently measured, intended microphone-active interval**, not from remote peer clocks, total connection time, or screen overlap alone. Missing time at the start/end must not be silently excluded by defining the active interval from first/last successful emitted packet.

Recorded peak gaps in microphone helper timestamps were 1.35 seconds on Windows and 2.10 seconds on Ubuntu. They are symptoms; their timestamps are taken close to helper pipe output, not hardware exposure/audio callback timing. We still need to tell *where* work stalled.

**Primary question:** Is audio absent because the capture device/provider did not supply it, the helper stopped reading while blocked writing, Rust queues or encoding throttled capture, or the reliable Kyber transport delayed downstream consumers and fed back through bounded queues? Do not assume a single cause or that 4G loss is required.

## 2. Hypotheses and distinguishing evidence

| Hypothesis | Evidence that would support it | Discriminator |
| --- | --- | --- |
| H1: OS/GStreamer/Media Foundation microphone capture stalls | Long gaps at provider sample acquisition with downstream write unblocked | Reproduce in capture-only mode with a continuously draining sink |
| H2: Capture helper stdout/pipe blocks | Source callback is active but measured write/flush duration spikes; next pull/read delayed | Run same helper with fast sink, deliberately slow sink, and normal Rust reader |
| H3: Helper → Rust raw PCM queue pressure | Bounded raw queue is full or producer awaits send; packet IDs/capture timestamps jump | Record queue occupancy, send wait and consumption cadence per track |
| H4: Codec or scheduling starvation | Continuous source blocks arrive but encoder input processing or task scheduling has long gaps | Measure work time, off-CPU wait and encode queue; compare audio-only to camera+screen |
| H5: QUIC reliable endpoint induces backpressure | Audio pauses align with blocked Kyber send / rising queues; synthetic/full media differ, impairment amplifies | Compare capture-only, loopback, LAN, and controlled local network impairment |
| H6: Clock/measurement artifact or test setup | Expected duration inadvertently includes picker wait, capture started late, device switched, or sample rate differs | Explicit lifecycle markers; reconcile exact audio counts at every boundary |
| H7: Playback issue | Received/decoded PCM is complete but sink misses/drains early or actual sound stops | Compare decoded sample frames, helper-accepted frames and output-drain markers |

H7 is lower priority because the 4G sample-frame accounting matches received Opus packets, but still retain assertions. A healthy 15-minute synthetic call is a control, **not** proof that physical capture works continuously.

## 3. Minimal instrumentation — preserve performance and privacy

Instrument existing boundaries in the A2 branch, not a separate new media stack.

**Source helper** (Windows Media Foundation / Ubuntu GStreamer):
- Monotonic timestamps and counters for source opened, capture approved, first provider buffer, **each provider read/pull completion**, format/size, PCM sample frames accepted, blocks assembled, timestamp immediately before pipe write, write/flush start/end, last source buffer, stop requested, and clean process exit.
- Separate provider-buffer cadence, normalization/assembly (20 ms blocks), and stdout write-block duration. A timestamp generated before writing cannot by itself distinguish missing capture from blocked output.
- Counter for provider-supplied sample frames that never became a complete 960-frame block, whether and why remaining partial PCM was discarded, and any explicit provider discontinuity/dropped sample indication.
- On Windows specifically, inspect asynchronous IMFSourceReader callback/wait behavior and the PCM assembly buffer; on Ubuntu inspect GStreamer appsink pull cadence, PCM pending buffer and stdout.flush blocking.
- Avoid writing diagnostic text into the binary PCM stdout protocol. Use bounded structured stderr messages or a separate opt-in diagnostics channel/file outside the checkout; never dump voice samples.

**Rust / Kyber stages**:
- Counter/identifier per completed 960-frame block at capture-helper → Rust read, raw channel enqueue/dequeue, codec input/encoded packet, Kyber send attempt/completion, receiver arrival/decoder, playback queue/pipe and drain.
- Measure *wait times* on each bounded channel and Kyber send, separate from CPU work. Record queue depths and peak blocked intervals, including zero-progress spans and task/child exits.
- Compare monotonic clocks **within one process only** unless a calibrated mapping is present; do not subtract helper and Rust timestamps or Windows and Ubuntu timestamps as if their epochs match.
- Keep video/camera separate from microphone stage timings so screen sharing is identifiable as a correlated load source.
- Monitor **capture and playback helper child processes**, not only the Rust peer, for CPU, RSS and unexpected exits.
- No infinite packet/event accumulation: aggregate fixed-time buckets (e.g. 1 s), histograms, interval extrema and bounded diagnostic examples. Keep fine-grained per-packet data limited/sampled in soak runs; retain existing short-run detailed diagnostics where useful.
- Avoid logging microphone sample contents, camera pixels, private credentials, user-specific addresses, or screen contents.

**Lifecycle and expected-value accounting**:
- Record the **requested capture run duration**, source approval, first provider sample, first emitted block, deliberate mute/pause, last successfully captured block, requested stop, final provider activity, and helper exit.
- Define a source-active reference interval by an explicit **intent-to-capture start and stop state**; report startup latency and the *entire* active interval, including periods of silence/no samples. For an implementation that begins its countdown on first received sample, record that fact and report separately the requested session timing.
- A 48,000 Hz, 20 ms stereo PCM packet contains **960 sample frames**, or 1,920 interleaved scalar samples. Count sample frames, not twice that number.
- Compare expected sample frames over eligible active duration with provider-supplied, helper-emitted, Rust-read, encoded, transmitted, received, decoded and sink-drained sample frames. Account explicitly for codec priming (observed 312-frame skip), final partial block, and deliberate audio disable. Do not hide losses by resetting the expected timebase at the next successful block.

## 4. Experiment sequence — no 4G required

Run tests on both platforms and preserve the same actual microphone/device names and backend settings used during the long-call test. Start with 30–60 s diagnostics; use 15 minutes for confirmed reproduction and post-fix acceptance, not for every probe.

### Phase P0 — reproduce the accounting from saved evidence

1. Read locally retained data under target/skyber-openvpn/ if still present; record the current commit, binary hashes, exact peer.json configurations, start/stop timestamps and source helper logs. These are not committed to GitHub and must not be assumed available to CI.
2. Recompute expected vs observed sample frames from packet counts, Opus priming, real capture-active intervals and helper writes.
3. Plot time-bucketed microphone output for both peers alongside video load, send waits and transport RTT/loss. Separate factual correlations from inferred causes.
4. Produce a short *observed-versus-hypothesized* finding list before modifying the pipeline.

### Phase P1 — isolate each microphone helper

1. Run the existing Windows and Ubuntu helpers **without Kyber or audio playback**, reading binary stdout continuously with a non-blocking/draining test consumer.
2. The consumer validates framing (8-byte helper timestamp + 960 stereo F32 PCM frames), counts blocks/sample frames, records monotonic arrival intervals, and does not store PCM payloads. Handle partial pipe reads correctly.
3. Compare 30 s, 2 min, and 15 min, both otherwise idle and with representative background UI/video load. Check device renegotiation, background throttling and per-helper CPU/memory.
4. Inject a deliberately **slow stdout reader** as a separate diagnostic to establish what pipe-induced backpressure looks like. Never interpret this deliberately impaired run as a performance pass.
5. If the fast-drain capture-only helper drops audio, investigate provider callback/pull cadence, OS device behavior, and buffer handling **before** changing QUIC.

### Phase P2 — local Kyber with real microphone

1. Keep audio capture real; run two local Skyber peers on loopback (Windows first, Ubuntu guest second) with an in-memory decode sink. Test audio-only, then audio+camera, then audio+camera+screen.
2. Track complete audio duration through all stages. Compare P1 to P2 with identical duration/device and similar host load.
3. Run with decoded playback enabled and disabled to distinguish playback backpressure. Confirm the playback drain reports the exact frames accepted, not wall-clock acoustic output.
4. If the deficit begins only with video/screen, isolate codec CPU load, frame-conversion work, blocking sends and shared scheduling; do not generalize from audio-only success.

### Phase P3 — two physical machines on home LAN

1. Restore the known working bridged VirtualBox LAN topology. Keep the same two physically distinct machines, but **no OpenVPN, 4G, public port forwards or mobile hotspot**.
2. Reuse the working Ubuntu-initiated authenticated Kyber session if the reverse Wi-Fi bridge/UDP direction is still unresolved. Distinguish **QUIC connection initiator** from each direction of media publication.
3. Run unidirectional live microphone, bidirectional live microphone, then bidirectional all six live tracks, using the same device options, screen consent and sustained-profile settings as the original 4G run.
4. Repeat a 15-minute fully loaded run if shorter tests do not explain the failure. Compare P1/P2/P3 stage-by-stage; note whether loss emerges only with cross-host send or with video concurrency.
5. Record the actual usable capture interval for each side, device sample rate, QUIC RTT/loss, the delivered audio-duration fraction, largest gaps and root cause candidates. Never label LAN test timings as glass-to-glass.

### Phase P4 — controlled impairment, then optional final 4G confirmation

Only after P1–P3 identify and correct a cause, add reproducible delay/jitter/loss or intentionally slowed consumer in a **dedicated opt-in test lane**. Measure whether reliable Kyber media sends induce blocked queues and capture starvation. Prefer controlled impairment on the LAN/test adapter and do not disturb shared networks.

A **single final 4G/OpenVPN comparison** may confirm the fix under mobile conditions, but is **not a prerequisite** for investigating, automating, merging, or completing local audio-continuity work. Gate C's NAT traversal and Internet connectivity are separate.

## 5. Automated regression design and acceptance criteria

Deliver a dedicated harness, e.g. scripts/test-a2-audio-continuity.py (proposed name; not yet implemented), using existing private peer configs, existing source selectors and an explicit opt-in hardware/soak flag. Do not run live microphones during ordinary cargo test, Qt CI, or docs edits.

**Fast, deterministic regression (normal relevant code PRs):**
- Model a continuous 20 ms block producer with injected consumer stalls and bounded channels; assert no silent timebase reset or hidden source-frame deficit.
- Validate sample-frame mathematics (48 kHz, 960 frames/packet, stereo channel interpretation, configurable priming), lifecycle interval logic, reports, partial reads, one missing block, a deliberate gap, explicit cancellation, clean EOF and helper crash.
- Assert per-stage counters reconcile or explain any discrepancy, including source starvation vs backpressure vs decoder/presentation shortfall.
- Test percentile/gap metrics with injected monotonic timestamps; no dependence on real sleep timing or hardware.

**Opt-in integration/hardware lane:**
- Capture-only tests for each platform, followed by local loopback and two-host LAN as above. Mark actual skipped hardware cases as **not run**, not passed.
- Run a short smoke (30–60 s) with every adapter change; run a longer 2-minute diagnostic on relevant failures; run two separate 15-minute capture/loaded-LAN acceptance tests after a proposed fix.
- A failure should include structured summary fields for run configuration, device name (redacted when necessary), stage counts, intended capture-active duration, observed sample-frame duration, longest observed gap, measured stalls/queue waits, child-process status, platform, software versions, and local evidence paths.
- Keep logs, user-device metadata and any incidental recordings local. Commit only privacy-safe anonymized summaries.

**Acceptance targets (provisional, subject to validating real device behavior):**
- **Accounting:** no unexplained loss of complete 20 ms blocks at stage boundaries. Every intentional drop has an explicit reason and count. Decoder and drain counts reconcile with encoded packets and measured codec priming.
- **Duration:** production and decoded output each correspond to at least **99.5%** of the actual intended audio-active interval (after documented startup/stop/priming treatment) over a sustained 15-minute healthy-device baseline, with the reason for any residual deficit recorded. Do not shorten the interval to make the ratio pass.
- **Continuity:** no unexplained block-production gap exceeding **200 ms** during an intended continuous session; report p50/p95/p99 gaps and the largest gap separately. Acoustic silence is *not* missing capture if full-rate PCM silence blocks continue.
- **Stability:** bounded queues, no growing backlog over 15 minutes, no unexpected helper failure and explicit end-of-call drain/cancel; record helper and parent resource trends.
- **Verification:** repeat on Windows and Ubuntu, and run the two-host full-track case. If a platform legitimately cannot meet a provisional threshold, publish measured constraints and decide on product scope; do not silently relax the assertion.

The 15-minute test must not assert that the first packet arrives at call establishment or that screen sharing immediately starts. It must assert that after activation, microphone publication remains continuous through screen consent, screen stop and all concurrent media activity.

## 6. Implementation work packages

1. **Observation-only PR:** provider-versus-pipe timing, per-stage audio counters, blocked-send diagnostics, lifecycle timing, concise per-second summaries and fast deterministic tests. Keep behavior, codec and transport unchanged so failures remain attributable.
2. **Capture-only runner:** test each platform helper as a child with a continuously draining PCM consumer; optional artificial slow-consumer mode. Ensure the runner is usable without the second machine or 4G.
3. **Opt-in 15-minute loopback/LAN regression:** add reliable stage reconciliation and pass/fail reports. Reuse A1/A2 scaffolding and two-machine credentials; do not create a parallel media protocol.
4. **Root-cause fix PR:** make the smallest correction supported by measurements; preserve bounded memory, explicit error reporting, real device consent and Kyber transport. Add a targeted fast reproduction for the bug.
5. **Post-fix acceptance note:** record before/after metrics on both OSes, LAN results and remaining limitations in the A2-2 results/ADR, plus a separate longer-run log artifact kept locally.

Do not make speculative changes to QUIC reliability mode, microphone polling or codec packet cadence before the observation-only phase identifies the bottleneck. Evaluate alternative Kyber media reliability semantics separately for long-term low-latency design.

## 7. Exit / decision

**Investigation complete:** reproducible missing-audio scenario exists **without 4G**, the first responsible stage or mechanism is demonstrated by evidence, a targeted correction and fast regression are implemented, 15-minute Windows and Ubuntu hardware tests meet the explicitly documented continuity criteria, and the two-PC LAN test verifies concurrent real media.

**If capture-only tests are healthy but LAN+screen fails:** treat pipeline/transport backpressure as a first-class A2-3 and Gate C risk, retain diagnostic reproduction and fix or clearly identify scope limitations. **If capture-only fails:** fix the platform capture provider/reader first.

This investigation should not block exploratory Gate C design (STUN, NAT traversal, public endpoint authentication and relay architecture). It **should** block a claim of production-quality sustained voice calls until the audio deficit is resolved.

## 8. Reference evidence

- [A2-2 instrumentation and 4G long-run observations](ADR/A2-2_TIMING.md)
- [A2-1a Windows capture ownership and tests](ADR/A2-1A_WINDOWS_CAPTURE.md)
- [A2-1b Ubuntu microphone capture via GStreamer](ADR/A2-1B_UBUNTU_CAPTURE.md)
- [A2-1c output and drain behavior](ADR/A2-1C_PLAYBACK.md)
- [A2 hardware/soak test tier policy](TESTING.md)
- [Gate C scope and A2 staging](PLAN_SKYBER.md)
