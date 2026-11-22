"""Run the consent-based A2-1 live-source loopback check."""

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import platform
import runpy
import sys


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("peer", type=Path)
    ubuntu = sys.platform == "linux"
    device_type = str if ubuntu else int
    parser.add_argument("--camera", type=device_type, required=True)
    parser.add_argument("--microphone", type=device_type, required=True)
    parser.add_argument("--duration-ms", type=int, default=4500,
                        help="live capture duration, up to 900000 ms for sustained runs")
    parser.add_argument("--acceptance", action="store_true",
                        help="enforce 15-minute microphone continuity thresholds")
    parser.add_argument("--output", type=Path,
                        default=Path("target/a2-1b/runs" if ubuntu else "target/a2-1a/runs"))
    screen = parser.add_mutually_exclusive_group()
    screen.add_argument("--with-screen", action="store_true")
    screen.add_argument("--cancel-screen", action="store_true")
    args = parser.parse_args()
    if not (ubuntu or sys.platform == "win32"):
        parser.error("this check runs on Windows and Ubuntu")
    if not 500 <= args.duration_ms <= 900_000:
        parser.error("duration must be between 500 and 900000 ms")
    if args.acceptance and args.duration_ms != 900_000:
        parser.error("--acceptance requires --duration-ms 900000")
    if args.acceptance and args.cancel_screen:
        parser.error("--acceptance cannot be combined with --cancel-screen")
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    run = args.output / stamp
    run.mkdir(parents=True)
    suite = runpy.run_path(str(Path(__file__).with_name("test-a1.py")))
    print(f"A2-1 capture results directory {run}", flush=True)
    with_screen = args.with_screen or args.cancel_screen
    if args.acceptance:
        os.environ["SKYBER_AUDIO_DIAGNOSTICS"] = "1"
    run_duration_ms = args.duration_ms + (6000 if with_screen else 0)
    if args.cancel_screen:
        print("Cancel the screen capture picker without selecting a source.", flush=True)
    elif with_screen:
        print("Select a non-sensitive window in the screen capture picker within 60 seconds.", flush=True)
    suite["run_case"](
        args.peer.resolve(strict=True), run / "live", suite["available_port"](),
        "av_with_screen" if with_screen else "av", "listener",
        duration=run_duration_ms,
        screen_stop=args.duration_ms if with_screen else None,
        failure="capture_cancelled" if args.cancel_screen else None,
        capture={"camera": args.camera, "microphone": args.microphone, "screen": with_screen},
        sustained=run_duration_ms > 15_000,
    )
    if args.acceptance:
        live = run / "live"
        listener = suite["read_result"](live, "listener")
        initiator = suite["read_result"](live, "initiator")
        helper = None
        for line in suite["read_stderr"](live, "listener").splitlines():
            prefix = "SKYBER_AUDIO_DIAGNOSTICS "
            if line.startswith(prefix):
                helper = json.loads(line[len(prefix):])
        sender = next(row for row in listener["tracks"] if row["track"] == "microphone")
        receiver = next(row for row in initiator["tracks"] if row["track"] == "microphone")
        report, failures = audio_continuity_report(args.duration_ms, helper, sender, receiver)
        result = {
            "configuration": {
                "platform": sys.platform,
                "platform_release": platform.release(),
                "python_version": platform.python_version(),
                "peer_binary": str(args.peer.resolve()),
                "camera_device": args.camera,
                "microphone_device": args.microphone,
                "screen_capture_requested": with_screen,
                "requested_active_duration_ms": args.duration_ms,
            },
            "audio_continuity": report,
            "resource_samples_path": "live/resource-samples.json",
            "peer_processes_exit_status": 0,
            "failures": failures,
        }
        (run / "audio-continuity-acceptance.json").write_text(
            json.dumps(result, indent=2) + "\n", encoding="utf-8"
        )
        print(json.dumps({"audio_continuity_acceptance": report,
                          "failures": failures}, sort_keys=True), flush=True)
        if failures:
            raise RuntimeError("15-minute live audio continuity acceptance failed")


