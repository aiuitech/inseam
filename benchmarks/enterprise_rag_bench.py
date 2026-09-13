#!/usr/bin/env python3
"""Run EnterpriseRAG-Bench against the installed inseam CLI."""

from __future__ import annotations

import argparse
import json
import os
import random
import re
import shutil
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from harness import (
    EMBEDDING_DIMENSIONS,
    EMBEDDING_MODEL,
    FINDER_SEEDS,
    FIXTURES_ROOT,
    INDEX_PROGRESS_INTERVAL_SECONDS,
    INDEX_TIMEOUT_SECONDS,
    MAX_INDEX_CONCURRENCY,
    MAX_KEYWORDS,
    MAX_LLM_CALL_BUDGET,
    MAX_SUMMARY_TARGET_CHARS,
    METADATA_TIMEOUT_SECONDS,
    OPENROUTER_BASE_URL,
    PROGRESS_INTERVAL_SECONDS,
    RUNS_ROOT as ALL_RUNS_ROOT,
    STRUCTURAL_CHOICES,
    SUMMARIZATION_LANE,
    SUMMARIZATION_MODEL,
    VECTOR_SCOPES,
    BenchmarkError,
    CommandResult,
    begin_attempt,
    bounded_argument,
    complete_run,
    count_argument,
    distance_argument,
    download_verified,
    finder_override_argument,
    finish_attempt,
    fixture_document_count,
    inseam_arguments,
    inseam_identity,
    load_index_completion,
    manifest_option_boolean,
    manifest_option_choice,
    manifest_option_count,
    manifest_option_distance,
    manifest_option_finder_overrides,
    manifest_option_integer,
    manifest_options_object,
    new_run_id,
    parse_query_results,
    probability_argument,
    manifest_option_probability,
    manifest_option_weight,
    weight_argument,
    prepare_search_index,
    print_reused_index,
    query_arguments,
    read_json_lines,
    read_json_object,
    record_run_outcome,
    record_final_index_footprint,
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
    write_retrieval_observability,
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
# The leaderboard fixes no answer model: its two baselines answer with
# GPT-5.4 and every product submission brings its own. `stealth/ox-alpha`,
# the first choice here, left OpenRouter in September 2026; the default is
# now a cheap open model, and both models are run options recorded in the
# manifest so a run says what answered and what judged.
ANSWER_MODEL = "z-ai/glm-5.3-flash"
EVALUATION_MODEL = "z-ai/glm-5.3-flash"
MODEL_ID_MAX_CHARS = 128
# OpenRouter rejects batch jobs above 5,000 requests; the endpoint plugin
# refuses larger values at boot. Parking 65,536 planners keeps several
# jobs filling at once.
SUMMARY_BATCH_REQUESTS_MAX = 5_000
SUMMARY_BATCH_CONCURRENCY = 65_536
MAX_QUESTIONS = 1_000
MAX_TURNS = 64
MAX_QUERY_RESULTS = 25
# Attribution (design/vocabulary.md, what the harness reads) asks the Finder
# for a deeper list than the scored one, so a gold document that missed the
# top `query_limit` still reports its rank and ledger. `inseam query` clamps
# at 50. Scoring keeps the first `query_limit` results, so recall and MRR
# mean what they always meant.
ATTRIBUTION_LIMIT = 50
assert ATTRIBUTION_LIMIT >= MAX_QUERY_RESULTS
# Every question expects at most ten gold documents, so the attribution file
# holds at most this many rows.
ATTRIBUTION_ROWS_MAX = MAX_QUESTIONS * 10
ATTRIBUTION_TOP_ROWS = 25
# The seed channels a ledger decomposes a score into, and the row kinds walk
# mass arrives through. Order matters: a tie for the largest contributor is
# broken by the first channel or kind listed.
CHANNELS = ("prose", "lexical", "vector", "exact", "cluster")
ROW_KINDS = ("prose", "summary", "entry", "term", "identifier", "entity", "alias", "facet", "other")
# Bounds on what one query's `meta` may carry before the harness refuses it:
# fragments per evidence entry match the sweep's `max_fragments_per_source`;
# ledger rows and excluded hubs are far past what a query reports.
EVIDENCE_FRAGMENTS_MAX = 400
LEDGER_ROWS_MAX = 1_000
HUBS_EXCLUDED_MAX = 1_000
# Distinct vocabulary rows one run's ledgers may name in total.
ROWS_TRACKED_MAX = 1_000_000
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
# A corpus slice is every document any question expects plus a seeded random
# sample of the rest, hard-linked under `slices/<count>/documents`, so a
# retrieval strategy can be compared in minutes instead of the hours a full
# index takes. Its scores are development numbers: the distractor set is a
# fraction of the corpus, so they are comparable only with other slices of
# the same size, never with a full run. The seed is fixed so every slice of
# a size holds the same documents.
CORPUS_SLICE_SEED = 0
CORPUS_SLICE_MAX = DOCUMENTS_MAX
# The embedder's vector scopes, plus `none`: no embedder mounted at all.
EMBEDDER_CHOICES = VECTOR_SCOPES | frozenset({"none"})
# Past the longest document in the release (22,060 characters), so every
# document is its own summary and no model is asked to shorten one.
SUMMARY_TARGET_CHARS_WHOLE_DOCUMENT = 24_000


@dataclass(frozen=True)
class RunOptions:
    question_limit: int
    query_limit: int
    turns: int
    index_concurrency: int
    llm_call_budget: int
    evaluation_parallelism: int
    skip_evaluation: bool
    # Retrieval only: no agent answer, no judge. The retrieval block is the
    # whole score, which is what a change to indexing is measured by.
    skip_agent: bool = False
    # 0 indexes the full corpus; a count indexes a slice of that many
    # documents (CORPUS_SLICE_SEED).
    corpus_slice: int = 0
    # The summarizer's target length; text within it is its own summary and
    # costs no model call. The default is past the longest document, so each
    # document is its own summary: one full-text row holding the whole text,
    # which scored best on the corpus (design/benchmarking.md).
    summary_target_chars: int = SUMMARY_TARGET_CHARS_WHOLE_DOCUMENT
    # Keywords planted beside each summary for full-text search; 0 plants none.
    keywords_max: int = 12
    # Whether the markdown structural transform (which claims plain text too)
    # runs, so each source's text reaches full-text search through its
    # sections. `off` leaves the summary and keywords as the only text rows.
    structural: str = "off"
    # The embedder's scope: `summaries` is one vector per source; `none`
    # mounts no embedder, so the index is full-text only and costs nothing.
    # The default is none: on this corpus vector seeds lowered the fused
    # ranking below full-text alone (design/benchmarking.md).
    embedding_vectors: str = "none"
    # Vector seeds farther than this cosine distance are dropped before
    # fusion, so a weak vector list cannot drag a strong full-text one.
    finder_max_vector_distance: float = 0.75
    # The model that answers questions through `inseam agent`, and the one
    # the upstream evaluator judges with.
    answer_model: str = ANSWER_MODEL
    evaluation_model: str = EVALUATION_MODEL
    # Calls the hints transform may make per run; 0 leaves it unmounted.
    # One call per document plants its cues, glossary terms, identifiers,
    # discriminators, and entities (design/indexing.md).
    hints_llm_call_budget: int = 0
    # Which seed lists the Finder runs before fusion; one alone is a
    # diagnostic for which search the fusion is carrying.
    finder_seeds: str = "both"
    finder_seed_k: int = 60
    finder_rrf_k: int = 60
    finder_damping: float = 0.5
    finder_lexical_weight: float = 1.0
    directory_summary_target_chars: int = 0
    # Query-time Finder overrides (`inseam query --finder KEY=VALUE`), one
    # per entry. They never enter the composition: a sweep over one index is
    # a matrix of runs that differ by exactly this list.
    finder_overrides: list[str] = field(default_factory=list)
    # Whether every retrieval query asks for its score ledger (`--explain`),
    # which attribution reads. Off for runs recorded before the ledger
    # existed; the CLI turns it on by default.
    explain: bool = False

    def __post_init__(self) -> None:
        if self.skip_agent and not self.skip_evaluation:
            raise BenchmarkError("a retrieval-only run has no answers to evaluate")
        if len(self.finder_overrides) > harness.FINDER_OVERRIDES_MAX:
            raise BenchmarkError(f"more than {harness.FINDER_OVERRIDES_MAX} finder overrides")


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
machine_id = "enterprise-rag-bench"
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
agent_model = "{options.answer_model}"
batch_requests_max = {SUMMARY_BATCH_REQUESTS_MAX}

[[entry]]
id = "embedder"
[entry.config]
{embedder_config(options)}

[[entry]]
id = "markdown"
disabled = {"false" if options.structural == "markdown" else "true"}

[[entry]]
id = "directory"
disabled = false

[[entry]]
id = "chunker"
disabled = true

[[entry]]
id = "finder"
[entry.config]
seeds = "{options.finder_seeds}"
seed_k = {options.finder_seed_k}
rrf_k = {options.finder_rrf_k}
damping = {options.finder_damping}
lexical_weight = {options.finder_lexical_weight}
max_vector_distance = {options.finder_max_vector_distance}

[[entry]]
id = "summarizer"
[entry.config]
target_chars = {options.summary_target_chars}
{f"directory_target_chars = {options.directory_summary_target_chars}" if options.directory_summary_target_chars else ""}
keywords_max = {options.keywords_max}
llm_call_budget = {options.llm_call_budget}
llm_lane = "{SUMMARIZATION_LANE}"

[[entry]]
id = "entities"
disabled = true

[[entry]]
id = "hints"
disabled = {"false" if options.hints_llm_call_budget > 0 else "true"}
[entry.config]
llm_call_budget = {options.hints_llm_call_budget}
llm_lane = "{SUMMARIZATION_LANE}"

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


def embedder_config(options: RunOptions) -> str:
    if options.embedding_vectors == "none":
        return 'provider = "none"'
    return (
        f'provider = "endpoint"\nmodel = "{EMBEDDING_MODEL}"\n'
        f"dimensions = {EMBEDDING_DIMENSIONS}\n"
        f'vectors = "{options.embedding_vectors}"'
    )


def corpus_root(options: RunOptions) -> Path:
    """The fixture root whose `documents` directory a run indexes."""
    if options.corpus_slice == 0:
        return FIXTURE_ROOT
    return FIXTURE_ROOT / "slices" / str(options.corpus_slice)


def corpus_document_paths() -> list[Path]:
    documents = FIXTURE_ROOT / "documents"
    paths: list[Path] = []
    for path in sorted(documents.rglob("*.txt")):
        paths.append(path)
        if len(paths) > DOCUMENTS_MAX:
            raise BenchmarkError(f"corpus has more than {DOCUMENTS_MAX} documents")
    if len(paths) < DOCUMENTS_MIN:
        raise BenchmarkError(f"corpus has only {len(paths)} documents; run setup first")
    return paths


def corpus_slice_members(count: int, questions: list[dict[str, Any]]) -> list[Path]:
    """Every expected document, then a seeded sample of the rest, up to `count`."""
    assert 0 < count <= CORPUS_SLICE_MAX
    paths = corpus_document_paths()
    expected_ids: set[str] = set()
    for question in questions:
        expected_ids.update(str(value) for value in question.get("expected_doc_ids", []))
    expected = [path for path in paths if DOCUMENT_ID_PATTERN.search(path.name) and
                DOCUMENT_ID_PATTERN.search(path.name).group(0) in expected_ids]
    if len(expected) > count:
        raise BenchmarkError(
            f"the questions expect {len(expected)} documents; a slice must hold at least that"
        )
    found = {DOCUMENT_ID_PATTERN.search(p.name).group(0) for p in expected}
    missing = expected_ids - found
    if missing:
        raise BenchmarkError(f"{len(missing)} expected documents are not in the corpus")
    rest = [path for path in paths if path not in set(expected)]
    sampler = random.Random(CORPUS_SLICE_SEED)
    sampled = sampler.sample(rest, count - len(expected))
    members = sorted(expected + sampled)
    assert len(members) == count
    return members


def materialize_corpus_slice(count: int) -> Path:
    """Hard-link a slice's documents under `slices/<count>/documents`, once."""
    root = FIXTURE_ROOT / "slices" / str(count)
    documents = root / "documents"
    marker = root / "documents.json"
    if marker.exists() and documents.is_dir():
        return root
    if documents.exists():
        shutil.rmtree(documents)
    members = corpus_slice_members(count, load_questions(MAX_QUESTIONS, exact=False))
    source_root = FIXTURE_ROOT / "documents"
    for path in members:
        target = documents / path.relative_to(source_root)
        target.parent.mkdir(parents=True, exist_ok=True)
        os.link(path, target)
    write_json(
        marker,
        {
            "archive_sha256": ARCHIVE_SHA256,
            "extracted_at": utc_now(),
            "text_file_count": len(members),
            "slice_seed": CORPUS_SLICE_SEED,
        },
    )
    print(f"Corpus slice of {count:,} documents ready at {root}", flush=True)
    return root


def load_questions(limit: int, exact: bool = True) -> list[dict[str, Any]]:
    question_path = FIXTURE_ROOT / "questions.jsonl"
    lines = question_path.read_text(encoding="utf-8").splitlines()
    if len(lines) > MAX_QUESTIONS:
        raise BenchmarkError(f"questions file has {len(lines)} rows; hard limit is {MAX_QUESTIONS}")
    questions = [json.loads(line) for line in lines if line.strip()]
    if exact and len(questions) < limit:
        raise BenchmarkError(f"requested {limit} questions but the fixture has {len(questions)}")
    selected = questions[:limit]
    assert len(selected) <= limit
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


def question_retrieval(
    question: dict[str, Any],
    options: RunOptions,
    data_dir: Path,
    composition: Path,
    log_dir: Path,
    progress_prefix: str,
) -> dict[str, Any]:
    """Run the Finder for one question and record the scored list beside its attribution."""
    question_id = str(question["question_id"])
    query = run_logged(
        query_arguments(
            data_dir,
            composition,
            str(question["question"]),
            max(options.query_limit, ATTRIBUTION_LIMIT),
            options.finder_overrides,
            options.explain,
        ),
        log_dir / f"{question_id}-query.log",
        timeout_seconds=QUERY_TIMEOUT_SECONDS,
        progress_label=f"{progress_prefix} retrieval",
    )
    require_success(query, f"querying {question_id}")
    results_deep = parse_query_results(query.stdout, ATTRIBUTION_LIMIT)
    query_meta = query_meta_object(query.stdout)
    # The scored list is the first `query_limit`; the deeper list exists only
    # so attribution can say where a missed gold document actually landed.
    query_results = results_deep[: options.query_limit]
    assert len(query_results) <= options.query_limit
    addresses = [str(result["address"]) for result in query_results]
    return {
        "retrieval_duration_seconds": round(query.duration_seconds, 6),
        "query_meta": query_meta,
        "results": query_results,
        "retrieved_document_ids": extract_document_ids(addresses),
        "attribution": attribution_rows(question, results_deep, query_meta),
    }


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
    retrieval = question_retrieval(
        question, options, data_dir, composition, log_dir, progress_prefix
    )
    retrieved_document_ids = retrieval["retrieved_document_ids"]
    if options.skip_agent:
        return {
            "question_id": question_id,
            "question_type": question.get("question_type"),
            "question": question_text,
            **retrieval,
            "answer_duration_seconds": 0.0,
            "duration_seconds": retrieval["retrieval_duration_seconds"],
            "agent_document_ids": [],
            "document_ids": retrieved_document_ids,
            "answer": "",
        }
    agent = run_logged(
        [
            *inseam_arguments(data_dir, composition),
            "agent",
            question_text,
            "--model",
            options.answer_model,
            "--turns",
            str(options.turns),
        ],
        log_dir / f"{question_id}-agent.log",
        timeout_seconds=AGENT_TIMEOUT_SECONDS,
        progress_label=f"{progress_prefix} answer",
    )
    require_success(agent, f"answering {question_id}")
    agent_document_ids = extract_document_ids([agent.stdout])
    document_ids = extract_document_ids([*retrieved_document_ids, *agent_document_ids])
    return {
        "question_id": question_id,
        "question_type": question.get("question_type"),
        "question": question_text,
        **retrieval,
        "answer_duration_seconds": round(agent.duration_seconds, 6),
        "duration_seconds": round(
            retrieval["retrieval_duration_seconds"] + agent.duration_seconds, 6
        ),
        "agent_document_ids": agent_document_ids,
        "document_ids": document_ids,
        "answer": extract_agent_answer(agent.stdout),
    }


def query_meta_object(stdout: str) -> dict[str, Any]:
    """The CLI's `meta` object, or an empty one when a binary predates it."""
    meta = json.loads(stdout).get("meta", {})
    if type(meta) is not dict:
        raise BenchmarkError("`inseam query --json` returned a meta that is not an object")
    evidence = bounded_list(meta.get("evidence"), ATTRIBUTION_LIMIT, "evidence")
    for entry in evidence:
        if type(entry) is not dict or type(entry.get("address")) is not str:
            raise BenchmarkError("`inseam query --json` evidence entry has no address")
        bounded_list(entry.get("fragments"), EVIDENCE_FRAGMENTS_MAX, "evidence fragments")
        ledger = entry.get("ledger")
        if ledger is not None:
            if type(ledger) is not dict:
                raise BenchmarkError("`inseam query --json` ledger is not an object")
            bounded_list(ledger.get("rows"), LEDGER_ROWS_MAX, "ledger rows")
            bounded_list(ledger.get("channels"), len(CHANNELS) * 2, "ledger channels")
    bounded_list(meta.get("hubs_excluded"), HUBS_EXCLUDED_MAX, "hubs_excluded")
    return meta


def bounded_list(value: Any, count_max: int, description: str) -> list[Any]:
    """An optional list from the CLI: absent reads as empty, oversize is refused."""
    assert count_max > 0
    if value is None:
        return []
    if type(value) is not list:
        raise BenchmarkError(f"`inseam query --json` {description} is not a list")
    if len(value) > count_max:
        raise BenchmarkError(f"`inseam query --json` {description} exceeds {count_max} entries")
    return value


def evidence_by_address(query_meta: dict[str, Any]) -> dict[str, dict[str, Any]]:
    """Each result's evidence object keyed by its address; empty without `meta.evidence`."""
    evidence = bounded_list(query_meta.get("evidence"), ATTRIBUTION_LIMIT, "evidence")
    by_address: dict[str, dict[str, Any]] = {}
    for entry in evidence:
        by_address.setdefault(str(entry["address"]), entry)
    return by_address


def attribution_rows(
    question: dict[str, Any], results: list[dict[str, Any]], query_meta: dict[str, Any]
) -> list[dict[str, Any]]:
    """One row per gold document: its 1-based rank in the deep list, or null, and its evidence."""
    assert len(results) <= ATTRIBUTION_LIMIT
    evidence = evidence_by_address(query_meta)
    address_by_document: dict[str, str] = {}
    rank_by_document: dict[str, int] = {}
    for index, result in enumerate(results):
        address = str(result["address"])
        for document_id in extract_document_ids([address]):
            if document_id in rank_by_document:
                continue
            rank_by_document[document_id] = index + 1
            address_by_document[document_id] = address
    rows: list[dict[str, Any]] = []
    for document_id in extract_document_ids(
        [str(value) for value in question.get("expected_doc_ids", [])]
    ):
        rank = rank_by_document.get(document_id)
        rows.append(
            {
                "question_id": str(question["question_id"]),
                "question_type": question.get("question_type"),
                "document_id": document_id,
                "rank": rank,
                "evidence": (
                    None if rank is None else evidence.get(address_by_document[document_id])
                ),
            }
        )
    return rows


def retrieval_ranking(query: dict[str, Any]) -> list[str]:
    """Keep unjudged folders in their actual ranks instead of promoting documents."""
    if "results" not in query:
        return query["retrieved_document_ids"]
    ranking: list[str] = []
    for result in query["results"]:
        address = str(result["address"])
        if result.get("envelope", {}).get("content_type") == "inode/directory":
            ranking.append(address)
            continue
        document_ids = extract_document_ids([address])
        ranking.append(document_ids[0] if document_ids else address)
    return ranking


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
        retrieved = retrieval_ranking(query)
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
        "attribution_ready": attribution_ready(queries),
    }


