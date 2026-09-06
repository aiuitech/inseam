#!/usr/bin/env python3
"""Run EnterpriseRAG-Bench against the installed inseam CLI."""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from harness import (
    EMBEDDING_DIMENSIONS,
    EMBEDDING_MODEL,
    FIXTURES_ROOT,
    INDEX_PROGRESS_INTERVAL_SECONDS,
    INDEX_TIMEOUT_SECONDS,
    MAX_INDEX_CONCURRENCY,
    MAX_LLM_CALL_BUDGET,
    METADATA_TIMEOUT_SECONDS,
    OPENROUTER_BASE_URL,
    PROGRESS_INTERVAL_SECONDS,
    RUNS_ROOT as ALL_RUNS_ROOT,
    SUMMARIZATION_LANE,
    SUMMARIZATION_MODEL,
    BenchmarkError,
    CommandResult,
    begin_attempt,
    bounded_argument,
    complete_run,
    download_verified,
    finish_attempt,
    fixture_document_count,
    inseam_arguments,
    inseam_identity,
    load_index_completion,
    manifest_option_integer,
    manifest_options_object,
    new_run_id,
    parse_query_results,
    prepare_search_index,
    print_reused_index,
    query_arguments,
    read_json_lines,
    read_json_object,
    record_run_outcome,
    require_api_key,
    require_program,
    require_success,
    run_capture,
    run_logged,
    run_main,
    set_run_error,
    system_specs,
    update_run_phase,
    utc_now,
    validate_index_data_path,
    validate_resumable_status,
    validate_run_id,
    write_json,
    write_json_lines,
)
import harness


FIXTURE_ROOT = FIXTURES_ROOT / "enterprise-rag-bench"
RUNS_ROOT = ALL_RUNS_ROOT / "enterprise-rag-bench"
RELEASE = "v1.0.0"
UPSTREAM_REVISION = "d36685e273713975ee20299bbf1ab64165575b3c"
UPSTREAM_URL = "https://github.com/onyx-dot-app/EnterpriseRAG-Bench.git"
RELEASE_URL = f"https://github.com/onyx-dot-app/EnterpriseRAG-Bench/releases/download/{RELEASE}"
ARCHIVE_SHA256 = "9d1174928696ad08bc15f3f104739519de633c1605a4ec2034e0e3c0087bc5cd"
QUESTIONS_SHA256 = "f9524b9157cd43aae36b99333a124738804306ea6d07f332d49faa6d3d147905"
ANSWER_MODEL = "stealth/ox-alpha"
EVALUATION_MODEL = "stealth/ox-alpha"
# OpenRouter rejects batch jobs above 5,000 requests; the endpoint plugin
# refuses larger values at boot. Parking 65,536 planners keeps several
# jobs filling at once.
SUMMARY_BATCH_REQUESTS_MAX = 5_000
SUMMARY_BATCH_CONCURRENCY = 65_536
MAX_QUESTIONS = 1_000
MAX_TURNS = 64
MAX_QUERY_RESULTS = 25
# The release holds slightly more than 500,000 documents; anything past this
# bound means the archive is not the pinned one.
DOCUMENTS_MIN = 500_000
DOCUMENTS_MAX = 600_000
SOURCES_MAX = DOCUMENTS_MAX + 1
SETUP_TIMEOUT_SECONDS = 7_200
QUERY_TIMEOUT_SECONDS = 600
AGENT_TIMEOUT_SECONDS = 1_800
EVALUATION_TIMEOUT_SECONDS = 604_800
DOCUMENT_ID_PATTERN = re.compile(r"dsid_[0-9a-f]{32}")


@dataclass(frozen=True)
class RunOptions:
    question_limit: int
    query_limit: int
    turns: int
    index_concurrency: int
    llm_call_budget: int
    evaluation_parallelism: int
    skip_evaluation: bool


