#!/usr/bin/env python3
"""Replay retrieval or a small judged agent sample against a completed index."""
from __future__ import annotations

import argparse
import dataclasses
import json
import re
import time
from pathlib import Path
from typing import Any

import beir
import enterprise_rag_bench as enterprise
import harness

RUNNERS = {"beir": beir, "enterprise": enterprise}
QUERY_OPTIONS = {
    "finder_seeds": "seeds",
    "finder_seed_k": "seed_k",
    "finder_rrf_k": "rrf_k",
    "finder_damping": "damping",
    "finder_lexical_weight": "lexical_weight",
    "finder_max_vector_distance": "max_vector_distance",
}


def composition_with_query_options(text: str, options: dict[str, Any]) -> str:
    """Patch only Finder values; preserve every index-shape setting byte for byte."""
    entries = text.split("[[entry]]")
    if len(entries) > 128:
        raise harness.BenchmarkError("composition exceeds 128 entries")
    found = 0
    for index, entry in enumerate(entries):
        if re.search(r'^id = "finder"$', entry, re.MULTILINE) is None:
            continue
        found += 1
        if entry.count("[entry.config]\n") != 1:
            raise harness.BenchmarkError("Finder entry must have one configuration table")
        for option, field in QUERY_OPTIONS.items():
            value = json.dumps(options[option])
            line = f"{field} = {value}"
            pattern = rf"^{field} = .*?$"
            if re.search(pattern, entry, re.MULTILINE):
                entry = re.sub(pattern, lambda _: line, entry, flags=re.MULTILINE)
            else:
                entry = entry.replace("[entry.config]\n", f"[entry.config]\n{line}\n", 1)
        entries[index] = entry
    if found != 1:
        raise harness.BenchmarkError("composition must contain exactly one Finder entry")
    return "[[entry]]".join(entries)


def load_origin(runner: Any, run_id: str) -> tuple[Path, dict[str, Any], Path]:
    harness.validate_run_id(run_id)
    origin = runner.RUNS_ROOT / run_id
    manifest = harness.read_json_object(origin / "manifest.json")
    if manifest.get("status") != "completed":
        raise harness.BenchmarkError("requery requires a completed source run")
    if manifest.get("benchmark") != runner.benchmark_pins():
        raise harness.BenchmarkError("source run uses different benchmark pins")
    data_dir = harness.validate_index_data_path(run_id, manifest, runner.FIXTURE_ROOT)
    harness.load_index_completion(origin, manifest, runner.SOURCES_MAX)
    return origin, manifest, data_dir


def selected_options(runner: Any, origin: dict[str, Any], arguments: argparse.Namespace) -> Any:
    try:
        options = runner.RunOptions(**origin["options"])
    except TypeError as error:
        raise harness.BenchmarkError(f"source run options are incompatible: {error}") from error
    changes = {name: getattr(arguments, name) for name in QUERY_OPTIONS
               if getattr(arguments, name) is not None}
    overrides = getattr(arguments, "finder_override", None)
    if overrides:
        # Query-time overrides ride each request, never the composition
        # (design/vocabulary.md, dials): a replay may sweep them over one
        # index, and the manifest records exactly which list ran.
        if "finder_overrides" not in {field.name for field in dataclasses.fields(options)}:
            raise harness.BenchmarkError(f"{runner.__name__} replays do not take finder overrides")
        changes["finder_overrides"] = list(overrides)
    if runner is enterprise:
        changes.update(skip_agent=True, skip_evaluation=True)
        if getattr(arguments, "bookend_count", 0):
            if options.corpus_slice != 0:
                raise harness.BenchmarkError("judged bookends require the full document corpus")
            changes.update(skip_agent=False, skip_evaluation=False,
                           question_limit=2 * arguments.bookend_count,
                           answer_model=arguments.answer_model,
                           evaluation_model=arguments.evaluation_model)
    return dataclasses.replace(options, **changes)


def select_bookends(questions: list[dict[str, Any]], count: int) -> list[dict[str, Any]]:
    """Keep disjoint beginning/end questions in release order for paired comparisons."""
    if not 1 <= count <= enterprise.MAX_QUESTIONS // 2:
        raise harness.BenchmarkError("bookend count is outside the supported bounds")
    if 2 * count > len(questions):
        raise harness.BenchmarkError("beginning and end question slices would overlap")
    selected = questions[:count] + questions[-count:]
    assert len(selected) == 2 * count
    assert len({row["question_id"] for row in selected}) == len(selected)
    return selected


def replay_inputs(runner: Any, options: Any, bookend_count: int) -> list[dict[str, Any]]:
    if runner is beir:
        return beir.load_queries(options.query_count)
    if bookend_count:
        return select_bookends(enterprise.load_questions(enterprise.MAX_QUESTIONS, exact=False), bookend_count)
    return enterprise.load_questions(options.question_limit)


def score(runner: Any, run_dir: Path, rows: list[dict[str, Any]], inputs: list[dict[str, Any]], options: Any) -> dict[str, Any]:
    if runner is beir:
        aggregate, per_query = beir.beir_scores(rows, beir.load_qrels(), beir.metric_cutoffs(options.results_per_query))
        harness.write_json_lines(run_dir / "query-scores.jsonl", per_query, beir.MAX_QUERIES)
        beir.write_trec_run(run_dir, rows)
        return {"beir": aggregate}
    scores = {"retrieval": enterprise.retrieval_scores(rows, inputs)}
    if not options.skip_evaluation:
        scores["enterprise_rag_bench"] = enterprise.evaluate(
            run_dir, run_dir / "logs", options.evaluation_parallelism,
            len(inputs), options.evaluation_model)
        enterprise.pip_freeze(run_dir)
    return scores


