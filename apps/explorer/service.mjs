import { readFile, writeFile, rename, stat, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { composition, finderOptions, integer, text } from './config.mjs';
import { createIndex, discover, library } from './library.mjs';
import { Runner } from './process.mjs';

export class Explorer {
  runner = new Runner();
  constructor(repository, root, binary) {
    this.repository = repository;
    this.root = root;
    this.binary = binary;
  }

  async directory(id) {
    const found = (await library(this.root)).find(index => index.id === id);
    if (!found) throw new Error('Select an index from your library');
    return join(this.root, found.id);
  }

  async database(operation, directory, options = {}) {
    const path = join(directory, 'catalog.sqlite3');
    if (!(await stat(path).catch(() => null))) {
      if (operation === 'overview') return { totals: { sources: 0, indexed: 0, fragments: 0, relations: 0, search_rows: 0 }, types: [], edges: [] };
      if (operation === 'catalog') return { entries: [], count: 0, offset: 0 };
      throw new Error('This index is empty. Index a folder first.');
    }
    const output = await this.runner.command('python3', [join(this.repository, 'apps/explorer/database.py'),
      operation, path, JSON.stringify(options)]);
    return JSON.parse(output);
  }

  async cli(directory, args, configPath = join(directory, 'composition.toml'), timeout = 600000) {
    return this.runner.command(this.binary, ['--data-dir', directory, '--composition', configPath, ...args], timeout);
  }

  async query(directory, body) {
    const source = await readFile(join(directory, 'composition.toml'), 'utf8');
    const settings = finderOptions(body);
    const path = join(directory, 'query.toml');
    await writeFile(path, composition(source, { finder: settings }), { mode: 0o600 });
    try {
      const result = JSON.parse(await this.cli(directory, ['query', text(body.text, 'Query'),
        '--limit', String(integer(body.limit ?? 12, 1, 50, 'Result limit')), '--json'], path));
      return { ...result, settings };
    } finally { await rm(path, { force: true }); }
  }

  async saveConfig(directory, body) {
    const value = composition(text(body.text, 'Composition', 262144));
    const temporary = join(directory, 'composition.tmp');
    await writeFile(temporary, value, { mode: 0o600 });
    await rename(temporary, join(directory, 'composition.toml'));
    return { saved: true };
  }

  async index(directory, body) {
    const root = text(body.root, 'Root');
    const maximum = integer(body.maxSources ?? 1000, 1, 1000000, 'Maximum sources');
    const args = ['index', root];
    if (body.catalogOnly === true) args.push('--catalog-only');
    else args.push('--max-sources', String(maximum));
    if (body.rebuild === true) args.push('--rebuild');
    if (body.batch === true) args.push('--batch');
    if (body.host) args.push('--host', text(body.host, 'Host', 256));
    return { output: await this.cli(directory, args, undefined, 21600000) };
  }

  async import(body) {
    const runs = body.run ? await discover(this.repository) : [];
    const run = runs.find(run => run.id === body.run);
    if (body.run && !run) throw new Error('Benchmark run no longer exists');
    return createIndex(this.root, body, run, (source, destination) => this.runner.command('python3',
      [join(this.repository, 'apps/explorer/database.py'), 'backup', source, destination], 21600000));
  }

  async request(route, body) {
    if (route === 'library') return { indexes: await library(this.root), runs: await discover(this.repository) };
    if (route === 'job') return { job: this.runner.job, busy: this.runner.busy };
    if (route === 'cancel') { this.runner.cancel(); return { cancelled: true }; }
    if (route === 'create') return this.runner.start(() => this.import(body), 'Create working index');
    const directory = await this.directory(body.id);
    if (route === 'index') return this.runner.start(() => this.index(directory, body), 'Index sources');
    return this.runner.exclusive(() => this.operation(route, directory, body));
  }

  async operation(route, directory, body) {
    switch (route) {
      case 'overview': return this.database('overview', directory);
      case 'catalog': return this.database('catalog', directory, {
        term: String(body.term ?? '').slice(0, 4096), contentType: String(body.contentType ?? '').slice(0, 256),
        offset: integer(body.offset ?? 0, 0, 10000000, 'Offset'),
      });
      case 'query': return this.query(directory, body);
      case 'expand': return JSON.parse(await this.cli(directory, ['expand', text(body.address, 'Address'), '--json']));
      case 'scan': return JSON.parse(await this.cli(directory, ['scan', text(body.address, 'Address'), '--start',
        String(integer(body.start, 1, 100000000, 'Start line')), '--end',
        String(integer(body.end, Number(body.start), Number(body.start) + 1999, 'End line')), '--json']));
      case 'config': return { text: await readFile(join(directory, 'composition.toml'), 'utf8') };
      case 'save-config': return this.saveConfig(directory, body);
      default: throw new Error('Unknown operation');
    }
  }
}