def extract_documents(archive: Path) -> None:
    unzip = require_program("unzip")
    documents = FIXTURE_ROOT / "documents"
    marker = FIXTURE_ROOT / "documents.json"
    if marker.exists() and documents.is_dir():
        return
    documents.mkdir(parents=True, exist_ok=True)
    result = run_capture(
        [unzip, "-q", str(archive), "-d", str(documents)],
        timeout_seconds=SETUP_TIMEOUT_SECONDS,
    )
    require_success(result, "extracting all_documents.zip")
    text_file_count = 0
    for path in documents.rglob("*.txt"):
        text_file_count += 1
        if text_file_count > DOCUMENTS_MAX:
            raise BenchmarkError(
                f"archive contains more than the {DOCUMENTS_MAX}-document safety limit"
            )
        assert path.is_file()
    if text_file_count < DOCUMENTS_MIN:
        raise BenchmarkError(
            f"extracted only {text_file_count} text documents; expected at least {DOCUMENTS_MIN}"
        )
    write_json(
        marker,
        {
            "archive_sha256": ARCHIVE_SHA256,
            "extracted_at": utc_now(),
            "text_file_count": text_file_count,
        },
    )


def checkout_evaluator() -> Path:
    git = require_program("git")
    evaluator = FIXTURE_ROOT / "evaluator"
    evaluator_exists = (evaluator / ".git").exists()
    if not evaluator_exists:
        evaluator.parent.mkdir(parents=True, exist_ok=True)
        result = run_capture(
            [git, "clone", "--filter=blob:none", "--no-checkout", UPSTREAM_URL, str(evaluator)],
            timeout_seconds=SETUP_TIMEOUT_SECONDS,
        )
        require_success(result, "cloning the EnterpriseRAG-Bench evaluator")
        require_success(
            run_capture(
                [
                    git,
                    "sparse-checkout",
                    "set",
                    "answer_evaluation",
                    "src",
                    "requirements.txt",
                    "pyproject.toml",
                ],
                timeout_seconds=SETUP_TIMEOUT_SECONDS,
                cwd=evaluator,
            ),
            "configuring the evaluator sparse checkout",
        )
    else:
        require_clean_evaluator(git, evaluator)
    require_success(
        run_capture(
            [git, "fetch", "--depth", "1", "origin", UPSTREAM_REVISION],
            timeout_seconds=SETUP_TIMEOUT_SECONDS,
            cwd=evaluator,
        ),
        "fetching the pinned evaluator revision",
    )
    require_success(
        run_capture(
            [git, "checkout", "--detach", UPSTREAM_REVISION],
            timeout_seconds=SETUP_TIMEOUT_SECONDS,
            cwd=evaluator,
        ),
        "checking out the pinned evaluator revision",
    )
    require_clean_evaluator(git, evaluator)
    return evaluator


def require_clean_evaluator(git: str, evaluator: Path) -> None:
    status = run_capture(
        [git, "status", "--porcelain"],
        timeout_seconds=SETUP_TIMEOUT_SECONDS,
        cwd=evaluator,
    )
    require_success(status, "checking evaluator state")
    if status.stdout.strip():
        raise BenchmarkError(
            f"{evaluator} has local changes; preserve or remove them before setup"
        )


def setup_evaluator_environment(evaluator: Path) -> None:
    python = require_program("python3")
    virtual_environment = evaluator / ".venv"
    if not (virtual_environment / "bin" / "python").exists():
        require_success(
            run_capture(
                [python, "-m", "venv", str(virtual_environment)],
                timeout_seconds=SETUP_TIMEOUT_SECONDS,
            ),
            "creating the evaluator virtual environment",
        )
    evaluator_python = virtual_environment / "bin" / "python"
    install = run_capture(
        [str(evaluator_python), "-m", "pip", "install", "openai", "pydantic"],
        timeout_seconds=SETUP_TIMEOUT_SECONDS,
        cwd=evaluator,
    )
    require_success(install, "installing evaluator dependencies")
    import_check = run_capture(
        [str(evaluator_python), "-c", "import src.scripts.answer_evaluation.metrics_based_eval"],
        timeout_seconds=METADATA_TIMEOUT_SECONDS,
        cwd=evaluator,
    )
    require_success(import_check, "importing the EnterpriseRAG-Bench evaluator")


