"""Tests for the BEIR harness: fixture parsing, scoring, and the run ledger."""

import dataclasses
import io
import json
import os
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

import beir


SMALL_DATASET = dataclasses.replace(beir.NFCORPUS, document_count=3, query_count=2, qrel_count=4)
QRELS = (
    "query-id\tcorpus-id\tscore\n"
    "PLAIN-2\tMED-10\t2\n"
    "PLAIN-2\tMED-11\t1\n"
    "PLAIN-2\tMED-12\t1\n"
    "PLAIN-3\tMED-20\t1\n"
)


class BeirScoringTests(unittest.TestCase):
    def test_scores_a_ranking_with_trec_eval_semantics(self) -> None:
        grades = {"MED-10": 2, "MED-11": 1, "MED-12": 1}
        ranking = ["MED-11", "MED-99", "MED-10", "MED-98", "MED-97"]

        metrics = beir.score_query(ranking, grades, [1, 3, 5])

        # DCG@3 = 1/log2(2) + 0 + 2/log2(4) = 2.0; ideal = 2/log2(2) + 1/log2(3) + 1/log2(4).
        ideal_at_3 = 2.0 + 1.0 / 1.5849625007211563 + 0.5
        self.assertAlmostEqual(metrics["ndcg@1"], 1.0 / 2.0)
        self.assertAlmostEqual(metrics["ndcg@3"], 2.0 / ideal_at_3)
        # Average precision divides by all three relevant documents, not the two found.
        self.assertAlmostEqual(metrics["map@3"], (1.0 + 2.0 / 3.0) / 3.0)
        self.assertAlmostEqual(metrics["recall@1"], 1.0 / 3.0)
        self.assertAlmostEqual(metrics["recall@5"], 2.0 / 3.0)
        self.assertAlmostEqual(metrics["precision@5"], 2.0 / 5.0)

    def test_ranking_without_hits_scores_zero(self) -> None:
        metrics = beir.score_query(["MED-1", "MED-2"], {"MED-9": 1}, [1, 2])

        self.assertEqual(metrics["ndcg@2"], 0.0)
        self.assertEqual(metrics["map@2"], 0.0)
        self.assertEqual(metrics["recall@2"], 0.0)
        self.assertEqual(metrics["precision@2"], 0.0)

    def test_query_without_relevant_documents_is_an_error(self) -> None:
        with self.assertRaises(beir.BenchmarkError):
            beir.score_query(["MED-1"], {"MED-1": 0}, [1])

    def test_aggregate_averages_over_queries_and_records_cutoffs(self) -> None:
        qrels = beir.parse_qrels(QRELS)
        queries = [
            {"query_id": "PLAIN-2", "retrieved_document_ids": ["MED-10"]},
            {"query_id": "PLAIN-3", "retrieved_document_ids": ["MED-1"]},
        ]

        aggregate, per_query = beir.beir_scores(queries, qrels, [1])

        self.assertEqual(aggregate["ndcg@1"], 0.5)
        self.assertEqual(aggregate["precision@1"], 0.5)
        self.assertEqual(aggregate["queries_evaluated"], 2)
        self.assertEqual(aggregate["cutoffs"], [1])
        self.assertEqual([row["query_id"] for row in per_query], ["PLAIN-2", "PLAIN-3"])

    def test_metric_cutoffs_stop_at_the_result_limit(self) -> None:
        self.assertEqual(beir.metric_cutoffs(10), [1, 3, 5, 10])
        self.assertEqual(beir.metric_cutoffs(4), [1, 3, 4])
        self.assertEqual(beir.metric_cutoffs(25), [1, 3, 5, 10, 25])


