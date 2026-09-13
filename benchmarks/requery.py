#!/usr/bin/env python3
"""Rescore a completed benchmark index using query-time controls only."""
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
    if runner is enterprise:
        changes.update(skip_agent=True, skip_evaluation=True)
    return dataclasses.replace(options, **changes)


def score(runner: Any, run_dir: Path, rows: list[dict[str, Any]], inputs: list[dict[str, Any]], options: Any) -> dict[str, Any]:
    if runner is beir:
        aggregate, per_query = beir.beir_scores(rows, beir.load_qrels(), beir.metric_cutoffs(options.results_per_query))
        harness.write_json_lines(run_dir / "query-scores.jsonl", per_query, beir.MAX_QUERIES)
        beir.write_trec_run(run_dir, rows)
        return {"beir": aggregate}
    return {"retrieval": enterprise.retrieval_scores(rows, inputs)}


def run(arguments: argparse.Namespace) -> None:
    runner = RUNNERS[arguments.benchmark]
    origin, source_manifest, data_dir = load_origin(runner, arguments.run_id)
    options = selected_options(runner, source_manifest, arguments)
    started_at = harness.utc_now()
    replay_id = harness.new_run_id(started_at)
    run_dir = origin / "retrieval" / replay_id
    run_dir.mkdir(parents=True, exist_ok=False)
    composition = run_dir / "composition.toml"
    composition.write_text(composition_with_query_options(
        (origin / "composition.toml").read_text(), vars(options)), encoding="utf-8")
    manifest = {
        "schema_version": 1, "run_kind": "retrieval-replay", "run_id": replay_id,
        "status": "running", "started_at": started_at, "queries_completed": 0,
        "origin_run_id": arguments.run_id, "origin_manifest_sha256": harness.sha256_file(origin / "manifest.json"),
        "data_dir": str(data_dir), "benchmark": runner.benchmark_pins(),
        "options": vars(options), "inseam": harness.inseam_identity(),
        "models": runner.model_assignments(options), "phase": "querying",
        "system": harness.system_specs(data_dir), "indexing": None,
        "indexing_note": "Reuses the origin index. No indexing or repair command is run.",
    }
    inputs = (beir.load_queries(options.query_count) if runner is beir
              else enterprise.load_questions(options.question_limit))
    started = time.monotonic()
    harness.write_json(run_dir / "manifest.json", manifest)
    try:
        rows = runner.run_queries(run_dir, data_dir, composition, manifest, inputs, options, [], run_dir / "logs")
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
    print(f"Retrieval replay recorded in {run_dir}", flush=True)


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("benchmark", choices=RUNNERS)
    parser.add_argument("run_id")
    parser.add_argument("--finder-seeds", choices=sorted(harness.FINDER_SEEDS))
    parser.add_argument("--finder-seed-k", type=harness.bounded_argument("finder-seed-k", 1000))
    parser.add_argument("--finder-rrf-k", type=harness.bounded_argument("finder-rrf-k", 1000))
    parser.add_argument("--finder-damping", type=harness.probability_argument)
    parser.add_argument("--finder-lexical-weight", type=harness.weight_argument)
    parser.add_argument("--finder-max-vector-distance", type=harness.distance_argument)
    return parser.parse_args()


if __name__ == "__main__":
    raise SystemExit(harness.run_main(lambda: run(parse_arguments())))
