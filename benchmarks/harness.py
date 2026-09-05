"""Plumbing shared by every benchmark runner in this directory.

A runner owns its dataset pins, composition, question loop, and scoring.
Everything that is the same for every benchmark lives here: bounded
external commands with heartbeats, verified downloads, run identity and
machine specifications, the `inseam index` and `inseam repair` steps, and
the attempt ledger that makes a crashed or interrupted run resumable.
Nothing here reads a module-level fixture path; the runner passes its own
roots so tests can redirect them.
"""

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
from typing import Any, Callable


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
FIXTURES_ROOT = REPOSITORY_ROOT / "benchmark" / "fixtures"
RUNS_ROOT = REPOSITORY_ROOT / "benchmarks" / "runs"
OPENROUTER_BASE_URL = "https://openrouter.ai/api/v1"
# The model policy is one policy for every benchmark (design/benchmarking.md):
# summaries ride the llm endpoint's batch lane, where the sweep parks every
# planner on its summary call and one OpenRouter batch job carries them.
SUMMARIZATION_MODEL = "google/gemini-2.5-flash-lite"
SUMMARIZATION_LANE = "batch"
EMBEDDING_MODEL = "openai/text-embedding-3-small"
EMBEDDING_DIMENSIONS = 384
MAX_INDEX_CONCURRENCY = 128
MAX_LLM_CALL_BUDGET = 1_000_000
MAX_RUN_ATTEMPTS = 100
HASH_BLOCK_BYTES = 8 * 1024 * 1024
METADATA_TIMEOUT_SECONDS = 30
DOWNLOAD_TIMEOUT_SECONDS = 7_200
INDEX_TIMEOUT_SECONDS = 259_200
PROGRESS_INTERVAL_SECONDS = 5
INDEX_PROGRESS_INTERVAL_SECONDS = 30
RUN_ID_PATTERN = re.compile(r"[0-9]{8}T[0-9]{6}Z-[0-9a-f]{12}")
RUN_PHASES = ("indexing", "querying", "evaluating")
STATUS_SOURCE_PATTERN = re.compile(
    r"^sources\s+(\d+) \((\d+) indexed\)$", re.MULTILINE
)
STATUS_SEARCH_ROWS_PATTERN = re.compile(r"^search rows\s+(\d+)$", re.MULTILINE)
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
    progress_probe: Callable[[float], str] | None = None,
    progress_interval_seconds: int = PROGRESS_INTERVAL_SECONDS,
    cwd: Path = REPOSITORY_ROOT,
    environment: dict[str, str] | None = None,
) -> CommandResult:
    assert arguments
    assert timeout_seconds > 0
    assert progress_interval_seconds > 0
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
        result = run_capture_wait(
            process,
            started,
            timeout_seconds,
            progress_label,
            progress_probe,
            progress_interval_seconds,
        )
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
    progress_probe: Callable[[float], str] | None,
    progress_interval_seconds: int,
) -> CommandResult:
    heartbeat_count = (
        timeout_seconds + progress_interval_seconds - 1
    ) // progress_interval_seconds
    assert heartbeat_count > 0
    for _heartbeat_index in range(heartbeat_count):
        elapsed_seconds = time.monotonic() - started
        remaining_seconds = timeout_seconds - elapsed_seconds
        wait_seconds = min(progress_interval_seconds, max(0.001, remaining_seconds))
        try:
            stdout, stderr = process.communicate(timeout=wait_seconds)
        except subprocess.TimeoutExpired:
            if progress_label is not None:
                elapsed_seconds = time.monotonic() - started
                detail = (
                    progress_probe(elapsed_seconds)
                    if progress_probe is not None
                    else f"{format_duration(elapsed_seconds)} elapsed"
                )
                print(f"{progress_label}: {detail}", flush=True)
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


def run_logged(
    arguments: list[str],
    log_path: Path,
    *,
    timeout_seconds: int,
    progress_label: str | None = None,
    progress_probe: Callable[[float], str] | None = None,
    progress_interval_seconds: int = PROGRESS_INTERVAL_SECONDS,
    cwd: Path = REPOSITORY_ROOT,
    environment: dict[str, str] | None = None,
) -> CommandResult:
    result = run_capture(
        arguments,
        timeout_seconds=timeout_seconds,
        progress_label=progress_label,
        progress_probe=progress_probe,
        progress_interval_seconds=progress_interval_seconds,
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


def download_verified(url: str, destination: Path, expected_sha256: str) -> Path:
    """Fetch `url` to `destination` unless a byte-identical copy is there.

    The download resumes a partial file and the checksum is checked after
    every download, so a corrupt or tampered fixture never reaches extraction.
    """
    curl = require_program("curl")
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
                url,
            ],
            timeout_seconds=DOWNLOAD_TIMEOUT_SECONDS,
        )
        require_success(result, f"downloading {destination.name}")
    actual_sha256 = sha256_file(destination)
    if actual_sha256 != expected_sha256:
        raise BenchmarkError(
            f"{destination} has SHA-256 {actual_sha256}; expected {expected_sha256}. "
            "Remove that file and rerun setup."
        )
    return destination


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


