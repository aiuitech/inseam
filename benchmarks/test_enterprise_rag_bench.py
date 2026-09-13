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

    def test_folder_rank_does_not_inflate_reciprocal_rank(self) -> None:
        queries = [{"question_id": "q1", "retrieved_document_ids": [DOCUMENT_A], "results": [
            {"address": f"inseam://fs/{DOCUMENT_A}", "envelope": {"content_type": "inode/directory"}},
            {"address": f"inseam://fs/folder/{DOCUMENT_A}_file.txt"},
        ]}]
        scores = benchmark.retrieval_scores(queries, [
            {"question_id": "q1", "expected_doc_ids": [DOCUMENT_A]},
        ])
        self.assertEqual(scores["mean_reciprocal_rank"], 0.5)
        self.assertEqual(scores["average_document_recall_pct"], 100.0)

    def test_composition_defaults_to_whole_document_full_text_rows(self) -> None:
        options = benchmark.RunOptions(1, 8, 12, 8, 0, 4, False)

        composition = benchmark.composition_text(options)

        self.assertIn('base_url = "https://openrouter.ai/api/v1"', composition)
        self.assertIn('api_key_env = "OPENROUTER_API_KEY"', composition)
        self.assertIn('transform_model = "google/gemini-2.5-flash-lite"', composition)
        self.assertIn('llm_lane = "batch"', composition)
        self.assertIn('transform_reasoning_effort = "none"', composition)
        self.assertIn('agent_model = "z-ai/glm-5.3-flash"', composition)
        self.assertIn('batch_requests_max = 5000', composition)
        self.assertIn('provider = "none"', composition)
        self.assertNotIn("text-embedding", composition)
        self.assertIn("target_chars = 24000", composition)
        self.assertIn('batch_concurrency = 65536', composition)
        self.assertEqual(composition.count("disabled = true"), 4)
        self.assertEqual(composition.count("llm_call_budget = 0"), 2)

    def test_composition_mounts_the_embedder_and_sections_on_request(self) -> None:
        options = benchmark.RunOptions(
            1, 8, 12, 8, 500, 4, False,
            summary_target_chars=200,
            structural="markdown",
            embedding_vectors="summaries",
        )

        composition = benchmark.composition_text(options)

        self.assertIn('model = "openai/text-embedding-3-small"', composition)
        self.assertIn('dimensions = 384', composition)
        self.assertIn('vectors = "summaries"', composition)
        self.assertIn('target_chars = 200', composition)
        self.assertEqual(composition.count("disabled = true"), 3)
        self.assertEqual(composition.count("llm_call_budget = 500"), 1)

    def test_a_retrieval_only_run_cannot_ask_for_evaluation(self) -> None:
        with self.assertRaises(benchmark.BenchmarkError):
            benchmark.RunOptions(1, 8, 12, 8, 0, 4, False, skip_agent=True)

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
            options = benchmark.RunOptions(
                1, 8, 12, 8, 500, 4, True,
                finder_overrides=["hub_degree_max=200"],
                explain=True,
            )

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
                    "hints": "disabled",
                    "answer_generation": "z-ai/glm-5.3-flash",
                    "answer_evaluation": "skipped",
                    "embeddings": "disabled",
                    "embedding_dimensions": 0,
                    "embedding_vectors": "none",
                },
            )
            self.assertEqual(
                manifest["scores"]["retrieval"]["average_document_recall_pct"],
                100.0,
            )
            self.assertTrue(manifest["scores"]["retrieval"]["attribution_ready"])
            self.assertEqual(manifest["options"]["finder_overrides"], ["hub_degree_max=200"])
            self.assertTrue(manifest["options"]["explain"])
            section = manifest["scores"]["retrieval_attribution"]
            self.assertEqual(section["channels"]["lexical"]["gold_only_seeded"], 1)
            self.assertEqual(section["row_kinds"]["term"]["gold_largest"], 1)
            queries = json.loads(
                run_directories[0].joinpath("queries.jsonl").read_text().splitlines()[0]
            )
            argv = queries["query_meta"]["argv"]
            self.assertEqual(argv[argv.index("--limit") + 1], "50")
            self.assertIn("--explain", argv)
            self.assertEqual(argv[argv.index("--finder") + 1], "hub_degree_max=200")
            self.assertEqual(len(queries["results"]), 1)
            attribution = json.loads(
                run_directories[0].joinpath("attribution.jsonl").read_text().splitlines()[0]
            )
            self.assertEqual(attribution["document_id"], DOCUMENT_A)
            self.assertEqual(attribution["rank"], 1)
            self.assertEqual(attribution["evidence"]["ledger"]["channels"][0]["channel"], "lexical")
            tables = json.loads(
                run_directories[0].joinpath("retrieval-attribution.json").read_text()
            )
            self.assertEqual(tables["hubs_excluded"][0]["degree"], 900)
            self.assertEqual(tables["by_question_type"]["basic"]["gold_documents_found"], 1)
            progress = output.getvalue()
            self.assertIn("Retrieval: recall 100.0%", progress)
            self.assertIn("attribution.jsonl", progress)
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


