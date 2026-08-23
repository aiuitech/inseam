#!/usr/bin/env python3
"""Run EnterpriseRAG-Bench against the installed inseam CLI."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
FIXTURE_ROOT = REPOSITORY_ROOT / "benchmark" / "fixtures" / "enterprise-rag-bench"
RUNS_ROOT = REPOSITORY_ROOT / "benchmarks" / "runs"
RELEASE = "v1.0.0"
UPSTREAM_REVISION = "d36685e273713975ee20299bbf1ab64165575b3c"
UPSTREAM_URL = "https://github.com/onyx-dot-app/EnterpriseRAG-Bench.git"
RELEASE_URL = f"https://github.com/onyx-dot-app/EnterpriseRAG-Bench/releases/download/{RELEASE}"
ARCHIVE_SHA256 = "9d1174928696ad08bc15f3f104739519de633c1605a4ec2034e0e3c0087bc5cd"
QUESTIONS_SHA256 = "f9524b9157cd43aae36b99333a124738804306ea6d07f332d49faa6d3d147905"
MODEL = "stealth/ox-alpha"
OPENROUTER_BASE_URL = "https://openrouter.ai/api/v1"
MAX_QUESTIONS = 1_000
MAX_TURNS = 64
MAX_QUERY_RESULTS = 25
MAX_INDEX_CONCURRENCY = 128
MAX_LLM_CALL_BUDGET = 1_000_000
HASH_BLOCK_BYTES = 8 * 1024 * 1024
METADATA_TIMEOUT_SECONDS = 30
SETUP_TIMEOUT_SECONDS = 7_200
QUERY_TIMEOUT_SECONDS = 600
AGENT_TIMEOUT_SECONDS = 1_800
INDEX_TIMEOUT_SECONDS = 259_200
EVALUATION_TIMEOUT_SECONDS = 604_800
DOCUMENT_ID_PATTERN = re.compile(r"dsid_[0-9a-f]{32}")


class BenchmarkError(RuntimeError):
    """An operating error that should stop the benchmark cleanly."""


@dataclass(frozen=True)
class CommandResult:
    returncode: int
    duration_seconds: float
    stdout: str
    stderr: str


@dataclass(frozen=True)
class RunOptions:
    question_limit: int
    query_limit: int
    turns: int
    index_concurrency: int
    llm_call_budget: int
    evaluation_parallelism: int
    skip_evaluation: bool


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def require_program(name: str) -> str:
    path = shutil.which(name)
    if path is None:
        raise BenchmarkError(f"required program `{name}` is not on PATH")
    return path


def run_capture(
    arguments: list[str],
    *,
    timeout_seconds: int,
    cwd: Path = REPOSITORY_ROOT,
    environment: dict[str, str] | None = None,
) -> CommandResult:
    assert arguments
    assert timeout_seconds > 0
    started = time.monotonic()
    try:
        completed = subprocess.run(
            arguments,
            cwd=cwd,
            env=environment,
            capture_output=True,
            text=True,
            check=False,
            timeout=timeout_seconds,
        )
    except subprocess.TimeoutExpired as error:
        duration_seconds = time.monotonic() - started
        stdout = timeout_output(error.stdout)
        stderr = timeout_output(error.stderr)
        stderr += f"\ncommand timed out after {timeout_seconds} seconds"
        return CommandResult(124, duration_seconds, stdout, stderr)
    duration_seconds = time.monotonic() - started
    assert duration_seconds >= 0.0
    return CommandResult(completed.returncode, duration_seconds, completed.stdout, completed.stderr)


def run_logged(
    arguments: list[str],
    log_path: Path,
    *,
    timeout_seconds: int,
    cwd: Path = REPOSITORY_ROOT,
    environment: dict[str, str] | None = None,
) -> CommandResult:
    result = run_capture(
        arguments,
        timeout_seconds=timeout_seconds,
        cwd=cwd,
        environment=environment,
    )
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log_path.write_text(result.stdout + result.stderr, encoding="utf-8")
    return result


def timeout_output(value: str | bytes | None) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")
    return value


def require_success(result: CommandResult, description: str) -> None:
    if result.returncode == 0:
        return
    detail = result.stderr.strip() or result.stdout.strip() or "no output"
    raise BenchmarkError(f"{description} failed with exit code {result.returncode}: {detail}")


def sha256_file(path: Path) -> str:
    size_bytes = path.stat().st_size
    block_count = (size_bytes + HASH_BLOCK_BYTES - 1) // HASH_BLOCK_BYTES
    assert block_count >= 0
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for _block_index in range(block_count):
            block = handle.read(HASH_BLOCK_BYTES)
            assert block
            digest.update(block)
        assert handle.read(1) == b""
    return digest.hexdigest()


def download_verified(name: str, expected_sha256: str) -> Path:
    curl = require_program("curl")
    destination = FIXTURE_ROOT / "downloads" / name
    destination.parent.mkdir(parents=True, exist_ok=True)
    if not destination.exists() or sha256_file(destination) != expected_sha256:
        result = run_capture(
            [
                curl,
                "--fail",
                "--location",
                "--show-error",
                "--continue-at",
                "-",
                "--retry",
                "3",
                "--output",
                str(destination),
                f"{RELEASE_URL}/{name}",
            ],
            timeout_seconds=SETUP_TIMEOUT_SECONDS,
        )
        require_success(result, f"downloading {name}")
    actual_sha256 = sha256_file(destination)
    if actual_sha256 != expected_sha256:
        raise BenchmarkError(
            f"{destination} has SHA-256 {actual_sha256}; expected {expected_sha256}. "
            "Remove that file and rerun setup."
        )
    return destination


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
        if text_file_count > 600_000:
            raise BenchmarkError(
                "archive contains more than the 600000-document safety limit"
            )
        assert path.is_file()
    if text_file_count < 500_000:
        raise BenchmarkError(
            f"extracted only {text_file_count} text documents; expected at least 500000"
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
    archive = download_verified("all_documents.zip", ARCHIVE_SHA256)
    questions = download_verified("questions.jsonl", QUESTIONS_SHA256)
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


def read_memory_bytes() -> int | None:
    system = platform.system()
    if system == "Darwin":
        result = run_capture(
            ["sysctl", "-n", "hw.memsize"],
            timeout_seconds=METADATA_TIMEOUT_SECONDS,
        )
        return int(result.stdout.strip()) if result.returncode == 0 else None
    if system == "Linux":
        meminfo = Path("/proc/meminfo")
        if not meminfo.exists():
            return None
        match = re.search(r"^MemTotal:\s+(\d+)\s+kB$", meminfo.read_text(), re.MULTILINE)
        return int(match.group(1)) * 1024 if match else None
    return None


def read_cpu_model() -> str:
    if platform.system() == "Darwin":
        result = run_capture(
            ["sysctl", "-n", "machdep.cpu.brand_string"],
            timeout_seconds=METADATA_TIMEOUT_SECONDS,
        )
        if result.returncode == 0 and result.stdout.strip():
            return result.stdout.strip()
    return platform.processor() or "unknown"


def system_specs() -> dict[str, Any]:
    disk = shutil.disk_usage(FIXTURE_ROOT)
    return {
        "os": platform.system(),
        "os_release": platform.release(),
        "os_version": platform.version(),
        "architecture": platform.machine(),
        "cpu_model": read_cpu_model(),
        "logical_cpu_count": os.cpu_count(),
        "memory_bytes": read_memory_bytes(),
        "disk_total_bytes": disk.total,
        "disk_free_bytes_at_start": disk.free,
        "hostname": platform.node(),
        "python_version": platform.python_version(),
    }


def git_value(arguments: list[str]) -> str | None:
    result = run_capture(
        ["git", *arguments], timeout_seconds=METADATA_TIMEOUT_SECONDS
    )
    if result.returncode != 0:
        return None
    return result.stdout.strip()


def inseam_identity() -> dict[str, Any]:
    binary_text = require_program("inseam")
    binary = Path(binary_text).resolve()
    version = run_capture(
        [str(binary), "--version"], timeout_seconds=METADATA_TIMEOUT_SECONDS
    )
    require_success(version, "reading the inseam version")
    dirty = git_value(["status", "--porcelain"])
    return {
        "cli_version": version.stdout.strip(),
        "binary_path": str(binary),
        "binary_sha256": sha256_file(binary),
        "repository_revision": git_value(["rev-parse", "HEAD"]),
        "repository_dirty": bool(dirty),
    }


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
transform_model = "{MODEL}"
agent_model = "{MODEL}"

[[entry]]
id = "summarizer"
[entry.config]
target_chars = 400
llm_call_budget = {options.llm_call_budget}

[[entry]]
id = "entities"
[entry.config]
max_per_source = 12
llm_call_budget = {options.llm_call_budget}

[[entry]]
id = "sweep"
[entry.config]
max_sources = 0
concurrency = {options.index_concurrency}
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


def parse_query_results(stdout: str) -> list[dict[str, Any]]:
    payload = json.loads(stdout)
    results = payload.get("results")
    if not isinstance(results, list):
        raise BenchmarkError("`inseam query --json` did not return a results array")
    if len(results) > MAX_QUERY_RESULTS:
        raise BenchmarkError(
            f"inseam returned {len(results)} results; hard limit is {MAX_QUERY_RESULTS}"
        )
    return results


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
) -> dict[str, Any]:
    question_id = str(question["question_id"])
    question_text = str(question["question"])
    base = ["inseam", "--data-dir", str(data_dir), "--composition", str(composition)]
    query = run_logged(
        [*base, "query", question_text, "--limit", str(options.query_limit), "--json"],
        log_dir / f"{question_id}-query.log",
        timeout_seconds=QUERY_TIMEOUT_SECONDS,
    )
    require_success(query, f"querying {question_id}")
    query_results = parse_query_results(query.stdout)
    agent = run_logged(
        [*base, "agent", question_text, "--model", MODEL, "--turns", str(options.turns)],
        log_dir / f"{question_id}-agent.log",
        timeout_seconds=AGENT_TIMEOUT_SECONDS,
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


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def append_json_line(path: Path, value: Any) -> None:
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(value, sort_keys=True) + "\n")


def create_run(options: RunOptions) -> tuple[Path, Path, dict[str, Any]]:
    started_at = utc_now()
    revision = git_value(["rev-parse", "--short=12", "HEAD"]) or "unknown"
    run_id = f"{started_at[:19].replace('-', '').replace(':', '')}Z-{revision}"
    run_dir = RUNS_ROOT / run_id
    run_dir.mkdir(parents=True, exist_ok=False)
    data_dir = FIXTURE_ROOT / "nodes" / run_id
    data_dir.mkdir(parents=True, exist_ok=False)
    manifest = {
        "schema_version": 1,
        "run_id": run_id,
        "status": "running",
        "started_at": started_at,
        "finished_at": None,
        "duration_seconds": None,
        "benchmark": {
            "name": "EnterpriseRAG-Bench",
            "release": RELEASE,
            "upstream_revision": UPSTREAM_REVISION,
            "archive_sha256": ARCHIVE_SHA256,
            "questions_sha256": QUESTIONS_SHA256,
        },
        "models": {
            "summarization": MODEL,
            "entity_extraction": MODEL,
            "answer_generation": MODEL,
            "answer_evaluation": MODEL,
            "embeddings": "openai/text-embedding-3-small",
        },
        "timeouts_seconds": {
            "setup_command": SETUP_TIMEOUT_SECONDS,
            "finder_query": QUERY_TIMEOUT_SECONDS,
            "agent_answer": AGENT_TIMEOUT_SECONDS,
            "indexing": INDEX_TIMEOUT_SECONDS,
            "evaluation": EVALUATION_TIMEOUT_SECONDS,
        },
        "options": vars(options),
        "system": system_specs(),
        "inseam": inseam_identity(),
        "fixture_path": str(FIXTURE_ROOT),
        "index_data_path": str(data_dir),
        "indexing": None,
        "queries_completed": 0,
        "scores": None,
    }
    write_json(run_dir / "manifest.json", manifest)
    return run_dir, data_dir, manifest


def index_documents(run_dir: Path, data_dir: Path, composition: Path) -> dict[str, Any]:
    started_at = utc_now()
    result = run_logged(
        [
            "inseam",
            "--data-dir",
            str(data_dir),
            "--composition",
            str(composition),
            "index",
            str(FIXTURE_ROOT / "documents"),
        ],
        run_dir / "logs" / "index.log",
        timeout_seconds=INDEX_TIMEOUT_SECONDS,
    )
    finished_at = utc_now()
    require_success(result, "indexing EnterpriseRAG-Bench")
    return {
        "started_at": started_at,
        "finished_at": finished_at,
        "duration_seconds": round(result.duration_seconds, 6),
        "returncode": result.returncode,
        "log": "logs/index.log",
    }


def run_queries(
    run_dir: Path,
    data_dir: Path,
    composition: Path,
    manifest: dict[str, Any],
    questions: list[dict[str, Any]],
    options: RunOptions,
) -> list[dict[str, Any]]:
    queries: list[dict[str, Any]] = []
    query_path = run_dir / "queries.jsonl"
    answers_path = run_dir / "answers.jsonl"
    for index, question in enumerate(questions):
        question_id = str(question["question_id"])
        print(f"[{index + 1}/{len(questions)}] {question_id}", flush=True)
        started_at = utc_now()
        record = question_commands(question, options, data_dir, composition, run_dir / "logs")
        record["started_at"] = started_at
        record["finished_at"] = utc_now()
        append_json_line(query_path, record)
        append_json_line(
            answers_path,
            {
                "question_id": question_id,
                "answer": record["answer"],
                "document_ids": record["document_ids"],
            },
        )
        queries.append(record)
        manifest["queries_completed"] = len(queries)
        write_json(run_dir / "manifest.json", manifest)
    assert len(queries) == len(questions)
    return queries


def evaluator_environment() -> dict[str, str]:
    api_key = os.environ.get("OPENROUTER_API_KEY")
    if not api_key:
        raise BenchmarkError("OPENROUTER_API_KEY must be set")
    environment = os.environ.copy()
    environment.update(
        {
            "LLM_PROVIDER": "openai",
            "LLM_API_KEY": api_key,
            "LLM_MODEL_NAME": MODEL,
            "CHEAP_LLM_MODEL_NAME": MODEL,
            "OPENAI_BASE_URL": OPENROUTER_BASE_URL,
        }
    )
    return environment


def evaluate(run_dir: Path, parallelism: int) -> dict[str, Any]:
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
        run_dir / "logs" / "evaluation.log",
        timeout_seconds=EVALUATION_TIMEOUT_SECONDS,
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
    if not (FIXTURE_ROOT / "setup.json").exists():
        raise BenchmarkError(f"benchmark fixture is missing; run `{sys.argv[0]} setup` first")
    evaluator_environment()
    questions = load_questions(options.question_limit)
    run_dir, data_dir, manifest = create_run(options)
    composition = run_dir / "composition.toml"
    composition.write_text(composition_text(options), encoding="utf-8")
    started = time.monotonic()
    try:
        manifest["indexing"] = index_documents(run_dir, data_dir, composition)
        write_json(run_dir / "manifest.json", manifest)
        queries = run_queries(run_dir, data_dir, composition, manifest, questions, options)
        scores = {"retrieval": retrieval_scores(queries, questions)}
        if not options.skip_evaluation:
            scores["enterprise_rag_bench"] = evaluate(run_dir, options.evaluation_parallelism)
            pip_freeze(run_dir)
        manifest["scores"] = scores
        manifest["status"] = "completed"
    except BaseException as error:
        manifest["status"] = "failed"
        manifest["error"] = str(error)
        raise
    finally:
        manifest["finished_at"] = utc_now()
        manifest["duration_seconds"] = round(time.monotonic() - started, 6)
        write_json(run_dir / "manifest.json", manifest)
    print(f"Benchmark run recorded in {run_dir}")


def positive_bounded(value: int, name: str, maximum: int) -> int:
    if value < 1:
        raise argparse.ArgumentTypeError(f"{name} must be at least 1")
    if value > maximum:
        raise argparse.ArgumentTypeError(f"{name} must be at most {maximum}")
    return value


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser("setup", help="download and verify the dataset and evaluator")
    run_parser = subparsers.add_parser("run", help="index, query, answer, and score a new run")
    run_parser.add_argument(
        "--limit",
        type=lambda value: positive_bounded(int(value), "limit", MAX_QUESTIONS),
        default=500,
    )
    run_parser.add_argument(
        "--query-limit",
        type=lambda value: positive_bounded(
            int(value), "query-limit", MAX_QUERY_RESULTS
        ),
        default=8,
    )
    run_parser.add_argument(
        "--turns",
        type=lambda value: positive_bounded(int(value), "turns", MAX_TURNS),
        default=12,
    )
    run_parser.add_argument(
        "--index-concurrency",
        type=lambda value: positive_bounded(
            int(value), "index-concurrency", MAX_INDEX_CONCURRENCY
        ),
        default=8,
    )
    run_parser.add_argument(
        "--llm-call-budget",
        type=lambda value: positive_bounded(
            int(value), "llm-call-budget", MAX_LLM_CALL_BUDGET
        ),
        default=500,
    )
    run_parser.add_argument(
        "--evaluation-parallelism",
        type=lambda value: positive_bounded(int(value), "evaluation-parallelism", 64),
        default=4,
    )
    run_parser.add_argument(
        "--skip-evaluation",
        action="store_true",
        help="skip the LLM-judged answer metrics",
    )
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        if arguments.command == "setup":
            setup()
        else:
            options = RunOptions(
                question_limit=arguments.limit,
                query_limit=arguments.query_limit,
                turns=arguments.turns,
                index_concurrency=arguments.index_concurrency,
                llm_call_budget=arguments.llm_call_budget,
                evaluation_parallelism=arguments.evaluation_parallelism,
                skip_evaluation=arguments.skip_evaluation,
            )
            run_benchmark(options)
    except BenchmarkError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
