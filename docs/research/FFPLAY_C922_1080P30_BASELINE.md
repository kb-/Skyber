# C922 Full HD webcam preview baseline

Measured on Windows 11 on 2026-11-24 (local time) with the Logitech C922 and FFplay 7.0.1. The user confirmed the preview was visible and judged its performance suitable as a baseline for Skyber's standard **1920 × 1080 at 30 fps** webcam profile.

## Setup

DirectShow reported two Full HD modes on this device. MJPEG supports up to 30 fps, while uncompressed YUYV422 supports up to 5 fps. FFplay negotiated 1920 × 1080 MJPEG at 30 fps and decoded it into a local preview window. The measurement used a 960 × 540 window with the Full HD source and no audio, re-encoding, or network transport.

To open the same capture mode with per-frame timestamps in the console, run this in Windows PowerShell:

```powershell
ffplay -hide_banner -nostats -f dshow -video_size 1920x1080 -framerate 30 -vcodec mjpeg -an -vf 'showinfo=checksum=0' -i 'video=c922 Pro Stream Webcam'
```

For the measured run, a local helper read the frame numbers and presentation timestamps from FFplay's `showinfo` filter. It calculated the change in frame number divided by the change in source timestamp over intervals of at least one second, updated the on-screen **Capture FPS** label, and saved each result to CSV. This measures the cadence of decoded frames reaching the filter on the source timeline. It does not measure screen refresh, displayed FPS, or camera-to-screen latency.

## Results

| Metric | Observed |
| --- | ---: |
| Recorded intervals | 38 |
| Source time covered | 40.03 s |
| Frame advances | 983 |
| Average over the full sample | 24.56 fps |
| Per-interval range | 5.73–30.00 fps |
| Intervals at or above 29.5 fps | 27 of 38 |
| Intervals below 20 fps | 10 of 38 |

Several intervals delivered about 30 fps, including the final three. The run also had dips, including intervals near 15 fps. The measurement did not isolate their cause.

## What this establishes for Skyber

Use the C922's MJPEG mode as a reference for native Full HD capture on Windows. The local run shows that the camera and FFplay can negotiate and preview this mode, with the user finding the result suitable. Sustained 30 fps remains to be verified.

For the [Kyber media reference proof](KYBER_MEDIA_UPSTREAM_COMPARISON.md), use the same resolution and camera mode and compare capture, encode, transport, decode, and displayed frame cadence. Measure latency and resource use separately. The FFplay baseline does not exercise the Skyber/Kyber call path.