def attribution_ready(queries: list[dict[str, Any]]) -> bool:
    """Whether any result in the run carried a ledger, so the channel tables mean something."""
    for query in queries:
        evidence = bounded_list(
            query.get("query_meta", {}).get("evidence"), ATTRIBUTION_LIMIT, "evidence"
        )
        for entry in evidence:
            if type(entry.get("ledger")) is dict:
                return True
    return False


def evidence_seeded_channels(evidence: dict[str, Any]) -> set[str]:
    """The channels whose seed list held any fragment of this result."""
    fragments = bounded_list(evidence.get("fragments"), EVIDENCE_FRAGMENTS_MAX, "fragments")
    seeded: set[str] = set()
    for fragment in fragments:
        for channel in CHANNELS:
            if fragment.get(f"{channel}_rank") is not None:
                seeded.add(channel)
    return seeded


def evidence_channel_mass(evidence: dict[str, Any]) -> dict[str, float]:
    """Seed plus walk mass per channel from the ledger; empty without one."""
    ledger = evidence.get("ledger")
    if type(ledger) is not dict:
        return {}
    mass: dict[str, float] = {}
    for line in bounded_list(ledger.get("channels"), len(CHANNELS) * 2, "ledger channels"):
        channel = line.get("channel")
        if channel not in CHANNELS:
            continue
        mass[channel] = float(line.get("seed", 0.0)) + float(line.get("walk", 0.0))
    return mass


