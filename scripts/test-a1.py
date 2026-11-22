"""Run the opt-in Gate A1 proof on a Windows host with a staged codec DLL."""

import argparse
from contextlib import ExitStack
import ctypes
from ctypes import wintypes
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import sys
import tempfile
import time


ROLES = ("listener", "initiator")


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def available_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
        udp.bind(("127.0.0.1", 0))
        return int(udp.getsockname()[1])


def await_marker(marker: Path, peer: subprocess.Popen[bytes], deadline: float) -> None:
    while not marker.exists():
        require(peer.poll() is None, f"peer exited before writing {marker.name}")
        require(time.monotonic() < deadline, f"deadline waiting for {marker.name}")
        time.sleep(0.025)


def read_result(output: Path, role: str) -> dict:
    return json.loads((output / f"{role}.stdout").read_text(encoding="utf-8"))


def process_cpu_ms(pid: int) -> float:
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel.OpenProcess.restype = wintypes.HANDLE
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.CloseHandle.restype = wintypes.BOOL
    kernel.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
    kernel.GetProcessTimes.restype = wintypes.BOOL
    process = kernel.OpenProcess(0x1000, False, pid)  # PROCESS_QUERY_LIMITED_INFORMATION
    if not process:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        created, exited, system, user = [wintypes.FILETIME() for _ in range(4)]
        if not kernel.GetProcessTimes(
            process, *(ctypes.byref(value) for value in (created, exited, system, user))
        ):
            raise ctypes.WinError(ctypes.get_last_error())
        ticks = sum((t.dwHighDateTime << 32) | t.dwLowDateTime for t in (system, user))
        return ticks / 10_000
    finally:
        kernel.CloseHandle(process)


def process_resource_sample(pid: int) -> tuple[int, int, int]:
    """Return process CPU microseconds, resident bytes, and peak resident bytes."""
    if sys.platform == "win32":
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        psapi = ctypes.WinDLL("psapi", use_last_error=True)
        kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        kernel.OpenProcess.restype = wintypes.HANDLE
        kernel.CloseHandle.argtypes = [wintypes.HANDLE]
        kernel.CloseHandle.restype = wintypes.BOOL
        kernel.GetProcessTimes.argtypes = [wintypes.HANDLE] + [
            ctypes.POINTER(wintypes.FILETIME)
        ] * 4
        kernel.GetProcessTimes.restype = wintypes.BOOL

        class ProcessMemoryCountersEx(ctypes.Structure):
            _fields_ = [
                ("cb", wintypes.DWORD), ("PageFaultCount", wintypes.DWORD),
                ("PeakWorkingSetSize", ctypes.c_size_t), ("WorkingSetSize", ctypes.c_size_t),
                ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
                ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                ("PagefileUsage", ctypes.c_size_t),
                ("PeakPagefileUsage", ctypes.c_size_t),
                ("PrivateUsage", ctypes.c_size_t),
            ]

        psapi.GetProcessMemoryInfo.argtypes = [
            wintypes.HANDLE, ctypes.POINTER(ProcessMemoryCountersEx), wintypes.DWORD
        ]
        psapi.GetProcessMemoryInfo.restype = wintypes.BOOL
        process = kernel.OpenProcess(0x1000 | 0x0010, False, pid)
        if not process:
            raise ctypes.WinError(ctypes.get_last_error())
        try:
            created, exited, system, user = [wintypes.FILETIME() for _ in range(4)]
            if not kernel.GetProcessTimes(
                process, *(ctypes.byref(value) for value in (created, exited, system, user))
            ):
                raise ctypes.WinError(ctypes.get_last_error())
            ticks = sum((t.dwHighDateTime << 32) | t.dwLowDateTime for t in (system, user))
            counters = ProcessMemoryCountersEx()
            counters.cb = ctypes.sizeof(counters)
            if not psapi.GetProcessMemoryInfo(process, ctypes.byref(counters), counters.cb):
                raise ctypes.WinError(ctypes.get_last_error())
            return ticks // 10, counters.WorkingSetSize, counters.PeakWorkingSetSize
        finally:
            kernel.CloseHandle(process)

    stat = Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    fields = stat[stat.rfind(")") + 2:].split()
    ticks = int(fields[11]) + int(fields[12])  # utime and stime, fields 14 and 15.
    ticks_per_second = os.sysconf("SC_CLK_TCK")
    status = Path(f"/proc/{pid}/status").read_text(encoding="ascii")
    memory = {line.split(":", 1)[0]: int(line.split()[1]) * 1024
              for line in status.splitlines()
              if line.startswith(("VmRSS:", "VmHWM:"))}
    return ticks * 1_000_000 // ticks_per_second, memory["VmRSS"], memory["VmHWM"]


