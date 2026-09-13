#!/usr/bin/env python3
"""Run and resume full GLM evaluations, with bounded execution retries."""
import argparse
import json
from pathlib import Path
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
import enterprise_rag_bench as enterprise
import harness
import requery

ATTEMPTS = 3


def run_queries(run_dir, data_dir, composition, manifest, inputs, options, rows, logs):
    assert len(inputs) <= enterprise.MAX_QUESTIONS
    manifest['execution'] = {'answer_workers':1, 'attempts_per_question_max':ATTEMPTS,
        'retry_note':'Retries apply only to failed execution, never to judged answer quality.'}
    completed = {row['question_id']:row for row in rows}
    for question in inputs:
        if question['question_id'] in completed:
            continue
        for attempt in range(1, ATTEMPTS + 1):
            started = harness.utc_now()
            attempt_logs = logs / 'continued' / f"{manifest.get('resume_count', 0)}-attempt-{attempt}"
            try:
                record = enterprise.question_commands(question, options, data_dir,
                    composition, attempt_logs, f"[{question['question_id']}]")
                record.update(started_at=started, finished_at=harness.utc_now(),
                              answer_attempts=attempt)
                completed[question['question_id']] = record
                break
            except harness.BenchmarkError as error:
                manifest.setdefault('execution_errors', []).append({
                    'question_id':question['question_id'], 'attempt':attempt, 'error':str(error)})
                harness.write_json(run_dir / 'manifest.json', manifest)
                if attempt == ATTEMPTS:
                    raise
        rows = [completed[row['question_id']] for row in inputs if row['question_id'] in completed]
        enterprise.write_run_checkpoint(run_dir, rows)
        manifest['queries_completed'] = len(rows)
        harness.write_json(run_dir / 'manifest.json', manifest)
        print(f"Completed {len(rows)}/{len(inputs)} questions", flush=True)
    assert len(rows) == len(inputs)
    enterprise.write_retrieval_attribution(run_dir, rows, inputs)
    return rows


def resume(path):
    manifest = harness.read_json_object(path / 'manifest.json')
    assert manifest['status'] in ('interrupted', 'failed')
    assert harness.inseam_identity()['binary_sha256'] == manifest['inseam']['binary_sha256']
    inputs = enterprise.load_questions(enterprise.MAX_QUESTIONS, exact=False)
    rows = [json.loads(line) for line in (path / 'queries.jsonl').read_text().splitlines()]
    assert len({row['question_id'] for row in rows}) == len(rows)
    options = enterprise.RunOptions(**manifest['options'])
    prior_seconds = manifest.get('duration_seconds', 0)
    manifest.setdefault('continuations', []).append({'started_at':harness.utc_now(),
        'prior_status':manifest['status'], 'prior_duration_seconds':prior_seconds,
        'prior_queries_completed':len(rows)})
    manifest['resume_count'] = manifest.get('resume_count', 0) + 1
    manifest.update(status='running', phase='querying')
    started = time.monotonic()
    data_dir = Path(manifest['data_dir'])
    try:
        rows = run_queries(path, data_dir, path / 'composition.toml', manifest,
                                inputs, options, rows, path / 'logs')
        manifest['phase'] = 'evaluating'
        harness.write_json(path / 'manifest.json', manifest)
        manifest['scores'] = requery.score(enterprise, path, rows, inputs, options)
        harness.write_retrieval_observability(path, rows)
        harness.record_final_index_footprint(manifest, data_dir)
        manifest.update(status='completed', phase='completed')
    except BaseException as error:
        manifest.update(status='failed', phase='failed', error=str(error))
        raise
    finally:
        manifest['duration_seconds'] = round(prior_seconds + time.monotonic() - started, 6)
        manifest['finished_at'] = harness.utc_now()
        harness.write_json(path / 'manifest.json', manifest)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    choice = parser.add_mutually_exclusive_group(required=True)
    choice.add_argument('--resume', type=Path)
    choice.add_argument('--origin')
    args = parser.parse_args()
    if args.resume:
        resume(args.resume.resolve())
    else:
        enterprise.run_queries = run_queries
        arguments = argparse.Namespace(benchmark='enterprise', run_id=args.origin,
            bookend_count=250, answer_model='z-ai/glm-5.3-flash',
            evaluation_model='z-ai/glm-5.3-flash', **dict.fromkeys(requery.QUERY_OPTIONS))
        arguments.finder_lexical_weight = 0.1
        requery.run(arguments)

if __name__ == '__main__':
    main()