class AttributionTests(unittest.TestCase):
    """The aggregation over synthetic query records; no binary, no files."""

    def test_attribution_rows_report_deep_ranks_and_missing_gold(self) -> None:
        question = {"question_id": "q1", "question_type": "semantic",
                    "expected_doc_ids": [DOCUMENT_A, DOCUMENT_B]}
        results = [{"address": f"inseam://fs/{OTHER}_file.txt"}] * 11
        results.append({"address": address(DOCUMENT_A)})
        meta = {"evidence": [{"address": address(DOCUMENT_A), "fragments": []}]}

        rows = benchmark.attribution_rows(question, results, meta)

        self.assertEqual([row["rank"] for row in rows], [12, None])
        self.assertEqual(rows[0]["evidence"], {"address": address(DOCUMENT_A), "fragments": []})
        self.assertIsNone(rows[1]["evidence"])
        self.assertEqual(rows[0]["question_type"], "semantic")

    def test_channel_counts_separate_seeded_only_seeded_and_largest(self) -> None:
        entries = [
            gold_entry(DOCUMENT_A, evidence(
                seeded={"prose": 3, "lexical": 1},
                channels={"prose": 0.2, "lexical": 0.5},
            )),
            gold_entry(DOCUMENT_B, evidence(seeded={"cluster": 2}, channels={"cluster": 0.4})),
            gold_entry(OTHER, None),
        ]

        counts = benchmark.channel_counts(entries)

        self.assertEqual(counts["prose"], {"seeded": 1, "only_seeded": 0, "largest": 0})
        self.assertEqual(counts["lexical"], {"seeded": 1, "only_seeded": 0, "largest": 1})
        self.assertEqual(counts["cluster"], {"seeded": 1, "only_seeded": 1, "largest": 1})
        self.assertEqual(counts["vector"], {"seeded": 0, "only_seeded": 0, "largest": 0})

    def test_largest_contributor_ignores_zero_mass_and_breaks_ties_in_order(self) -> None:
        self.assertIsNone(benchmark.largest_contributor({"prose": 0.0}, benchmark.CHANNELS))
        self.assertEqual(
            benchmark.largest_contributor({"cluster": 0.3, "prose": 0.3}, benchmark.CHANNELS),
            "prose",
        )

    def test_noise_is_scored_documents_that_are_neither_gold_nor_valid(self) -> None:
        question = {"question_id": "q1", "question_type": "basic",
                    "expected_doc_ids": [DOCUMENT_A], "valid_doc_ids": [DOCUMENT_B]}
        query = {
            "question_id": "q1",
            "results": [
                {"address": address(DOCUMENT_A)},
                {"address": address(DOCUMENT_B)},
                {"address": "inseam://fs/folder", "envelope": {"content_type": "inode/directory"}},
                {"address": address(OTHER)},
                {"address": address(OTHER)},
            ],
            "query_meta": {"evidence": [{"address": address(OTHER), "fragments": []}]},
            "attribution": benchmark.attribution_rows(question, [{"address": address(DOCUMENT_A)}], {}),
        }

        gold, noise = benchmark.attribution_entries([query], [question])

        self.assertEqual([entry["document_id"] for entry in gold], [DOCUMENT_A])
        self.assertEqual([entry["document_id"] for entry in noise], [OTHER])
        self.assertEqual(noise[0]["rank"], 4)
        self.assertEqual(noise[0]["evidence"], {"address": address(OTHER), "fragments": []})

    def test_row_kinds_and_rows_count_documents_carried(self) -> None:
        gold = [
            gold_entry(DOCUMENT_A, evidence(
                kinds={"term": 0.3, "identifier": 0.1},
                rows=[("upload", "term", 12), ("dsid", "identifier", 2)],
            )),
            gold_entry(DOCUMENT_B, evidence(kinds={"term": 0.2}, rows=[("upload", "term", 12)])),
        ]
        noise = [
            gold_entry(OTHER, evidence(kinds={"facet": 0.9}, rows=[("invoice", "facet", 4000)])),
            gold_entry(OTHER, evidence(kinds={"facet": 0.9}, rows=[("invoice", "facet", 4000),
                                                                     ("upload", "term", 12)])),
        ]

        summary = benchmark.attribution_summary(gold, noise)

        self.assertEqual(summary["row_kinds"]["term"], {"gold_largest": 2, "noise_largest": 0})
        self.assertEqual(summary["row_kinds"]["facet"], {"gold_largest": 0, "noise_largest": 2})
        self.assertEqual(summary["rows_carried_gold"][0], {
            "text": "upload", "kind": "term", "document_frequency": 12,
            "gold_documents": 2, "noise_documents": 1,
        })
        self.assertEqual(summary["rows_carried_noise"][0]["text"], "invoice")
        self.assertEqual(summary["rows_carried_noise"][0]["document_frequency"], 4000)
        self.assertEqual([row["text"] for row in summary["rows_carried_gold"]], ["upload", "dsid"])
        self.assertEqual(summary["gold_documents_with_ledger"], 2)

    def test_attribution_splits_by_question_type_and_reports_readiness(self) -> None:
        questions = [
            {"question_id": "q1", "question_type": "basic", "expected_doc_ids": [DOCUMENT_A]},
            {"question_id": "q2", "question_type": "semantic", "expected_doc_ids": [DOCUMENT_B]},
            {"question_id": "q3", "question_type": "info_not_found", "expected_doc_ids": []},
        ]
        queries = [
            query_record(questions[0], [DOCUMENT_A], {
                address(DOCUMENT_A): evidence(seeded={"lexical": 1}, channels={"lexical": 0.5}),
            }),
            query_record(questions[1], [OTHER], {
                address(OTHER): evidence(seeded={"cluster": 1}, channels={"cluster": 0.5}),
            }),
            query_record(questions[2], [OTHER], {}),
        ]

        tables = benchmark.retrieval_attribution(queries, questions)

        self.assertTrue(tables["attribution_ready"])
        self.assertEqual(tables["gold_documents"], 2)
        self.assertEqual(tables["gold_documents_found"], 1)
        self.assertEqual(tables["noise_documents"], 1)
        self.assertEqual(tables["channels"]["lexical"]["gold_largest"], 1)
        self.assertEqual(tables["channels"]["cluster"]["noise_largest"], 1)
        self.assertEqual(sorted(tables["by_question_type"]), ["basic", "semantic"])
        self.assertEqual(tables["by_question_type"]["basic"]["channels"]["lexical"]["gold_seeded"], 1)
        self.assertEqual(tables["by_question_type"]["semantic"]["channels"]["cluster"]["noise_only_seeded"], 1)
        self.assertEqual(tables["by_question_type"]["semantic"]["gold_documents_found"], 0)

    def test_hubs_keep_the_highest_degree_per_text(self) -> None:
        queries = [
            {"question_id": "q1", "query_meta": {"hubs_excluded": [
                {"fragment": "f1", "degree": 100, "text": "invoice"},
                {"fragment": "f2", "degree": 50, "text": "upload"},
            ]}},
            {"question_id": "q2", "query_meta": {"hubs_excluded": [
                {"fragment": "f1", "degree": 300, "text": "invoice"},
            ]}},
            {"question_id": "q3"},
        ]

        hubs = benchmark.hubs_excluded(queries)

        self.assertEqual([(hub["text"], hub["degree"]) for hub in hubs],
                         [("invoice", 300), ("upload", 50)])

    def test_retrieval_scores_say_whether_ledgers_were_present(self) -> None:
        questions = [{"question_id": "q1", "expected_doc_ids": [DOCUMENT_A]}]
        without = [{"question_id": "q1", "retrieved_document_ids": [DOCUMENT_A]}]
        with_ledger = [{"question_id": "q1", "retrieved_document_ids": [DOCUMENT_A],
                        "query_meta": {"evidence": [{"address": address(DOCUMENT_A), "ledger": {}}]}}]

        self.assertFalse(benchmark.retrieval_scores(without, questions)["attribution_ready"])
        self.assertTrue(benchmark.retrieval_scores(with_ledger, questions)["attribution_ready"])

    def test_oversize_evidence_is_refused_at_the_boundary(self) -> None:
        payload = {"results": [], "meta": {"evidence": [{"address": "a"}] * 51}}
        with self.assertRaises(benchmark.BenchmarkError):
            benchmark.query_meta_object(json.dumps(payload))

    def test_options_round_trip_finder_overrides_and_explain(self) -> None:
        options = benchmark.RunOptions(
            1, 8, 12, 8, 0, 4, True, skip_agent=True,
            finder_overrides=["seed_lists.cluster.weight=0", "hub_degree_max=200"],
            explain=True,
        )
        manifest = {"options": dict(vars(options))}

        self.assertEqual(benchmark.options_from_manifest(manifest), options)

        legacy = {"options": {key: value for key, value in vars(options).items()
                              if key not in {"finder_overrides", "explain"}}}
        self.assertEqual(benchmark.options_from_manifest(legacy).finder_overrides, [])
        self.assertFalse(benchmark.options_from_manifest(legacy).explain)

        malformed = {"options": {**vars(options), "finder_overrides": ["no equals sign"]}}
        with self.assertRaises(benchmark.BenchmarkError):
            benchmark.options_from_manifest(malformed)