def monitor_processes(
    processes: dict[str, subprocess.Popen[bytes]], timeout: float, interval: float = 0.5,
) -> dict[str, list[dict[str, int]]]:
    """Wait for peer processes while sampling their local CPU and memory use."""
    origin = time.monotonic_ns()
    samples: dict[str, list[dict[str, int]]] = {role: [] for role in processes}
    observed_peak_resident = {role: 0 for role in processes}
    next_sample = 0.0
    deadline = time.monotonic() + timeout
    while any(peer.poll() is None for peer in processes.values()):
        elapsed_us = (time.monotonic_ns() - origin) // 1000
        if elapsed_us / 1_000_000 >= next_sample:
            for role, peer in processes.items():
                if peer.poll() is not None:
                    continue
                try:
                    cpu_us, resident_bytes, peak_resident_bytes = process_resource_sample(peer.pid)
                except (OSError, KeyError, ValueError):
                    continue
                observed_peak_resident[role] = max(
                    observed_peak_resident[role], resident_bytes, peak_resident_bytes
                )
                samples[role].append({
                    "test_runner_timestamp_us": elapsed_us,
                    "process_cpu_us": cpu_us,
                    "resident_bytes": resident_bytes,
                    "peak_resident_bytes": observed_peak_resident[role],
                })
            next_sample += interval
        if time.monotonic() >= deadline:
            active = next(role for role, peer in processes.items() if peer.poll() is None)
            raise subprocess.TimeoutExpired(processes[active].args, timeout)
        time.sleep(0.05)
    return samples


def verify_resource_samples(samples: dict[str, list[dict[str, int]]]) -> None:
    for role in ROLES:
        rows = samples.get(role, [])
        require(bool(rows), f"{role} has no process resource samples")
        require(all(left["test_runner_timestamp_us"] < right["test_runner_timestamp_us"]
                    and left["process_cpu_us"] <= right["process_cpu_us"]
                    and left["peak_resident_bytes"] <= right["peak_resident_bytes"]
                    for left, right in zip(rows, rows[1:])),
                f"{role} process resource samples are not monotonic")
        require(all(row["resident_bytes"] > 0
                    and row["peak_resident_bytes"] >= row["resident_bytes"]
                    and row["process_cpu_us"] >= 0 for row in rows),
                f"{role} process resource sample is invalid")


