import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Explorer } from '../service.mjs';
import { composition } from '../config.mjs';

test('offline index supports every search mode and survives a consistent snapshot', async () => {
  const root = await mkdtemp(join(tmpdir(), 'inseam-explorer-integration-'));
  const repository = fileURLToPath(new URL('../../../', import.meta.url));
  const explorer = new Explorer(repository, join(root, 'indexes'), process.env.INSEAM_BINARY ?? 'inseam');
  try {
    const sources = join(root, 'sources');
    await mkdir(sources);
    await writeFile(join(sources, 'coffee.md'), '# Coffee\nEspresso extraction uses finely ground coffee.\n');
    await writeFile(join(sources, 'tea.md'), '# Tea\nGreen tea brewing uses cooler water.\n');
    const index = await explorer.runner.exclusive(() => explorer.import({ name: 'Offline' }));
    const directory = await explorer.directory(index.id);
    const before = await explorer.request('overview', { id: index.id });
    assert.equal(before.totals.sources, 0);
    const configuration = (await explorer.request('config', { id: index.id })).text;
    await explorer.saveConfig(directory, { text: composition(configuration, { embedder: { provider: 'hashed', dimensions: 64 } }) });
    const indexed = await explorer.runner.exclusive(() => explorer.index(directory, { root: sources }));
    assert.match(indexed.output, /\$0\.0000 spent/);
    const saved = await readFile(join(directory, 'composition.toml'), 'utf8');
    for (const mode of ['both', 'full-text', 'vector']) {
      const response = await explorer.request('query', { id: index.id, text: 'coffee espresso', mode });
      assert.ok(response.results.length);
      assert.equal(response.meta.evidence.length, response.results.length);
      if (mode === 'vector') {
        assert.equal(response.meta.fts_hits, 0);
        assert.ok(response.meta.vector_hits > 0);
      }
    }
    assert.equal(await readFile(join(directory, 'composition.toml'), 'utf8'), saved);
    const response = await explorer.request('query', { id: index.id, text: 'coffee', mode: 'full-text' });
    const address = response.results[0].address;
    assert.ok((await explorer.request('expand', { id: index.id, address })).fragments.length);
    assert.match((await explorer.request('scan', { id: index.id, address, start: 1, end: 2 })).text, /Coffee/);
    assert.equal((await explorer.request('catalog', { id: index.id, term: 'tea' })).count, 1);
    const copy = await explorer.runner.exclusive(() => explorer.import({ name: 'Snapshot', data: directory,
      composition: join(directory, 'composition.toml') }));
    assert.deepEqual(await explorer.request('overview', { id: copy.id }), await explorer.request('overview', { id: index.id }));
    const copied = await explorer.request('query', { id: copy.id, text: 'coffee espresso', mode: 'vector' });
    assert.ok(copied.results.length);
    await assert.rejects(explorer.request('scan', { id: index.id, address, start: 1, end: 3000 }), /End line/);
  } finally { await rm(root, { recursive: true, force: true }); }
});