def setup() -> None:
    FIXTURE_ROOT.mkdir(parents=True, exist_ok=True)
    downloads = FIXTURE_ROOT / "downloads"
    archive = download_verified(
        f"{RELEASE_URL}/all_documents.zip", downloads / "all_documents.zip", ARCHIVE_SHA256
    )
    questions = download_verified(
        f"{RELEASE_URL}/questions.jsonl", downloads / "questions.jsonl", QUESTIONS_SHA256
    )
    shutil.copyfile(questions, FIXTURE_ROOT / "questions.jsonl")
    extract_documents(archive)
    evaluator = checkout_evaluator()
    setup_evaluator_environment(evaluator)
    write_json(
        FIXTURE_ROOT / "setup.json",
        {
            "benchmark": "EnterpriseRAG-Bench",
            "release": RELEASE,
            "upstream_revision": UPSTREAM_REVISION,
            "archive_sha256": ARCHIVE_SHA256,
            "questions_sha256": QUESTIONS_SHA256,
            "completed_at": utc_now(),
        },
    )
    print(f"EnterpriseRAG-Bench is ready at {FIXTURE_ROOT}")


def composition_text(options: RunOptions) -> str:
    return f'''[[entry]]
id = "fs"
[entry.config]
host_id = "enterprise-rag-bench"
skip_hidden = true
gitignore = false
ignore = []

[[entry]]
id = "llm"
[entry.config]
base_url = "{OPENROUTER_BASE_URL}"
api_key_env = "OPENROUTER_API_KEY"
transform_model = "{SUMMARIZATION_MODEL}"
transform_reasoning_effort = "none"
agent_model = "{ANSWER_MODEL}"
batch_requests_max = {SUMMARY_BATCH_REQUESTS_MAX}

[[entry]]
id = "embedder"
[entry.config]
provider = "endpoint"
model = "{EMBEDDING_MODEL}"
dimensions = {EMBEDDING_DIMENSIONS}
vectors = "summaries"

[[entry]]
id = "markdown"
disabled = true

[[entry]]
id = "chunker"
disabled = true

[[entry]]
id = "summarizer"
[entry.config]
target_chars = 200
llm_call_budget = {options.llm_call_budget}
llm_lane = "{SUMMARIZATION_LANE}"

[[entry]]
id = "entities"
disabled = true

[[entry]]
id = "sweep"
[entry.config]
max_sources = 0
concurrency = {options.index_concurrency}
batch_concurrency = {SUMMARY_BATCH_CONCURRENCY}
max_fragments_per_source = 400
max_depth = 6
max_content_bytes = 2000000
ignore = []
'''


def load_questions(limit: int) -> list[dict[str, Any]]:
    question_path = FIXTURE_ROOT / "questions.jsonl"
    lines = question_path.read_text(encoding="utf-8").splitlines()
    if len(lines) > MAX_QUESTIONS:
        raise BenchmarkError(f"questions file has {len(lines)} rows; hard limit is {MAX_QUESTIONS}")
    questions = [json.loads(line) for line in lines if line.strip()]
    if len(questions) < limit:
        raise BenchmarkError(f"requested {limit} questions but the fixture has {len(questions)}")
    selected = questions[:limit]
    assert len(selected) == limit
    return selected


def extract_document_ids(values: list[str]) -> list[str]:
    found: set[str] = set()
    ordered: list[str] = []
    for value in values:
        for document_id in DOCUMENT_ID_PATTERN.findall(value):
            if document_id in found:
                continue
            found.add(document_id)
            ordered.append(document_id)
    assert len(ordered) == len(found)
    return ordered


def extract_agent_answer(stdout: str) -> str:
    lines = stdout.splitlines()
    footer_index = len(lines)
    answer_start = 0
    for index, line in enumerate(lines):
        if line.startswith("· model "):
            answer_start = index + 1
        elif line.startswith("→ ") or line.startswith("  ← "):
            answer_start = index + 1
        elif line.startswith("· ") and " turns, " in line and " tool calls, " in line:
            footer_index = index
    if footer_index < answer_start:
        raise BenchmarkError("could not locate the answer in `inseam agent` output")
    answer = "\n".join(lines[answer_start:footer_index]).strip()
    if not answer:
        raise BenchmarkError("`inseam agent` returned an empty answer")
    return answer