OTHER = "dsid_cccccccccccccccccccccccccccccccc"


def address(document_id: str) -> str:
    return f"inseam://enterprise-rag-bench/x/{document_id}_file.txt"


def evidence(
    seeded: dict[str, int] | None = None,
    channels: dict[str, float] | None = None,
    kinds: dict[str, float] | None = None,
    rows: list[tuple[str, str, int]] | None = None,
) -> dict:
    """A synthetic evidence object: one fragment seeded at the given ranks, one ledger."""
    fragment = {"fragment": "f1"}
    for channel, rank in (seeded or {}).items():
        fragment[f"{channel}_rank"] = rank
    return {
        "address": "inseam://synthetic",
        "fragments": [fragment],
        "ledger": {
            "channels": [{"channel": channel, "seed": mass, "walk": 0.0}
                         for channel, mass in (channels or {}).items()],
            "walk_by_row_kind": dict(kinds or {}),
            "rows": [{"fragment": "r", "kind": kind, "text": text,
                      "document_frequency": frequency, "mass": 0.1}
                     for text, kind, frequency in (rows or [])],
        },
    }


def gold_entry(document_id: str, evidence_object: dict | None) -> dict:
    return {"question_type": "basic", "document_id": document_id,
            "rank": None if evidence_object is None else 1, "evidence": evidence_object}