def verify_timing(row: dict) -> None:
    samples = row.get("timings")
    require(isinstance(samples, list), "missing per-packet timing samples")
    summary = row.get("timing_summary", {})
    require(summary.get("clock_domain") == "peer_monotonic_us_since_media_start",
            "timing clock domain is missing or changed")
    if row["sent_packets"]:
        require(len(samples) == row["sent_packets"], "published timing sample count changed")
        if row.get("encoder_backend") == "kyber-avconv-libx264":
            require(summary["encode_us"]["samples"] == 0
                    and summary["transport_send_us"]["samples"] == row["sent_packets"],
                    "upstream timing must report transport only")
            packet_ids = [sample.get("packet_id_us") for sample in samples]
            require(all(isinstance(value, int) for value in packet_ids)
                    and all(left < right for left, right in zip(packet_ids, packet_ids[1:])),
                    "upstream packet PTS values are not increasing")
            for sample in samples:
                require(all(isinstance(sample.get(name), int) for name in (
                    "transport_send_start_us", "transport_send_end_us")),
                    "upstream packet is missing transport timestamps")
                require(sample["transport_send_start_us"] <= sample["transport_send_end_us"],
                        "upstream transport timestamps are out of order")
            return
        require(summary["encode_us"]["samples"] == row["sent_packets"]
                and summary["transport_send_us"]["samples"] == row["sent_packets"],
                "published stage sample count changed")
        require(summary["encode_queue_depth_max"] <= 2,
                "encoded packet queue exceeded its bound")
        helper_times = [sample.get("capture_helper_timestamp_us") for sample in samples]
        if any(timestamp is not None for timestamp in helper_times):
            require(all(isinstance(timestamp, int) and timestamp >= 0 for timestamp in helper_times),
                    "capture helper timestamp is missing or invalid")
            require(all(left < right for left, right in zip(helper_times, helper_times[1:])),
                    "capture helper timestamps are not increasing")
        for sample in samples:
            require(all(isinstance(sample.get(name), int) for name in (
                "packet_id_us", "source_ready_us", "encode_start_us", "encode_end_us",
                "transport_send_start_us", "transport_send_end_us")),
                "published packet is missing a timing boundary")
            require(sample["source_ready_us"] <= sample["encode_start_us"]
                    <= sample["encode_end_us"]
                    <= sample["transport_send_start_us"]
                    <= sample["transport_send_end_us"],
                    "published timing boundaries are out of order")
    if row["received_packets"]:
        require(len(samples) == row["received_packets"], "received timing sample count changed")
        require(summary["decode_us"]["samples"] == row["received_packets"],
                "decoded stage sample count changed")
        require(summary["playback_queue_depth_max"] <= 2,
                "playback queue exceeded its bound")
        for sample in samples:
            require(all(isinstance(sample.get(name), int) for name in (
                "packet_id_us", "transport_receive_us", "decode_start_us", "decode_end_us")),
                "received packet is missing a timing boundary")
            require(sample["transport_receive_us"] <= sample["decode_start_us"]
                    <= sample["decode_end_us"],
                    "received timing boundaries are out of order")
        if row.get("playback_bytes", 0):
            presented = [sample for sample in samples
                         if sample.get("playback_submit_start_us") is not None]
            require(bool(presented), "playback has no submission timestamps")
            require(all(sample["playback_submit_start_us"] <= sample["playback_submit_end_us"]
                        for sample in presented), "playback submission timing is out of order")
            require(summary["playback_submit_us"]["samples"] == len(presented),
                    "playback stage sample count changed")


def verify_transport(result: dict) -> None:
    transport = result.get("transport")
    require(isinstance(transport, dict), "missing Kyber transport statistics")
    require(transport.get("clock_domain") == "peer_monotonic_us_since_media_start",
            "transport clock domain is missing or changed")
    samples = transport.get("samples")
    require(isinstance(samples, list) and len(samples) >= 2,
            "transport run has too few counter samples")
    require(all(left["peer_timestamp_us"] < right["peer_timestamp_us"]
                for left, right in zip(samples, samples[1:])),
            "transport sample timestamps are not increasing")
    rtt_values = [sample["rtt_us"] for sample in samples if sample["rtt_us"] is not None]
    require(transport["rtt_us"]["samples"] == len(rtt_values),
            "RTT sample count does not match transport samples")
    loss_values = [sample["lost_packets_total"] for sample in samples
                   if sample["lost_packets_total"] is not None]
    require(all(left <= right for left, right in zip(loss_values, loss_values[1:])),
            "cumulative transport loss counter decreased")
    if loss_values:
        require(transport["lost_packets_total_start"] == loss_values[0]
                and transport["lost_packets_total_end"] == loss_values[-1]
                and transport["lost_packets_delta"] == loss_values[-1] - loss_values[0],
                "transport loss delta does not match cumulative counters")
    dropped = transport.get("protocol_dropped_packets")
    require(dropped is None or isinstance(dropped, int) and dropped >= 0,
            "protocol dropped-packet metric is invalid")


