"""Measure live microphone capture with a continuously draining local reader."""

import argparse
import json
import os
from pathlib import Path
import runpy
import statistics
import subprocess
import sys
import time


SAMPLE_FRAMES_PER_BLOCK = 960
CHANNELS = 2
BYTES_PER_SAMPLE = 4
RECORD_BYTES = 8 + SAMPLE_FRAMES_PER_BLOCK * CHANNELS * BYTES_PER_SAMPLE
PROCESS_RESOURCE_SAMPLE = runpy.run_path(str(Path(__file__).with_name("test-a1.py")))[
    "process_resource_sample"
]


def read_exact(stream, size: int) -> bytes:
    chunks = bytearray()
    while len(chunks) < size:
        chunk = stream.read(size - len(chunks))
        if not chunk:
            if chunks:
                raise RuntimeError(f"truncated capture record ({len(chunks)} of {size} bytes)")
            return b""
        chunks.extend(chunk)
    return bytes(chunks)


def percentile(values: list[int], percent: int) -> int | None:
    if not values:
        return None
    ordered = sorted(values)
    return ordered[(len(ordered) * percent + 99) // 100 - 1]


def provider_frame_reconciliation(record_count: int, diagnostics: dict) -> tuple[int, bool] | None:
    provider_frames = diagnostics.get("provider_sample_frames")
    if provider_frames is None:
        return None
    if "pending_pcm_bytes" in diagnostics:
        partial_frames = diagnostics["pending_pcm_bytes"] // (CHANNELS * BYTES_PER_SAMPLE)
    elif "partial_interleaved_samples" in diagnostics:
        partial_frames = diagnostics["partial_interleaved_samples"] // CHANNELS
    else:
        return None
    expected_frames = record_count * SAMPLE_FRAMES_PER_BLOCK + partial_frames
    return provider_frames, provider_frames == expected_frames


def frame_coverage(observed_frames: int, expected_frames: int) -> float | None:
    if expected_frames <= 0:
        return None
    return observed_frames / expected_frames


def capture_acceptance_failures(duration_ms: int, slow_read_ms: int,
                                diagnostics: dict | None, record_count: int,
                                gaps: list[int]) -> list[str]:
    failures = []
    if duration_ms < 900_000 or slow_read_ms:
        failures.append("acceptance mode requires a 15-minute run with a fast reader")
    if diagnostics is None:
        return failures + ["acceptance mode requires helper diagnostics"]
    if diagnostics.get("requested_active_duration_us") != duration_ms * 1000:
        failures.append("helper requested active duration does not match the test")
    reconciliation = provider_frame_reconciliation(record_count, diagnostics)
    provider_frames = reconciliation[0] if reconciliation else None
    if reconciliation is not None and not reconciliation[1]:
        failures.append("provider frames do not reconcile with emitted PCM blocks")
    observed_frames = provider_frames if provider_frames is not None else (
        record_count * SAMPLE_FRAMES_PER_BLOCK
    )
    coverage = frame_coverage(observed_frames, duration_ms * 48)
    if coverage is None or coverage < 0.995:
        failures.append("captured sample frames fell below 99.5% of requested duration")
    provider_gap = diagnostics.get("max_provider_gap_us")
    if not isinstance(provider_gap, int):
        failures.append("helper maximum provider gap is missing")
    if max([*gaps, provider_gap if isinstance(provider_gap, int) else 0], default=0) > 200_000:
        failures.append("an unexplained provider or emitted-block gap exceeded 200 ms")
    return failures


def capture(helper: Path, device: str, duration_ms: int, slow_read_ms: int,
            acceptance: bool) -> dict:
    if os.name == "nt":
        command = [str(helper), "--stream", "microphone", device, str(duration_ms)]
    else:
        command = [str(helper), "microphone", str(duration_ms), "--device", device]
    environment = os.environ.copy()
    environment["SKYBER_AUDIO_DIAGNOSTICS"] = "1"
    child = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=environment)
    timestamps = []
    runner_start_ns = time.monotonic_ns()
    next_resource_sample_ns = runner_start_ns
    helper_resources = []
    try:
        assert child.stdout is not None
        while True:
            header = read_exact(child.stdout, 8)
            if not header:
                break
            sample_time_ns = time.monotonic_ns()
            if sample_time_ns >= next_resource_sample_ns:
                cpu_us, resident_bytes, peak_resident_bytes = PROCESS_RESOURCE_SAMPLE(child.pid)
                helper_resources.append({
                    "runner_timestamp_us": (sample_time_ns - runner_start_ns) // 1000,
                    "process_cpu_us": cpu_us,
                    "resident_bytes": resident_bytes,
                    "peak_resident_bytes": peak_resident_bytes,
                })
                next_resource_sample_ns = sample_time_ns + 1_000_000_000
            pcm = read_exact(child.stdout, RECORD_BYTES - 8)
            if not pcm:
                raise RuntimeError("capture helper ended between a record header and PCM payload")
            timestamps.append(int.from_bytes(header, "little"))
            if slow_read_ms:
                time.sleep(slow_read_ms / 1000)
        stderr = child.stderr.read() if child.stderr is not None else b""
        status = child.wait()
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
    gaps = [right - left for left, right in zip(timestamps, timestamps[1:])]
    if any(gap <= 0 for gap in gaps):
        raise RuntimeError("capture helper timestamps are not strictly increasing")
    diagnostics = None
    for line in stderr.decode("utf-8", errors="replace").splitlines():
        if line.startswith("SKYBER_AUDIO_DIAGNOSTICS "):
            diagnostics = json.loads(line.partition(" ")[2])
    if status:
        raise RuntimeError(f"capture helper exited {status}: {stderr.decode(errors='replace')}")
    if not timestamps:
        raise RuntimeError("capture helper emitted no complete microphone records")
    reconciliation = provider_frame_reconciliation(len(timestamps), diagnostics) if diagnostics else None
    provider_frames = reconciliation[0] if reconciliation else None
    if reconciliation is not None and not reconciliation[1]:
        raise RuntimeError(f"helper PCM accounting mismatch for {provider_frames} provider frames")
    if acceptance:
        failures = capture_acceptance_failures(
            duration_ms, slow_read_ms, diagnostics, len(timestamps), gaps
        )
        if failures:
            raise RuntimeError("; ".join(failures))
    return {
        "duration_ms_requested": duration_ms,
        "slow_reader_delay_ms": slow_read_ms,
        "records": len(timestamps),
        "sample_frames": len(timestamps) * SAMPLE_FRAMES_PER_BLOCK,
        "sample_frame_coverage_of_requested_duration": round(
            len(timestamps) * SAMPLE_FRAMES_PER_BLOCK / (duration_ms * 48), 6
        ),
        "provider_sample_frames": provider_frames,
        "provider_frame_coverage_of_requested_duration": (
            round(frame_coverage(provider_frames, duration_ms * 48), 6)
            if provider_frames is not None else None
        ),
        "capture_helper_span_us": timestamps[-1] - timestamps[0],
        "gap_us": {
            "samples": len(gaps),
            "p50": percentile(gaps, 50),
            "p95": percentile(gaps, 95),
            "p99": percentile(gaps, 99),
            "maximum": max(gaps, default=None),
            "mean": round(statistics.fmean(gaps)) if gaps else None,
        },
        "helper_diagnostics": diagnostics,
        "helper_resources": {
            "clock_domain": "continuity_runner_monotonic_us_since_start",
            "samples": helper_resources,
        },
        "acceptance_passed": acceptance,
        "provider_frame_accounting_matches": (
            reconciliation[1] if reconciliation is not None else None
        ),
        "helper_exit_status": status,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("helper", type=Path)
    parser.add_argument("--device", required=True, help="Windows device index or Ubuntu source name")
    parser.add_argument("--duration-ms", type=int, default=60_000)
    parser.add_argument("--slow-read-ms", type=int, default=0,
                        help="deliberately delay after each PCM block to demonstrate pipe backpressure")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--acceptance", action="store_true",
                        help="enforce 15-minute duration and continuity targets")
    args = parser.parse_args()
    if not 500 <= args.duration_ms <= 900_000 or args.slow_read_ms < 0:
        parser.error("duration must be 500..900000 ms and slow-reader delay cannot be negative")
    result = capture(args.helper.resolve(strict=True), args.device, args.duration_ms,
                     args.slow_read_ms, args.acceptance)
    report = json.dumps(result, indent=2) + "\n"
    if args.output:
        args.output.write_text(report, encoding="utf-8")
    print(report, end="")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, json.JSONDecodeError) as error:
        sys.exit(f"audio continuity capture failed: {error}")
