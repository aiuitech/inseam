#!/usr/bin/env python3
"""Run a BEIR retrieval benchmark against the installed inseam CLI.

BEIR (Benchmarking IR) is a suite of zero-shot retrieval datasets that share
one shape: a corpus of documents, a set of queries, and graded relevance
judgments (qrels). The score is the standard trec_eval family, headlined by
nDCG@10. This runner pins the smallest dataset in the suite, NFCorpus, so a
run costs minutes and cents rather than hours and dollars.
"""

from __future__ import annotations

import argparse
import json
import math
import re
import shutil
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

from harness import (
    distance_argument,
    manifest_option_distance,
    FINDER_SEEDS,
    MAX_KEYWORDS,
    MAX_SUMMARY_TARGET_CHARS,
    STRUCTURAL_CHOICES,
    count_argument,
    manifest_option_choice,
    manifest_option_count,
    EMBEDDING_DIMENSIONS,
    EMBEDDING_MODEL,
    FIXTURES_ROOT,
    INDEX_PROGRESS_INTERVAL_SECONDS,
    INDEX_TIMEOUT_SECONDS,
    MAX_INDEX_CONCURRENCY,
    MAX_LLM_CALL_BUDGET,
    OPENROUTER_BASE_URL,
    PROGRESS_INTERVAL_SECONDS,
    RUNS_ROOT as ALL_RUNS_ROOT,
    SUMMARIZATION_LANE,
    SUMMARIZATION_MODEL,
    BenchmarkError,
    begin_attempt,
    bounded_argument,
    complete_run,
    download_verified,
    fixture_document_count,
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


@dataclass(frozen=True)
class Dataset:
    """One BEIR dataset release, pinned by content hash and row counts."""

    name: str
    split: str
    archive_url: str
    archive_sha256: str
    document_count: int
    query_count: int
    qrel_count: int


# NFCorpus is the smallest BEIR corpus: 3,633 PubMed abstracts and article
# pages, 323 test queries from NutritionFacts.org, and 12,334 graded
# judgments. The archive is the one BEIR's own loader downloads.
NFCORPUS = Dataset(
    name="nfcorpus",
    split="test",
    archive_url="https://public.ukp.informatik.tu-darmstadt.de/thakur/BEIR/datasets/nfcorpus.zip",
    archive_sha256="efe5be03f8c5b86a5870102d0599d227c8c6e2484328e68c6522560385671b0b",
    document_count=3_633,
    query_count=323,
    qrel_count=12_334,
)
DATASET = NFCORPUS
FIXTURE_ROOT = FIXTURES_ROOT / f"beir-{DATASET.name}"
RUNS_ROOT = ALL_RUNS_ROOT / f"beir-{DATASET.name}"
# Vectors cover every fragment here, unlike the EnterpriseRAG-Bench lean
# shape: the corpus is tiny, so embedding each abstract whole costs cents and
# measures the product's default search surface (design/benchmarking.md).
EMBEDDING_VECTORS = "all"
MAX_QUERIES = 1_000
MAX_DOCUMENTS = 10_000
SOURCES_MAX = MAX_DOCUMENTS + 1
MAX_QREL_ROWS = 100_000
# `inseam query` clamps its result limit to 50, so a larger cutoff would
# silently score a truncated ranking.
MAX_QUERY_RESULTS = 50
SUMMARIZATION_LANES = frozenset({"batch", "interactive"})


METRIC_CUTOFFS = (1, 3, 5, 10)
SETUP_TIMEOUT_SECONDS = 1_800
QUERY_TIMEOUT_SECONDS = 600
DOCUMENT_ID_PATTERN = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}")
DOCUMENT_FILE_SUFFIX = ".txt"
# The corpus variants a run may index. `text` is BEIR's row as a text file:
# title on the first line, a blank line, the abstract. `markdown` is the same
# file with the title as a `#` heading, so the markdown transform reads the
# document as an outline and the summarizer leads with the title.
CORPUS_DIRECTORIES = {"text": "documents", "markdown": "documents-markdown"}
CORPUS_SUFFIXES = {"text": ".txt", "markdown": ".md"}
# Widths a run may ask text-embedding-3-small for (Matryoshka; native 1536).
EMBEDDING_DIMENSIONS_MAX = 1536
QRELS_HEADER = ["query-id", "corpus-id", "score"]