def evidence_row_kind_mass(evidence: dict[str, Any]) -> dict[str, float]:
    ledger = evidence.get("ledger")
    if type(ledger) is not dict:
        return {}
    by_kind = ledger.get("walk_by_row_kind")
    if type(by_kind) is not dict:
        return {}
    mass: dict[str, float] = {}
    for reported_kind, value in by_kind.items():
        kind = reported_kind if reported_kind in ROW_KINDS else "other"
        mass[kind] = mass.get(kind, 0.0) + float(value)
    return mass


def largest_contributor(mass: dict[str, float], order: tuple[str, ...]) -> str | None:
    """The first name in `order` carrying the strictly largest positive mass."""
    largest: str | None = None
    largest_mass = 0.0
    for name in order:
        value = mass.get(name, 0.0)
        if value > largest_mass:
            largest = name
            largest_mass = value
    return largest


def evidence_rows(evidence: dict[str, Any]) -> list[dict[str, Any]]:
    """The vocabulary rows that carried mass into this result, as the ledger lists them."""
    ledger = evidence.get("ledger")
    if type(ledger) is not dict:
        return []
    rows = []
    for row in bounded_list(ledger.get("rows"), LEDGER_ROWS_MAX, "ledger rows"):
        kind = row.get("kind")
        rows.append(
            {
                "text": str(row.get("text", row.get("fragment", ""))),
                "kind": kind if kind in ROW_KINDS else "other",
                "document_frequency": row.get("document_frequency"),
            }
        )
    return rows