def write_json_lines(path: Path, values: list[dict[str, Any]], count_max: int) -> None:
    assert count_max > 0
    assert len(values) <= count_max
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    lines = (json.dumps(value, sort_keys=True) for value in values)
    temporary.write_text("".join(f"{line}\n" for line in lines), encoding="utf-8")
    os.replace(temporary, path)


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


def system_specs(disk_path: Path) -> dict[str, Any]:
    disk = shutil.disk_usage(disk_path)
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


def new_run_id(started_at: str) -> str:
    revision = git_value(["rev-parse", "--short=12", "HEAD"]) or "unknown"
    run_id = f"{started_at[:19].replace('-', '').replace(':', '')}Z-{revision}"
    assert RUN_ID_PATTERN.fullmatch(run_id) is not None or revision == "unknown"
    return run_id


def require_api_key() -> None:
    if not os.environ.get("OPENROUTER_API_KEY"):
        raise BenchmarkError("OPENROUTER_API_KEY must be set")


def inseam_arguments(data_dir: Path, composition: Path) -> list[str]:
    return ["inseam", "--data-dir", str(data_dir), "--composition", str(composition)]


def format_index_progress(
    status_output: str, document_count: int, elapsed_seconds: float
) -> str:
    assert document_count > 0
    sources = STATUS_SOURCE_PATTERN.search(status_output)
    search_rows = STATUS_SEARCH_ROWS_PATTERN.search(status_output)
    if sources is None or search_rows is None:
        raise BenchmarkError("`inseam status` output has no indexing counts")
    cataloged = int(sources.group(1))
    indexed = int(sources.group(2))
    if indexed > cataloged or cataloged > document_count:
        raise BenchmarkError("`inseam status` returned impossible indexing counts")
    return (
        f"{format_duration(elapsed_seconds)} elapsed · "
        f"{indexed:,} / {document_count:,} indexed · "
        f"{cataloged:,} cataloged · {int(search_rows.group(1)):,} search rows"
    )


def read_index_progress(
    data_dir: Path,
    composition: Path,
    document_count: int,
    elapsed_seconds: float,
) -> str:
    result = run_capture(
        [*inseam_arguments(data_dir, composition), "status"],
        timeout_seconds=METADATA_TIMEOUT_SECONDS,
    )
    if result.returncode != 0:
        return f"{format_duration(elapsed_seconds)} elapsed · status unavailable"
    try:
        return format_index_progress(result.stdout, document_count, elapsed_seconds)
    except BenchmarkError:
        return f"{format_duration(elapsed_seconds)} elapsed · status unavailable"


def fixture_document_count(fixture_root: Path, documents_max: int) -> int | None:
    """The document count recorded by setup, or None before extraction."""
    assert documents_max > 0
    marker = fixture_root / "documents.json"
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
    if document_count > documents_max:
        raise BenchmarkError(f"fixture metadata at {marker} exceeds the document safety limit")
    return document_count


def index_documents(
    run_dir: Path,
    data_dir: Path,
    composition: Path,
    documents: Path,
    document_count: int | None,
    sources_max: int,
    description: str,
) -> dict[str, Any]:
    """Run `inseam index` over `documents`, polling `inseam status` for progress."""
    assert sources_max > 0
    progress_label = "Indexing benchmark documents"
    progress_probe: Callable[[float], str] | None = None
    if document_count is not None:
        assert 0 < document_count <= sources_max
        progress_label = f"Indexing {document_count:,} benchmark documents"
        progress_probe = lambda elapsed_seconds: read_index_progress(
            data_dir, composition, document_count, elapsed_seconds
        )
    started_at = utc_now()
    result = run_logged(
        [*inseam_arguments(data_dir, composition), "index", str(documents)],
        run_dir / "logs" / "index.log",
        timeout_seconds=INDEX_TIMEOUT_SECONDS,
        progress_label=progress_label,
        progress_probe=progress_probe,
        progress_interval_seconds=INDEX_PROGRESS_INTERVAL_SECONDS,
    )
    finished_at = utc_now()
    require_success(result, f"indexing {description}")
    return {
        "started_at": started_at,
        "finished_at": finished_at,
        "duration_seconds": round(result.duration_seconds, 6),
        "returncode": result.returncode,
        "log": "logs/index.log",
        "summary": capture_index_summary(result.stdout, sources_max),
    }