@dataclass(frozen=True)
class RunOptions:
    query_count: int
    results_per_query: int
    index_concurrency: int
    llm_call_budget: int
    # The summarizer's target length. Text within it is its own summary
    # and costs no model call, so a target past the corpus's longest
    # document embeds every abstract whole and makes the run model-free.
    summary_target_chars: int = 200
    # The lane summary calls ride. Batch is the right trade for thousands of
    # calls; a run that makes a handful (a model-free run summarizes only the
    # corpus folder) would wait on a one-request batch job for nothing.
    summarization_lane: str = SUMMARIZATION_LANE
    # Keywords planted beside each summary for full-text search; 0 plants
    # none, the control for whether the row earns its place.
    keywords_max: int = 12
    # The embedder's width; the model is Matryoshka-trained, so any width up
    # to its native 1536 is a real vector and the footprint scales with it.
    embedding_dimensions: int = EMBEDDING_DIMENSIONS
    # Which materialized corpus is indexed (CORPUS_DIRECTORIES).
    corpus: str = "text"
    # Whether the markdown structural transform runs over the roots.
    structural: str = "off"
    # The finder's seed lists: both fused, or one alone as a diagnostic.
    finder_seeds: str = "both"
    # Vector hits farther than this cosine distance are dropped before
    # fusion; 1.0 keeps every nearest-k hit.
    finder_max_vector_distance: float = 0.75


def validate_document_id(document_id: str) -> str:
    if DOCUMENT_ID_PATTERN.fullmatch(document_id) is None:
        raise BenchmarkError(f"document ID {document_id!r} cannot be a file name")
    return document_id


def document_file_name(document_id: str, suffix: str = DOCUMENT_FILE_SUFFIX) -> str:
    assert suffix in CORPUS_SUFFIXES.values()
    return f"{validate_document_id(document_id)}{suffix}"


def document_text(row: dict[str, Any]) -> str:
    title = str(row.get("title") or "").strip()
    body = str(row.get("text") or "").strip()
    if not body:
        raise BenchmarkError(f"document {row.get('_id')!r} has no text")
    if title:
        return f"{title}\n\n{body}\n"
    return f"{body}\n"


def document_markdown(row: dict[str, Any]) -> str:
    """The `markdown` corpus variant: the title as the document's heading."""
    title = " ".join(str(row.get("title") or "").split())
    body = str(row.get("text") or "").strip()
    if not body:
        raise BenchmarkError(f"document {row.get('_id')!r} has no text")
    if title:
        return f"# {title}\n\n{body}\n"
    return f"{body}\n"


def extract_archive(archive: Path) -> Path:
    unzip = require_program("unzip")
    extracted = FIXTURE_ROOT / "downloads" / "extracted"
    dataset_dir = extracted / DATASET.name
    corpus = dataset_dir / "corpus.jsonl"
    if not corpus.is_file():
        extracted.mkdir(parents=True, exist_ok=True)
        result = run_capture(
            [unzip, "-q", "-o", str(archive), "-d", str(extracted)],
            timeout_seconds=SETUP_TIMEOUT_SECONDS,
        )
        require_success(result, f"extracting {archive.name}")
    for name in ("corpus.jsonl", "queries.jsonl", f"qrels/{DATASET.split}.tsv"):
        if not (dataset_dir / name).is_file():
            raise BenchmarkError(f"{archive.name} does not contain {DATASET.name}/{name}")
    return dataset_dir


def materialize_documents(corpus: Path) -> int:
    """Write one text file per corpus row so the filesystem host can index it.

    The file name is the BEIR document ID, which is how a Finder result address
    maps back to a judgment.
    """
    documents = FIXTURE_ROOT / "documents"
    marker = FIXTURE_ROOT / "documents.json"
    existing = fixture_document_count(FIXTURE_ROOT, MAX_DOCUMENTS)
    if existing == DATASET.document_count and documents.is_dir():
        # A fixture set up before the markdown variant existed gains it here.
        if not markdown_documents_complete(existing):
            materialize_markdown_documents(corpus, existing)
        return existing
    if documents.exists():
        shutil.rmtree(documents)
    documents.mkdir(parents=True)
    seen_lowercase: set[str] = set()
    written = 0
    for row in read_json_lines(corpus, MAX_DOCUMENTS):
        document_id = validate_document_id(str(row["_id"]))
        # Case-insensitive file systems would merge IDs that differ by case.
        if document_id.lower() in seen_lowercase:
            raise BenchmarkError(f"document ID {document_id!r} collides with another")
        seen_lowercase.add(document_id.lower())
        (documents / document_file_name(document_id)).write_text(
            document_text(row), encoding="utf-8"
        )
        written += 1
    materialize_markdown_documents(corpus, written)
    if written != DATASET.document_count:
        raise BenchmarkError(
            f"corpus has {written} documents; the pin expects {DATASET.document_count}"
        )
    write_json(
        marker,
        {
            "archive_sha256": DATASET.archive_sha256,
            "extracted_at": utc_now(),
            "text_file_count": written,
        },
    )
    return written