def question_commands(
    question: dict[str, Any],
    options: RunOptions,
    data_dir: Path,
    composition: Path,
    log_dir: Path,
    progress_prefix: str,
) -> dict[str, Any]:
    question_id = str(question["question_id"])
    question_text = str(question["question"])
    query = run_logged(
        query_arguments(data_dir, composition, question_text, options.query_limit),
        log_dir / f"{question_id}-query.log",
        timeout_seconds=QUERY_TIMEOUT_SECONDS,
        progress_label=f"{progress_prefix} retrieval",
    )
    require_success(query, f"querying {question_id}")
    query_results = parse_query_results(query.stdout, MAX_QUERY_RESULTS)
    agent = run_logged(
        [
            *inseam_arguments(data_dir, composition),
            "agent",
            question_text,
            "--model",
            ANSWER_MODEL,
            "--turns",
            str(options.turns),
        ],
        log_dir / f"{question_id}-agent.log",
        timeout_seconds=AGENT_TIMEOUT_SECONDS,
        progress_label=f"{progress_prefix} answer",
    )
    require_success(agent, f"answering {question_id}")
    addresses = [str(result["address"]) for result in query_results]
    retrieved_document_ids = extract_document_ids(addresses)
    agent_document_ids = extract_document_ids([agent.stdout])
    document_ids = extract_document_ids([*retrieved_document_ids, *agent_document_ids])
    return {
        "question_id": question_id,
        "question_type": question.get("question_type"),
        "question": question_text,
        "retrieval_duration_seconds": round(query.duration_seconds, 6),
        "answer_duration_seconds": round(agent.duration_seconds, 6),
        "duration_seconds": round(query.duration_seconds + agent.duration_seconds, 6),
        "results": query_results,
        "retrieved_document_ids": retrieved_document_ids,
        "agent_document_ids": agent_document_ids,
        "document_ids": document_ids,
        "answer": extract_agent_answer(agent.stdout),
    }


def retrieval_scores(
    queries: list[dict[str, Any]], questions: list[dict[str, Any]]
) -> dict[str, Any]:
    expected_by_id = {
        str(question["question_id"]): question.get("expected_doc_ids", [])
        for question in questions
    }
    recalls: list[float] = []
    reciprocal_ranks: list[float] = []
    hits = 0
    for query in queries:
        expected = set(expected_by_id[query["question_id"]])
        if not expected:
            continue
        retrieved = query["retrieved_document_ids"]
        matched = expected.intersection(retrieved)
        recalls.append(len(matched) / len(expected))
        first_rank = next(
            (index + 1 for index, value in enumerate(retrieved) if value in expected),
            None,
        )
        reciprocal_ranks.append(0.0 if first_rank is None else 1.0 / first_rank)
        if matched:
            hits += 1
    evaluated = len(recalls)
    return {
        "questions_with_expected_documents": evaluated,
        "average_document_recall_pct": (
            round(100.0 * sum(recalls) / evaluated, 2) if evaluated else 0.0
        ),
        "document_hit_rate_pct": round(100.0 * hits / evaluated, 2) if evaluated else 0.0,
        "mean_reciprocal_rank": round(sum(reciprocal_ranks) / evaluated, 6) if evaluated else 0.0,
    }


def write_run_checkpoint(run_dir: Path, queries: list[dict[str, Any]]) -> None:
    write_json_lines(run_dir / "queries.jsonl", queries, MAX_QUESTIONS)
    answers = [
        {
            "question_id": query["question_id"],
            "answer": query["answer"],
            "document_ids": query["document_ids"],
        }
        for query in queries
    ]
    assert len(answers) == len(queries)
    write_json_lines(run_dir / "answers.jsonl", answers, MAX_QUESTIONS)


def create_run(options: RunOptions) -> tuple[Path, Path, dict[str, Any]]:
    started_at = utc_now()
    run_id = new_run_id(started_at)
    run_dir = RUNS_ROOT / run_id
    run_dir.mkdir(parents=True, exist_ok=False)
    data_dir = FIXTURE_ROOT / "nodes" / run_id
    data_dir.mkdir(parents=True, exist_ok=False)
    manifest = {
        "schema_version": 2,
        "run_id": run_id,
        "status": "running",
        "phase": "starting",
        "started_at": started_at,
        "finished_at": None,
        "duration_seconds": None,
        "benchmark": benchmark_pins(),
        "models": model_assignments(),
        "timeouts_seconds": {
            "setup_command": SETUP_TIMEOUT_SECONDS,
            "finder_query": QUERY_TIMEOUT_SECONDS,
            "agent_answer": AGENT_TIMEOUT_SECONDS,
            "indexing": INDEX_TIMEOUT_SECONDS,
            "evaluation": EVALUATION_TIMEOUT_SECONDS,
        },
        "progress_interval_seconds": PROGRESS_INTERVAL_SECONDS,
        "index_progress_interval_seconds": INDEX_PROGRESS_INTERVAL_SECONDS,
        "options": vars(options),
        "system": system_specs(FIXTURE_ROOT),
        "inseam": inseam_identity(),
        "fixture_path": str(FIXTURE_ROOT),
        "index_data_path": str(data_dir),
        "indexing": None,
        "queries_completed": 0,
        "scores": None,
        "attempts": [],
    }
    write_json(run_dir / "manifest.json", manifest)
    return run_dir, data_dir, manifest


