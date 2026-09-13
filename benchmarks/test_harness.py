"""Tests for the plumbing every benchmark runner shares."""

import argparse
import json
import tempfile
import time
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

    def test_observability_preserves_folder_ranks_and_phase_percentiles(self) -> None:
        queries = [{"query_id": "q1", "query_meta": {"seeds_ms": 7}, "results": [
            {"envelope": {"content_type": "inode/directory"}},
            {"envelope": {"content_type": "text/plain"}},
        ]}]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            harness.write_retrieval_observability(root, queries)
            result = json.loads((root / "retrieval-observability.json").read_text())
        self.assertEqual(result["folder_results"], 1)
        self.assertEqual(result["queries"][0]["folder_ranks"], [1])
        self.assertEqual(result["distributions"]["seeds_ms"], {"median": 7, "p95": 7, "max": 7})

    def test_probability_rejects_invalid_values_at_both_boundaries(self) -> None:
        for value in (-1, 1, float("nan"), float("inf")):
            with self.subTest(value=value):
                with self.assertRaises(argparse.ArgumentTypeError):
                    harness.probability_argument(str(value))
                with self.assertRaises(harness.BenchmarkError):
                    harness.manifest_option_probability({"damping": value}, "damping")
        self.assertEqual(harness.probability_argument("0"), 0.0)

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
        self.assertEqual(summary["summaries_verbatim"], 0)
        self.assertEqual(summary["summaries_llm"], 492)

    def test_parses_the_verbatim_summary_count_when_printed(self) -> None:
        output = (
            "3634 sources seen: 3634 indexed, 0 unchanged, 0 catalog-only, "
            "0 past cutoff, 0 ignored\n"
            "10902 fragments, 7268 relations, 0 keyed fragments anchored\n"
            "summaries: 3633 verbatim, 1 llm, 0 extractive, 0 envelope · "
            "3634 embedded · $0.0003 spent\n"
        )

        summary = harness.parse_index_summary(output, 4_000)

        self.assertEqual(summary["summaries_verbatim"], 3_633)
        self.assertEqual(summary["summaries_llm"], 1)
        self.assertEqual(summary["summaries_extractive"], 0)
        self.assertEqual(summary["embeddings"], 3_634)
        self.assertEqual(summary["cost_usd"], 0.0003)

    def test_parses_index_reuse_counts_and_defaults_them_to_zero(self) -> None:
        output = (
            "10 sources seen: 10 indexed, 0 unchanged, 0 catalog-only, "
            "0 past cutoff, 0 ignored\n"
            "20 fragments, 10 relations, 0 keyed fragments anchored\n"
            "reused: 7 embeddings, 3 transform outputs\n"
            "summaries: 10 llm, 0 extractive, 0 envelope · "
            "13 embedded · $0.0100 spent\n"
        )
        summary = harness.parse_index_summary(output, 100)
        self.assertEqual(summary["embeddings_reused"], 7)
        self.assertEqual(summary["transforms_reused"], 3)

        without = harness.parse_index_summary(
            output.replace("reused: 7 embeddings, 3 transform outputs\n", ""), 100
        )
        self.assertEqual(without["embeddings_reused"], 0)
        self.assertEqual(without["transforms_reused"], 0)

    def test_measures_index_footprint_against_source_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            data_dir = Path(root) / "node"
            documents = Path(root) / "documents"
            (data_dir / "nested").mkdir(parents=True)
            documents.mkdir()
            (data_dir / "catalog.sqlite3").write_bytes(b"x" * 300)
            (data_dir / "nested" / "wal").write_bytes(b"y" * 100)
            (documents / "a.txt").write_bytes(b"a" * 150)
            (documents / "b.txt").write_bytes(b"b" * 50)

            footprint = harness.measure_index_footprint(data_dir, documents)

        self.assertEqual(footprint["index_bytes"], 400)
        self.assertEqual(footprint["index_files"], 2)
        self.assertEqual(footprint["source_bytes"], 200)
        self.assertEqual(footprint["source_files"], 2)
        self.assertEqual(footprint["index_to_source_ratio"], 2.0)

    def test_footprint_walk_refuses_more_files_than_the_bound(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            for i in range(3):
                (Path(root) / f"{i}.txt").write_bytes(b"z")
            with self.assertRaises(harness.BenchmarkError):
                harness.directory_bytes(Path(root), 2)

    def test_index_summary_rejects_sources_past_the_bound(self) -> None:
        output = (
            "4001 sources seen: 4001 indexed, 0 unchanged, 0 catalog-only, "
            "0 past cutoff, 0 ignored\n"
            "4001 fragments, 0 relations, 0 keyed fragments anchored\n"
            "summaries: 1 llm, 4000 extractive, 0 envelope · 4001 embedded · $0.1 spent\n"
        )

        with self.assertRaises(harness.BenchmarkError):
            harness.parse_index_summary(output, 4_000)

    def test_index_heartbeats_do_not_boot_a_competing_store_writer(self) -> None:
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
        self.assertIsNone(run.call_args.kwargs["progress_probe"])
        self.assertEqual(record["summary"]["sources_seen"], 1)

    def test_run_id_carries_the_start_time_and_revision(self) -> None:
        with mock.patch.object(harness, "git_value", return_value="0123456789ab"):
            run_id = harness.new_run_id("2026-09-05T13:06:07.123Z")

        self.assertEqual(run_id, "20260905T130607Z-0123456789ab")
        self.assertIsNotNone(harness.RUN_ID_PATTERN.fullmatch(run_id))


if __name__ == "__main__":
    unittest.main()


class KilledAttemptTests(unittest.TestCase):
    def manifest(self) -> dict:
        return {
            "run_id": "20260101T000000Z-abcdefabcdef",
            "status": "running",
            "phase": "querying",
            "queries_completed": 7,
            "attempts": [
                {"attempt_number": 1, "status": "interrupted", "duration_seconds": 12.5},
                {"attempt_number": 2, "status": "running", "duration_seconds": None},
            ],
        }

    def test_a_running_record_is_refused_without_after_kill(self) -> None:
        with self.assertRaises(harness.BenchmarkError):
            harness.validate_resumable_status(
                "20260101T000000Z-abcdefabcdef", self.manifest()
            )

    def test_after_kill_closes_the_open_attempt_as_interrupted(self) -> None:
        manifest = self.manifest()
        harness.validate_resumable_status(
            "20260101T000000Z-abcdefabcdef", manifest, after_kill=True
        )
        self.assertEqual(manifest["status"], "interrupted")
        self.assertEqual(manifest["attempts"][-1]["status"], "interrupted")
        self.assertEqual(manifest["attempts"][-1]["ending_queries_completed"], 7)
        self.assertIsNone(manifest["attempts"][-1]["duration_seconds"])

    def test_totals_skip_attempts_that_recorded_no_duration(self) -> None:
        manifest = self.manifest()
        harness.close_killed_attempt(manifest)
        manifest["attempts"].append(
            {"attempt_number": 3, "status": "running", "duration_seconds": None}
        )
        manifest["status"] = "completed"
        manifest["phase"] = "completed"
        with tempfile.TemporaryDirectory() as directory:
            harness.finish_attempt(Path(directory), manifest, time.monotonic())
        self.assertEqual(manifest["attempts_unmeasured"], 1)
        self.assertGreaterEqual(manifest["duration_seconds"], 12.5)
