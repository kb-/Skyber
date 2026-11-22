# A2-2 timing and packet identity

## Initial implementation

The gate peer records a bounded timing sample for each published or
received media packet. `packet_id_us` is the sender's monotonic time at
the point a complete raw frame reaches the Rust capture pipe. The peer
uses that value as the media packet PTS, increasing it by one if two
frames arrive within the same microsecond. The receiver preserves this
identifier for correlation. It does not use the sender value as a
receiver timestamp.

All other `_us` fields and duration summaries use the local peer's
monotonic clock, relative to that peer's media-run start. The JSON names
this clock domain as `peer_monotonic_us_since_media_start`. Durations
are calculated only between events recorded on the same peer. Windows
and Linux clocks are never compared.

The current boundaries are:

| Field | Boundary |
| --- | --- |
| `capture_helper_timestamp_us` | The platform capture helper's monotonic timestamp immediately before writing a complete fixed-size sample to its pipe, relative to helper start. This is not the camera sensor's exposure time and uses a separate helper clock domain. |
| `source_ready_us` | Rust has read a complete fixed-size frame and its timestamp from the capture helper pipe, relative to peer media-run start. |
| `encode_start_us`, `encode_end_us` | Around input conversion and the software codec call. |
| `encode_queue_depth` | Number of entries already in the bounded encoded-packet queue before enqueue. |
| `transport_send_start_us`, `transport_send_end_us` | Immediately around the Kyber media `send` call. This is not a network delivery or acknowledgment time. |
| `transport_receive_us` | When the Kyber media `recv` call returns a packet. |
| `decode_start_us`, `decode_end_us` | Around the codec decode call. |
| `playback_queue_depth`, `playback_enqueue_us` | Queue state and enqueue completion for decoded bytes accepted by the presentation task. |
| `playback_submit_start_us`, `playback_submit_end_us` | Rust's write of decoded bytes to the platform helper pipe. This is not display scanout or speaker completion. |
| `presentation_events` | For each Windows `WM_PAINT` or GTK `after-paint` event, the video helper reports the decoded packet ID and a helper-local monotonic timestamp. This is a window/compositor paint boundary, not physical display scanout. |
| `presentation_summary.decoded_frames_without_paint_event` | Decoded frame count minus correlated paint-event count. This is a presentation shortfall indicator, not a scanout or network-loss count. |
| `presentation_superseded_frames` | Video helper count of decoded frames overwritten in its presentation slot before a paint. |
| `audio_drain_events` | One helper-local event after all decoded PCM sample frames have completed the Windows `waveOut` buffer queue or the Ubuntu GStreamer sink has posted EOS after drain. The count must equal the decoded sample-frame total. This is not acoustic output or per-packet speaker latency. |

Video results also summarize event count, helper-local presentation rate,
and p50/p95 inter-event intervals. These describe paint cadence on that
helper only; they are not cross-host latency values. The decoded-frame
shortfall also exposes when a frame was superseded or otherwise had no
paint callback before the helper finished.

Each capture helper prefixes every fixed-size sample with an unsigned
little-endian 64-bit helper-relative monotonic timestamp. The peer records
that value without subtracting it from a Rust or receiver clock. Each track
result includes per-packet samples and p50/p95 durations with
sample counts. Queue depths are recorded for the capture, encode and
playback queues. The samples contain timing values and packet IDs only,
never media bytes. The sender's packet PTS is a correlation key; a
receiver must not subtract it from receiver-local timestamps.

Each peer result also includes one-second connection-stat samples from the
pinned Kyber `KyProtoStatsProvider`: Quinn path RTT and cumulative lost
transport packets. The report includes peer-local sample times, RTT
p50/p95, and the cumulative loss-counter delta across the run. These loss
counters cover the QUIC connection, including control traffic; they are not
counts of lost media frames. Kyproto's separate protocol dropped-packet
counter is retained when available, but remains `null` for the reliable
media endpoints used in this gate.

## Wi-Fi evidence (2026-11-22)

The instrumented peer was run on the Windows 11 host and the Ubuntu
26.04.1 LTS guest in both directions using the live webcam and microphone.
For each track, sender and receiver packet counts matched, all packet IDs
matched, every received packet was decoded, and the receiver recorded
playback-helper submissions. The regression verifier also checked local
event ordering and queue bounds. Compact raw results are retained at
`target/a2-2/live/20261121T230559.854087Z/metrics-results.json`.

| Direction | Track | Packets | Sender encode p50/p95 | Sender transport submit p50/p95 | Receiver decode p50/p95 | Playback pipe submit p50/p95 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Windows → Ubuntu | Microphone | 292 / 292 | 173 / 267 µs | 7 / 30 µs | 182 / 621 µs | 10 / 40 µs |
| Windows → Ubuntu | Camera | 66 / 66 | 441 / 669 µs | 12 / 29 µs | 717 / 1,594 µs | 329 / 767 µs |
| Ubuntu → Windows | Microphone | 193 / 193 | 431 / 705 µs | 8 / 13 µs | 73 / 170 µs | 8 / 20 µs |
| Ubuntu → Windows | Camera | 94 / 94 | 774 / 1,632 µs | 11 / 47 µs | 118 / 397 µs | 15 / 47 µs |