def benchmark_pins() -> dict[str, str]:
    return {
        "name": "EnterpriseRAG-Bench",
        "release": RELEASE,
        "upstream_revision": UPSTREAM_REVISION,
        "archive_sha256": ARCHIVE_SHA256,
        "questions_sha256": QUESTIONS_SHA256,
    }


def model_assignments() -> dict[str, str | int]:
    return {
        "summarization": SUMMARIZATION_MODEL,
        "summarization_lane": SUMMARIZATION_LANE,
        "entity_extraction": "disabled",
        "answer_generation": ANSWER_MODEL,
        "answer_evaluation": EVALUATION_MODEL,
        "embeddings": EMBEDDING_MODEL,
        "embedding_dimensions": EMBEDDING_DIMENSIONS,
    }


def index_documents(
    run_dir: Path, data_dir: Path, composition: Path, log_path: Path | None = None
) -> dict[str, Any]:
    return harness.index_documents(
        run_dir,
        data_dir,
        composition,
        FIXTURE_ROOT / "documents",
        fixture_document_count(FIXTURE_ROOT, DOCUMENTS_MAX),
        SOURCES_MAX,
        "EnterpriseRAG-Bench",
        log_path,
    )


def run_queries(
    run_dir: Path,
    data_dir: Path,
    composition: Path,
    manifest: dict[str, Any],
    questions: list[dict[str, Any]],
    options: RunOptions,
    queries: list[dict[str, Any]],
    log_dir: Path,
) -> list[dict[str, Any]]:
    completed_count = len(queries)
    assert completed_count <= len(questions)
    print(
        f"Query checkpoint: {completed_count}/{len(questions)} completed",
        flush=True,
    )
    for index in range(completed_count, len(questions)):
        question = questions[index]
        question_id = str(question["question_id"])
        progress_prefix = f"[{index + 1}/{len(questions)}] {question_id}"
        started_at = utc_now()
        record = question_commands(
            question,
            options,
            data_dir,
            composition,
            log_dir,
            progress_prefix,
        )
        record["started_at"] = started_at
        record["finished_at"] = utc_now()
        queries.append(record)
        assert len(queries) == index + 1
        write_run_checkpoint(run_dir, queries)
        manifest["queries_completed"] = len(queries)
        write_json(run_dir / "manifest.json", manifest)
    assert len(queries) == len(questions)
    return queries


def evaluator_environment() -> dict[str, str]:
    require_api_key()
    environment = os.environ.copy()
    environment.update(
        {
            "LLM_PROVIDER": "openai",
            "LLM_API_KEY": os.environ["OPENROUTER_API_KEY"],
            "LLM_MODEL_NAME": EVALUATION_MODEL,
            "CHEAP_LLM_MODEL_NAME": EVALUATION_MODEL,
            "OPENAI_BASE_URL": OPENROUTER_BASE_URL,
        }
    )
    return environment