def markdown_documents_complete(expected_count: int) -> bool:
    documents = FIXTURE_ROOT / CORPUS_DIRECTORIES["markdown"]
    if not documents.is_dir():
        return False
    suffix = CORPUS_SUFFIXES["markdown"]
    count = sum(1 for _ in documents.glob(f"*{suffix}"))
    return count == expected_count


def materialize_markdown_documents(corpus: Path, expected_count: int) -> None:
    """Write the `markdown` corpus variant beside the text one: the same rows
    with the title as a `#` heading (`document_markdown`)."""
    assert 0 < expected_count <= MAX_DOCUMENTS
    documents = FIXTURE_ROOT / CORPUS_DIRECTORIES["markdown"]
    if documents.exists():
        shutil.rmtree(documents)
    documents.mkdir(parents=True)
    written = 0
    for row in read_json_lines(corpus, MAX_DOCUMENTS):
        document_id = validate_document_id(str(row["_id"]))
        (documents / document_file_name(document_id, CORPUS_SUFFIXES["markdown"])).write_text(
            document_markdown(row), encoding="utf-8"
        )
        written += 1
    if written != expected_count:
        raise BenchmarkError(
            f"markdown corpus has {written} documents; the text corpus has {expected_count}"
        )


def parse_qrels(text: str) -> dict[str, dict[str, int]]:
    """Parse BEIR's TSV judgments into relevance grades per query."""
    lines = text.splitlines()
    if not lines or lines[0].split("\t") != QRELS_HEADER:
        raise BenchmarkError("qrels file does not start with the BEIR header")
    if len(lines) - 1 > MAX_QREL_ROWS:
        raise BenchmarkError(f"qrels file exceeds the {MAX_QREL_ROWS}-row safety limit")
    qrels: dict[str, dict[str, int]] = {}
    for line_number, line in enumerate(lines[1:], start=2):
        fields = line.split("\t")
        if len(fields) != 3:
            raise BenchmarkError(f"qrels line {line_number} does not have three fields")
        query_id, document_id, score_text = fields
        validate_document_id(document_id)
        try:
            score = int(score_text)
        except ValueError as error:
            raise BenchmarkError(f"qrels line {line_number} has a non-integer score") from error
        if score < 0:
            raise BenchmarkError(f"qrels line {line_number} has a negative score")
        grades = qrels.setdefault(query_id, {})
        if document_id in grades:
            raise BenchmarkError(f"qrels line {line_number} repeats a judgment")
        grades[document_id] = score
    return qrels


def select_queries(
    queries_path: Path, qrels: dict[str, dict[str, int]]
) -> list[dict[str, str]]:
    """Keep the split's judged queries in the order BEIR lists them."""
    selected: list[dict[str, str]] = []
    for row in read_json_lines(queries_path, MAX_QREL_ROWS):
        query_id = str(row["_id"])
        if query_id not in qrels:
            continue
        text = str(row["text"]).strip()
        if not text:
            raise BenchmarkError(f"query {query_id!r} has no text")
        selected.append({"query_id": query_id, "query": text})
    return selected


def setup() -> None:
    FIXTURE_ROOT.mkdir(parents=True, exist_ok=True)
    archive = download_verified(
        DATASET.archive_url,
        FIXTURE_ROOT / "downloads" / f"{DATASET.name}.zip",
        DATASET.archive_sha256,
    )
    dataset_dir = extract_archive(archive)
    document_count = materialize_documents(dataset_dir / "corpus.jsonl")
    qrels_source = dataset_dir / "qrels" / f"{DATASET.split}.tsv"
    qrels = parse_qrels(qrels_source.read_text(encoding="utf-8"))
    qrel_count = sum(len(grades) for grades in qrels.values())
    if qrel_count != DATASET.qrel_count:
        raise BenchmarkError(
            f"qrels have {qrel_count} judgments; the pin expects {DATASET.qrel_count}"
        )
    queries = select_queries(dataset_dir / "queries.jsonl", qrels)
    if len(queries) != DATASET.query_count:
        raise BenchmarkError(
            f"split has {len(queries)} judged queries; the pin expects {DATASET.query_count}"
        )
    shutil.copyfile(qrels_source, FIXTURE_ROOT / "qrels.tsv")
    write_json_lines(FIXTURE_ROOT / "queries.jsonl", queries, MAX_QUERIES)
    write_json(
        FIXTURE_ROOT / "setup.json",
        {
            **benchmark_pins(),
            "materialized_document_count": document_count,
            "completed_at": utc_now(),
        },
    )
    print(f"BEIR {DATASET.name} is ready at {FIXTURE_ROOT}")