def audio_continuity_report(duration_ms: int, helper: dict | None,
                            sender: dict, receiver: dict) -> tuple[dict, list[str]]:
    failures = []
    if helper is None:
        return {}, ["capture helper diagnostics are missing"]

    requested_active_us = helper.get("requested_active_duration_us")
    expected_frames = requested_active_us * 48 // 1000 if isinstance(
        requested_active_us, int
    ) else 0
    provider_frames = helper.get("provider_sample_frames")
    coverage = (provider_frames / expected_frames
                if isinstance(provider_frames, int) and expected_frames > 0 else None)
    provider_gap = helper.get("max_provider_gap_us")
    queue_wait = sender.get("timing_summary", {}).get("capture_queue_wait_us", {})
    sent = sender.get("sent_packets", 0)
    received = receiver.get("received_packets", 0)
    decoded_samples = receiver.get("samples", 0)
    if isinstance(helper.get("pending_pcm_bytes"), int):
        pending_sample_frames = helper["pending_pcm_bytes"] // 8
    elif isinstance(helper.get("partial_interleaved_samples"), int):
        pending_sample_frames = helper["partial_interleaved_samples"] // 2
    else:
        pending_sample_frames = None
    reconciled_provider_frames = (
        helper.get("emitted_blocks", 0) * 960 + pending_sample_frames
        if isinstance(pending_sample_frames, int)
        and isinstance(helper.get("emitted_blocks"), int) else None
    )
    helper_timestamps = [sample.get("capture_helper_timestamp_us")
                         for sample in sender.get("timings", [])]
    helper_timestamps = [value for value in helper_timestamps if isinstance(value, int)]
    emitted_gaps = [right - left for left, right in zip(
        helper_timestamps, helper_timestamps[1:]
    )]
    emitted_gap_summary = summarize_gaps(emitted_gaps)
    block_coverage = sent * 960 / expected_frames if expected_frames else None
    expected_decoded_samples = max(0, expected_frames - 312)
    decoded_coverage = decoded_samples / expected_decoded_samples if expected_decoded_samples else None
    report = {
        "requested_active_duration_us": requested_active_us,
        "expected_provider_sample_frames": expected_frames,
        "provider_sample_frames": provider_frames,
        "provider_coverage": coverage,
        "max_provider_gap_us": provider_gap,
        "emitted_block_gap_us": emitted_gap_summary,
        "emitted_block_coverage": block_coverage,
        "decoded_sample_frame_coverage": decoded_coverage,
        "max_stdout_flush_us": helper.get(
            "max_stdout_flush_us", helper.get("max_stdout_write_flush_us")
        ),
        "emitted_blocks": helper.get("emitted_blocks"),
        "pending_sample_frames": pending_sample_frames,
        "reconciled_provider_sample_frames": reconciled_provider_frames,
        "sent_packets": sent,
        "received_packets": received,
        "decoded_frames": receiver.get("decoded_frames"),
        "decoded_sample_frames": decoded_samples,
        "capture_queue_wait_us": queue_wait,
    }
    if not isinstance(provider_frames, int) or coverage is None or coverage < 0.995:
        failures.append("provider sample-frame coverage is below 99.5%")
    if not isinstance(provider_gap, int) or provider_gap > 200_000:
        failures.append("maximum provider gap exceeds 200 ms")
    if block_coverage is None or block_coverage < 0.995:
        failures.append("published microphone duration is below 99.5%")
    if decoded_coverage is None or decoded_coverage < 0.995:
        failures.append("decoded microphone duration is below 99.5%")
    if len(helper_timestamps) != sent or any(
        right <= left for left, right in zip(helper_timestamps, helper_timestamps[1:])
    ):
        failures.append("published microphone block timestamps are missing or not increasing")
    if (emitted_gap_summary["maximum_us"] is None
            or emitted_gap_summary["maximum_us"] > 200_000):
        failures.append("maximum emitted-block gap exceeds 200 ms")
    if not isinstance(requested_active_us, int) or requested_active_us < duration_ms * 1000:
        failures.append("helper requested active duration is shorter than 15 minutes")
    if helper.get("emitted_blocks") != sent:
        failures.append("helper emitted-block count does not match published packets")
    if reconciled_provider_frames != provider_frames:
        failures.append("provider frames do not reconcile with emitted blocks and pending PCM")
    if received != sent:
        failures.append("published and received microphone packet counts differ")
    if receiver.get("decoded_frames") != received:
        failures.append("received and decoded microphone frame counts differ")
    if decoded_samples != sent * 960 - 312:
        failures.append("decoded sample frames do not reconcile with Opus priming")
    if queue_wait.get("samples") != sent or queue_wait.get("p95_us") is None:
        failures.append("capture queue wait samples do not match published packets")
    return report, failures


def summarize_gaps(gaps: list[int]) -> dict:
    ordered = sorted(gaps)
    return {
        "samples": len(ordered),
        "p50_us": nearest_rank(ordered, 50),
        "p95_us": nearest_rank(ordered, 95),
        "p99_us": nearest_rank(ordered, 99),
        "maximum_us": ordered[-1] if ordered else None,
    }


def nearest_rank(values: list[int], percentile: int) -> int | None:
    if not values:
        return None
    index = (len(values) * percentile + 99) // 100 - 1
    return values[index]


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, json.JSONDecodeError, KeyError, StopIteration) as error:
        sys.exit(f"A2 capture failed: {error}")