def verify_media(output: Path, profile: str, direction: str, stop_screen: bool, *, lan: bool = False) -> None:
    received = []
    track_names = ["microphone"]
    if profile != "audio":
        track_names.append("camera")
    if profile == "av_with_screen":
        track_names.append("screen")
    for role, other in [ROLES, ROLES[::-1]]:
        rows = read_result(output, role)["tracks"]
        for row in rows:
            verify_timing(row)
        incoming = {row["track"]: row for row in rows if row["received_packets"]}
        outgoing = {row["track"]: row for row in rows if row["sent_packets"]}
        expected_out = set(track_names) if direction in ("both", role) else set()
        expected_in = set(track_names) if direction in ("both", other) else set()
        require(set(outgoing) == expected_out, f"{role} published incorrect tracks")
        require(set(incoming) == expected_in, f"{role} received incorrect tracks")
        if not incoming:
            continue
        sent = {
            row["track"]: row["sent_packets"]
            for row in read_result(output, other)["tracks"]
            if row["sent_packets"]
        }
        for name, observed in incoming.items():
            require(observed["received_packets"] == sent[name], f"{role} lost {name} packets")
            require(observed["decoded_frames"] > 0, f"{role} decoded no {name} frames")
        frequency = 440 if other == "listener" else 660
        microphone = incoming["microphone"]
        require(abs(microphone["tone_hz"] - frequency) < 10, f"{role} decoded the wrong tone")
        if "screen" in incoming:
            screen, camera = incoming["screen"], incoming["camera"]
            require(screen["fingerprint"] != camera["fingerprint"], "video tracks decoded alike")
            if stop_screen:
                for continuing in (camera, microphone):
                    require(
                        continuing["last_decoded_us"] - screen["last_decoded_us"] >= 500_000,
                        f"{role} stopped another track with screen share",
                    )
        received.extend(incoming.values())
    if direction == "both" and lan:
        for role in ROLES:
            rows = [row for row in read_result(output, role)["tracks"] if row["received_packets"]]
            overlap = min(row["last_decoded_us"] for row in rows) - max(
                row["first_decoded_us"] for row in rows
            )
            require(overlap >= 5_000_000, f"{role} decoded less than five seconds of overlap")
    if direction == "both" and not lan:
        began = max(row["first_decoded_us"] for row in received)
        ended = min(row["last_decoded_us"] for row in received)
        require(ended - began >= 5_000_000, "less than five seconds of overlapping decoded output")
        print(f"{output.name} decoded overlap {(ended - began) / 1_000_000:.3f}s", flush=True)


def verify_live_media(output: Path, *, stop_screen: bool) -> None:
    published = read_result(output, "listener")["tracks"]
    received = read_result(output, "initiator")["tracks"]
    sent = {row["track"]: row["sent_packets"] for row in published}
    incoming = {row["track"]: row for row in received}
    expected = {"microphone", "camera", "screen"} if stop_screen else {"microphone", "camera"}
    require(set(sent) == set(incoming) == expected, "unexpected live tracks")
    for track, row in incoming.items():
        require(row["received_packets"] == sent[track] and row["decoded_frames"] > 0,
                f"live {track} failed to decode or lost packets")
    require(incoming["microphone"]["samples"] > 0, "microphone decoded no samples")
    if stop_screen:
        screen = incoming["screen"]
        require(screen["fingerprint"] != incoming["camera"]["fingerprint"],
                "screen and camera decoded alike")
        overlap = min(row["last_decoded_us"] for row in incoming.values()) - max(
            row["first_decoded_us"] for row in incoming.values()
        )
        require(overlap >= 2_000_000, "less than two seconds of concurrent live media")
        for track in ("microphone", "camera"):
            require(incoming[track]["last_decoded_us"] - screen["last_decoded_us"] >= 500_000,
                    f"{track} did not continue after screen sharing stopped")
    print("Live tracks and packet counts verified", flush=True)