def attribution_entries(
    queries: list[dict[str, Any]], questions: list[dict[str, Any]]
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """Gold entries from each query's attribution rows; noise entries from its scored results.

    Noise is a scored document that is neither gold nor listed as valid for
    the question. Each entry is `{question_type, document_id, rank, evidence}`.
    """
    questions_by_id = {str(question["question_id"]): question for question in questions}
    gold: list[dict[str, Any]] = []
    noise: list[dict[str, Any]] = []
    for query in queries:
        question = questions_by_id[str(query["question_id"])]
        expected = set(str(value) for value in question.get("expected_doc_ids", []))
        if not expected:
            continue
        rows = bounded_list(query.get("attribution"), ATTRIBUTION_ROWS_MAX, "attribution")
        gold.extend(rows)
        noise.extend(noise_entries(query, expected, question))
    if len(gold) > ATTRIBUTION_ROWS_MAX:
        raise BenchmarkError(f"more than {ATTRIBUTION_ROWS_MAX} gold attribution rows")
    return gold, noise


def noise_entries(
    query: dict[str, Any], expected: set[str], question: dict[str, Any]
) -> list[dict[str, Any]]:
    valid = expected | set(str(value) for value in question.get("valid_doc_ids", []))
    evidence = evidence_by_address(query.get("query_meta", {}))
    seen: set[str] = set()
    entries: list[dict[str, Any]] = []
    for index, result in enumerate(query.get("results", [])):
        address = str(result["address"])
        for document_id in extract_document_ids([address]):
            if document_id in valid or document_id in seen:
                continue
            seen.add(document_id)
            entries.append(
                {
                    "question_type": question.get("question_type"),
                    "document_id": document_id,
                    "rank": index + 1,
                    "evidence": evidence.get(address),
                }
            )
    return entries


def channel_counts(entries: list[dict[str, Any]]) -> dict[str, dict[str, int]]:
    """Per channel: documents it seeded, seeded alone, and was the largest contributor for."""
    counts = {channel: {"seeded": 0, "only_seeded": 0, "largest": 0} for channel in CHANNELS}
    for entry in entries:
        evidence = entry.get("evidence")
        if type(evidence) is not dict:
            continue
        seeded = evidence_seeded_channels(evidence)
        for channel in seeded:
            counts[channel]["seeded"] += 1
        if len(seeded) == 1:
            counts[next(iter(seeded))]["only_seeded"] += 1
        largest = largest_contributor(evidence_channel_mass(evidence), CHANNELS)
        if largest is not None:
            counts[largest]["largest"] += 1
    return counts


def row_kind_counts(entries: list[dict[str, Any]]) -> dict[str, int]:
    """Per row kind: documents whose walk mass arrived mostly through that kind."""
    counts = {kind: 0 for kind in ROW_KINDS}
    for entry in entries:
        evidence = entry.get("evidence")
        if type(evidence) is not dict:
            continue
        largest = largest_contributor(evidence_row_kind_mass(evidence), ROW_KINDS)
        if largest is not None:
            counts[largest] += 1
    return counts


def row_carriers(
    gold: list[dict[str, Any]], noise: list[dict[str, Any]]
) -> dict[tuple[str, str], dict[str, Any]]:
    """Every ledger row named across both lists, with the documents it carried into each."""
    carriers: dict[tuple[str, str], dict[str, Any]] = {}
    for column, entries in (("gold_documents", gold), ("noise_documents", noise)):
        for entry in entries:
            evidence = entry.get("evidence")
            if type(evidence) is not dict:
                continue
            named: set[tuple[str, str]] = set()
            for row in evidence_rows(evidence):
                key = (row["text"], row["kind"])
                if key in named:
                    continue
                named.add(key)
                carrier = carriers.get(key)
                if carrier is None:
                    if len(carriers) >= ROWS_TRACKED_MAX:
                        raise BenchmarkError(f"ledgers name more than {ROWS_TRACKED_MAX} rows")
                    carrier = {**row, "gold_documents": 0, "noise_documents": 0}
                    carriers[key] = carrier
                carrier[column] += 1
                if carrier["document_frequency"] is None:
                    carrier["document_frequency"] = row["document_frequency"]
    return carriers


def top_row_carriers(
    carriers: dict[tuple[str, str], dict[str, Any]], column: str
) -> list[dict[str, Any]]:
    ranked = sorted(
        (carrier for carrier in carriers.values() if carrier[column] > 0),
        key=lambda carrier: (-carrier[column], carrier["text"], carrier["kind"]),
    )
    return ranked[:ATTRIBUTION_TOP_ROWS]


def attribution_summary(
    gold: list[dict[str, Any]], noise: list[dict[str, Any]]
) -> dict[str, Any]:
    """The channel, row-kind, and row tables over one gold list and one noise list."""
    gold_channels = channel_counts(gold)
    noise_channels = channel_counts(noise)
    gold_kinds = row_kind_counts(gold)
    noise_kinds = row_kind_counts(noise)
    carriers = row_carriers(gold, noise)
    return {
        "gold_documents": len(gold),
        "gold_documents_found": sum(1 for entry in gold if entry.get("rank") is not None),
        "gold_documents_with_ledger": sum(
            1 for entry in gold if type((entry.get("evidence") or {}).get("ledger")) is dict
        ),
        "noise_documents": len(noise),
        "channels": {
            channel: {
                "gold_seeded": gold_channels[channel]["seeded"],
                "gold_only_seeded": gold_channels[channel]["only_seeded"],
                "gold_largest": gold_channels[channel]["largest"],
                "noise_seeded": noise_channels[channel]["seeded"],
                "noise_only_seeded": noise_channels[channel]["only_seeded"],
                "noise_largest": noise_channels[channel]["largest"],
            }
            for channel in CHANNELS
        },
        "row_kinds": {
            kind: {"gold_largest": gold_kinds[kind], "noise_largest": noise_kinds[kind]}
            for kind in ROW_KINDS
        },
        "rows_carried_gold": top_row_carriers(carriers, "gold_documents"),
        "rows_carried_noise": top_row_carriers(carriers, "noise_documents"),
    }


def hubs_excluded(queries: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """The union of every query's excluded hubs, each at its highest reported degree."""
    hubs: dict[str, dict[str, Any]] = {}
    for query in queries:
        reported = bounded_list(
            query.get("query_meta", {}).get("hubs_excluded"), HUBS_EXCLUDED_MAX, "hubs_excluded"
        )
        for hub in reported:
            key = str(hub.get("text") or hub.get("fragment", ""))
            degree = int(hub.get("degree", 0))
            known = hubs.get(key)
            if known is None or degree > known["degree"]:
                hubs[key] = {
                    "text": key,
                    "fragment": hub.get("fragment"),
                    "kind": hub.get("kind"),
                    "degree": degree,
                }
    ranked = sorted(hubs.values(), key=lambda hub: (-hub["degree"], hub["text"]))
    return ranked[:ATTRIBUTION_TOP_ROWS]


def retrieval_attribution(
    queries: list[dict[str, Any]], questions: list[dict[str, Any]]
) -> dict[str, Any]:
    """What each channel bought and what it cost, overall and per question type."""
    gold, noise = attribution_entries(queries, questions)
    question_types = sorted(
        {str(entry.get("question_type")) for entry in gold}
        | {str(entry.get("question_type")) for entry in noise}
    )
    by_question_type = {
        question_type: attribution_summary(
            [entry for entry in gold if str(entry.get("question_type")) == question_type],
            [entry for entry in noise if str(entry.get("question_type")) == question_type],
        )
        for question_type in question_types
    }
    return {
        "attribution_ready": attribution_ready(queries),
        "attribution_limit": ATTRIBUTION_LIMIT,
        **attribution_summary(gold, noise),
        "hubs_excluded": hubs_excluded(queries),
        "by_question_type": by_question_type,
    }


def write_retrieval_attribution(
    run_dir: Path, queries: list[dict[str, Any]], questions: list[dict[str, Any]]
) -> dict[str, Any]:
    attribution = retrieval_attribution(queries, questions)
    write_json(run_dir / "retrieval-attribution.json", attribution)
    return attribution


def attribution_manifest_section(attribution: dict[str, Any]) -> dict[str, Any]:
    """The headline tables for the manifest; the rows and per-type split stay in the file."""
    return {
        "attribution_ready": attribution["attribution_ready"],
        "gold_documents": attribution["gold_documents"],
        "gold_documents_found": attribution["gold_documents_found"],
        "gold_documents_with_ledger": attribution["gold_documents_with_ledger"],
        "noise_documents": attribution["noise_documents"],
        "channels": attribution["channels"],
        "row_kinds": attribution["row_kinds"],
        "detail": "retrieval-attribution.json",
    }


def print_retrieval_report(
    retrieval: dict[str, Any], attribution: dict[str, Any], query_limit: int
) -> None:
    print(
        f"Retrieval: recall {retrieval['average_document_recall_pct']}%, "
        f"hit rate {retrieval['document_hit_rate_pct']}%, "
        f"MRR {retrieval['mean_reciprocal_rank']} over "
        f"{retrieval['questions_with_expected_documents']} questions",
        flush=True,
    )
    print(
        f"Attribution: {attribution['gold_documents']} gold documents, "
        f"{attribution['gold_documents_found']} found within {ATTRIBUTION_LIMIT}, "
        f"{attribution['noise_documents']} noise documents in the top {query_limit}; "
        "per-document ledgers in attribution.jsonl, tables in retrieval-attribution.json",
        flush=True,
    )
    if not attribution["attribution_ready"]:
        print(
            "  no result carried a ledger (an older binary, or --no-explain); "
            "attribution.jsonl records ranks only",
            flush=True,
        )
        return
    print(f"  {'channel':<12}{'gold seeded':>12}{'only':>6}{'largest':>9}"
          f"{'noise seeded':>14}{'only':>6}{'largest':>9}", flush=True)
    for channel, counts in attribution["channels"].items():
        print(f"  {channel:<12}{counts['gold_seeded']:>12}{counts['gold_only_seeded']:>6}"
              f"{counts['gold_largest']:>9}{counts['noise_seeded']:>14}"
              f"{counts['noise_only_seeded']:>6}{counts['noise_largest']:>9}", flush=True)
    print(f"  {'row kind':<12}{'gold largest':>13}{'noise largest':>15}", flush=True)
    for kind, counts in attribution["row_kinds"].items():
        print(f"  {kind:<12}{counts['gold_largest']:>13}{counts['noise_largest']:>15}", flush=True)


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
    attribution: list[dict[str, Any]] = []
    for query in queries:
        attribution.extend(bounded_list(query.get("attribution"), ATTRIBUTION_ROWS_MAX, "attribution"))
    if len(attribution) > ATTRIBUTION_ROWS_MAX:
        raise BenchmarkError(f"more than {ATTRIBUTION_ROWS_MAX} gold attribution rows")
    write_json_lines(run_dir / "attribution.jsonl", attribution, ATTRIBUTION_ROWS_MAX)


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
        "models": model_assignments(options),
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


def model_assignments(options: RunOptions) -> dict[str, str | int]:
    corpus = "full" if options.corpus_slice == 0 else f"slice-{options.corpus_slice}"
    return {
        "summarization": SUMMARIZATION_MODEL,
        "summarization_lane": SUMMARIZATION_LANE,
        "corpus": corpus,
        "structural": options.structural,
        "finder_seeds": options.finder_seeds,
        "finder_max_vector_distance": options.finder_max_vector_distance,
        "entity_extraction": "disabled",
        "hints": SUMMARIZATION_MODEL if options.hints_llm_call_budget > 0 else "disabled",
        "answer_generation": "skipped" if options.skip_agent else options.answer_model,
        "answer_evaluation": "skipped" if options.skip_evaluation else options.evaluation_model,
        "embeddings": "disabled" if options.embedding_vectors == "none" else EMBEDDING_MODEL,
        "embedding_dimensions": 0 if options.embedding_vectors == "none" else EMBEDDING_DIMENSIONS,
        "embedding_vectors": options.embedding_vectors,
    }


def index_documents(
    run_dir: Path,
    data_dir: Path,
    composition: Path,
    options: RunOptions,
    log_path: Path | None = None,
) -> dict[str, Any]:
    root = corpus_root(options)
    return harness.index_documents(
        run_dir,
        data_dir,
        composition,
        root / "documents",
        fixture_document_count(root, DOCUMENTS_MAX),
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
    # Written here rather than by the caller so a retrieval replay, which
    # shares this loop, records the same tables as a fresh run.
    write_retrieval_attribution(run_dir, queries, questions)
    return queries


def evaluator_environment(evaluation_model: str) -> dict[str, str]:
    require_api_key()
    environment = os.environ.copy()
    environment.update(
        {
            "LLM_PROVIDER": "openai",
            "LLM_API_KEY": os.environ["OPENROUTER_API_KEY"],
            "LLM_MODEL_NAME": evaluation_model,
            "CHEAP_LLM_MODEL_NAME": evaluation_model,
            "OPENAI_BASE_URL": OPENROUTER_BASE_URL,
        }
    )
    return environment


def evaluate(
    run_dir: Path,
    log_dir: Path,
    parallelism: int,
    question_count: int,
    evaluation_model: str,
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
        environment=evaluator_environment(evaluation_model),
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
    evaluator_environment(options.evaluation_model)
    if options.corpus_slice > 0:
        materialize_corpus_slice(options.corpus_slice)
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


def resume_benchmark(run_id: str, after_kill: bool = False) -> None:
    require_fixture()
    loaded = load_resumable_run(run_id, after_kill)
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
                run_dir, data_dir, composition, options, index_log
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
        attribution = retrieval_attribution(queries, questions)
        scores = {
            "retrieval": retrieval_scores(queries, questions),
            "retrieval_attribution": attribution_manifest_section(attribution),
        }
        print_retrieval_report(scores["retrieval"], attribution, options.query_limit)
        if not options.skip_evaluation:
            update_run_phase(run_dir, manifest, "evaluating")
            scores["enterprise_rag_bench"] = evaluate(
                run_dir,
                log_dir,
                options.evaluation_parallelism,
                len(questions),
                options.evaluation_model,
            )
            pip_freeze(run_dir)
        record_final_index_footprint(manifest, data_dir)
        write_retrieval_observability(run_dir, queries)
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
    after_kill: bool,
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
    options = options_from_manifest(manifest)
    validate_resumable_manifest(run_id, manifest, options, after_kill)
    questions = load_questions(options.question_limit)
    composition = run_dir / "composition.toml"
    if not composition.is_file():
        raise BenchmarkError(f"run `{run_id}` has no composition.toml")
    expected_composition = composition_text(options)
    if "finder_lexical_weight" not in manifest["options"]:
        expected_composition = expected_composition.replace("lexical_weight = 1.0\n", "")
    if composition.read_text(encoding="utf-8") != expected_composition:
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


def validate_resumable_manifest(
    run_id: str, manifest: dict[str, Any], options: RunOptions, after_kill: bool
) -> None:
    validate_resumable_status(run_id, manifest, after_kill)
    if manifest.get("benchmark") != benchmark_pins():
        raise BenchmarkError(f"run `{run_id}` uses different benchmark inputs")
    if manifest.get("models") != model_assignments(options):
        raise BenchmarkError(f"run `{run_id}` uses different models")


def options_from_manifest(manifest: dict[str, Any]) -> RunOptions:
    # Runs recorded before a dial existed resume with the value they ran at.
    value = manifest_options_object(
        manifest,
        set(RunOptions.__annotations__),
        defaults={"finder_lexical_weight": 1.0, "finder_overrides": [], "explain": False},
    )
    return RunOptions(
        question_limit=manifest_option_integer(value, "question_limit", MAX_QUESTIONS),
        query_limit=manifest_option_integer(value, "query_limit", MAX_QUERY_RESULTS),
        turns=manifest_option_integer(value, "turns", MAX_TURNS),
        index_concurrency=manifest_option_integer(
            value, "index_concurrency", MAX_INDEX_CONCURRENCY
        ),
        llm_call_budget=manifest_option_count(value, "llm_call_budget", MAX_LLM_CALL_BUDGET),
        evaluation_parallelism=manifest_option_integer(
            value, "evaluation_parallelism", 64
        ),
        skip_evaluation=manifest_option_boolean(value, "skip_evaluation"),
        skip_agent=manifest_option_boolean(value, "skip_agent"),
        corpus_slice=manifest_option_count(value, "corpus_slice", CORPUS_SLICE_MAX),
        summary_target_chars=manifest_option_integer(
            value, "summary_target_chars", MAX_SUMMARY_TARGET_CHARS
        ),
        keywords_max=manifest_option_count(value, "keywords_max", MAX_KEYWORDS),
        structural=manifest_option_choice(value, "structural", STRUCTURAL_CHOICES),
        embedding_vectors=manifest_option_choice(value, "embedding_vectors", EMBEDDER_CHOICES),
        directory_summary_target_chars=manifest_option_count(value, "directory_summary_target_chars", MAX_SUMMARY_TARGET_CHARS),
        finder_lexical_weight=manifest_option_weight(value, "finder_lexical_weight"),
        finder_damping=manifest_option_probability(value, "finder_damping"),
        finder_seed_k=manifest_option_integer(value, "finder_seed_k", 1000),
        finder_rrf_k=manifest_option_integer(value, "finder_rrf_k", 1000),
        finder_seeds=manifest_option_choice(value, "finder_seeds", FINDER_SEEDS),
        finder_max_vector_distance=manifest_option_distance(value, "finder_max_vector_distance"),
        answer_model=manifest_option_model(value, "answer_model"),
        evaluation_model=manifest_option_model(value, "evaluation_model"),
        hints_llm_call_budget=manifest_option_count(
            value, "hints_llm_call_budget", MAX_LLM_CALL_BUDGET
        ),
        finder_overrides=manifest_option_finder_overrides(value, "finder_overrides"),
        explain=manifest_option_boolean(value, "explain"),
    )


def manifest_option_model(value: dict[str, Any], name: str) -> str:
    option = value[name]
    if type(option) is not str or not option or len(option) > MODEL_ID_MAX_CHARS:
        raise BenchmarkError(f"run option {name} is not a model id")
    return option


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
        type=count_argument("llm-call-budget", MAX_LLM_CALL_BUDGET),
        default=0,
        help="summary calls per indexing run; 0 (the default) makes the index model-free",
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
    run_parser.add_argument(
        "--skip-agent",
        action="store_true",
        help="retrieval only: no agent answers, no judge (implies --skip-evaluation)",
    )
    run_parser.add_argument(
        "--corpus-slice",
        type=count_argument("corpus-slice", CORPUS_SLICE_MAX),
        default=0,
        help="index a slice of this many documents (every expected one plus a seeded sample)",
    )
    run_parser.add_argument(
        "--summary-target-chars",
        type=bounded_argument("summary-target-chars", MAX_SUMMARY_TARGET_CHARS),
        default=SUMMARY_TARGET_CHARS_WHOLE_DOCUMENT,
    )
    run_parser.add_argument(
        "--keywords-max", type=count_argument("keywords-max", MAX_KEYWORDS), default=12
    )
    run_parser.add_argument("--structural", choices=sorted(STRUCTURAL_CHOICES), default="off")
    run_parser.add_argument("--vectors", choices=sorted(EMBEDDER_CHOICES), default="none")
    run_parser.add_argument("--finder-seeds", choices=sorted(FINDER_SEEDS), default="both")
    run_parser.add_argument(
        "--finder-max-vector-distance", type=distance_argument, default=0.75
    )
    run_parser.add_argument("--answer-model", default=ANSWER_MODEL)
    run_parser.add_argument("--evaluation-model", default=EVALUATION_MODEL)
    run_parser.add_argument(
        "--hints-llm-call-budget",
        type=count_argument("hints-llm-call-budget", MAX_LLM_CALL_BUDGET),
        default=0,
        help="mount the hints transform with this many calls per run; 0 (the default) leaves it off",
    )
    run_parser.add_argument("--finder-seed-k", type=bounded_argument("finder-seed-k", 1000), default=60)
    run_parser.add_argument("--finder-rrf-k", type=bounded_argument("finder-rrf-k", 1000), default=60)
    run_parser.add_argument("--finder-damping", type=probability_argument, default=0.5)
    run_parser.add_argument("--directory-summary-target-chars", type=count_argument("directory-summary-target-chars", MAX_SUMMARY_TARGET_CHARS), default=0)
    run_parser.add_argument("--finder-lexical-weight", type=weight_argument, default=1.0)
    add_query_time_arguments(run_parser)
    resume_parser = subparsers.add_parser(
        "resume",
        help="reuse a completed index and continue a failed or interrupted run",
    )
    resume_parser.add_argument("run_id", help="existing run directory name")
    resume_parser.add_argument(
        "--after-kill",
        action="store_true",
        help="the run still reads `running` because its process was killed; close that attempt and resume",
    )
    return parser.parse_args()


def add_query_time_arguments(run_parser: argparse.ArgumentParser) -> None:
    """The dials that change a query, not the index (design/vocabulary.md, dials)."""
    run_parser.add_argument(
        "--finder-override",
        action="append",
        type=finder_override_argument,
        default=[],
        metavar="KEY=VALUE",
        help="a query-time Finder override passed to every query as `--finder KEY=VALUE`; repeatable",
    )
    run_parser.add_argument(
        "--explain",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="ask every retrieval query for its score ledger (the default); --no-explain skips it",
    )


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
            skip_evaluation=arguments.skip_evaluation or arguments.skip_agent,
            skip_agent=arguments.skip_agent,
            corpus_slice=arguments.corpus_slice,
            summary_target_chars=arguments.summary_target_chars,
            keywords_max=arguments.keywords_max,
            structural=arguments.structural,
            embedding_vectors=arguments.vectors,
            finder_seeds=arguments.finder_seeds,
            finder_seed_k=arguments.finder_seed_k,
            finder_rrf_k=arguments.finder_rrf_k,
            finder_damping=arguments.finder_damping,
            finder_lexical_weight=arguments.finder_lexical_weight,
            directory_summary_target_chars=arguments.directory_summary_target_chars,
            finder_max_vector_distance=arguments.finder_max_vector_distance,
            answer_model=arguments.answer_model,
            evaluation_model=arguments.evaluation_model,
            hints_llm_call_budget=arguments.hints_llm_call_budget,
            finder_overrides=arguments.finder_override,
            explain=arguments.explain,
        )
        return run_main(lambda: run_benchmark(options))
    return run_main(lambda: resume_benchmark(arguments.run_id, arguments.after_kill))


if __name__ == "__main__":
    raise SystemExit(main())
