#!/usr/bin/env python3
"""Run the pinned Kyber avconv synthetic video encode/decode probes."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--directory", type=Path, default=Path("target/a2-2/win")
    )
    parser.add_argument("--frames", type=int, default=150)
    args = parser.parse_args()
    if sys.platform != "win32":
        parser.error("the pinned avconv reference probes currently target Windows")
    if not 1 <= args.frames <= 9000:
        parser.error("--frames must be between 1 and 9000")

    encoder = args.directory / "skyber-avconv-frame-probe.exe"
    decoder = args.directory / "skyber-avconv-packet-decode-probe.exe"
    if not encoder.is_file() or not decoder.is_file():
        parser.error("build both probes first with scripts/build-kyber-avconv-frame-probe.ps1")

    source = subprocess.Popen(
        [str(encoder), "--emit", "--synthetic", str(args.frames)],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    sink = subprocess.Popen(
        [str(decoder)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        if source.stdout is None or sink.stdin is None:
            raise RuntimeError("could not create the synthetic avconv test pipes")
        while chunk := source.stdout.read(64 * 1024):
            sink.stdin.write(chunk)
        sink.stdin.close()
        decoder_output, decoder_error = sink.communicate(timeout=30)
        _, encoder_error = source.communicate(timeout=30)
    except BaseException:
        for process in (source, sink):
            if process.poll() is None:
                process.kill()
        source.wait()
        sink.wait()
        raise

    if source.returncode != 0:
        raise RuntimeError(
            f"Kyber avconv encoder exited {source.returncode}: "
            f"{encoder_error.decode(errors='replace')}"
        )
    if sink.returncode != 0:
        raise RuntimeError(
            f"Kyber avconv decoder exited {sink.returncode}: "
            f"{decoder_error.decode(errors='replace')}"
        )
    rows = [line for line in decoder_output.splitlines() if line.strip()]
    if len(rows) != 1:
        raise RuntimeError(f"expected one decoder summary, got {len(rows)} lines")
    summary = json.loads(rows[0])
    if summary.get("decoder_backend") != "kyber-avconv-h264":
        raise RuntimeError(f"unexpected decoder backend: {summary!r}")
    if summary.get("packets") != args.frames:
        raise RuntimeError(f"expected {args.frames} packets, got {summary!r}")
    if summary.get("decoded_frames") != args.frames:
        raise RuntimeError(f"expected {args.frames} decoded frames, got {summary!r}")
    if not summary.get("frame_checksum"):
        raise RuntimeError(f"decoded frame checksum was empty: {summary!r}")
    print(json.dumps(summary, indent=2))
    print(encoder_error.decode(errors="replace"), end="")
    print("Kyber avconv synthetic encode/decode round trip passed")


if __name__ == "__main__":
    main()