def run_case(
    peer_exe: Path,
    output: Path,
    port: int,
    profile: str = "audio",
    direction: str = "both",
    duration: int = 5800,
    screen_stop: int | None = None,
    failure: str | None = None,
    capture: dict | None = None,
    playback: bool = False,
    audio_output: int | str | None = None,
    sustained: bool = False,
    upstream_avconv_frames: int | None = None,
) -> None:
    output.mkdir()
    with tempfile.TemporaryDirectory(prefix="skyber-a1-") as tmp, ExitStack() as cleanup:
        directory = Path(tmp)
        token = secrets.token_hex(24)
        for role in ROLES:
            (directory / role).mkdir()
        listener = directory / "listener"
        cert = listener / "cert.pem"
        key = listener / "key.pem"
        subprocess.run(
            [
                "openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt",
                "ec_paramgen_curve:prime256v1", "-nodes", "-subj", "/CN=localhost",
                "-addext", "subjectAltName=IP:127.0.0.1,DNS:localhost", "-days", "1",
                "-out", str(cert), "-keyout", str(key),
            ],
            check=True,
            capture_output=True,
        )
        fingerprint_line = subprocess.check_output(
            ["openssl", "x509", "-in", str(cert), "-noout", "-fingerprint", "-sha256"],
            text=True,
        )
        fingerprint = fingerprint_line.split("=", maxsplit=1)[1].replace(":", "").strip()
        peers: dict[str, subprocess.Popen[bytes]] = {}
        for role in ROLES:
            if role == "initiator" and failure == "timeout":
                break
            work = directory / role
            config = {
                "role": role,
                "port": port,
                "fingerprint": fingerprint,
                "secret": token,
                "profile": profile,
                "direction": direction,
                "duration_ms": duration,
                "sustained": sustained,
                "screen_stop_ms": screen_stop,
                "playback_drain_timeout_ms": 12000 if playback else 0,
            }
            play = playback and role == "initiator"
            if play and audio_output is not None:
                config["audio_output"] = audio_output
            live = capture is not None and role == "listener"
            if capture is not None:
                config["capture_wait_ms"] = 63000
            if live:
                config["capture"] = capture
            if upstream_avconv_frames is not None and role == "listener":
                config["upstream_avconv_frames"] = upstream_avconv_frames
            if role == "initiator" and failure == "auth":
                config["secret"] = secrets.token_hex(24)
            if role == "initiator" and failure == "certificate":
                config["fingerprint"] = "00" * 32
            if role == "initiator" and failure == "track_map":
                config["profile"] = "av"
            (work / "peer.json").write_text(json.dumps(config), encoding="utf-8")
            stdout = cleanup.enter_context((output / f"{role}.stdout").open("wb"))
            stderr = cleanup.enter_context((output / f"{role}.stderr").open("wb"))
            peer = subprocess.Popen(
                [str(peer_exe), *(["--capture"] if live else []),
                 *(["--playback"] if play else []), str(work)],
                stdout=stdout,
                stderr=stderr,
                creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            )
            peers[role] = peer
            cleanup.callback(stop_process, peer)
            if role == "listener":
                await_marker(work / "ready", peer, time.monotonic() + 5)

        if failure == "hangup":
            await_marker(listener / "microphone.receiving", peers["listener"], time.monotonic() + 6)
            stop_process(peers["initiator"])
        timeout_seconds = (
            duration / 1000 + 85 if capture is not None else 35 if playback else 20
        )
        resources = monitor_processes(peers, timeout_seconds)
        (output / "resource-samples.json").write_text(json.dumps({
            "clock_domain": "test_runner_monotonic_us_since_launch",
            "processes": resources,
        }, indent=2) + "\n", encoding="utf-8")
        if failure is None:
            verify_resource_samples(resources)
        for role, peer in peers.items():
            require((peer.returncode == 0) == (failure is None),
                    f"{role} unexpected exit {peer.returncode}")
        if failure == "auth":
            require("authentication rejected" in read_stderr(output, "listener"), "wrong auth error")
        if failure == "timeout":
            require("connection timed out" in read_stderr(output, "listener"), "wrong timeout error")
        if failure == "certificate":
            require("Certificate hash mismatch" in read_stderr(output, "initiator"), "wrong TLS error")
        if failure == "track_map":
            require(
                any(
                    any(message in read_stderr(output, role) for message in (
                        "unexpected remote",
                        "peer did not configure",
                    ))
                    for role in ROLES
                ),
                "neither peer rejected the mismatched track map",
            )
        if failure == "capture_cancelled":
            require("Screen selection cancelled" in read_stderr(output, "listener"),
                    "screen picker did not report cancellation")
            require(not any((directory / "initiator").glob("*.receiving")),
                    "media was received after cancelling consent")
        if failure is None and capture is not None:
            for role in ROLES:
                verify_transport(read_result(output, role))
            verify_live_media(output, stop_screen=screen_stop is not None)
        if failure is None and capture is None:
            for role in ROLES:
                verify_transport(read_result(output, role))
            verify_media(output, profile, direction, screen_stop is not None, lan=sys.platform != "win32")
        if failure is None and playback:
            for row in read_result(output, "initiator")["tracks"]:
                if not row["received_packets"]:
                    continue
                track = row["track"]
                expected = row["samples"] * 4 if track == "microphone" else row["decoded_frames"] * 86400
                require(row.get("playback_bytes") == expected and expected > 0,
                        f"{track} playback did not receive every decoded frame")
                if track in ("camera", "screen"):
                    events = row.get("presentation_events", [])
                    require(bool(events), f"{track} has no native presentation events")
                    packet_ids = {sample["packet_id_us"] for sample in row["timings"]}
                    require(all(event["packet_id_us"] in packet_ids
                                and event["helper_clock_domain"] == "playback_helper_monotonic_us_since_start"
                                for event in events),
                            f"{track} presentation event does not match a decoded packet")
                    require(all(left["helper_timestamp_us"] < right["helper_timestamp_us"]
                                for left, right in zip(events, events[1:])),
                            f"{track} presentation clock is not increasing")
                    summary = row.get("presentation_summary", {})
                    superseded = row.get("presentation_superseded_frames")
                    require(summary.get("presented_frames") == len(events)
                            and summary.get("decoded_frames_without_paint_event")
                                == max(0, row["decoded_frames"] - len(events))
                            and isinstance(superseded, int)
                            and 0 <= superseded
                                <= summary["decoded_frames_without_paint_event"]
                            and summary.get("clock_domain") == "playback_helper_monotonic_us_since_start"
                            and summary.get("inter_event_interval_us", {}).get("samples")
                                == max(0, len(events) - 1),
                            f"{track} presentation summary is incomplete")
            for track in ("camera", "screen"):
                if profile == "av_with_screen" or (profile == "av" and track == "camera"):
                    require(f"{track} frames" in read_stderr(output, "initiator"),
                            f"{track} viewer did not report presentation")
            require("stereo sample frames" in read_stderr(output, "initiator"),
                    "audio helper did not report completed playback")
            audio = read_result(output, "initiator")["tracks"]
            audio = next(row for row in audio if row["track"] == "microphone")
            drain_events = audio.get("audio_drain_events", [])
            require(len(drain_events) == 1
                    and drain_events[0].get("sample_frames") == audio["samples"]
                    and drain_events[0].get("helper_timestamp_us", 0) > 0
                    and drain_events[0].get("helper_clock_domain")
                        == "playback_helper_monotonic_us_since_start",
                    "audio helper drain event does not match decoded sample frames")
        if failure is None and sys.platform == "win32":
            usage = {role: process_cpu_ms(peer.pid) for role, peer in peers.items()}
            (output / "cpu-ms.json").write_text(json.dumps(usage), encoding="utf-8")
    print(f"{output.name} passed", flush=True)


