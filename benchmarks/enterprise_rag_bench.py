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
MAX_RUN_ATTEMPTS = 100
HASH_BLOCK_BYTES = 8 * 1024 * 1024
METADATA_TIMEOUT_SECONDS = 30
SETUP_TIMEOUT_SECONDS = 7_200
QUERY_TIMEOUT_SECONDS = 600
AGENT_TIMEOUT_SECONDS = 1_800
INDEX_TIMEOUT_SECONDS = 259_200
EVALUATION_TIMEOUT_SECONDS = 604_800
PROGRESS_INTERVAL_SECONDS = 5
DOCUMENT_ID_PATTERN = re.compile(r"dsid_[0-9a-f]{32}")
RUN_ID_PATTERN = re.compile(r"[0-9]{8}T[0-9]{6}Z-[0-9a-f]{12}")
INDEX_SOURCE_PATTERN = re.compile(
    r"(\d+) sources seen: (\d+) indexed, (\d+) unchanged, (\d+) catalog-only, "
    r"(\d+) past cutoff, (\d+) ignored"
)
INDEX_FRAGMENT_PATTERN = re.compile(
    r"(\d+) fragments, (\d+) relations, (\d+) keyed fragments anchored"
)
INDEX_SUMMARY_PATTERN = re.compile(
    r"summaries: (\d+) llm, (\d+) extractive, (\d+) envelope .*? "
    r"(\d+) embedded .*? \$([0-9]+(?:\.[0-9]+)?) spent"
)


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
    progress_label: str | None = None,
    cwd: Path = REPOSITORY_ROOT,
    environment: dict[str, str] | None = None,
) -> CommandResult:
    assert arguments
    assert timeout_seconds > 0
    started = time.monotonic()
    if progress_label is not None:
        print(f"{progress_label}...", flush=True)
    process = subprocess.Popen(
        arguments,
        cwd=cwd,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        result = run_capture_wait(process, started, timeout_seconds, progress_label)
    except KeyboardInterrupt:
        process.terminate()
        run_capture_reap(process)
        raise
    if progress_label is not None:
        outcome = "done" if result.returncode == 0 else "failed"
        duration = format_duration(result.duration_seconds)
        print(f"{progress_label}: {outcome} in {duration}", flush=True)
    return result


def run_capture_wait(
    process: subprocess.Popen[str],
    started: float,
    timeout_seconds: int,
    progress_label: str | None,
) -> CommandResult:
    heartbeat_count = (timeout_seconds + PROGRESS_INTERVAL_SECONDS - 1) // PROGRESS_INTERVAL_SECONDS
    assert heartbeat_count > 0
    for _heartbeat_index in range(heartbeat_count):
        elapsed_seconds = time.monotonic() - started
        remaining_seconds = timeout_seconds - elapsed_seconds
        wait_seconds = min(PROGRESS_INTERVAL_SECONDS, max(0.001, remaining_seconds))
        try:
            stdout, stderr = process.communicate(timeout=wait_seconds)
        except subprocess.TimeoutExpired:
            if progress_label is not None:
                elapsed_seconds = time.monotonic() - started
                print(f"{progress_label}: {format_duration(elapsed_seconds)} elapsed", flush=True)
        else:
            duration_seconds = time.monotonic() - started
            assert duration_seconds >= 0.0
            assert process.returncode is not None
            return CommandResult(process.returncode, duration_seconds, stdout, stderr)
    process.kill()
    stdout, stderr = process.communicate()
    duration_seconds = time.monotonic() - started
    assert duration_seconds >= 0.0
    stderr += f"\ncommand timed out after {timeout_seconds} seconds"
    return CommandResult(124, duration_seconds, stdout, stderr)


def run_capture_reap(process: subprocess.Popen[str]) -> None:
    try:
        process.communicate(timeout=PROGRESS_INTERVAL_SECONDS)
    except subprocess.TimeoutExpired:
        process.kill()
        process.communicate()


def format_duration(duration_seconds: float) -> str:
    assert duration_seconds >= 0.0
    seconds = int(duration_seconds)
    if seconds < 60:
        return f"{seconds}s"
    minutes, seconds = divmod(seconds, 60)
    if minutes < 60:
        return f"{minutes}m {seconds:02d}s"
    hours, minutes = divmod(minutes, 60)
    return f"{hours}h {minutes:02d}m"


def run_logged(
    arguments: list[str],
    log_path: Path,
    *,
    timeout_seconds: int,
    progress_label: str | None = None,
    cwd: Path = REPOSITORY_ROOT,
    environment: dict[str, str] | None = None,
) -> CommandResult:
    result = run_capture(
        arguments,
        timeout_seconds=timeout_seconds,
        progress_label=progress_label,
        cwd=cwd,
        environment=environment,
    )
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log_path.write_text(result.stdout + result.stderr, encoding="utf-8")
    return result


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
    progress_prefix: str,
) -> dict[str, Any]:
    question_id = str(question["question_id"])
    question_text = str(question["question"])
    base = ["inseam", "--data-dir", str(data_dir), "--composition", str(composition)]
    query = run_logged(
        [*base, "query", question_text, "--limit", str(options.query_limit), "--json"],
        log_dir / f"{question_id}-query.log",
        timeout_seconds=QUERY_TIMEOUT_SECONDS,
        progress_label=f"{progress_prefix} retrieval",
    )
    require_success(query, f"querying {question_id}")
    query_results = parse_query_results(query.stdout)
    agent = run_logged(
        [*base, "agent", question_text, "--model", MODEL, "--turns", str(options.turns)],
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


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def read_json_object(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise BenchmarkError(f"could not read JSON at {path}: {error}") from error
    if type(value) is not dict:
        raise BenchmarkError(f"expected a JSON object at {path}")
    return value


def read_json_lines(path: Path, count_max: int) -> list[dict[str, Any]]:
    assert count_max > 0
    if not path.exists():
        return []
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise BenchmarkError(f"could not read JSON lines at {path}: {error}") from error
    records: list[dict[str, Any]] = []
    source_lines = text.splitlines()
    for line_number, line in enumerate(source_lines, start=1):
        if len(records) >= count_max:
            raise BenchmarkError(f"{path} exceeds the {count_max}-record safety limit")
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            if line_number == len(source_lines):
                if not text.endswith("\n"):
                    break
            message = f"invalid JSON on line {line_number} of {path}: {error}"
            raise BenchmarkError(message) from error
        if type(value) is not dict:
            raise BenchmarkError(f"expected an object on line {line_number} of {path}")
        records.append(value)
    return records


def write_json_lines(path: Path, values: list[dict[str, Any]]) -> None:
    assert len(values) <= MAX_QUESTIONS
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    lines = (json.dumps(value, sort_keys=True) for value in values)
    temporary.write_text("".join(f"{line}\n" for line in lines), encoding="utf-8")
    os.replace(temporary, path)


def write_run_checkpoint(run_dir: Path, queries: list[dict[str, Any]]) -> None:
    write_json_lines(run_dir / "queries.jsonl", queries)
    answers = [
        {
            "question_id": query["question_id"],
            "answer": query["answer"],
            "document_ids": query["document_ids"],
        }
        for query in queries
    ]
    assert len(answers) == len(queries)
    write_json_lines(run_dir / "answers.jsonl", answers)


def create_run(options: RunOptions) -> tuple[Path, Path, dict[str, Any]]:
    started_at = utc_now()
    revision = git_value(["rev-parse", "--short=12", "HEAD"]) or "unknown"
    run_id = f"{started_at[:19].replace('-', '').replace(':', '')}Z-{revision}"
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
        "progress_interval_seconds": PROGRESS_INTERVAL_SECONDS,
        "options": vars(options),
        "system": system_specs(),
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


def index_documents(run_dir: Path, data_dir: Path, composition: Path) -> dict[str, Any]:
    document_count = fixture_document_count()
    progress_label = "Indexing benchmark documents"
    if document_count is not None:
        progress_label = f"Indexing {document_count:,} benchmark documents"
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
        progress_label=progress_label,
    )
    finished_at = utc_now()
    require_success(result, "indexing EnterpriseRAG-Bench")
    return {
        "started_at": started_at,
        "finished_at": finished_at,
        "duration_seconds": round(result.duration_seconds, 6),
        "returncode": result.returncode,
        "log": "logs/index.log",
        "summary": capture_index_summary(result.stdout),
    }


def prepare_search_index(
    data_dir: Path, composition: Path, log_dir: Path
) -> dict[str, Any]:
    started_at = utc_now()
    result = run_logged(
        [
            "inseam",
            "--data-dir",
            str(data_dir),
            "--composition",
            str(composition),
            "repair",
        ],
        log_dir / "search-index.log",
        timeout_seconds=INDEX_TIMEOUT_SECONDS,
        progress_label="Preparing libSQL vector search index",
    )
    require_success(result, "repairing the vector search index")
    return {
        "started_at": started_at,
        "finished_at": utc_now(),
        "duration_seconds": round(result.duration_seconds, 6),
        "returncode": result.returncode,
        "log": str((log_dir / "search-index.log").relative_to(log_dir.parents[1])),
    }


def capture_index_summary(output: str) -> dict[str, Any]:
    try:
        return parse_index_summary(output)
    except BenchmarkError as error:
        return {"parse_error": str(error)}


def parse_index_summary(output: str) -> dict[str, int | float]:
    sources = INDEX_SOURCE_PATTERN.search(output)
    fragments = INDEX_FRAGMENT_PATTERN.search(output)
    summaries = INDEX_SUMMARY_PATTERN.search(output)
    if sources is None:
        raise BenchmarkError("index output has no source completion summary")
    if fragments is None:
        raise BenchmarkError("index output has no fragment completion summary")
    if summaries is None:
        raise BenchmarkError("index output has no transform completion summary")
    values = [int(value) for value in (*sources.groups(), *fragments.groups())]
    transform_values = [int(value) for value in summaries.groups()[:4]]
    if values[0] < 1:
        raise BenchmarkError("index completion summary reports no sources")
    if values[0] > 600_001:
        raise BenchmarkError("index completion summary exceeds the source safety limit")
    return {
        "sources_seen": values[0],
        "sources_indexed": values[1],
        "sources_unchanged": values[2],
        "sources_catalog_only": values[3],
        "sources_past_cutoff": values[4],
        "sources_ignored": values[5],
        "fragments": values[6],
        "relations": values[7],
        "keyed_fragments": values[8],
        "summaries_llm": transform_values[0],
        "summaries_extractive": transform_values[1],
        "summaries_envelope": transform_values[2],
        "embeddings": transform_values[3],
        "cost_usd": float(summaries.group(5)),
    }


def fixture_document_count() -> int | None:
    marker = FIXTURE_ROOT / "documents.json"
    if not marker.exists():
        return None
    try:
        payload = json.loads(marker.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise BenchmarkError(f"could not read fixture metadata at {marker}: {error}") from error
    document_count = payload.get("text_file_count")
    if type(document_count) is not int:
        raise BenchmarkError(f"fixture metadata at {marker} has no integer text_file_count")
    if document_count < 1:
        raise BenchmarkError(f"fixture metadata at {marker} has an invalid text_file_count")
    if document_count > 600_000:
        raise BenchmarkError(f"fixture metadata at {marker} exceeds the document safety limit")
    return document_count


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
    run_dir, data_dir, manifest, composition, questions, options, queries = loaded
    write_run_checkpoint(run_dir, queries)
    execute_benchmark(
        run_dir,
        data_dir,
        manifest,
        composition,
        questions,
        options,
        queries,
        False,
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
    try:
        if index_required:
            manifest["indexing"] = index_documents(run_dir, data_dir, composition)
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
        manifest["scores"] = scores
        manifest["status"] = "completed"
        manifest["phase"] = "completed"
    except KeyboardInterrupt:
        set_run_error(manifest, "interrupted", "interrupted by user")
        raise
    except Exception as error:
        set_run_error(manifest, "failed", str(error) or error.__class__.__name__)
        raise
    finally:
        finish_attempt(run_dir, manifest, started)
    print(f"Benchmark run recorded in {run_dir}")


def begin_attempt(
    run_dir: Path,
    manifest: dict[str, Any],
    phase: str,
    identity: dict[str, Any],
) -> tuple[float, Path]:
    attempts = manifest["attempts"]
    assert type(attempts) is list
    if len(attempts) >= MAX_RUN_ATTEMPTS:
        raise BenchmarkError(f"run exceeds the {MAX_RUN_ATTEMPTS}-attempt safety limit")
    attempt_number = len(attempts) + 1
    log_relative = f"logs/attempt-{attempt_number:03d}"
    attempts.append(
        {
            "attempt_number": attempt_number,
            "resumed": attempt_number > 1,
            "started_at": utc_now(),
            "finished_at": None,
            "duration_seconds": None,
            "starting_phase": phase,
            "starting_queries_completed": manifest["queries_completed"],
            "ending_queries_completed": None,
            "status": "running",
            "error": None,
            "inseam": identity,
            "log_directory": log_relative,
            "search_index_preparation": None,
        }
    )
    manifest["status"] = "running"
    manifest["phase"] = phase
    manifest["finished_at"] = None
    manifest.pop("error", None)
    write_json(run_dir / "manifest.json", manifest)
    return time.monotonic(), run_dir / log_relative


def finish_attempt(run_dir: Path, manifest: dict[str, Any], started: float) -> None:
    duration_seconds = round(time.monotonic() - started, 6)
    assert duration_seconds >= 0.0
    attempt = manifest["attempts"][-1]
    attempt["finished_at"] = utc_now()
    attempt["duration_seconds"] = duration_seconds
    attempt["ending_queries_completed"] = manifest["queries_completed"]
    attempt["status"] = manifest["status"]
    attempt["ending_phase"] = manifest["phase"]
    attempt["error"] = manifest.get("error")
    durations = [value["duration_seconds"] for value in manifest["attempts"]]
    assert all(type(value) in {int, float} for value in durations)
    manifest["duration_seconds"] = round(sum(durations), 6)
    manifest["finished_at"] = attempt["finished_at"]
    manifest["resume_count"] = len(manifest["attempts"]) - 1
    write_json(run_dir / "manifest.json", manifest)


def set_run_error(manifest: dict[str, Any], status: str, error: str) -> None:
    assert status in {"failed", "interrupted"}
    assert error
    manifest["status"] = status
    manifest["phase"] = status
    manifest["error"] = error


def print_run_start(
    run_dir: Path,
    data_dir: Path,
    manifest: dict[str, Any],
    questions: list[dict[str, Any]],
    queries: list[dict[str, Any]],
    index_required: bool,
) -> None:
    verb = "Starting" if index_required else "Resuming"
    attempt_number = len(manifest["attempts"])
    print(f"{verb} EnterpriseRAG-Bench run {manifest['run_id']}", flush=True)
    print(f"  attempt: {attempt_number}", flush=True)
    print(f"  artifacts: {run_dir}", flush=True)
    print(f"  index data: {data_dir}", flush=True)
    print(f"  questions: {len(queries)}/{len(questions)} completed", flush=True)


def print_reused_index(manifest: dict[str, Any]) -> None:
    indexing = manifest["indexing"]
    summary = indexing["summary"]
    duration = format_duration(float(indexing["duration_seconds"]))
    sources = summary.get("sources_seen")
    fragments = summary.get("fragments")
    if type(sources) is int:
        if type(fragments) is int:
            message = f"{sources:,} sources, {fragments:,} fragments, "
        else:
            message = f"{sources:,} sources, "
    else:
        message = "completion recorded, "
    print(f"Reusing completed index: {message}original indexing time {duration}", flush=True)


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
]:
    if RUN_ID_PATTERN.fullmatch(run_id) is None:
        raise BenchmarkError(f"invalid benchmark run ID `{run_id}`")
    run_dir = RUNS_ROOT / run_id
    manifest_path = run_dir / "manifest.json"
    if not manifest_path.is_file():
        raise BenchmarkError(f"benchmark run `{run_id}` does not exist")
    manifest = read_json_object(manifest_path)
    migrate_manifest(manifest)
    validate_resumable_manifest(run_id, manifest)
    options = options_from_manifest(manifest.get("options"))
    questions = load_questions(options.question_limit)
    composition = run_dir / "composition.toml"
    if not composition.is_file():
        raise BenchmarkError(f"run `{run_id}` has no composition.toml")
    if composition.read_text(encoding="utf-8") != composition_text(options):
        raise BenchmarkError(f"run `{run_id}` composition does not match its options")
    data_dir = validate_index_data_path(run_id, manifest)
    load_index_completion(run_dir, manifest)
    queries = load_completed_queries(run_dir, manifest, questions)
    return run_dir, data_dir, manifest, composition, questions, options, queries


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
    if manifest.get("run_id") != run_id:
        raise BenchmarkError(f"run directory and manifest ID differ for `{run_id}`")
    status = manifest.get("status")
    if status not in {"failed", "interrupted"}:
        message = f"run `{run_id}` has status {status!r}; expected failed or interrupted"
        raise BenchmarkError(message)
    benchmark = manifest.get("benchmark")
    expected_benchmark = {
        "name": "EnterpriseRAG-Bench",
        "release": RELEASE,
        "upstream_revision": UPSTREAM_REVISION,
        "archive_sha256": ARCHIVE_SHA256,
        "questions_sha256": QUESTIONS_SHA256,
    }
    if benchmark != expected_benchmark:
        raise BenchmarkError(f"run `{run_id}` uses different benchmark inputs")
    expected_models = {
        "summarization": MODEL,
        "entity_extraction": MODEL,
        "answer_generation": MODEL,
        "answer_evaluation": MODEL,
        "embeddings": "openai/text-embedding-3-small",
    }
    if manifest.get("models") != expected_models:
        raise BenchmarkError(f"run `{run_id}` uses different models")


def options_from_manifest(value: Any) -> RunOptions:
    if type(value) is not dict:
        raise BenchmarkError("run manifest has no options object")
    expected = set(RunOptions.__annotations__)
    if set(value) != expected:
        raise BenchmarkError("run manifest options do not match this runner")
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


def manifest_option_integer(value: dict[str, Any], name: str, maximum: int) -> int:
    option = value[name]
    if type(option) is not int:
        raise BenchmarkError(f"run option {name} is not an integer")
    if option < 1:
        raise BenchmarkError(f"run option {name} must be at least 1")
    if option > maximum:
        raise BenchmarkError(f"run option {name} exceeds the {maximum} safety limit")
    return option


def validate_index_data_path(run_id: str, manifest: dict[str, Any]) -> Path:
    expected = FIXTURE_ROOT / "nodes" / run_id
    recorded = manifest.get("index_data_path")
    if type(recorded) is not str:
        raise BenchmarkError(f"run `{run_id}` has no index data path")
    if Path(recorded).resolve() != expected.resolve():
        raise BenchmarkError(f"run `{run_id}` points at unexpected index data")
    if not expected.is_dir():
        raise BenchmarkError(f"run `{run_id}` index data is missing at {expected}")
    return expected


def load_index_completion(run_dir: Path, manifest: dict[str, Any]) -> None:
    indexing = manifest.get("indexing")
    if type(indexing) is not dict:
        raise BenchmarkError(f"run `{manifest['run_id']}` has no completed index record")
    if indexing.get("returncode") != 0:
        raise BenchmarkError(f"run `{manifest['run_id']}` indexing did not complete")
    duration = indexing.get("duration_seconds")
    if type(duration) not in {int, float}:
        raise BenchmarkError("completed index record has no duration")
    if duration <= 0:
        raise BenchmarkError("completed index record has a non-positive duration")
    log_path = run_dir / "logs" / "index.log"
    if not log_path.is_file():
        raise BenchmarkError(f"completed index log is missing at {log_path}")
    summary = indexing.get("summary")
    if type(summary) is not dict:
        indexing["summary"] = capture_index_summary(log_path.read_text(encoding="utf-8"))


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


def update_run_phase(run_dir: Path, manifest: dict[str, Any], phase: str) -> None:
    assert phase in {"indexing", "querying", "evaluating"}
    manifest["phase"] = phase
    write_json(run_dir / "manifest.json", manifest)


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
    resume_parser = subparsers.add_parser(
        "resume",
        help="reuse a completed index and continue a failed or interrupted run",
    )
    resume_parser.add_argument("run_id", help="existing run directory name")
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        if arguments.command == "setup":
            setup()
        elif arguments.command == "run":
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
        else:
            resume_benchmark(arguments.run_id)
    except BenchmarkError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("\ninterrupted; partial run artifacts were preserved", file=sys.stderr)
        return 130
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