def evaluate(
    run_dir: Path,
    log_dir: Path,
    parallelism: int,
    question_count: int,
) -> dict[str, Any]:
    evaluator = FIXTURE_ROOT / "evaluator"
    evaluator_python = evaluator / ".venv" / "bin" / "python"
    if not evaluator_python.exists():
        raise BenchmarkError("evaluator environment is missing; run setup first")
    raw_results = run_dir / "enterprise-rag-bench-results.json"
    result = run_logged(
        [
            str(evaluator_python),
            "-m",
            "src.scripts.answer_evaluation.metrics_based_eval",
            "--answers-file",
            str(run_dir / "answers.jsonl"),
            "--questions-file",
            str(FIXTURE_ROOT / "questions.jsonl"),
            "--results-file",
            str(raw_results),
            "--parallelism",
            str(parallelism),
            "--no-correction",
        ],
        log_dir / "evaluation.log",
        timeout_seconds=EVALUATION_TIMEOUT_SECONDS,
        progress_label=f"Evaluating {question_count} answers with {parallelism} workers",
        cwd=evaluator,
        environment=evaluator_environment(),
    )
    require_success(result, "evaluating answers")
    payload = json.loads(raw_results.read_text(encoding="utf-8"))
    return {
        "duration_seconds": round(result.duration_seconds, 6),
        "aggregate_stats": payload["aggregate_stats"],
        "question_type_stats": payload["question_type_stats"],
        "raw_results": raw_results.name,
    }


def pip_freeze(run_dir: Path) -> None:
    python = FIXTURE_ROOT / "evaluator" / ".venv" / "bin" / "python"
    result = run_capture(
        [str(python), "-m", "pip", "freeze"],
        timeout_seconds=METADATA_TIMEOUT_SECONDS,
    )
    require_success(result, "recording evaluator dependencies")
    (run_dir / "evaluator-dependencies.txt").write_text(result.stdout, encoding="utf-8")


def run_benchmark(options: RunOptions) -> None:
    require_fixture()
    evaluator_environment()
    questions = load_questions(options.question_limit)
    run_dir, data_dir, manifest = create_run(options)
    composition = run_dir / "composition.toml"
    composition.write_text(composition_text(options), encoding="utf-8")
    execute_benchmark(
        run_dir,
        data_dir,
        manifest,
        composition,
        questions,
        options,
        [],
        True,
        manifest["inseam"],
    )


def resume_benchmark(run_id: str) -> None:
    require_fixture()
    evaluator_environment()
    loaded = load_resumable_run(run_id)
    (
        run_dir,
        data_dir,
        manifest,
        composition,
        questions,
        options,
        queries,
        index_required,
    ) = loaded
    write_run_checkpoint(run_dir, queries)
    execute_benchmark(
        run_dir,
        data_dir,
        manifest,
        composition,
        questions,
        options,
        queries,
        index_required,
        inseam_identity(),
    )


def require_fixture() -> None:
    if not (FIXTURE_ROOT / "setup.json").exists():
        raise BenchmarkError(f"benchmark fixture is missing; run `{sys.argv[0]} setup` first")


def execute_benchmark(
    run_dir: Path,
    data_dir: Path,
    manifest: dict[str, Any],
    composition: Path,
    questions: list[dict[str, Any]],
    options: RunOptions,
    queries: list[dict[str, Any]],
    index_required: bool,
    identity: dict[str, Any],
) -> None:
    initial_phase = "indexing" if index_required else "querying"
    started, log_dir = begin_attempt(run_dir, manifest, initial_phase, identity)
    print_run_start(run_dir, data_dir, manifest, questions, queries, index_required)
    error: BaseException | None = None
    try:
        if index_required:
            attempt = manifest["attempts"][-1]
            if attempt["attempt_number"] == 1:
                index_log = run_dir / "logs" / "index.log"
            else:
                index_log = log_dir / "index.log"
            attempt["indexing_log"] = str(index_log.relative_to(run_dir))
            write_json(run_dir / "manifest.json", manifest)
            manifest["indexing"] = index_documents(
                run_dir, data_dir, composition, index_log
            )
        else:
            print_reused_index(manifest)
        attempt = manifest["attempts"][-1]
        attempt["search_index_preparation"] = prepare_search_index(
            data_dir, composition, log_dir
        )
        write_json(run_dir / "manifest.json", manifest)
        update_run_phase(run_dir, manifest, "querying")
        queries = run_queries(
            run_dir,
            data_dir,
            composition,
            manifest,
            questions,
            options,
            queries,
            log_dir,
        )
        scores = {"retrieval": retrieval_scores(queries, questions)}
        if not options.skip_evaluation:
            update_run_phase(run_dir, manifest, "evaluating")
            scores["enterprise_rag_bench"] = evaluate(
                run_dir, log_dir, options.evaluation_parallelism, len(questions)
            )
            pip_freeze(run_dir)
        complete_run(manifest, scores)
    except BaseException as caught:
        error = caught
        raise
    finally:
        record_run_outcome(run_dir, manifest, started, error)
    print(f"Benchmark run recorded in {run_dir}")


