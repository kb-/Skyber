# A2 audio continuity issue

**Status:** Investigation paused with an environment-specific VirtualBox USB
capture limitation recorded; no Skyber audio fix is confirmed. Updated 2026-11-22.

## Symptoms

A 900-second bidirectional call with camera, microphone, and screen sharing
showed much less decoded audio than the uninterrupted 48 kHz reference. The
Windows-to-Ubuntu direction delivered 35,091,528 decoded sample frames
(about 731 seconds); Ubuntu-to-Windows delivered 13,550,088 frames (about
282 seconds). Published media packet IDs matched at the receivers, but the
source microphones' intended active intervals were not independently measured
in that run. Matching packets therefore does not establish continuous capture.

## Findings

| Capture source | Duration | Sample-frame coverage | Largest provider gap |
| --- | ---: | ---: | ---: |
| Windows microphone index 0 | 900 s | 99.958% | 68.7 ms |
| Ubuntu onboard microphone | 900 s | 99.951% | 30.9 ms |
| Ubuntu VM, Logitech C922 microphone, first run | 900 s | 75.15% | 4.13 s |
| Ubuntu VM, Logitech C922 microphone, confirmation | 900 s | 57.95% | 4.12 s |

A 15-minute Ubuntu C922 loopback showed 57.21% provider coverage. Its emitted
blocks matched the peer's published packets, and all published packets were
received and decoded. Rust capture-queue wait p95 was 1 microsecond and the
largest helper output flush was 2.9 ms. This run places the observed shortfall
before peer queueing and loopback delivery.

Plain ALSA capture of the C922 to `/dev/null` also failed with an
`Input/output error`: 120 seconds at 32 kHz stopped after about 30 seconds;
60-second attempts at 32 kHz and 16 kHz failed similarly. A 10-second 32 kHz
attempt completed, and the onboard-microphone ALSA control passed for 60
seconds. No microphone audio was saved in these checks. See
[A2-2 timing evidence](ADR/A2-2_TIMING.md) for run details.

## Sandbox and cause assessment

The direct ALSA failure reproduces independently of Skyber's GStreamer helper,
Rust peer, and Kyber transport. Retesting directly in the Ubuntu guest outside
the Windows coding-agent sandbox reproduced the failure.

The evidence does not identify whether the remaining cause is the C922, the
Windows host's USB controller, VirtualBox USB passthrough, or the Ubuntu
guest's device/kernel path. It also does not establish acoustic speaker output
or prove that the original two-direction call's capture sources were the same
as those in the later C922 tests. No audio-capture or codec fix is confirmed.

The investigation is closed for now because direct C922 capture remains
unreliable in this VirtualBox setup, while the onboard Ubuntu microphone
completed the 15-minute loopback with 44,929 Opus packets and 99.951% provider
coverage. Continue Ubuntu synthetic-media testing. Keep sustained, loaded-call
audio quality open as a product validation item; these results do not establish
that long-call audio is fixed across devices or environments.

## VirtualBox USB 2.0 retest (2026-11-22)

After a guest restart, `lsusb -t` showed the C922 video and audio interfaces
on the virtual EHCI controller at 480 Mbit/s. The Linux guest still listed the
C922 under ALSA and PipeWire. The direct command below returned `Input/output
error` shortly after starting.

```bash
arecord -D hw:Webcam,0 -t raw -f S16_LE -r 32000 -c 2 -d 120 /dev/null
```

The fast-draining Skyber capture harness, requested for 60 seconds, timed out
after about 5 seconds with zero provider buffers. A 15-second onboard microphone
control using the same helper in the same guest produced 742 blocks with a
maximum block gap of 30.0 ms. The USB 2.0 setting did not resolve the C922
failure in this retest.

## Virtual audio input with the USB filter disabled (2026-11-22)

With the VirtualBox USB filter disabled and the C922 selected as the Windows
host's default input by the operator, the guest no longer enumerated the C922
as a USB device. Ubuntu's default input was the virtual Intel ICH source
`alsa_input.pci-0000_00_05.0.analog-stereo`. This configuration tests the VM's
virtual audio route. The guest cannot independently identify the physical
microphone selected on the host.

The 60-second Skyber capture harness received 2,990 complete blocks and
2,870,400 sample frames (99.67% coverage). The maximum provider and emitted
block gaps were 31.2 ms and 39.9 ms, respectively. Signal checks then showed
the guest was receiving silence:

- Direct stereo ALSA capture from `hw:I82801AAICH,0` at 48 kHz completed
  120 seconds and returned all 5,760,000 expected frames. Every sample was zero.
- A 10-second run of the Skyber helper on the default source returned 493
  complete blocks, all zero, with the PipeWire input at volume 1.00 and ALSA
  capture enabled.
- A 5-second capture from the alternate virtual microphone input
  `plughw:I82801AAICH,1` also returned only zero samples.

The timing check shows that the VM delivers buffers on the virtual audio
route. It does not establish that microphone audio reaches the guest. Check
the C922 input meter on the Windows host and whether Audio Input is enabled
in VirtualBox before using this route for a call. The guest did not list a
video source in PipeWire with this configuration. Only counts and peak levels
were retained from the signal checks.

## Next diagnostic step

Test the same C922 with the VM detached on the second physical Windows host
that runs VirtualBox, then attached to the VM. Compare host capture, direct
guest ALSA capture, and the existing fast-draining guest capture harness.
If host capture succeeds and guest ALSA capture fails, change one available
VirtualBox USB setting at a time.
Keep any setting that restores sustained capture and document it in the A2
run instructions. Change Skyber only if the measurements place the failure in
its capture helper or media pipeline.

The current provisional sustained-capture targets are at least 99.5% of
intended sample frames and no unexplained gap over 200 ms. The two-host A2
loaded-call and audible-playback checks remain separate acceptance evidence;
the existing drain counters do not measure sound pressure or prove audible
continuity.
