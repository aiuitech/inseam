"""Tests for the EnterpriseRAG-Bench harness boundary parsers and scores."""

import io
import json
import os
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

import enterprise_rag_bench as benchmark


DOCUMENT_A = "dsid_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
DOCUMENT_B = "dsid_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"


class EnterpriseRagBenchTests(unittest.TestCase):
    def test_extracts_agent_answer_after_last_tool_result(self) -> None:
        output = (
            "· model stealth/ox-alpha\n\n"
            "→ query {\"text\":\"question\"}\n"
            "  ← query: 200 chars\n\n"
            f"The answer cites inseam://enterprise-rag-bench/x/{DOCUMENT_A}_file.txt.\n\n"
            "· 2 turns, 1 tool calls, $0.0000 spent\n"
        )

        answer = benchmark.extract_agent_answer(output)

        self.assertEqual(
            answer,
            f"The answer cites inseam://enterprise-rag-bench/x/{DOCUMENT_A}_file.txt.",
        )

    def test_document_ids_keep_retrieval_order_and_dedupe(self) -> None:
        values = [f"first {DOCUMENT_B}", f"repeat {DOCUMENT_B} then {DOCUMENT_A}"]

        document_ids = benchmark.extract_document_ids(values)

        self.assertEqual(document_ids, [DOCUMENT_B, DOCUMENT_A])

    def test_retrieval_scores_use_expected_documents(self) -> None:
        questions = [
            {"question_id": "qst_0001", "expected_doc_ids": [DOCUMENT_A]},
            {"question_id": "qst_0002", "expected_doc_ids": [DOCUMENT_B]},
            {"question_id": "qst_0003", "expected_doc_ids": []},
        ]
        queries = [
            {"question_id": "qst_0001", "retrieved_document_ids": [DOCUMENT_A]},
            {
                "question_id": "qst_0002",
                "retrieved_document_ids": [DOCUMENT_A, DOCUMENT_B],
            },
            {"question_id": "qst_0003", "retrieved_document_ids": []},
        ]

        scores = benchmark.retrieval_scores(queries, questions)

        self.assertEqual(scores["questions_with_expected_documents"], 2)
        self.assertEqual(scores["average_document_recall_pct"], 100.0)
        self.assertEqual(scores["document_hit_rate_pct"], 100.0)
        self.assertEqual(scores["mean_reciprocal_rank"], 0.75)

    def test_composition_uses_lean_remote_index(self) -> None:
        options = benchmark.RunOptions(1, 8, 12, 8, 500, 4, False)

        composition = benchmark.composition_text(options)

        self.assertIn('base_url = "https://openrouter.ai/api/v1"', composition)
        self.assertIn('api_key_env = "OPENROUTER_API_KEY"', composition)
        self.assertIn('transform_model = "google/gemini-2.5-flash-lite"', composition)
        self.assertIn('llm_lane = "batch"', composition)
        self.assertIn('transform_reasoning_effort = "none"', composition)
        self.assertIn('agent_model = "stealth/ox-alpha"', composition)
        self.assertIn('batch_requests_max = 5000', composition)
        self.assertIn('model = "openai/text-embedding-3-small"', composition)
        self.assertIn('dimensions = 384', composition)
        self.assertIn('vectors = "summaries"', composition)
        self.assertIn('target_chars = 200', composition)
        self.assertIn('batch_concurrency = 65536', composition)
        self.assertEqual(composition.count("disabled = true"), 3)
        self.assertEqual(composition.count("llm_call_budget = 500"), 1)

    def test_run_records_manifest_timings_results_and_scores(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_text:
            temporary = Path(temporary_text)
            fixture = temporary / "fixture"
            runs = temporary / "runs"
            fake_bin = temporary / "bin"
            fixture.joinpath("documents").mkdir(parents=True)
            fixture.joinpath("documents.json").write_text(
                json.dumps({"text_file_count": 1}) + "\n"
            )
            fixture.joinpath("setup.json").write_text("{}\n")
            fixture.joinpath("questions.jsonl").write_text(
                json.dumps(
                    {
                        "question_id": "qst_0001",
                        "question_type": "basic",
                        "question": "What is recorded?",
                        "expected_doc_ids": [DOCUMENT_A],
                    }
                )
                + "\n"
            )
            fake_bin.mkdir()
            inseam = fake_bin / "inseam"
            inseam.write_text(fake_inseam_source())
            inseam.chmod(0o755)
            environment = {
                "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
                "OPENROUTER_API_KEY": "test-key",
            }
            options = benchmark.RunOptions(1, 8, 12, 8, 500, 4, True)

            output = io.StringIO()
            with redirect_stdout(output):
                with mock.patch.object(benchmark, "FIXTURE_ROOT", fixture):
                    with mock.patch.object(benchmark, "RUNS_ROOT", runs):
                        with mock.patch.dict(os.environ, environment):
                            benchmark.run_benchmark(options)

            run_directories = list(runs.iterdir())
            self.assertEqual(len(run_directories), 1)
            manifest = json.loads(
                run_directories[0].joinpath("manifest.json").read_text()
            )
            self.assertEqual(manifest["status"], "completed")
            self.assertEqual(manifest["phase"], "completed")
            self.assertEqual(manifest["queries_completed"], 1)
            self.assertEqual(manifest["indexing"]["summary"]["sources_seen"], 1)
            self.assertEqual(len(manifest["attempts"]), 1)
            self.assertEqual(manifest["attempts"][0]["ending_queries_completed"], 1)
            self.assertIsNotNone(
                manifest["attempts"][0]["search_index_preparation"]
            )
            self.assertEqual(manifest["inseam"]["cli_version"], "inseam 9.9.9")
            self.assertEqual(
                manifest["models"],
                {
                    "summarization": "google/gemini-2.5-flash-lite",
                    "summarization_lane": "batch",
                    "corpus": "full",
                    "structural": "off",
                    "finder_seeds": "both",
                    "finder_max_vector_distance": 0.75,
                    "entity_extraction": "disabled",
                    "answer_generation": "stealth/ox-alpha",
                    "answer_evaluation": "skipped",
                    "embeddings": "openai/text-embedding-3-small",
                    "embedding_dimensions": 384,
                    "embedding_vectors": "summaries",
                },
            )
            self.assertEqual(
                manifest["scores"]["retrieval"]["average_document_recall_pct"],
                100.0,
            )
            progress = output.getvalue()
            self.assertIn("Indexing 1 benchmark documents...", progress)
            self.assertIn("Preparing libSQL vector search index...", progress)
            self.assertIn("Query checkpoint: 0/1 completed", progress)
            self.assertIn("[1/1] qst_0001 retrieval...", progress)
            self.assertIn("[1/1] qst_0001 answer...", progress)
            self.assertIn("Benchmark run recorded", progress)

    def test_resume_reuses_legacy_completed_index(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_text:
            temporary = Path(temporary_text)
            fixture, runs, environment = prepare_fake_fixture(temporary)
            options = benchmark.RunOptions(1, 8, 12, 8, 500, 4, True)

            with mock.patch.object(benchmark, "FIXTURE_ROOT", fixture):
                with mock.patch.object(benchmark, "RUNS_ROOT", runs):
                    with mock.patch.dict(os.environ, environment):
                        run_dir, data_dir, manifest = benchmark.create_run(options)
                        composition = run_dir / "composition.toml"
                        composition.write_text(benchmark.composition_text(options))
                        manifest["indexing"] = benchmark.index_documents(
                            run_dir, data_dir, composition, options, run_dir / "logs" / "index.log"
                        )
                        manifest["status"] = "failed"
                        manifest["phase"] = "failed"
                        manifest["finished_at"] = benchmark.utc_now()
                        manifest["duration_seconds"] = 1.0
                        manifest["schema_version"] = 1
                        manifest.pop("attempts")
                        benchmark.write_json(run_dir / "manifest.json", manifest)
                        with mock.patch.object(
                            benchmark,
                            "index_documents",
                            side_effect=AssertionError("index must not run"),
                        ):
                            with redirect_stdout(io.StringIO()) as output:
                                benchmark.resume_benchmark(manifest["run_id"])

            resumed = json.loads(run_dir.joinpath("manifest.json").read_text())
            self.assertEqual(resumed["status"], "completed")
            self.assertEqual(resumed["queries_completed"], 1)
            self.assertEqual(len(resumed["attempts"]), 2)
            self.assertTrue(resumed["attempts"][1]["resumed"])
            self.assertEqual(resumed["indexing"]["summary"]["sources_seen"], 1)
            self.assertIn("Reusing completed index: 1 sources", output.getvalue())

    def test_resume_continues_an_incomplete_index_in_the_same_node(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_text:
            temporary = Path(temporary_text)
            fixture, runs, environment = prepare_fake_fixture(temporary)
            options = benchmark.RunOptions(1, 8, 12, 8, 500, 4, True)

            with mock.patch.object(benchmark, "FIXTURE_ROOT", fixture):
                with mock.patch.object(benchmark, "RUNS_ROOT", runs):
                    with mock.patch.dict(os.environ, environment):
                        run_dir, data_dir, manifest = benchmark.create_run(options)
                        composition = run_dir / "composition.toml"
                        composition.write_text(benchmark.composition_text(options))
                        started, _log_dir = benchmark.begin_attempt(
                            run_dir, manifest, "indexing", manifest["inseam"]
                        )
                        benchmark.harness.set_run_error(manifest, "failed", "source read failed")
                        benchmark.harness.finish_attempt(run_dir, manifest, started)

                        with redirect_stdout(io.StringIO()) as output:
                            benchmark.resume_benchmark(manifest["run_id"])

            resumed = json.loads(run_dir.joinpath("manifest.json").read_text())
            self.assertEqual(resumed["status"], "completed")
            self.assertEqual(resumed["index_data_path"], str(data_dir))
            self.assertEqual(len(resumed["attempts"]), 2)
            self.assertEqual(resumed["attempts"][1]["starting_phase"], "indexing")
            self.assertEqual(
                resumed["attempts"][1]["indexing_log"],
                "logs/attempt-002/index.log",
            )
            self.assertEqual(
                resumed["indexing"]["log"], "logs/attempt-002/index.log"
            )
            self.assertIn("Resuming EnterpriseRAG-Bench run", output.getvalue())

    def test_interruption_records_the_terminal_phase(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_text:
            temporary = Path(temporary_text)
            fixture = temporary / "fixture"
            run_dir = temporary / "run"
            data_dir = temporary / "data"
            fixture.mkdir()
            run_dir.mkdir()
            data_dir.mkdir()
            fixture.joinpath("setup.json").write_text("{}\n")
            manifest = {
                "run_id": "test-run",
                "status": "running",
                "phase": "starting",
                "queries_completed": 0,
                "attempts": [],
                "inseam": {},
            }
            options = benchmark.RunOptions(1, 8, 12, 8, 500, 4, True)

            with redirect_stdout(io.StringIO()):
                with mock.patch.object(benchmark, "FIXTURE_ROOT", fixture):
                    with mock.patch.object(benchmark, "evaluator_environment"):
                        with mock.patch.object(
                            benchmark, "load_questions", return_value=[{}]
                        ):
                            with mock.patch.object(
                                benchmark,
                                "create_run",
                                return_value=(run_dir, data_dir, manifest),
                            ):
                                with mock.patch.object(
                                    benchmark,
                                    "index_documents",
                                    side_effect=KeyboardInterrupt,
                                ):
                                    with self.assertRaises(KeyboardInterrupt):
                                        benchmark.run_benchmark(options)

            recorded = json.loads(run_dir.joinpath("manifest.json").read_text())
            self.assertEqual(recorded["status"], "interrupted")
            self.assertEqual(recorded["phase"], "interrupted")
            self.assertEqual(recorded["error"], "interrupted by user")


def prepare_fake_fixture(
    temporary: Path,
) -> tuple[Path, Path, dict[str, str]]:
    fixture = temporary / "fixture"
    runs = temporary / "runs"
    fake_bin = temporary / "bin"
    fixture.joinpath("documents").mkdir(parents=True)
    fixture.joinpath("documents.json").write_text(
        json.dumps({"text_file_count": 1}) + "\n"
    )
    fixture.joinpath("setup.json").write_text("{}\n")
    fixture.joinpath("questions.jsonl").write_text(
        json.dumps(
            {
                "question_id": "qst_0001",
                "question_type": "basic",
                "question": "What is recorded?",
                "expected_doc_ids": [DOCUMENT_A],
            }
        )
        + "\n"
    )
    fake_bin.mkdir()
    inseam = fake_bin / "inseam"
    inseam.write_text(fake_inseam_source())
    inseam.chmod(0o755)
    environment = {
        "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
        "OPENROUTER_API_KEY": "test-key",
    }
    return fixture, runs, environment


def fake_inseam_source() -> str:
    query = json.dumps(
        {
            "results": [
                {
                    "address": f"inseam://enterprise-rag-bench/x/{DOCUMENT_A}_file.txt",
                    "score": 1.0,
                }
            ]
        }
    )
    return f'''#!/usr/bin/env python3
import sys
if "--version" in sys.argv:
    print("inseam 9.9.9")
elif "query" in sys.argv:
    print({query!r})
elif "agent" in sys.argv:
    print("· model stealth/ox-alpha\\n")
    print("answer from {DOCUMENT_A}\\n")
    print("· 1 turns, 0 tool calls, $0.0000 spent")
elif "index" in sys.argv:
    print("1 sources seen: 1 indexed, 0 unchanged, 0 catalog-only, 0 past cutoff, 0 ignored")
    print("1 fragments, 0 relations, 0 keyed fragments anchored")
    print("summaries: 1 llm, 0 extractive, 0 envelope · 1 embedded · $0.001 spent")
elif "repair" in sys.argv:
    print("search rows    1")
    print("vector index  already ready")
else:
    raise SystemExit(2)
'''


if __name__ == "__main__":
    unittest.main()
