"""Run the opt-in Gate A1 proof on a Windows host with a staged codec DLL."""

import argparse
from contextlib import ExitStack
import ctypes
from ctypes import wintypes
from datetime import datetime, timezone
import json
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
        if not kernel.GetProcessTimes(process, created, exited, system, user):
            raise ctypes.WinError(ctypes.get_last_error())
        ticks = sum((t.dwHighDateTime << 32) | t.dwLowDateTime for t in (system, user))
        return ticks / 10_000
    finally:
        kernel.CloseHandle(process)


def verify_media(output: Path, profile: str, direction: str, stop_screen: bool, *, lan: bool = False) -> None:
    received = []
    track_names = ["microphone"]
    if profile != "audio":
        track_names.append("camera")
    if profile == "av_with_screen":
        track_names.append("screen")
    for role, other in [ROLES, ROLES[::-1]]:
        rows = read_result(output, role)["tracks"]
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
                "screen_stop_ms": screen_stop,
            }
            play = playback and role == "initiator"
            if play and audio_output is not None:
                config["audio_output"] = audio_output
            live = capture is not None and role == "listener"
            if capture is not None:
                config["capture_wait_ms"] = 63000
            if live:
                config["capture"] = capture
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
        deadline = time.monotonic() + (85 if capture is not None else 20)
        for role, peer in peers.items():
            status = peer.wait(timeout=max(0.1, deadline - time.monotonic()))
            require((status == 0) == (failure is None), f"{role} unexpected exit {status}")
        if failure == "auth":
            require("authentication rejected" in read_stderr(output, "listener"), "wrong auth error")
        if failure == "timeout":
            require("connection timed out" in read_stderr(output, "listener"), "wrong timeout error")
        if failure == "certificate":
            require("Certificate hash mismatch" in read_stderr(output, "initiator"), "wrong TLS error")
        if failure == "track_map":
            require(
                any("unexpected remote" in read_stderr(output, role) for role in ROLES),
                "neither peer rejected the mismatched track map",
            )
        if failure == "capture_cancelled":
            require("Screen selection cancelled" in read_stderr(output, "listener"),
                    "screen picker did not report cancellation")
            require(not any((directory / "initiator").glob("*.receiving")),
                    "media was received after cancelling consent")
        if failure is None and capture is not None:
            verify_live_media(output, stop_screen=screen_stop is not None)
        if failure is None and capture is None:
            verify_media(output, profile, direction, screen_stop is not None, lan=sys.platform != "win32")
        if failure is None and playback:
            for row in read_result(output, "initiator")["tracks"]:
                if not row["received_packets"]:
                    continue
                track = row["track"]
                expected = row["samples"] * 4 if track == "microphone" else row["decoded_frames"] * 86400
                require(row.get("playback_bytes") == expected and expected > 0,
                        f"{track} playback did not receive every decoded frame")
            for track in ("camera", "screen"):
                if profile == "av_with_screen" or (profile == "av" and track == "camera"):
                    require(f"{track} frames" in read_stderr(output, "initiator"),
                            f"{track} viewer did not report presentation")
            require("stereo sample frames" in read_stderr(output, "initiator"),
                    "audio helper did not report completed playback")
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
    args = parser.parse_args()
    peer_exe = args.peer.resolve(strict=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    output = args.output / stamp
    output.mkdir(parents=True)
    print(f"Gate A1 results directory {output}", flush=True)
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
