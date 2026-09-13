#!/usr/bin/env python3
"""Judged question replay through a bounded batch sharing one database owner."""
import argparse
import functools
import json
from pathlib import Path
import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
import enterprise_rag_bench as enterprise
import harness
import requery

ATTEMPTS_MAX = 3


def convert(question, raw, attempt):
    retrieval = raw['retrieval']
    results = retrieval['results'][:8]
    initial = enterprise.extract_document_ids([row['address'] for row in results])
    discovered = enterprise.extract_document_ids([json.dumps(raw['events']), raw['answer']])
    return {'question_id':question['question_id'], 'question':question['question'],
        'question_type':question.get('question_type'), 'results':results,
        'query_meta':retrieval['meta'], 'retrieved_document_ids':initial,
        'agent_document_ids':discovered,
        'document_ids':enterprise.extract_document_ids([*initial, *discovered]),
        'attribution':enterprise.attribution_rows(question, retrieval['results'], retrieval['meta']),
        'answer':raw['answer'], 'answer_attempts':attempt, 'turns':raw['turns'],
        'tool_calls':raw['tool_calls'],
        **{key:raw[key] for key in ('retrieval_duration_seconds','answer_duration_seconds','duration_seconds')}}


def run_queries(run_dir, data_dir, composition, manifest, inputs, options, rows, logs, *, effort=None):
    assert 1 <= len(inputs) <= 500
    assert options.query_limit == 8
    completed = {row['question_id']:row for row in rows}
    manifest['execution'] = {'answer_workers_max':8, 'database_owners':1,
        'answer_reasoning_effort':effort or 'endpoint-default', 'judge_reasoning_effort':effort or 'medium',
        'attempts_per_question_max':ATTEMPTS_MAX, 'cost_scope':'batch provider total; per-question cost unavailable'}
    for attempt in range(1, ATTEMPTS_MAX + 1):
        pending = [row for row in inputs if row['question_id'] not in completed]
        if not pending:
            break
        output = run_dir / 'batch' / f'attempt-{attempt}'
        output.mkdir(parents=True, exist_ok=True)
        question_file = output / 'input.jsonl'
        harness.write_json_lines(question_file, [{key:row[key] for key in ('question_id','question')}
                                                for row in pending], enterprise.MAX_QUESTIONS)
        effort_arguments = ['--reasoning-effort', effort] if effort else []
        result = harness.run_logged([*harness.inseam_arguments(data_dir, composition), 'agent-batch',
            '--input', str(question_file), '--output', str(output), '--model', options.answer_model,
            '--turns', str(options.turns), *effort_arguments],
            logs / f'batch-{attempt}.log', timeout_seconds=21600,
            progress_label=f'Answering {len(pending)} questions with eight shared-node sessions')
        manifest.setdefault('batch_attempts', []).append({'attempt':attempt,
            'duration_seconds':result.duration_seconds, 'returncode':result.returncode})
        for question in pending:
            path = output / f"{question['question_id']}.json"
            if path.exists():
                raw = harness.read_json_object(path)
                assert raw['question_id'] == question['question_id']
                assert raw['answer'].strip()
                completed[question['question_id']] = convert(question, raw, attempt)
        rows = [completed[row['question_id']] for row in inputs if row['question_id'] in completed]
        enterprise.write_run_checkpoint(run_dir, rows)
        manifest['queries_completed'] = len(rows)
        harness.write_json(run_dir / 'manifest.json', manifest)
    if len(rows) != len(inputs):
        raise harness.BenchmarkError(f'Only {len(rows)}/{len(inputs)} questions completed after three execution attempts')
    enterprise.write_retrieval_attribution(run_dir, rows, inputs)
    return rows


def evaluate(run_dir, log_dir, parallelism, question_count, model):
    evaluator = enterprise.FIXTURE_ROOT / 'evaluator'
    results = run_dir / 'enterprise-rag-bench-results.json'
    result = harness.run_logged([str(evaluator / '.venv/bin/python'),
        str(Path(__file__).with_name('judge_low.py')), '--answers-file', str(run_dir / 'answers.jsonl'),
        '--questions-file', str(enterprise.FIXTURE_ROOT / 'questions.jsonl'), '--results-file', str(results),
        '--parallelism', str(parallelism), '--no-correction'], log_dir / 'evaluation.log',
        timeout_seconds=enterprise.EVALUATION_TIMEOUT_SECONDS,
        progress_label=f'Evaluating {question_count} answers with GLM low effort',
        cwd=evaluator, environment=enterprise.evaluator_environment(model))
    harness.require_success(result, 'evaluating answers')
    payload = harness.read_json_object(results)
    return {'duration_seconds':result.duration_seconds, 'aggregate_stats':payload['aggregate_stats'],
        'question_type_stats':payload['question_type_stats'], 'raw_results':results.name}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('origin')
    parser.add_argument('--model', default='openai/gpt-5.4')
    parser.add_argument('--bookend-count', type=int, default=50)
    parser.add_argument('--reasoning-effort', choices=['low'])
    args = parser.parse_args()
    if not 1 <= args.bookend_count <= 250:
        parser.error('bookend count must be between 1 and 250')
    enterprise.run_queries = functools.partial(run_queries, effort=args.reasoning_effort)
    if args.reasoning_effort:
        enterprise.evaluate = evaluate
    arguments = argparse.Namespace(benchmark='enterprise', run_id=args.origin,
        bookend_count=args.bookend_count, answer_model=args.model, evaluation_model=args.model,
        **dict.fromkeys(requery.QUERY_OPTIONS))
    arguments.finder_lexical_weight = 0.1
    requery.run(arguments)

if __name__ == '__main__':
    main()