def print_run_start(
    run_dir: Path,
    data_dir: Path,
    manifest: dict[str, Any],
    questions: list[dict[str, Any]],
    queries: list[dict[str, Any]],
    index_required: bool,
) -> None:
    verb = "Starting" if len(manifest["attempts"]) == 1 else "Resuming"
    attempt_number = len(manifest["attempts"])
    print(f"{verb} EnterpriseRAG-Bench run {manifest['run_id']}", flush=True)
    print(f"  attempt: {attempt_number}", flush=True)
    print(f"  artifacts: {run_dir}", flush=True)
    print(f"  index data: {data_dir}", flush=True)
    print(f"  questions: {len(queries)}/{len(questions)} completed", flush=True)


def load_resumable_run(
    run_id: str,
) -> tuple[
    Path,
    Path,
    dict[str, Any],
    Path,
    list[dict[str, Any]],
    RunOptions,
    list[dict[str, Any]],
    bool,
]:
    validate_run_id(run_id)
    run_dir = RUNS_ROOT / run_id
    manifest_path = run_dir / "manifest.json"
    if not manifest_path.is_file():
        raise BenchmarkError(f"benchmark run `{run_id}` does not exist")
    manifest = read_json_object(manifest_path)
    migrate_manifest(manifest)
    validate_resumable_manifest(run_id, manifest)
    options = options_from_manifest(manifest)
    questions = load_questions(options.question_limit)
    composition = run_dir / "composition.toml"
    if not composition.is_file():
        raise BenchmarkError(f"run `{run_id}` has no composition.toml")
    if composition.read_text(encoding="utf-8") != composition_text(options):
        raise BenchmarkError(f"run `{run_id}` composition does not match its options")
    data_dir = validate_index_data_path(run_id, manifest, FIXTURE_ROOT)
    index_required = manifest.get("indexing") is None
    if not index_required:
        load_index_completion(run_dir, manifest, SOURCES_MAX)
    queries = load_completed_queries(run_dir, manifest, questions)
    return (
        run_dir,
        data_dir,
        manifest,
        composition,
        questions,
        options,
        queries,
        index_required,
    )


def migrate_manifest(manifest: dict[str, Any]) -> None:
    schema_version = manifest.get("schema_version")
    if schema_version == 2:
        if type(manifest.get("attempts")) is not list:
            raise BenchmarkError("schema 2 run manifest has no attempts list")
        return
    if schema_version != 1:
        raise BenchmarkError(f"cannot resume manifest schema {schema_version!r}")
    duration = manifest.get("duration_seconds")
    if type(duration) not in {int, float}:
        raise BenchmarkError("legacy run manifest has no duration")
    manifest["attempts"] = [legacy_attempt(manifest, float(duration))]
    manifest["resume_count"] = 0
    manifest["schema_version"] = 2


def legacy_attempt(manifest: dict[str, Any], duration_seconds: float) -> dict[str, Any]:
    status = manifest.get("status")
    phase = manifest.get("phase") or status
    return {
        "attempt_number": 1,
        "resumed": False,
        "started_at": manifest.get("started_at"),
        "finished_at": manifest.get("finished_at"),
        "duration_seconds": duration_seconds,
        "starting_phase": "indexing",
        "starting_queries_completed": 0,
        "ending_queries_completed": manifest.get("queries_completed"),
        "status": status,
        "ending_phase": phase,
        "error": manifest.get("error"),
        "inseam": manifest.get("inseam"),
        "log_directory": "logs",
        "search_index_preparation": None,
    }


def validate_resumable_manifest(run_id: str, manifest: dict[str, Any]) -> None:
    validate_resumable_status(run_id, manifest)
    if manifest.get("benchmark") != benchmark_pins():
        raise BenchmarkError(f"run `{run_id}` uses different benchmark inputs")
    if manifest.get("models") != model_assignments():
        raise BenchmarkError(f"run `{run_id}` uses different models")


