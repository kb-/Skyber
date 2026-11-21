# Gate A2-0 Wi-Fi results — 2026-11-21

The Windows-to-Ubuntu synthetic media suite passed all six cases on two
physical PCs over Wi-Fi. Run ID `20261121T203955.028119Z`. Windows 11 build
22631 was the listener. The initiator was an x86-64 Ubuntu 26.04.1 LTS
guest with kernel 7.0.0-44-generic, bridged through VirtualBox on the second
PC. Ubuntu 26.04.1 LTS is the Linux target for both A2 stages.

The Windows peer was rebuilt from Skyber commit `2066cdf` with the existing
MinGW toolchain in WSL. The guest used its staged native Linux peer and the
pinned Kyber 0.29.0 software codec profile. VirtualBox version and the second
host's Windows build were unavailable for this run.

## Observed results

| Case | Result |
| --- | --- |
| Windows publishes A/V to Ubuntu | Pass |
| Ubuntu publishes A/V to Windows | Pass |
| Both peers publish audio, camera, and screen concurrently | Pass |
| Incorrect Kyber token | Rejected before media registration |
| Incorrect certificate fingerprint | Rejected before media registration |
| Fresh authenticated A/V call on UDP 34829 | Pass |

In the concurrent case, each receiver decoded 400 Opus blocks, 120 camera
frames, and 103 screen frames with matching sender packet counts. Windows
observed a 660.04 Hz tone and Ubuntu observed a 440.11 Hz tone. The video
patterns were distinct. Receiver-local decoded overlap was about 6.80 s on
Windows and 6.79 s on Ubuntu. Audio and camera continued for at least 1.13 s
after the last screen frame on each receiver. These intervals use each
receiver's monotonic clock.

## Reproduction and remaining checks

The Windows Wi-Fi firewall rule allowed inbound UDP 34829 on the Private
profile from the guest address only. The run used `test-a2.py prepare`,
`test-a2.py run` on both peers for each case, and
`py -3 scripts/test-a2.py verify target/a2/runs/20261121T203955.028119Z`.
The run directory holds the machine-readable report and per-peer logs.
See the [runbook](../RUN_GATE_A2_0.md) for the commands.

SSH and SCP worked for staging the initiator configuration, launching the
guest peer, and collecting results. This run covered synthetic media.
Physical capture, playback, and end-to-end latency remain later gate work.

## Desktop-sharing verification

Windows Remote Desktop connected to the active `skyber-a2` GNOME session
over TCP 3389. WinApp CLI 0.8.0 captured the connected window, clicked the
test editor's maximize button, typed a marker, and saved the document over
RDP. The second screenshot showed the maximized editor and the marker.
An SSH read verified the saved text on Ubuntu. Screenshots stayed in the
local `target/a2/desktop` directory.