def query_record(question: dict, document_ids: list[str], evidence_by_address: dict) -> dict:
    results = [{"address": address(document_id)} for document_id in document_ids]
    meta = {"evidence": [{**value, "address": key} for key, value in evidence_by_address.items()]}
    return {
        "question_id": question["question_id"],
        "results": results,
        "retrieved_document_ids": document_ids,
        "query_meta": meta,
        "attribution": benchmark.attribution_rows(question, results, meta),
    }


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
    address = f"inseam://enterprise-rag-bench/x/{DOCUMENT_A}_file.txt"
    query = json.dumps(
        {
            "results": [{"address": address, "score": 1.0}],
            "meta": {
                "exact_hits": 1,
                "hubs_excluded": [{"fragment": "f9", "degree": 900, "text": "invoice"}],
                "evidence": [
                    {
                        "address": address,
                        "score_raw": 1.0,
                        "normalization": 1.0,
                        "fragments": [{"fragment": "f1", "lexical_rank": 1}],
                        "ledger": {
                            "channels": [{"channel": "lexical", "seed": 0.6, "walk": 0.1}],
                            "walk_by_row_kind": {"term": 0.1},
                            "rows": [{"fragment": "f2", "kind": "term", "text": "upload", "mass": 0.1}],
                        },
                    }
                ],
            },
        }
    )
    return f'''#!/usr/bin/env python3
import json
import sys
if "--version" in sys.argv:
    print("inseam 9.9.9")
elif "query" in sys.argv:
    payload = json.loads({query!r})
    payload["meta"]["argv"] = sys.argv[1:]
    print(json.dumps(payload))
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