def composition_text(options: RunOptions) -> str:
    return f'''[[entry]]
id = "fs"
[entry.config]
host_id = "beir-{DATASET.name}"
skip_hidden = true
gitignore = false
ignore = []

[[entry]]
id = "llm"
[entry.config]
base_url = "{OPENROUTER_BASE_URL}"
api_key_env = "OPENROUTER_API_KEY"
transform_model = "{SUMMARIZATION_MODEL}"
transform_reasoning_effort = "low"

[[entry]]
id = "embedder"
[entry.config]
provider = "endpoint"
model = "{EMBEDDING_MODEL}"
dimensions = {options.embedding_dimensions}
vectors = "{EMBEDDING_VECTORS}"

[[entry]]
id = "markdown"
disabled = {"false" if options.structural == "markdown" else "true"}

[[entry]]
id = "finder"
[entry.config]
seeds = "{options.finder_seeds}"
max_vector_distance = {options.finder_max_vector_distance}

[[entry]]
id = "chunker"
disabled = true

[[entry]]
id = "summarizer"
[entry.config]
target_chars = {options.summary_target_chars}
keywords_max = {options.keywords_max}
llm_call_budget = {options.llm_call_budget}
llm_lane = "{options.summarization_lane}"

[[entry]]
id = "entities"
disabled = true

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


def load_queries(count: int) -> list[dict[str, Any]]:
    queries = read_json_lines(FIXTURE_ROOT / "queries.jsonl", MAX_QUERIES)
    if len(queries) < count:
        raise BenchmarkError(f"requested {count} queries but the fixture has {len(queries)}")
    selected = queries[:count]
    assert len(selected) == count
    return selected


def load_qrels() -> dict[str, dict[str, int]]:
    return parse_qrels((FIXTURE_ROOT / "qrels.tsv").read_text(encoding="utf-8"))


def load_document_ids(corpus: str = "text") -> set[str]:
    documents = FIXTURE_ROOT / CORPUS_DIRECTORIES[corpus]
    suffix = CORPUS_SUFFIXES[corpus]
    document_ids: set[str] = set()
    for path in documents.glob(f"*{suffix}"):
        if len(document_ids) >= MAX_DOCUMENTS:
            raise BenchmarkError(f"fixture exceeds the {MAX_DOCUMENTS}-document safety limit")
        document_ids.add(path.name[: -len(suffix)])
    if len(document_ids) != DATASET.document_count:
        raise BenchmarkError(
            f"fixture has {len(document_ids)} documents; expected {DATASET.document_count}"
        )
    return document_ids


def document_id_from_address(
    address: str, document_ids: set[str], suffix: str = DOCUMENT_FILE_SUFFIX
) -> str:
    """Map a Finder result address back to the BEIR document it was written from."""
    file_name = address.rsplit("/", 1)[-1]
    if not file_name.endswith(suffix):
        raise BenchmarkError(f"result address {address!r} is not a benchmark document")
    document_id = file_name[: -len(suffix)]
    if document_id not in document_ids:
        raise BenchmarkError(f"result address {address!r} names an unknown document")
    return document_id


def retrieved_document_ids(
    results: list[dict[str, Any]], document_ids: set[str], suffix: str = DOCUMENT_FILE_SUFFIX
) -> list[str]:
    ordered = [
        document_id_from_address(str(result["address"]), document_ids, suffix)
        for result in results
    ]
    if len(set(ordered)) != len(ordered):
        raise BenchmarkError("Finder returned the same document twice in one ranking")
    return ordered


def query_command(
    query: dict[str, Any],
    options: RunOptions,
    data_dir: Path,
    composition: Path,
    log_dir: Path,
    document_ids: set[str],
    progress_prefix: str,
) -> dict[str, Any]:
    query_id = str(query["query_id"])
    query_text = str(query["query"])
    result = run_logged(
        query_arguments(data_dir, composition, query_text, options.results_per_query),
        log_dir / f"{query_id}-query.log",
        timeout_seconds=QUERY_TIMEOUT_SECONDS,
        progress_label=f"{progress_prefix} retrieval",
    )
    require_success(result, f"querying {query_id}")
    results = parse_query_results(result.stdout, options.results_per_query)
    return {
        "query_id": query_id,
        "query": query_text,
        "retrieval_duration_seconds": round(result.duration_seconds, 6),
        "results": results,
        "retrieved_document_ids": retrieved_document_ids(
            results, document_ids, CORPUS_SUFFIXES[options.corpus]
        ),
    }


def metric_cutoffs(results_per_query: int) -> list[int]:
    assert 0 < results_per_query <= MAX_QUERY_RESULTS
    cutoffs = [cutoff for cutoff in METRIC_CUTOFFS if cutoff <= results_per_query]
    if results_per_query not in cutoffs:
        cutoffs.append(results_per_query)
    assert cutoffs == sorted(cutoffs)
    return cutoffs


def score_query(
    retrieved: list[str], grades: dict[str, int], cutoffs: list[int]
) -> dict[str, float]:
    """trec_eval's ndcg_cut, map_cut, recall, and P for one ranking.

    Gains are the raw grades with a log2(rank + 1) discount, and recall and
    average precision divide by every judged relevant document, not only the
    ones inside the cutoff. Those are the conventions BEIR reports with.
    """
    assert cutoffs
    relevant_count = sum(1 for grade in grades.values() if grade > 0)
    if relevant_count == 0:
        raise BenchmarkError("cannot score a query with no relevant document")
    ideal_gains = sorted(grades.values(), reverse=True)
    metrics: dict[str, float] = {}
    for cutoff in cutoffs:
        gains = [grades.get(document_id, 0) for document_id in retrieved[:cutoff]]
        dcg = sum(gain / math.log2(rank + 1) for rank, gain in enumerate(gains, start=1))
        ideal_dcg = sum(
            gain / math.log2(rank + 1)
            for rank, gain in enumerate(ideal_gains[:cutoff], start=1)
        )
        assert ideal_dcg > 0.0
        hit_count = 0
        precision_sum = 0.0
        for rank, gain in enumerate(gains, start=1):
            if gain > 0:
                hit_count += 1
                precision_sum += hit_count / rank
        assert hit_count <= min(cutoff, relevant_count)
        metrics[f"ndcg@{cutoff}"] = dcg / ideal_dcg
        metrics[f"map@{cutoff}"] = precision_sum / relevant_count
        metrics[f"recall@{cutoff}"] = hit_count / relevant_count
        metrics[f"precision@{cutoff}"] = hit_count / cutoff
    return metrics


def beir_scores(
    queries: list[dict[str, Any]],
    qrels: dict[str, dict[str, int]],
    cutoffs: list[int],
) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    """Average each metric over the queries that ran, BEIR style."""
    if not queries:
        raise BenchmarkError("cannot score a run with no completed queries")
    per_query: list[dict[str, Any]] = []
    totals: dict[str, float] = {}
    for query in queries:
        query_id = str(query["query_id"])
        grades = qrels.get(query_id)
        if grades is None:
            raise BenchmarkError(f"query {query_id!r} has no judgments")
        metrics = score_query(query["retrieved_document_ids"], grades, cutoffs)
        per_query.append({"query_id": query_id, **metrics})
        for name, value in metrics.items():
            totals[name] = totals.get(name, 0.0) + value
    aggregate: dict[str, Any] = {
        name: round(total / len(queries), 5) for name, total in totals.items()
    }
    aggregate["queries_evaluated"] = len(queries)
    aggregate["cutoffs"] = cutoffs
    aggregate["semantics"] = (
        "trec_eval ndcg_cut, map_cut, recall, and P averaged over queries; "
        "linear gains; relevance grade > 0 counts as relevant"
    )
    return aggregate, per_query


def write_trec_run(run_dir: Path, queries: list[dict[str, Any]]) -> None:
    """Write the ranking in TREC run format so trec_eval can rescore it."""
    lines: list[str] = []
    for query in queries:
        ranking = query["retrieved_document_ids"]
        results = query["results"]
        assert len(ranking) == len(results)
        for rank, (document_id, result) in enumerate(zip(ranking, results), start=1):
            score = float(result["score"])
            lines.append(f"{query['query_id']} Q0 {document_id} {rank} {score:.6f} inseam")
    (run_dir / "run.trec").write_text("".join(f"{line}\n" for line in lines), encoding="utf-8")


def write_run_checkpoint(run_dir: Path, queries: list[dict[str, Any]]) -> None:
    write_json_lines(run_dir / "queries.jsonl", queries, MAX_QUERIES)


def benchmark_pins() -> dict[str, Any]:
    """The suite name plus every dataset pin, with the dataset under `dataset`."""
    pins: dict[str, Any] = {"name": "BEIR", "dataset": DATASET.name}
    pins.update({key: value for key, value in vars(DATASET).items() if key != "name"})
    assert pins["name"] == "BEIR"
    return pins


def model_assignments(options: RunOptions) -> dict[str, str | int]:
    return {
        "summarization": SUMMARIZATION_MODEL,
        "summarization_lane": options.summarization_lane,
        "corpus": options.corpus,
        "structural": options.structural,
        "finder_seeds": options.finder_seeds,
        "finder_max_vector_distance": options.finder_max_vector_distance,
        "entity_extraction": "disabled",
        "embeddings": EMBEDDING_MODEL,
        "embedding_dimensions": options.embedding_dimensions,
        "embedding_vectors": EMBEDDING_VECTORS,
    }


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
            "indexing": INDEX_TIMEOUT_SECONDS,
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


def index_documents(
    run_dir: Path, data_dir: Path, composition: Path, log_path: Path, corpus: str = "text"
) -> dict[str, Any]:
    return harness.index_documents(
        run_dir,
        data_dir,
        composition,
        FIXTURE_ROOT / CORPUS_DIRECTORIES[corpus],
        fixture_document_count(FIXTURE_ROOT, MAX_DOCUMENTS),
        SOURCES_MAX,
        f"BEIR {DATASET.name}",
        log_path,
    )


def run_queries(
    run_dir: Path,
    data_dir: Path,
    composition: Path,
    manifest: dict[str, Any],
    queries_to_run: list[dict[str, Any]],
    options: RunOptions,
    queries: list[dict[str, Any]],
    log_dir: Path,
) -> list[dict[str, Any]]:
    completed_count = len(queries)
    assert completed_count <= len(queries_to_run)
    document_ids = load_document_ids(options.corpus)
    print(f"Query checkpoint: {completed_count}/{len(queries_to_run)} completed", flush=True)
    for index in range(completed_count, len(queries_to_run)):
        query = queries_to_run[index]
        progress_prefix = f"[{index + 1}/{len(queries_to_run)}] {query['query_id']}"
        started_at = utc_now()
        record = query_command(
            query, options, data_dir, composition, log_dir, document_ids, progress_prefix
        )
        record["started_at"] = started_at
        record["finished_at"] = utc_now()
        queries.append(record)
        assert len(queries) == index + 1
        write_run_checkpoint(run_dir, queries)
        manifest["queries_completed"] = len(queries)
        write_json(run_dir / "manifest.json", manifest)
    assert len(queries) == len(queries_to_run)
    return queries


def require_fixture() -> None:
    if not (FIXTURE_ROOT / "setup.json").exists():
        raise BenchmarkError(f"benchmark fixture is missing; run `{sys.argv[0]} setup` first")


def run_benchmark(options: RunOptions) -> None:
    require_fixture()
    require_api_key()
    queries_to_run = load_queries(options.query_count)
    run_dir, data_dir, manifest = create_run(options)
    composition = run_dir / "composition.toml"
    composition.write_text(composition_text(options), encoding="utf-8")
    execute_benchmark(
        run_dir,
        data_dir,
        manifest,
        composition,
        queries_to_run,
        options,
        [],
        True,
        manifest["inseam"],
    )


def resume_benchmark(run_id: str, after_kill: bool = False) -> None:
    require_fixture()
    require_api_key()
    loaded = load_resumable_run(run_id, after_kill)
    run_dir, data_dir, manifest, composition, queries_to_run, options, queries, index_required = loaded
    write_run_checkpoint(run_dir, queries)
    execute_benchmark(
        run_dir,
        data_dir,
        manifest,
        composition,
        queries_to_run,
        options,
        queries,
        index_required,
        inseam_identity(),
    )


def execute_benchmark(
    run_dir: Path,
    data_dir: Path,
    manifest: dict[str, Any],
    composition: Path,
    queries_to_run: list[dict[str, Any]],
    options: RunOptions,
    queries: list[dict[str, Any]],
    index_required: bool,
    identity: dict[str, Any],
) -> None:
    initial_phase = "indexing" if index_required else "querying"
    started, log_dir = begin_attempt(run_dir, manifest, initial_phase, identity)
    print_run_start(run_dir, data_dir, manifest, queries_to_run, queries, index_required)
    error: BaseException | None = None
    try:
        if index_required:
            # The first attempt's log is the run's; a resumed attempt's own
            # directory keeps every indexing attempt's log apart.
            attempt = manifest["attempts"][-1]
            if attempt["attempt_number"] == 1:
                index_log = run_dir / "logs" / "index.log"
            else:
                index_log = log_dir / "index.log"
            attempt["indexing_log"] = str(index_log.relative_to(run_dir))
            write_json(run_dir / "manifest.json", manifest)
            manifest["indexing"] = index_documents(
                run_dir, data_dir, composition, index_log, options.corpus
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
            run_dir, data_dir, composition, manifest, queries_to_run, options, queries, log_dir
        )
        update_run_phase(run_dir, manifest, "evaluating")
        aggregate, per_query = beir_scores(
            queries, load_qrels(), metric_cutoffs(options.results_per_query)
        )
        write_json_lines(run_dir / "query-scores.jsonl", per_query, MAX_QUERIES)
        write_trec_run(run_dir, queries)
        complete_run(manifest, {"beir": aggregate})
        print_scores(aggregate)
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
    queries_to_run: list[dict[str, Any]],
    queries: list[dict[str, Any]],
    index_required: bool,
) -> None:
    verb = "Starting" if index_required else "Resuming"
    print(f"{verb} BEIR {DATASET.name} run {manifest['run_id']}", flush=True)
    print(f"  attempt: {len(manifest['attempts'])}", flush=True)
    print(f"  artifacts: {run_dir}", flush=True)
    print(f"  index data: {data_dir}", flush=True)
    print(f"  queries: {len(queries)}/{len(queries_to_run)} completed", flush=True)


def print_scores(aggregate: dict[str, Any]) -> None:
    for name in ("ndcg", "map", "recall", "precision"):
        values = [
            f"@{cutoff} {aggregate[f'{name}@{cutoff}']:.4f}" for cutoff in aggregate["cutoffs"]
        ]
        print(f"  {name:<9} {'  '.join(values)}", flush=True)


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
    if manifest.get("schema_version") != 2:
        raise BenchmarkError(f"cannot resume manifest schema {manifest.get('schema_version')!r}")
    if type(manifest.get("attempts")) is not list:
        raise BenchmarkError("run manifest has no attempts list")
    validate_resumable_status(run_id, manifest, after_kill)
    if manifest.get("benchmark") != benchmark_pins():
        raise BenchmarkError(f"run `{run_id}` uses different benchmark inputs")
    options = options_from_manifest(manifest)
    if manifest.get("models") != model_assignments(options):
        raise BenchmarkError(f"run `{run_id}` uses different models")
    queries_to_run = load_queries(options.query_count)
    composition = run_dir / "composition.toml"
    if not composition.is_file():
        raise BenchmarkError(f"run `{run_id}` has no composition.toml")
    if composition.read_text(encoding="utf-8") != composition_text(options):
        raise BenchmarkError(f"run `{run_id}` composition does not match its options")
    data_dir = validate_index_data_path(run_id, manifest, FIXTURE_ROOT)
    # No index record means the index attempt never completed: the resumed
    # attempt runs the reconciling sweep again in the same node, which
    # skips every source whose indexed mark is durable.
    index_required = manifest.get("indexing") is None
    if not index_required:
        load_index_completion(run_dir, manifest, SOURCES_MAX)
    queries = load_completed_queries(run_dir, manifest, queries_to_run)
    return run_dir, data_dir, manifest, composition, queries_to_run, options, queries, index_required


def options_from_manifest(manifest: dict[str, Any]) -> RunOptions:
    value = manifest_options_object(manifest, set(RunOptions.__annotations__))
    return RunOptions(
        query_count=manifest_option_integer(value, "query_count", MAX_QUERIES),
        results_per_query=manifest_option_integer(
            value, "results_per_query", MAX_QUERY_RESULTS
        ),
        index_concurrency=manifest_option_integer(
            value, "index_concurrency", MAX_INDEX_CONCURRENCY
        ),
        llm_call_budget=manifest_option_integer(
            value, "llm_call_budget", MAX_LLM_CALL_BUDGET
        ),
        summary_target_chars=manifest_option_integer(
            value, "summary_target_chars", MAX_SUMMARY_TARGET_CHARS
        ),
        summarization_lane=manifest_option_lane(value, "summarization_lane"),
        keywords_max=manifest_option_count(value, "keywords_max", MAX_KEYWORDS),
        embedding_dimensions=manifest_option_integer(
            value, "embedding_dimensions", EMBEDDING_DIMENSIONS_MAX
        ),
        corpus=manifest_option_choice(value, "corpus", frozenset(CORPUS_DIRECTORIES)),
        structural=manifest_option_choice(value, "structural", STRUCTURAL_CHOICES),
        finder_seeds=manifest_option_choice(value, "finder_seeds", FINDER_SEEDS),
        finder_max_vector_distance=manifest_option_distance(value, "finder_max_vector_distance"),
    )


def manifest_option_lane(value: dict[str, Any], name: str) -> str:
    option = value[name]
    if option not in SUMMARIZATION_LANES:
        raise BenchmarkError(f"run option {name} is not one of {sorted(SUMMARIZATION_LANES)}")
    return option


def load_completed_queries(
    run_dir: Path,
    manifest: dict[str, Any],
    queries_to_run: list[dict[str, Any]],
) -> list[dict[str, Any]]:
    queries = read_json_lines(run_dir / "queries.jsonl", MAX_QUERIES)
    if len(queries) > len(queries_to_run):
        raise BenchmarkError("query checkpoint has more records than requested queries")
    for index, query in enumerate(queries):
        expected_id = str(queries_to_run[index]["query_id"])
        if query.get("query_id") != expected_id:
            raise BenchmarkError(f"query checkpoint differs at query {index + 1}")
        if type(query.get("retrieved_document_ids")) is not list:
            raise BenchmarkError(f"query checkpoint has no retrievals at query {index + 1}")
        if type(query.get("results")) is not list:
            raise BenchmarkError(f"query checkpoint has no results at query {index + 1}")
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
    subparsers.add_parser("setup", help=f"download and verify BEIR {DATASET.name}")
    run_parser = subparsers.add_parser("run", help="index, query, and score a new run")
    run_parser.add_argument(
        "--limit",
        type=bounded_argument("limit", MAX_QUERIES),
        default=DATASET.query_count,
        help=f"queries to run (default: every {DATASET.split} query)",
    )
    run_parser.add_argument(
        "--query-limit",
        type=bounded_argument("query-limit", MAX_QUERY_RESULTS),
        default=10,
        help="Finder results per query; also the deepest metric cutoff",
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
        "--summarization-lane",
        choices=sorted(SUMMARIZATION_LANES),
        default=SUMMARIZATION_LANE,
        help="the lane summary calls ride; interactive suits a run that makes few calls",
    )
    run_parser.add_argument(
        "--keywords-max",
        type=count_argument("keywords-max", MAX_KEYWORDS),
        default=12,
        help="keywords planted beside each summary for full-text search; 0 plants none",
    )
    run_parser.add_argument(
        "--embedding-dimensions",
        type=bounded_argument("embedding-dimensions", EMBEDDING_DIMENSIONS_MAX),
        default=EMBEDDING_DIMENSIONS,
        help="the embedder's width; Matryoshka, so anything up to the native 1536",
    )
    run_parser.add_argument(
        "--corpus",
        choices=sorted(CORPUS_DIRECTORIES),
        default="text",
        help="which materialized corpus to index; markdown puts the title in a # heading",
    )
    run_parser.add_argument(
        "--structural",
        choices=sorted(STRUCTURAL_CHOICES),
        default="off",
        help="whether the markdown structural transform runs over the roots",
    )
    run_parser.add_argument(
        "--finder-seeds",
        choices=sorted(FINDER_SEEDS),
        default="both",
        help="the finder's seed lists; one alone shows which search the fusion is carrying",
    )
    run_parser.add_argument(
        "--finder-max-vector-distance",
        type=distance_argument,
        default=0.75,
        help="vector hits farther than this cosine distance are dropped; 1.0 keeps all",
    )
    run_parser.add_argument(
        "--summary-target-chars",
        type=bounded_argument("summary-target-chars", MAX_SUMMARY_TARGET_CHARS),
        default=200,
        help="summarizer target length; text within it is its own summary, no model call",
    )
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


def main() -> int:
    arguments = parse_arguments()
    if arguments.command == "setup":
        return run_main(setup)
    if arguments.command == "run":
        options = RunOptions(
            query_count=arguments.limit,
            results_per_query=arguments.query_limit,
            index_concurrency=arguments.index_concurrency,
            llm_call_budget=arguments.llm_call_budget,
            summary_target_chars=arguments.summary_target_chars,
            summarization_lane=arguments.summarization_lane,
            keywords_max=arguments.keywords_max,
            embedding_dimensions=arguments.embedding_dimensions,
            corpus=arguments.corpus,
            structural=arguments.structural,
            finder_seeds=arguments.finder_seeds,
            finder_max_vector_distance=arguments.finder_max_vector_distance,
        )
        return run_main(lambda: run_benchmark(options))
    return run_main(lambda: resume_benchmark(arguments.run_id, arguments.after_kill))


if __name__ == "__main__":
    raise SystemExit(main())