class BeirFixtureTests(unittest.TestCase):
    def test_parses_graded_qrels(self) -> None:
        qrels = beir.parse_qrels(QRELS)

        self.assertEqual(qrels["PLAIN-2"], {"MED-10": 2, "MED-11": 1, "MED-12": 1})
        self.assertEqual(qrels["PLAIN-3"], {"MED-20": 1})

    def test_rejects_qrels_with_a_foreign_header(self) -> None:
        with self.assertRaises(beir.BenchmarkError):
            beir.parse_qrels("qid\tdid\trel\nPLAIN-2\tMED-10\t2\n")

    def test_rejects_repeated_judgments(self) -> None:
        text = "query-id\tcorpus-id\tscore\nPLAIN-2\tMED-10\t2\nPLAIN-2\tMED-10\t1\n"

        with self.assertRaises(beir.BenchmarkError):
            beir.parse_qrels(text)

    def test_selects_judged_queries_in_file_order(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "queries.jsonl"
            path.write_text(
                json.dumps({"_id": "PLAIN-3", "text": "Second"}) + "\n"
                + json.dumps({"_id": "PLAIN-1", "text": "Unjudged"}) + "\n"
                + json.dumps({"_id": "PLAIN-2", "text": " First "}) + "\n"
            )

            selected = beir.select_queries(path, beir.parse_qrels(QRELS))

        self.assertEqual(
            selected,
            [
                {"query_id": "PLAIN-3", "query": "Second"},
                {"query_id": "PLAIN-2", "query": "First"},
            ],
        )

    def test_document_text_leads_with_the_title(self) -> None:
        text = beir.document_text({"_id": "MED-10", "title": "Statins", "text": "Body."})

        self.assertEqual(text, "Statins\n\nBody.\n")
        self.assertEqual(beir.document_text({"_id": "PLAIN-3", "text": "Body."}), "Body.\n")

    def test_rejects_document_ids_that_cannot_be_file_names(self) -> None:
        for document_id in ("", "../x", "a/b", "-lead"):
            with self.subTest(document_id=document_id):
                with self.assertRaises(beir.BenchmarkError):
                    beir.validate_document_id(document_id)

    def test_maps_result_addresses_back_to_documents(self) -> None:
        known = {"MED-10"}

        document_id = beir.document_id_from_address(
            "inseam://beir-nfcorpus/some/root/MED-10.txt", known
        )

        self.assertEqual(document_id, "MED-10")
        with self.assertRaises(beir.BenchmarkError):
            beir.document_id_from_address("inseam://beir-nfcorpus/root/MED-11.txt", known)
        with self.assertRaises(beir.BenchmarkError):
            beir.document_id_from_address("inseam://beir-nfcorpus/root/MED-10.md", known)

    def test_composition_embeds_every_fragment_with_the_shared_models(self) -> None:
        composition = beir.composition_text(beir.RunOptions(323, 10, 8, 500))

        self.assertIn('host_id = "beir-nfcorpus"', composition)
        self.assertIn('transform_model = "google/gemini-2.5-flash-lite"', composition)
        self.assertIn('llm_lane = "batch"', composition)
        self.assertIn('model = "openai/text-embedding-3-small"', composition)
        self.assertIn("dimensions = 384", composition)
        self.assertIn('vectors = "all"', composition)
        self.assertNotIn("agent_model", composition)
        self.assertEqual(composition.count("disabled = true"), 3)


class BeirRunTests(unittest.TestCase):
    def test_run_records_scores_checkpoint_and_trec_run(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_text:
            temporary = Path(temporary_text)
            fixture, runs, environment = prepare_fake_fixture(temporary)
            options = beir.RunOptions(2, 3, 8, 500)

            output = io.StringIO()
            with redirect_stdout(output):
                with mock.patch.object(beir, "FIXTURE_ROOT", fixture):
                    with mock.patch.object(beir, "RUNS_ROOT", runs):
                        with mock.patch.object(beir, "DATASET", SMALL_DATASET):
                            with mock.patch.dict(os.environ, environment):
                                beir.run_benchmark(options)

            run_dir = next(runs.iterdir())
            manifest = json.loads((run_dir / "manifest.json").read_text())
            self.assertEqual(manifest["status"], "completed")
            self.assertEqual(manifest["queries_completed"], 2)
            self.assertEqual(manifest["benchmark"]["name"], "BEIR")
            self.assertEqual(manifest["benchmark"]["dataset"], "nfcorpus")
            self.assertEqual(manifest["models"]["embedding_vectors"], "all")
            self.assertNotIn("answer_generation", manifest["models"])
            self.assertEqual(manifest["indexing"]["summary"]["sources_seen"], 3)
            self.assertIsNotNone(manifest["attempts"][0]["search_index_preparation"])
            scores = manifest["scores"]["beir"]
            self.assertEqual(scores["cutoffs"], [1, 3])
            # PLAIN-2 ranks MED-11 (grade 1) then MED-10 (grade 2); PLAIN-3 misses.
            self.assertEqual(scores["queries_evaluated"], 2)
            self.assertAlmostEqual(scores["precision@1"], 0.5)
            self.assertAlmostEqual(scores["recall@3"], (2.0 / 3.0 + 0.0) / 2.0, places=5)
            trec = (run_dir / "run.trec").read_text().splitlines()
            self.assertEqual(trec[0], "PLAIN-2 Q0 MED-11 1 1.000000 inseam")
            self.assertEqual(len(trec), 4)
            per_query = (run_dir / "query-scores.jsonl").read_text().splitlines()
            self.assertEqual(len(per_query), 2)
            progress = output.getvalue()
            self.assertIn("Indexing 3 benchmark documents...", progress)
            self.assertIn("[1/2] PLAIN-2 retrieval...", progress)
            self.assertIn("ndcg", progress)
            self.assertIn("Benchmark run recorded", progress)

    def test_resume_skips_indexing_and_continues_from_the_checkpoint(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_text:
            temporary = Path(temporary_text)
            fixture, runs, environment = prepare_fake_fixture(temporary)
            options = beir.RunOptions(2, 3, 8, 500)

            with redirect_stdout(io.StringIO()) as output:
                with mock.patch.object(beir, "FIXTURE_ROOT", fixture):
                    with mock.patch.object(beir, "RUNS_ROOT", runs):
                        with mock.patch.object(beir, "DATASET", SMALL_DATASET):
                            with mock.patch.dict(os.environ, environment):
                                run_dir, data_dir, manifest = beir.create_run(options)
                                composition = run_dir / "composition.toml"
                                composition.write_text(beir.composition_text(options))
                                manifest["indexing"] = beir.index_documents(
                                    run_dir, data_dir, composition, run_dir / "logs" / "index.log"
                                )
                                beir.harness.set_run_error(manifest, "failed", "boom")
                                manifest["attempts"] = []
                                beir.write_json(run_dir / "manifest.json", manifest)
                                with mock.patch.object(
                                    beir,
                                    "index_documents",
                                    side_effect=AssertionError("index must not run"),
                                ):
                                    beir.resume_benchmark(manifest["run_id"])

            resumed = json.loads((run_dir / "manifest.json").read_text())
            self.assertEqual(resumed["status"], "completed")
            self.assertEqual(resumed["queries_completed"], 2)
            self.assertTrue(resumed["attempts"][0]["resumed"] is False)
            self.assertIn("Reusing completed index: 3 sources", output.getvalue())

    def test_interruption_records_the_terminal_phase(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_text:
            temporary = Path(temporary_text)
            fixture, runs, environment = prepare_fake_fixture(temporary)
            options = beir.RunOptions(2, 3, 8, 500)

            with redirect_stdout(io.StringIO()):
                with mock.patch.object(beir, "FIXTURE_ROOT", fixture):
                    with mock.patch.object(beir, "RUNS_ROOT", runs):
                        with mock.patch.dict(os.environ, environment):
                            with mock.patch.object(
                                beir, "index_documents", side_effect=KeyboardInterrupt
                            ):
                                with self.assertRaises(KeyboardInterrupt):
                                    beir.run_benchmark(options)

            run_dir = next(runs.iterdir())
            recorded = json.loads((run_dir / "manifest.json").read_text())
            self.assertEqual(recorded["status"], "interrupted")
            self.assertEqual(recorded["phase"], "interrupted")
            self.assertEqual(recorded["attempts"][0]["error"], "interrupted by user")


def prepare_fake_fixture(temporary: Path) -> tuple[Path, Path, dict[str, str]]:
    fixture = temporary / "fixture"
    runs = temporary / "runs"
    fake_bin = temporary / "bin"
    documents = fixture / "documents"
    documents.mkdir(parents=True)
    for document_id in ("MED-10", "MED-11", "MED-20"):
        (documents / f"{document_id}.txt").write_text(f"{document_id}\n")
    (fixture / "documents.json").write_text(json.dumps({"text_file_count": 3}) + "\n")
    (fixture / "setup.json").write_text("{}\n")
    (fixture / "qrels.tsv").write_text(QRELS)
    (fixture / "queries.jsonl").write_text(
        json.dumps({"query_id": "PLAIN-2", "query": "statins"}) + "\n"
        + json.dumps({"query_id": "PLAIN-3", "query": "autophagy"}) + "\n"
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
    def response(*document_ids: str) -> str:
        return json.dumps(
            {
                "results": [
                    {
                        "address": f"inseam://beir-nfcorpus/root/{document_id}.txt",
                        "score": round(1.0 - 0.25 * index, 6),
                    }
                    for index, document_id in enumerate(document_ids)
                ]
            }
        )

    statins = response("MED-11", "MED-10")
    autophagy = response("MED-10", "MED-11")
    return f'''#!/usr/bin/env python3
import sys
if "--version" in sys.argv:
    print("inseam 9.9.9")
elif "query" in sys.argv:
    print({statins!r} if "statins" in sys.argv else {autophagy!r})
elif "index" in sys.argv:
    print("3 sources seen: 3 indexed, 0 unchanged, 0 catalog-only, 0 past cutoff, 0 ignored")
    print("6 fragments, 0 relations, 0 keyed fragments anchored")
    print("summaries: 3 llm, 0 extractive, 0 envelope · 6 embedded · $0.001 spent")
elif "repair" in sys.argv:
    print("search rows    6")
    print("vector index  already ready")
else:
    raise SystemExit(2)
'''


if __name__ == "__main__":
    unittest.main()