Each `Packets` cell is sent/received. These are per-peer stage durations,
not one-way network delay; the last column ends at the Rust-to-helper pipe.
The microphone run lengths differ because the live capture helpers finish
their fixed sample/frame windows at different times. This is an initial
instrumentation result and does not close the A2-2 gate.

### LAN connection-stat smoke run

A separate eight-second synthetic screen-stop call ran over Wi-Fi between
Windows 11 (`192.168.1.30`) and Ubuntu 26.04.1 (`192.168.1.36`). Both
peers emitted nine RTT samples, and each reported a zero change in the
cumulative QUIC lost-packet counter. Windows RTT p50/p95 was 3,802 / 5,314
µs; Ubuntu RTT p50/p95 was 3,554 / 10,820 µs. These are each peer's QUIC
path RTT estimates, not one-way latency. Nine samples are only a smoke
check and are insufficient for a stable network characterization. The
protocol dropped-packet metric was unavailable (`null`) on both peers.

### Video paint event check

The 12-second synthetic playback case passed on Windows and Ubuntu
26.04.1. Every reported paint event referenced a decoded packet ID, and
each helper-local clock increased.

| Platform | Track | Decoded | Painted | Superseded | No paint event | Paint rate | Inter-event p50/p95 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Windows GDI | Camera | 180 | 180 | 0 | 0 | 15.06 fps | 65,580 / 78,059 µs |
| Windows GDI | Screen | 105 | 105 | 0 | 0 | 15.12 fps | 63,339 / 78,233 µs |
| Ubuntu GTK | Camera | 180 | 161 | 18 | 19 | 15.07 fps | 66,950 / 71,411 µs |
| Ubuntu GTK | Screen | 105 | 87 | 17 | 18 | 15.10 fps | 66,895 / 71,839 µs |

The GTK viewer uses a latest-frame slot, so superseded frames can be
decoded without being painted. Superseded counts identify frames replaced
in that slot; the no-paint count can be larger when another frame has no
paint callback before shutdown. These events end at the native
window/compositor paint boundary, not display scanout. The latest checked
results are under `target/a2-2/presentation-shortfall-windows/` and on the
Ubuntu guest at `target/a2-2/presentation-shortfall-ubuntu/`.

### Audio output drain check

The 12-second synthetic playback case passed on Windows and Ubuntu
26.04.1. Each audio helper emitted exactly one drain event, and the event's
sample-frame count matched the receiver's decoded count of 575,688 on both
peers. Windows reported helper time 12,066,664 µs; Ubuntu reported
12,326,834 µs. The two helper clocks are independent. These events mark
completion at the platform output queue/sink drain boundary; they do not
measure sound pressure at the speaker or give per-packet output times.
Results are retained under
`target/a2-2/audio-drain-windows/20261122T002128.606686Z/` and on the
Ubuntu guest at `target/a2-2/audio-drain/20261122T002406.972306Z/`.

### Peer CPU and memory sampling

The test runner sampled peer-process CPU time and resident memory every
500 ms during a 12-second synthetic playback case. Windows produced 25
samples per peer; Ubuntu 26.04.1 produced 26. The observed sampled
resident high-water values were:

| Platform | Listener | Initiator |
| --- | ---: | ---: |
| Windows | 14.68 MiB | 14.06 MiB |
| Ubuntu 26.04.1 | 14.62 MiB | 13.44 MiB |

CPU time is included in each raw sample. Windows process accounting moved
in 15.625 ms steps in this short run, so these samples are not suitable for
fine CPU comparisons. The sampler measures the peer process only, not its
separate capture or playback helper children. Raw time series are retained
in the Windows playback result at
`target/a2-2/presentation-shortfall-windows/20261122T003808.513421Z/playback/`
and the Ubuntu guest's
`target/a2-2/presentation-shortfall-ubuntu/20261122T003833.911910Z/playback/`.

One full Windows playback run exceeded the peer's original four-second
post-media shutdown margin. The identical-duration direct run and one
complete wrapper retry passed. Playback tests now give both peers a
12-second post-media drain margin; they still time out if tracks or output
helpers fail to terminate within that bound.

### Helper timestamp protocol check

On the same Windows host and Ubuntu 26.04.1 guest, the updated Windows
Media Foundation and Ubuntu GStreamer helpers were exercised through the
live capture loopback suite. Every published media packet had one
helper-relative monotonic timestamp, the timestamps strictly increased,
and live track packet counts verified on both peers. The Windows helper
reported 217 microphone packets and 49 camera frames; Ubuntu with the
Logitech C922 reported 219 microphone packets and 71 camera frames. Media
payloads remained in memory. This checks the new local binary pipe framing;
it is not a hardware sensor timestamp or an end-to-end LAN latency sample.

## Limits and next measurements

These measurements describe application boundaries. They do not
measure camera exposure, transport one-way latency, physical display
scanout, acoustic output, or glass-to-glass delay. The audio drain event
marks completion at the platform output queue/sink boundary but does not
report acoustic output or per-packet speaker timing. Video presentation
events report a correlated window/compositor paint, not a per-packet
display device timestamp.

The remaining A2-2 work is to attribute no-paint shortfalls to specific
capture, encode, decode, or presentation queue events and sample resource
use for the separate media helpers. Glass-to-glass video and audio delay
require an external reference such as a high-speed camera and LED/display.
These are separate from the per-peer monotonic measurements above.