def prepare_search_index(
    data_dir: Path, composition: Path, log_dir: Path
) -> dict[str, Any]:
    started_at = utc_now()
    result = run_logged(
        [*inseam_arguments(data_dir, composition), "repair"],
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


def capture_index_summary(output: str, sources_max: int) -> dict[str, Any]:
    try:
        return parse_index_summary(output, sources_max)
    except BenchmarkError as error:
        return {"parse_error": str(error)}


def parse_index_summary(output: str, sources_max: int) -> dict[str, int | float]:
    assert sources_max > 0
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
    if values[0] > sources_max:
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


def parse_query_results(stdout: str, results_max: int) -> list[dict[str, Any]]:
    assert results_max > 0
    payload = json.loads(stdout)
    results = payload.get("results")
    if not isinstance(results, list):
        raise BenchmarkError("`inseam query --json` did not return a results array")
    if len(results) > results_max:
        raise BenchmarkError(
            f"inseam returned {len(results)} results; hard limit is {results_max}"
        )
    return results


def query_arguments(
    data_dir: Path, composition: Path, text: str, results_limit: int
) -> list[str]:
    assert results_limit > 0
    if not text.strip():
        raise BenchmarkError("cannot run an empty query")
    if text.startswith("-"):
        raise BenchmarkError("query text starting with `-` would be read as a flag")
    return [
        *inseam_arguments(data_dir, composition),
        "query",
        text,
        "--limit",
        str(results_limit),
        "--json",
    ]


def begin_attempt(
    run_dir: Path,
    manifest: dict[str, Any],
    phase: str,
    identity: dict[str, Any],
) -> tuple[float, Path]:
    assert phase in RUN_PHASES
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


def update_run_phase(run_dir: Path, manifest: dict[str, Any], phase: str) -> None:
    assert phase in RUN_PHASES
    manifest["phase"] = phase
    write_json(run_dir / "manifest.json", manifest)


def complete_run(manifest: dict[str, Any], scores: dict[str, Any]) -> None:
    assert manifest["status"] == "running"
    manifest["scores"] = scores
    manifest["status"] = "completed"
    manifest["phase"] = "completed"


def record_run_outcome(
    run_dir: Path,
    manifest: dict[str, Any],
    started: float,
    error: BaseException | None,
) -> None:
    """Translate how an attempt ended into the manifest, then close the attempt."""
    if isinstance(error, KeyboardInterrupt):
        set_run_error(manifest, "interrupted", "interrupted by user")
    elif error is not None:
        set_run_error(manifest, "failed", str(error) or error.__class__.__name__)
    else:
        assert manifest["status"] == "completed"
    finish_attempt(run_dir, manifest, started)


def validate_resumable_status(run_id: str, manifest: dict[str, Any]) -> None:
    if manifest.get("run_id") != run_id:
        raise BenchmarkError(f"run directory and manifest ID differ for `{run_id}`")
    status = manifest.get("status")
    if status not in {"failed", "interrupted"}:
        message = f"run `{run_id}` has status {status!r}; expected failed or interrupted"
        raise BenchmarkError(message)


def validate_run_id(run_id: str) -> None:
    if RUN_ID_PATTERN.fullmatch(run_id) is None:
        raise BenchmarkError(f"invalid benchmark run ID `{run_id}`")


def validate_index_data_path(
    run_id: str, manifest: dict[str, Any], fixture_root: Path
) -> Path:
    expected = fixture_root / "nodes" / run_id
    recorded = manifest.get("index_data_path")
    if type(recorded) is not str:
        raise BenchmarkError(f"run `{run_id}` has no index data path")
    if Path(recorded).resolve() != expected.resolve():
        raise BenchmarkError(f"run `{run_id}` points at unexpected index data")
    if not expected.is_dir():
        raise BenchmarkError(f"run `{run_id}` index data is missing at {expected}")
    return expected


def load_index_completion(
    run_dir: Path, manifest: dict[str, Any], sources_max: int
) -> None:
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
        indexing["summary"] = capture_index_summary(
            log_path.read_text(encoding="utf-8"), sources_max
        )


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


def manifest_option_integer(value: dict[str, Any], name: str, maximum: int) -> int:
    option = value[name]
    if type(option) is not int:
        raise BenchmarkError(f"run option {name} is not an integer")
    if option < 1:
        raise BenchmarkError(f"run option {name} must be at least 1")
    if option > maximum:
        raise BenchmarkError(f"run option {name} exceeds the {maximum} safety limit")
    return option


def manifest_options_object(manifest: dict[str, Any], names: set[str]) -> dict[str, Any]:
    value = manifest.get("options")
    if type(value) is not dict:
        raise BenchmarkError("run manifest has no options object")
    if set(value) != names:
        raise BenchmarkError("run manifest options do not match this runner")
    return value


def positive_bounded(value: int, name: str, maximum: int) -> int:
    if value < 1:
        raise argparse.ArgumentTypeError(f"{name} must be at least 1")
    if value > maximum:
        raise argparse.ArgumentTypeError(f"{name} must be at most {maximum}")
    return value


def bounded_argument(name: str, maximum: int) -> Callable[[str], int]:
    return lambda value: positive_bounded(int(value), name, maximum)


def run_main(command: Callable[[], None]) -> int:
    """Run a benchmark command and map its outcome to a process exit code."""
    try:
        command()
    except BenchmarkError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("\ninterrupted; partial run artifacts were preserved", file=sys.stderr)
        return 130
    return 0
