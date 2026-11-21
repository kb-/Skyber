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

Video results also summarize event count, helper-local presentation rate,
and p50/p95 inter-event intervals. These describe paint cadence on that
helper only; they are not cross-host latency values.

Each capture helper prefixes every fixed-size sample with an unsigned
little-endian 64-bit helper-relative monotonic timestamp. The peer records
that value without subtracting it from a Rust or receiver clock. Each track
result includes per-packet samples and p50/p95 durations with
sample counts. Queue depths are recorded for the capture, encode and
playback queues. The samples contain timing values and packet IDs only,
never media bytes. The sender's packet PTS is a correlation key; a
receiver must not subtract it from receiver-local timestamps.

## Wi-Fi evidence (2026-11-22)

The instrumented peer was run on the Windows 11 host and the Ubuntu
26.04.1 LTS guest in both directions using the live webcam and microphone.
For each track, sender and receiver packet counts matched, all packet IDs
matched, every received packet was decoded, and the receiver recorded
playback-helper submissions. The regression verifier also checked local
event ordering and queue bounds. Compact raw results are retained at
`target/a2-2/live/20261121T230559.854087Z/metrics-results.json`.

| Direction | Track | Packets | Sender encode p50/p95 | Receiver decode p50/p95 | Playback pipe submit p50/p95 |
| --- | --- | ---: | ---: | ---: | ---: |
| Windows → Ubuntu | Microphone | 292 / 292 | 173 / 266 µs | 182 / 619 µs | 10 / 33 µs |
| Windows → Ubuntu | Camera | 66 / 66 | 441 / 633 µs | 717 / 1,576 µs | 329 / 737 µs |
| Ubuntu → Windows | Microphone | 193 / 193 | 431 / 688 µs | 73 / 162 µs | 8 / 20 µs |
| Ubuntu → Windows | Camera | 94 / 94 | 774 / 1,579 µs | 118 / 368 µs | 15 / 46 µs |

Each `Packets` cell is sent/received. These are per-peer stage durations,
not one-way network delay; the last column ends at the Rust-to-helper pipe.
The microphone run lengths differ because the live capture helpers finish
their fixed sample/frame windows at different times. This is an initial
instrumentation result and does not close the A2-2 gate.

### Video paint event check

The 12-second synthetic playback case passed on Windows and Ubuntu
26.04.1. Every reported paint event referenced a decoded packet ID, and
each helper-local clock increased.

| Platform | Track | Decoded frames | Paint events | Paint rate | Inter-event p50/p95 |
| --- | --- | ---: | ---: | ---: | ---: |
| Windows GDI | Camera | 180 | 180 | 15.04 fps | 68,027 / 77,559 µs |
| Windows GDI | Screen | 105 | 105 | 15.05 fps | 68,299 / 77,653 µs |
| Ubuntu GTK | Camera | 180 | 163 | 15.10 fps | 67,107 / 71,275 µs |
| Ubuntu GTK | Screen | 105 | 86 | 15.20 fps | 67,179 / 69,328 µs |

The GTK viewer uses a latest-frame slot, so superseded frames can be
decoded without being painted. These events end at the native
window/compositor paint boundary, not display scanout. Structured Windows
and Ubuntu results are under `target/a2-2/presentation-windows-timeoutfix/`
and `target/a2-2/ubuntu-presentation-timeoutfix.stdout`.

One full Windows playback run exceeded the peer's original four-second
post-media shutdown margin. The identical-duration direct run and one
complete wrapper retry passed. The playback test peer now allows a
12-second drain margin for output helpers; it still times out if the
helpers or tracks fail to terminate within that bound.

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
scanout, acoustic output completion, or glass-to-glass delay. In
particular, the audio playback submission boundary ends at the helper
pipe. Video presentation events report a correlated window/compositor
paint, not a per-packet display device timestamp.

The remaining A2-2 work is to add native audio output completion events,
expose supported Kyber RTT and loss data, and collect CPU, memory,
frame-rate and drop measurements. Glass-to-glass video and audio delay
require an external reference such as a high-speed camera and
LED/display. These are separate from the per-peer monotonic measurements
above.