def read_stderr(output: Path, role: str) -> str:
    return (output / f"{role}.stderr").read_text(encoding="utf-8", errors="replace")


def stop_process(peer: subprocess.Popen[bytes]) -> None:
    if peer.poll() is None:
        peer.kill()
        peer.wait(timeout=5)


def main() -> None:
    require(sys.platform == "win32", "Gate A1 peer tests run on the Windows host")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("peer", type=Path)
    parser.add_argument("--output", type=Path, default=Path("target/a1/runs"))
    parser.add_argument("--audio-only", action="store_true")
    parser.add_argument("--upstream-avconv-frames", type=int,
                        help="run the synthetic Kyber avconv video through Kymux instead of the full suite")
    parser.add_argument("--playback", action="store_true",
                        help="show the received test video using the existing A1 presentation helper")
    args = parser.parse_args()
    peer_exe = args.peer.resolve(strict=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    output = args.output / stamp
    output.mkdir(parents=True)
    print(f"Gate A1 results directory {output}", flush=True)
    if args.upstream_avconv_frames is not None:
        require(1 <= args.upstream_avconv_frames <= 9000,
                "upstream avconv frame count must be 1..9000")
        run_case(peer_exe, output / "upstream-avconv-video", available_port(),
                 "av", "listener", duration=10_000, playback=args.playback,
                 upstream_avconv_frames=args.upstream_avconv_frames)
        verify_media(output / "upstream-avconv-video", "av", "listener", False)
    else:
        require(not args.playback, "--playback requires --upstream-avconv-frames")
        run_suite(peer_exe, output, args.audio_only)


def run_suite(peer_exe: Path, output: Path, audio_only: bool = False) -> None:
    port = available_port()
    overlap_margin = 1500 if sys.platform == "linux" else 0
    run_case(peer_exe, output / "audio-both", port, duration=5800 + overlap_margin)
    if audio_only:
        return
    run_case(peer_exe, output / "av-listener", port, "av", "listener", 2000)
    run_case(peer_exe, output / "av-initiator", port, "av", "initiator", 2000)
    run_case(peer_exe, output / "concurrent-screen-stop", port, "av_with_screen", duration=6500 + overlap_margin,
             screen_stop=5400 + overlap_margin)
    for error in ("auth", "certificate", "track_map", "timeout", "hangup"):
        run_case(peer_exe, output / error, port, failure=error)
    run_case(peer_exe, output / "rejoin-same-port", port, "av", "listener", 1000)


if __name__ == "__main__":
    main()
