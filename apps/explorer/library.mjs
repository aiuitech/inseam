import { mkdir, readFile, readdir, stat, writeFile, rename, rm } from 'node:fs/promises';
import { join, resolve, isAbsolute } from 'node:path';
import { randomUUID } from 'node:crypto';
import { composition, defaultComposition, text } from './config.mjs';

export async function jsonFile(path) {
  if ((await stat(path)).size > 2 * 1024 * 1024) throw new Error('Metadata file exceeds 2 MiB');
  return JSON.parse(await readFile(path, 'utf8'));
}

export async function discover(repository) {
  const root = join(repository, 'benchmarks/runs');
  const result = [];
  for (const suite of (await readdir(root, { withFileTypes: true })).slice(0, 32)) {
    if (!suite.isDirectory()) continue;
    for (const run of (await readdir(join(root, suite.name), { withFileTypes: true })).slice(-2000)) {
      if (!run.isDirectory()) continue;
      const folder = join(root, suite.name, run.name);
      try {
        const manifest = await jsonFile(join(folder, 'manifest.json'));
        if (!manifest.index_data_path) continue;
        const data = resolve(repository, manifest.index_data_path);
        const available = await stat(join(data, 'catalog.sqlite3')).then(() => true, () => false);
        result.push({ id: `${suite.name}/${run.name}`, name: `${suite.name} / ${run.name}`,
          data, composition: join(folder, 'composition.toml'), status: manifest.status,
          available, models: manifest.models, indexing: manifest.indexing });
      } catch { /* Incomplete runs need not have published a manifest yet. */ }
    }
  }
  return result.sort((a, b) => b.id.localeCompare(a.id));
}

export async function library(root) {
  await mkdir(root, { recursive: true });
  const entries = await readdir(root, { withFileTypes: true });
  const indexes = [];
  for (const entry of entries.slice(0, 512)) {
    if (!entry.isDirectory()) continue;
    try {
      const index = await jsonFile(join(root, entry.name, 'index.json'));
      if (index.id === entry.name && /^[a-f0-9-]{36}$/.test(index.id)) indexes.push(index);
    }
    catch { /* A failed import has no published index record. */ }
  }
  return indexes;
}

export async function createIndex(root, body, run, backup) {
  const existing = await library(root);
  if (existing.length >= 128) throw new Error('Library limit of 128 indexes reached');
  const id = randomUUID();
  const directory = join(root, id);
  const name = text(body.name, 'Index name', 120);
  const source = run?.data ?? body.data;
  if (source && !isAbsolute(source)) throw new Error('Data directory must be absolute');
  const compositionPath = run?.composition ?? body.composition;
  if (source && !compositionPath) throw new Error('Supply the composition used to build this index');
  if (compositionPath && (await stat(compositionPath)).size > 262144) throw new Error('Composition exceeds 256 KiB');
  const original = compositionPath ? await readFile(text(compositionPath, 'Composition path'), 'utf8') : defaultComposition;
  if (original.length > 262144) throw new Error('Composition exceeds 256 KiB');
  const config = composition(original);
  await mkdir(directory, { recursive: false });
  try {
    if (source) await backup(join(source, 'catalog.sqlite3'), join(directory, 'catalog.sqlite3'));
    await writeFile(join(directory, 'composition.toml'), config, { flag: 'wx', mode: 0o600 });
    const index = { id, name, origin: run?.id ?? source ?? 'New index', created: new Date().toISOString() };
    await writeFile(join(directory, 'index.json.tmp'), JSON.stringify(index), { mode: 0o600 });
    await rename(join(directory, 'index.json.tmp'), join(directory, 'index.json'));
    return index;
  } catch (error) {
    await rm(directory, { recursive: true, force: true });
    throw error;
  }
}
