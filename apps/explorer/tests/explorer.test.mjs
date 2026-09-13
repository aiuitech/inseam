import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { parse } from 'smol-toml';
import { composition, finderOptions, defaultComposition } from '../config.mjs';
import { createIndex, library, discover } from '../library.mjs';
import { Runner } from '../process.mjs';

test('query settings reject invalid modes and unbounded work', () => {
  for (const options of [{ mode: 'shell' }, { seedK: 1001 }, { hops: -1 }, { damping: 1 }, { damping: 'nan' }]) {
    assert.throws(() => finderOptions(options));
  }
  assert.equal(finderOptions({ mode: 'full-text', hops: 0 }).graph_hops, 0);
});
test('working composition preserves source settings and disables network access', () => {
  const source = '[[entry]]\nid="fs"\n[entry.config]\nmachine_id="original"\n[[entry]]\nid="finder"\n[entry.config]\nrrf_k=20\n';
  const document = parse(composition(source, { finder: finderOptions({}) }));
  assert.equal(document.entry.find(entry => entry.id === 'fs').config.machine_id, 'original');
  assert.equal(document.entry.find(entry => entry.id === 'finder').config.rrf_k, 20);
  for (const id of ['transport', 'sync', 'routing', 'google']) assert.equal(document.entry.find(entry => entry.id === id).disabled, true);
  assert.throws(() => composition('entry="wrong"'));
});
test('index import publishes only complete snapshots and preserves original bytes', async () => {
  const root = await mkdtemp(join(tmpdir(), 'inseam-explorer-'));
  try {
    const source = join(root, 'original'); await mkdir(source);
    const config = join(source, 'composition.toml'); await writeFile(config, '');
    const database = join(source, 'catalog.sqlite3'); await writeFile(database, 'source bytes');
    const indexes = join(root, 'indexes'); await mkdir(indexes);
    const index = await createIndex(indexes, { name: 'Copy', data: source, composition: config }, null,
      async (from, to) => writeFile(to, await readFile(from)));
    assert.equal((await library(indexes)).length, 1);
    assert.equal(await readFile(database, 'utf8'), 'source bytes');
    assert.notEqual(index.id, 'original');
    await assert.rejects(createIndex(indexes, { name: 'Broken', data: source, composition: config }, null,
      async () => { throw new Error('backup failed'); }));
    assert.equal((await library(indexes)).length, 1);
  } finally { await rm(root, { recursive: true, force: true }); }
});
test('runner serializes work and propagates process failure', async () => {
  const runner = new Runner();
  let release;
  const promise = runner.exclusive(() => new Promise(resolve => { release = resolve; }));
  await assert.rejects(runner.exclusive(async () => {}), /already running/);
  release(); await promise;
  await assert.rejects(runner.exclusive(() => runner.command(process.execPath, ['-e', 'process.exit(7)'])), /7/);
  assert.equal(runner.busy, false);
});
test('benchmark discovery includes missing data without pretending it is available', async () => {
  const root = await mkdtemp(join(tmpdir(), 'inseam-runs-'));
  try {
    const run = join(root, 'benchmarks/runs/example/run'); await mkdir(run, { recursive: true });
    await writeFile(join(run, 'manifest.json'), JSON.stringify({ index_data_path: '/missing/inseam', status: 'failed' }));
    const runs = await discover(root); assert.equal(runs.length, 1); assert.equal(runs[0].available, false);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('new indexes disable all default model calls', () => {
  const entries = parse(composition(defaultComposition)).entry;
  assert.equal(entries.find(entry => entry.id === 'llm').disabled, true);
  assert.equal(entries.find(entry => entry.id === 'hints').disabled, true);
  assert.equal(entries.find(entry => entry.id === 'summarizer').config.llm_call_budget, 0);
  assert.equal(entries.find(entry => entry.id === 'embedder').config.provider, 'none');
});