def record_source_diff(run_dir: Path) -> str:
    """Keep uncommitted Rust changes so a dirty binary's source can be identified."""
    result = harness.run_capture(
        ["git", "diff", "--binary", "HEAD", "--", "crates", "Cargo.toml", "Cargo.lock"],
        timeout_seconds=harness.METADATA_TIMEOUT_SECONDS, cwd=harness.REPOSITORY_ROOT)
    harness.require_success(result, "recording benchmark source changes")
    path = run_dir / "source-diff.patch"
    path.write_text(result.stdout, encoding="utf-8")
    return harness.sha256_file(path)


def run(arguments: argparse.Namespace) -> None:
    runner = RUNNERS[arguments.benchmark]
    if arguments.bookend_count:
        if runner is not enterprise:
            raise harness.BenchmarkError("judged bookends are only supported for EnterpriseRAG")
        enterprise.evaluator_environment(arguments.evaluation_model)
    origin, source_manifest, data_dir = load_origin(runner, arguments.run_id)
    options = selected_options(runner, source_manifest, arguments)
    inputs = replay_inputs(runner, options, arguments.bookend_count)
    started_at = harness.utc_now()
    replay_id = harness.new_run_id(started_at)
    run_kind = "agent" if arguments.bookend_count else "retrieval"
    run_dir = origin / run_kind / replay_id
    run_dir.mkdir(parents=True, exist_ok=False)
    composition = run_dir / "composition.toml"
    composition.write_text(composition_with_query_options(
        (origin / "composition.toml").read_text(), vars(options)), encoding="utf-8")
    manifest = {
        "schema_version": 1, "run_kind": f"{run_kind}-replay", "run_id": replay_id,
        "status": "running", "started_at": started_at, "queries_completed": 0,
        "origin_run_id": arguments.run_id, "origin_manifest_sha256": harness.sha256_file(origin / "manifest.json"),
        "data_dir": str(data_dir), "benchmark": runner.benchmark_pins(),
        "options": vars(options), "inseam": harness.inseam_identity(),
        "models": runner.model_assignments(options), "phase": "querying",
        "system": harness.system_specs(data_dir), "indexing": None,
        "indexing_note": "Reuses the origin index. No indexing or repair command is run.",
    }
    if arguments.bookend_count:
        manifest["source_diff_sha256"] = record_source_diff(run_dir)
        manifest["question_selection"] = {
            "method": "release-order-bookends", "count_per_end": arguments.bookend_count,
            "question_ids": [row["question_id"] for row in inputs],
        }
        harness.write_json_lines(run_dir / "selected-questions.jsonl", inputs, enterprise.MAX_QUESTIONS)
    started = time.monotonic()
    harness.write_json(run_dir / "manifest.json", manifest)
    try:
        rows = runner.run_queries(run_dir, data_dir, composition, manifest, inputs, options, [], run_dir / "logs")
        if arguments.bookend_count:
            manifest["phase"] = "evaluating"
            harness.write_json(run_dir / "manifest.json", manifest)
        manifest["scores"] = score(runner, run_dir, rows, inputs, options)
        harness.write_retrieval_observability(run_dir, rows)
        harness.record_final_index_footprint(manifest, data_dir)
        manifest["status"] = "completed"
        manifest["phase"] = "completed"
    except BaseException as error:
        manifest["status"] = "interrupted" if isinstance(error, KeyboardInterrupt) else "failed"
        manifest["phase"] = manifest["status"]
        manifest["error"] = str(error)
        raise
    finally:
        manifest["finished_at"] = harness.utc_now()
        manifest["duration_seconds"] = round(time.monotonic() - started, 6)
        harness.write_json(run_dir / "manifest.json", manifest)
    print(f"{run_kind.capitalize()} replay recorded in {run_dir}", flush=True)


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("benchmark", choices=RUNNERS)
    parser.add_argument("run_id")
    parser.add_argument("--bookend-count", type=harness.bounded_argument("bookend-count", enterprise.MAX_QUESTIONS // 2), default=0,
                        help="Judge the first N and last N questions with the agent on the full corpus.")
    parser.add_argument("--answer-model", default="openai/gpt-5.4")
    parser.add_argument("--evaluation-model", default="openai/gpt-5.4")
    parser.add_argument("--finder-seeds", choices=sorted(harness.FINDER_SEEDS))
    parser.add_argument("--finder-seed-k", type=harness.bounded_argument("finder-seed-k", 1000))
    parser.add_argument("--finder-rrf-k", type=harness.bounded_argument("finder-rrf-k", 1000))
    parser.add_argument("--finder-damping", type=harness.probability_argument)
    parser.add_argument("--finder-lexical-weight", type=harness.weight_argument)
    parser.add_argument("--finder-max-vector-distance", type=harness.distance_argument)
    parser.add_argument(
        "--finder-override",
        action="append",
        default=[],
        type=harness.finder_override_argument,
        metavar="KEY=VALUE",
        help="a query-time Finder override for every replayed query (repeatable); replaces the origin's list",
    )
    return parser.parse_args()


if __name__ == "__main__":
    raise SystemExit(harness.run_main(lambda: run(parse_arguments())))
