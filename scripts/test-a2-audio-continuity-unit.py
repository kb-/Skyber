"""Fast synthetic checks for the A2 microphone continuity harness."""

import io
import runpy
import unittest


HARNESS = runpy.run_path("scripts/test-a2-audio-continuity.py")
read_exact = HARNESS["read_exact"]
percentile = HARNESS["percentile"]
reconcile = HARNESS["provider_frame_reconciliation"]
frame_coverage = HARNESS["frame_coverage"]
capture_acceptance_failures = HARNESS["capture_acceptance_failures"]
CAPTURE = runpy.run_path("scripts/test-a2-capture.py")
audio_continuity_report = CAPTURE["audio_continuity_report"]
summarize_gaps = CAPTURE["summarize_gaps"]


class ShortReadStream(io.BytesIO):
    def read(self, size: int = -1) -> bytes:
        return super().read(min(size, 3))


class CaptureRecordTests(unittest.TestCase):
    def test_read_exact_reassembles_partial_pipe_reads(self) -> None:
        self.assertEqual(read_exact(ShortReadStream(b"abcdefgh"), 8), b"abcdefgh")

    def test_read_exact_returns_clean_eof(self) -> None:
        self.assertEqual(read_exact(io.BytesIO(), 8), b"")

    def test_read_exact_rejects_truncated_record(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "truncated capture record"):
            read_exact(io.BytesIO(b"abc"), 8)


class CaptureAcceptanceTests(unittest.TestCase):
    def test_healthy_provider_meets_capture_acceptance(self) -> None:
        diagnostics = {
            "requested_active_duration_us": 900_000_000,
            "provider_sample_frames": 43_200_000,
            "max_provider_gap_us": 30_000,
            "emitted_blocks": 45_000,
            "pending_pcm_bytes": 0,
        }
        self.assertEqual(
            capture_acceptance_failures(900_000, 0, diagnostics, 45_000, [20_000]), []
        )

    def test_starved_provider_fails_duration_and_gap_acceptance(self) -> None:
        diagnostics = {
            "requested_active_duration_us": 900_000_000,
            "provider_sample_frames": 24_712_800,
            "max_provider_gap_us": 4_124_515,
            "emitted_blocks": 25_742,
            "pending_pcm_bytes": 3_840,
        }
        failures = capture_acceptance_failures(
            900_000, 0, diagnostics, 25_742, [4_124_754]
        )
        self.assertEqual(len(failures), 2)
        self.assertTrue(any("99.5%" in item for item in failures))
        self.assertTrue(any("200 ms" in item for item in failures))

    def test_slow_reader_cannot_pass_capture_acceptance(self) -> None:
        diagnostics = {
            "requested_active_duration_us": 900_000_000,
            "provider_sample_frames": 43_200_000,
            "max_provider_gap_us": 30_000,
            "emitted_blocks": 45_000,
            "pending_pcm_bytes": 0,
        }
        failures = capture_acceptance_failures(900_000, 40, diagnostics, 45_000, [20_000])
        self.assertEqual(
            failures, ["acceptance mode requires a 15-minute run with a fast reader"]
        )


class GapSummaryTests(unittest.TestCase):
    def test_percentiles_use_nearest_rank(self) -> None:
        self.assertEqual(percentile([50, 10, 40, 20, 30], 50), 30)
        self.assertEqual(percentile([50, 10, 40, 20, 30], 95), 50)

    def test_percentile_reports_no_samples(self) -> None:
        self.assertIsNone(percentile([], 95))

    def test_live_block_gap_summary_reports_percentiles_and_maximum(self) -> None:
        self.assertEqual(summarize_gaps([50, 10, 40, 20, 30]), {
            "samples": 5,
            "p50_us": 30,
            "p95_us": 50,
            "p99_us": 50,
            "maximum_us": 50,
        })

    def test_provider_frames_reconcile_with_full_and_partial_blocks(self) -> None:
        self.assertEqual(reconcile(2, {"provider_sample_frames": 1920,
                                      "pending_pcm_bytes": 0}), (1920, True))
        self.assertEqual(reconcile(2, {"provider_sample_frames": 1924,
                                      "partial_interleaved_samples": 8}), (1924, True))

    def test_provider_frame_mismatch_is_reported(self) -> None:
        self.assertEqual(reconcile(2, {"provider_sample_frames": 1919,
                                      "pending_pcm_bytes": 0}), (1919, False))

    def test_coverage_uses_the_entire_requested_interval(self) -> None:
        self.assertEqual(frame_coverage(995, 1000), 0.995)
        self.assertEqual(frame_coverage(994, 1000), 0.994)
        self.assertIsNone(frame_coverage(0, 0))


class LiveAcceptanceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.helper = {
            "provider_sample_frames": 1_920,
            "max_provider_gap_us": 20_000,
            "emitted_blocks": 2,
            "pending_pcm_bytes": 0,
            "requested_active_duration_us": 40_000,
            "max_stdout_flush_us": 400,
        }
        self.sender = {
            "sent_packets": 2,
            "timings": [
                {"capture_helper_timestamp_us": 100_000},
                {"capture_helper_timestamp_us": 120_000},
            ],
            "timing_summary": {"capture_queue_wait_us": {"samples": 2, "p95_us": 1}},
        }
        self.receiver = {
            "received_packets": 2,
            "decoded_frames": 2,
            "samples": 1_608,
        }

    def test_healthy_live_run_meets_continuity_acceptance(self) -> None:
        report, failures = audio_continuity_report(
            40, self.helper, self.sender, self.receiver
        )
        self.assertFalse(failures)
        self.assertEqual(report["provider_coverage"], 1.0)
        self.assertEqual(report["emitted_block_gap_us"]["maximum_us"], 20_000)

    def test_missing_provider_audio_fails_continuity_acceptance(self) -> None:
        self.helper["provider_sample_frames"] = 1_000
        self.helper["max_provider_gap_us"] = 4_124_515
        _, failures = audio_continuity_report(
            40, self.helper, self.sender, self.receiver
        )
        self.assertIn("provider sample-frame coverage is below 99.5%", failures)
        self.assertIn("maximum provider gap exceeds 200 ms", failures)

    def test_missing_helper_diagnostics_fails_continuity_acceptance(self) -> None:
        report, failures = audio_continuity_report(40, None, self.sender, self.receiver)
        self.assertEqual(report, {})
        self.assertEqual(failures, ["capture helper diagnostics are missing"])

    def test_provider_frames_must_reconcile_with_emitted_pcm(self) -> None:
        self.helper["provider_sample_frames"] += 1
        _, failures = audio_continuity_report(
            40, self.helper, self.sender, self.receiver
        )
        self.assertIn(
            "provider frames do not reconcile with emitted blocks and pending PCM", failures
        )

    def test_decoded_output_must_meet_duration_coverage(self) -> None:
        self.receiver["samples"] = 1_000
        _, failures = audio_continuity_report(
            40, self.helper, self.sender, self.receiver
        )
        self.assertIn("decoded microphone duration is below 99.5%", failures)


if __name__ == "__main__":
    unittest.main()
