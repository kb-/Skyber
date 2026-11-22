# A2 audio continuity issue

**Status:** Open; root cause and remediation are not confirmed. Updated 2026-11-22.

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
Rust peer, and Kyber transport, which argues against those components being the
sole cause. The run record does not establish that `arecord` ran outside every
coding-agent process sandbox. A repeat from the Ubuntu guest's interactive
desktop terminal is needed to settle that distinction.

The evidence does not identify whether the remaining cause is the C922, the
Windows host's USB controller, VirtualBox USB passthrough, or the Ubuntu
guest's device/kernel path. It also does not establish acoustic speaker output
or prove that the original two-direction call's capture sources were the same
as those in the later C922 tests. No audio-capture or codec fix is confirmed.

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
