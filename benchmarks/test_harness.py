"""Tests for the plumbing every benchmark runner shares."""

import json
import unittest
from pathlib import Path
from unittest import mock

import harness


class HarnessTests(unittest.TestCase):
    def test_formats_progress_durations_for_scanning(self) -> None:
        cases = [
            (0.9, "0s"),
            (59.9, "59s"),
            (61.0, "1m 01s"),
            (3_660.0, "1h 01m"),
        ]

        for duration_seconds, expected in cases:
            with self.subTest(duration_seconds=duration_seconds):
                self.assertEqual(harness.format_duration(duration_seconds), expected)

    def test_parses_query_scores_without_changing_payload(self) -> None:
        payload = {
            "results": [
                {"address": "inseam://host/x/document.txt", "score": 1.0},
            ]
        }

        results = harness.parse_query_results(json.dumps(payload), 25)

        self.assertEqual(results, payload["results"])

    def test_rejects_more_results_than_the_bound(self) -> None:
        payload = {"results": [{"address": "a"}, {"address": "b"}]}

        with self.assertRaises(harness.BenchmarkError):
            harness.parse_query_results(json.dumps(payload), 1)

    def test_query_arguments_refuse_flag_like_text(self) -> None:
        with self.assertRaises(harness.BenchmarkError):
            harness.query_arguments(Path("data"), Path("composition"), "--limit", 10)
        with self.assertRaises(harness.BenchmarkError):
            harness.query_arguments(Path("data"), Path("composition"), "   ", 10)

    def test_query_arguments_pass_the_bounded_limit(self) -> None:
        arguments = harness.query_arguments(
            Path("data"), Path("composition"), "statins and cancer", 10
        )

        self.assertEqual(arguments[-4:], ["statins and cancer", "--limit", "10", "--json"])

    def test_parses_index_completion_numbers(self) -> None:
        output = (
            "511963 sources seen: 511963 indexed, 0 unchanged, 0 catalog-only, "
            "0 past cutoff, 0 ignored\n"
            "2376344 fragments, 1868268 relations, 5667 keyed fragments anchored\n"
            "summaries: 492 llm, 511470 extractive, 1 envelope · "
            "1864381 embedded · $13.0042 spent\n"
        )

        summary = harness.parse_index_summary(output, 600_001)

        self.assertEqual(summary["sources_seen"], 511_963)
        self.assertEqual(summary["fragments"], 2_376_344)
        self.assertEqual(summary["relations"], 1_868_268)
        self.assertEqual(summary["embeddings"], 1_864_381)
        self.assertEqual(summary["cost_usd"], 13.0042)

    def test_index_summary_rejects_sources_past_the_bound(self) -> None:
        output = (
            "4001 sources seen: 4001 indexed, 0 unchanged, 0 catalog-only, "
            "0 past cutoff, 0 ignored\n"
            "4001 fragments, 0 relations, 0 keyed fragments anchored\n"
            "summaries: 1 llm, 4000 extractive, 0 envelope · 4001 embedded · $0.1 spent\n"
        )

        with self.assertRaises(harness.BenchmarkError):
            harness.parse_index_summary(output, 4_000)

    def test_formats_index_progress_from_status(self) -> None:
        status = (
            "sources        12800 (12672 indexed)\n"
            "search rows    12672\n"
        )

        progress = harness.format_index_progress(status, 511_962, 65.0)

        self.assertEqual(
            progress,
            "1m 05s elapsed · 12,672 / 511,962 indexed · "
            "12,800 cataloged · 12,672 search rows",
        )

    def test_index_uses_status_probe_every_thirty_seconds(self) -> None:
        index_output = (
            "1 sources seen: 1 indexed, 0 unchanged, 0 catalog-only, "
            "0 past cutoff, 0 ignored\n"
            "1 fragments, 0 relations, 0 keyed fragments anchored\n"
            "summaries: 1 llm, 0 extractive, 0 envelope · "
            "1 embedded · $0.001 spent\n"
        )
        command = harness.CommandResult(0, 1.0, index_output, "")
        with mock.patch.object(harness, "run_logged", return_value=command) as run:
            record = harness.index_documents(
                Path("run"),
                Path("data"),
                Path("composition"),
                Path("documents"),
                511_962,
                600_001,
                "a fixture",
            )

        self.assertEqual(
            run.call_args.kwargs["progress_interval_seconds"],
            harness.INDEX_PROGRESS_INTERVAL_SECONDS,
        )
        self.assertIsNotNone(run.call_args.kwargs["progress_probe"])
        self.assertEqual(record["summary"]["sources_seen"], 1)

    def test_failed_index_status_probe_keeps_heartbeat_alive(self) -> None:
        failure = harness.CommandResult(1, 0.1, "", "store busy")
        with mock.patch.object(harness, "run_capture", return_value=failure):
            progress = harness.read_index_progress(
                Path("data"), Path("composition"), 511_962, 30.0
            )

        self.assertEqual(progress, "30s elapsed · status unavailable")

    def test_run_id_carries_the_start_time_and_revision(self) -> None:
        with mock.patch.object(harness, "git_value", return_value="0123456789ab"):
            run_id = harness.new_run_id("2026-09-05T13:06:07.123Z")

        self.assertEqual(run_id, "20260905T130607Z-0123456789ab")
        self.assertIsNotNone(harness.RUN_ID_PATTERN.fullmatch(run_id))


if __name__ == "__main__":
    unittest.main()
