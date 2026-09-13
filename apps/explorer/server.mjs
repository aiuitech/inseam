import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { resolve, join } from 'node:path';
import { randomBytes } from 'node:crypto';
import { Explorer } from './service.mjs';

const directory = fileURLToPath(new URL('.', import.meta.url));
const repository = resolve(directory, '../..');
const port = Number(process.env.INSEAM_EXPLORER_PORT ?? 7340);
if (!Number.isInteger(port) || port < 1024 || port > 65535) throw new Error('Invalid explorer port');
const origin = `http://127.0.0.1:${port}`;
const token = randomBytes(32).toString('hex');
const explorer = new Explorer(repository, process.env.INSEAM_EXPLORER_DATA ?? join(directory, '.indexes'),
  process.env.INSEAM_BINARY ?? 'inseam');
const assets = new Map([
  ['/', ['index.html', 'text/html']], ['/app.mjs', ['app.mjs', 'text/javascript']],
  ['/graph.mjs', ['graph.mjs', 'text/javascript']], ['/style.css', ['style.css', 'text/css']],
]);

async function body(request) {
  let size = 0;
  const chunks = [];
  for await (const chunk of request) {
    size += chunk.length;
    if (size > 300000) throw new Error('Request exceeds 300 KB');
    chunks.push(chunk);
  }
  return JSON.parse(Buffer.concat(chunks).toString('utf8'));
}

function json(response, status, value) {
  response.writeHead(status, { 'content-type': 'application/json', 'cache-control': 'no-store' });
  response.end(JSON.stringify(value));
}

async function serve(request, response) {
  response.setHeader('X-Content-Type-Options', 'nosniff');
  response.setHeader('Content-Security-Policy', "default-src 'self'; style-src 'self'; script-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'");
  if (request.headers.host !== `127.0.0.1:${port}`) return json(response, 403, { error: 'Invalid host' });
  if (request.url.startsWith('/api/')) {
    if (request.method !== 'POST' || request.headers.origin !== origin || request.headers['x-explorer-token'] !== token) {
      return json(response, 403, { error: 'Open the explorer at its loopback URL' });
    }
    try { return json(response, 200, await explorer.request(request.url.slice(5), await body(request))); }
    catch (error) { return json(response, 400, { error: error.message }); }
  }
  const asset = assets.get(request.url);
  if (!asset || request.method !== 'GET') return json(response, 404, { error: 'Not found' });
  let content = await readFile(join(directory, 'public', asset[0]), 'utf8');
  if (request.url === '/') content = content.replace('__TOKEN__', token);
  response.writeHead(200, { 'content-type': asset[1], 'cache-control': 'no-store' });
  response.end(content);
}

const server = createServer((request, response) => {
  void serve(request, response).catch(error => json(response, 500, { error: error.message }));
});
server.requestTimeout = 30000;
server.headersTimeout = 10000;
server.maxConnections = 32;
server.listen(port, '127.0.0.1', () => console.log(`Inseam index explorer: ${origin}`));
for (const signal of ['SIGTERM', 'SIGINT']) {
  process.on(signal, () => { explorer.runner.cancel(); server.close(); });
}