def options_from_manifest(manifest: dict[str, Any]) -> RunOptions:
    value = manifest_options_object(manifest, set(RunOptions.__annotations__))
    skip_evaluation = value["skip_evaluation"]
    if type(skip_evaluation) is not bool:
        raise BenchmarkError("run option skip_evaluation is not a boolean")
    return RunOptions(
        question_limit=manifest_option_integer(value, "question_limit", MAX_QUESTIONS),
        query_limit=manifest_option_integer(value, "query_limit", MAX_QUERY_RESULTS),
        turns=manifest_option_integer(value, "turns", MAX_TURNS),
        index_concurrency=manifest_option_integer(
            value, "index_concurrency", MAX_INDEX_CONCURRENCY
        ),
        llm_call_budget=manifest_option_integer(
            value, "llm_call_budget", MAX_LLM_CALL_BUDGET
        ),
        evaluation_parallelism=manifest_option_integer(
            value, "evaluation_parallelism", 64
        ),
        skip_evaluation=skip_evaluation,
    )


def load_completed_queries(
    run_dir: Path,
    manifest: dict[str, Any],
    questions: list[dict[str, Any]],
) -> list[dict[str, Any]]:
    queries = read_json_lines(run_dir / "queries.jsonl", MAX_QUESTIONS)
    if len(queries) > len(questions):
        raise BenchmarkError("query checkpoint has more records than requested questions")
    for index, query in enumerate(queries):
        expected_id = str(questions[index]["question_id"])
        if query.get("question_id") != expected_id:
            raise BenchmarkError(f"query checkpoint differs at question {index + 1}")
        if type(query.get("answer")) is not str:
            raise BenchmarkError(f"query checkpoint has no answer at question {index + 1}")
        if type(query.get("document_ids")) is not list:
            raise BenchmarkError(f"query checkpoint has no documents at question {index + 1}")
        if type(query.get("retrieved_document_ids")) is not list:
            raise BenchmarkError(f"query checkpoint has no retrievals at question {index + 1}")
    recorded_count = manifest.get("queries_completed")
    if type(recorded_count) is not int:
        raise BenchmarkError("run manifest has no completed query count")
    if recorded_count > len(queries):
        raise BenchmarkError("run manifest is ahead of its durable query checkpoint")
    manifest["queries_completed"] = len(queries)
    return queries


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser("setup", help="download and verify the dataset and evaluator")
    run_parser = subparsers.add_parser("run", help="index, query, answer, and score a new run")
    run_parser.add_argument(
        "--limit", type=bounded_argument("limit", MAX_QUESTIONS), default=500
    )
    run_parser.add_argument(
        "--query-limit", type=bounded_argument("query-limit", MAX_QUERY_RESULTS), default=8
    )
    run_parser.add_argument(
        "--turns", type=bounded_argument("turns", MAX_TURNS), default=12
    )
    run_parser.add_argument(
        "--index-concurrency",
        type=bounded_argument("index-concurrency", MAX_INDEX_CONCURRENCY),
        default=8,
    )
    run_parser.add_argument(
        "--llm-call-budget",
        type=bounded_argument("llm-call-budget", MAX_LLM_CALL_BUDGET),
        default=500,
    )
    run_parser.add_argument(
        "--evaluation-parallelism",
        type=bounded_argument("evaluation-parallelism", 64),
        default=4,
    )
    run_parser.add_argument(
        "--skip-evaluation",
        action="store_true",
        help="skip the LLM-judged answer metrics",
    )
    resume_parser = subparsers.add_parser(
        "resume",
        help="reuse a completed index and continue a failed or interrupted run",
    )
    resume_parser.add_argument("run_id", help="existing run directory name")
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    if arguments.command == "setup":
        return run_main(setup)
    if arguments.command == "run":
        options = RunOptions(
            question_limit=arguments.limit,
            query_limit=arguments.query_limit,
            turns=arguments.turns,
            index_concurrency=arguments.index_concurrency,
            llm_call_budget=arguments.llm_call_budget,
            evaluation_parallelism=arguments.evaluation_parallelism,
            skip_evaluation=arguments.skip_evaluation,
        )
        return run_main(lambda: run_benchmark(options))
    return run_main(lambda: resume_benchmark(arguments.run_id))


if __name__ == "__main__":
    raise SystemExit(main())
