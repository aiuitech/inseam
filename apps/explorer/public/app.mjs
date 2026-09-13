import { graph } from './graph.mjs';
const $ = selector => document.querySelector(selector);
const escape = value => String(value ?? '').replace(/[&<>"']/g, char => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[char]));
const number = value => Number(value ?? 0).toLocaleString();
const state = { id: '', indexes: [], runs: [], tab: 'overview', offset: 0, response: null, source: null, zoom: () => {}, polling: false };

async function api(route, data = {}) {
  const response = await fetch(`/api/${route}`, { method: 'POST', headers: {
    'content-type': 'application/json', 'x-explorer-token': $('meta[name="explorer-token"]').content,
  }, body: JSON.stringify({ id: state.id, ...data }) });
  const value = await response.json();
  if (!response.ok) throw new Error(value.error ?? 'Request failed');
  return value;
}
function notice(message, error = false) {
  const modal = document.querySelector('dialog[open]');
  document.querySelectorAll('.dialog-error').forEach(node => node.remove());
  if (error && modal) { const alert = document.createElement('p'); alert.className = 'dialog-error'; alert.setAttribute('role', 'alert'); alert.textContent = message; modal.prepend(alert); }
  const node = $('#notice'); node.hidden = !message; node.textContent = message; node.classList.toggle('error', error);
}
async function action(callback) {
  if (state.active) return;
  state.active = true;
  $('#index-select').disabled = true;
  try { notice('Working…'); await callback(); notice(''); }
  catch (error) { notice(error.message, true); }
  finally { state.active = false; $('#index-select').disabled = false; }
}
function bind(selector, callback) {
  $(selector).addEventListener('click', () => void action(callback));
}
function form(selector, callback) {
  $(selector).addEventListener('submit', event => { event.preventDefault(); void action(callback); });
}

async function refreshLibrary() {
  const data = await api('library');
  state.indexes = data.indexes; state.runs = data.runs;
  $('#index-select').innerHTML = '<option value="">Select an index</option>' + data.indexes.map(index =>
    `<option value="${escape(index.id)}">${escape(index.name)}</option>`).join('');
  $('#index-select').value = state.id;
  $('#benchmark-run').innerHTML = data.runs.map(run => `<option value="${escape(run.id)}" ${run.available ? '' : 'disabled'}>${escape(run.name)} · ${escape(run.status)}${run.available ? '' : ' · missing data'}</option>`).join('');
  runDetail();
}
function runDetail() {
  const run = state.runs.find(run => run.id === $('#benchmark-run').value);
  $('#run-detail').textContent = run ? `${run.status} · ${run.data}` : 'No local benchmark indexes found. Choose an existing directory or create a new index.';
}
async function selectIndex(id) {
  localStorage.setItem('inseam-explorer-index', id);
  state.id = id; state.response = null; state.offset = 0;
  for (const selector of ['#metrics', '#type-graph', '#type-list', '#type-detail', '#catalog-results']) $(selector).replaceChildren();
  $('#index-select').value = id;
  $('#results').replaceChildren(); $('#trace').replaceChildren(); $('#evidence').innerHTML = '<h2>Retrieval evidence</h2><p class="quiet">Run a search to inspect result scores.</p>';
  $('#origin').textContent = state.indexes.find(index => index.id === id)?.origin ?? '';
  await loadTab();
}
async function tab(name) {
  state.tab = name;
  document.querySelectorAll('.view').forEach(view => { view.hidden = view.id !== name; });
  document.querySelectorAll('[data-tab]').forEach(button => button.classList.toggle('active', button.dataset.tab === name));
  await loadTab();
}
async function loadTab() {
  if (!state.id) return;
  if (state.tab === 'overview') await overview();
  if (state.tab === 'catalog') await catalog();
  if (state.tab === 'indexing') $('#composition').value = (await api('config')).text;
}

async function overview() {
  const data = await api('overview');
  $('#metrics').innerHTML = ['sources', 'indexed', 'fragments', 'relations', 'search_rows'].map(key =>
    `<div class="metric"><strong>${number(data.totals[key])}</strong><span>${key.replace('_', ' ')}</span></div>`).join('');
  if (!data.types.length) {
    $('#type-graph').innerHTML = '<div class="empty"><h2>No fragments yet.</h2><p>Index a folder to build its graph.</p><button id="empty-index" class="primary">Index sources ↗</button></div>';
    bind('#empty-index', () => tab('indexing')); return;
  }
  const show = node => {
    const relations = data.edges.filter(edge => edge.source === node.id || edge.target === node.id);
    $('#type-detail').innerHTML = `<h3>${escape(node.id)}</h3><p class="quiet">${number(node.count)} fragments</p>` + relations.map(edge =>
      `<p class="quiet">${escape(edge.source)} → <b>${escape(edge.kind)}</b> → ${escape(edge.target)}<br>${number(edge.count)} relations</p>`).join('');
  };
  state.zoom = graph($('#type-graph'), data.types.map(type => ({ id: type.name, label: type.name.replace("text/x-inseam-", "").replace(";kind=", ": ").replace(";via=", ": "), count: type.count })), data.edges, show);
  $('#type-list').innerHTML = data.types.map((type, index) => `<button class="type-row" data-type-index="${index}"><span>${escape(type.name)}</span><b>${number(type.count)}</b></button>`).join('');
  $('#type-list').querySelectorAll('button').forEach(button => button.addEventListener('click', () => {
    const type = data.types[Number(button.dataset.typeIndex)]; show({ id: type.name, count: type.count });
  }));
  $('#type-detail').replaceChildren();
}

async function search() {
  const response = await api('query', { text: $('#query').value, mode: $('#mode').value,
    limit: $('#limit').value, seedK: $('#seed-k').value, hops: $('#hops').value, damping: $('#damping').value });
  state.response = response;
  const meta = response.meta;
  $('#trace').innerHTML = `<div class="trace-grid"><div class="trace-stage">01 / Seed<strong>${number(meta.seeds_ms)} ms</strong><small>${meta.fts_hits} prose · ${meta.lexical_hits} lexical · ${meta.vector_hits} vector</small></div><div class="trace-stage">02 / Fuse<strong>${number(meta.seeds)} seeds</strong><small>Reciprocal rank fusion</small></div><div class="trace-stage">03 / Propagate<strong>${number(meta.graph_ms)} ms</strong><small>${number(meta.relations)} relations loaded</small></div><div class="trace-stage">04 / Roll up<strong>${number(meta.rollup_ms)} ms</strong><small>${number(meta.candidate_sources)} candidate sources</small></div></div><p class="quiet">${number(meta.elapsed_ms)} ms total · ${response.results.length} results · ${escape(response.settings.seeds)} · ${response.settings.graph_hops} hops · damping ${response.settings.damping}</p><details><summary>Complete response JSON</summary><pre>${escape(JSON.stringify(response, null, 2))}</pre></details>`;
  $('#results').innerHTML = response.results.map((result, index) => `<button class="result" data-result="${index}"><div class="result-top"><span class="rank">${String(index + 1).padStart(2, '0')}</span><span class="title">${escape(result.envelope.title ?? result.address.split('/').pop())}</span><span class="score">${result.score.toFixed(4)}</span></div><div class="address">${escape(result.address)}</div><p>${escape(result.summary?.slice(0, 280) ?? 'No summary')}</p></button>`).join('') || '<p class="empty-copy">No results. Try another seed method or a broader query.</p>';
  $('#results').querySelectorAll('button').forEach(button => button.addEventListener('click', () => evidence(Number(button.dataset.result))));
  if (response.results.length) evidence(0);
  else $('#evidence').innerHTML = '<h2>No retrieval evidence</h2>';
}
function evidence(index) {
  const result = state.response.results[index];
  const trace = state.response.meta.evidence?.find(item => item.address === result.address);
  $('#results').querySelectorAll('button').forEach(button => button.classList.toggle('selected', Number(button.dataset.result) === index));
  $('#evidence').innerHTML = `<h2>Why result ${index + 1} ranked</h2><p class="address">${escape(result.address)}</p><button id="inspect-result">Explore source graph ↗</button>` +
    (trace ? scoreEvidence(trace) : '<p class="quiet">This binary did not return local score contributions. Use the current Inseam binary for detailed evidence.</p>') +
    `<h3>Matching hints</h3>${result.hints.map(hint => `<p class="quiet">#${hint.fragment} · ${escape(hint.mimetype)} · ${escape(JSON.stringify(hint.extent ?? ''))}</p><pre>${escape(hint.text)}</pre>`).join('') || '<p class="quiet">No scan hints. Summary or keyword fragments may carry this result.</p>'}<details><summary>Envelope and replicas</summary><pre>${escape(JSON.stringify({ envelope: result.envelope, replicas: result.replicas ?? [] }, null, 2))}</pre></details>`;
  bind('#inspect-result', () => inspect(result.address));
  $('#evidence').querySelectorAll('[data-evidence-fragment]').forEach(button => button.addEventListener('click', () => void action(() => inspect(result.address, Number(button.dataset.evidenceFragment)))));
}
function scoreEvidence(trace) {
  const rows = trace.fragments.map(fragment => `<tr><td><button data-evidence-fragment="${fragment.fragment}">#${fragment.fragment}</button></td><td>${fragment.prose_rank ?? '—'} / ${fragment.lexical_rank ?? '—'} / ${fragment.vector_rank ?? '—'}</td><td>${fragment.seed.toPrecision(4)}</td><td>${fragment.graph.toPrecision(4)}</td><td>×${fragment.weight}</td></tr>`).join('');
  return `<p class="quiet">Measured contributions from the top three scoring fragments, including summaries and keywords.</p><table><thead><tr><th>Fragment</th><th>Rank<br>Prose / Lex / Vec</th><th>Seed</th><th>Graph</th><th>Weight</th></tr></thead><tbody>${rows}</tbody></table><pre>raw = Σ (seed + graph) × weight\n    = ${trace.score_raw.toPrecision(6)}\nnormalized = raw / ${trace.normalization.toPrecision(6)}\n           = ${(trace.score_raw / trace.normalization).toPrecision(6)}</pre><p class="quiet">Seed ranks identify each retrieval list. Graph is the measured PageRank contribution; individual propagation paths are not recorded. The result list rounds scores at the transport boundary.</p>`;
}

async function catalog() {
  const data = await api('catalog', { term: $('#catalog-term').value, contentType: $('#catalog-type').value, offset: state.offset });
  $('#catalog-count').textContent = `${number(data.count)} sources · ${number(data.offset)}–${number(data.offset + data.entries.length)} shown`;
  $('#previous').disabled = state.offset === 0; $('#next').disabled = state.offset + 100 >= data.count;
  $('#catalog-results').innerHTML = '<table><thead><tr><th>Source</th><th>Type</th><th>Bytes</th><th>State</th></tr></thead><tbody>' + data.entries.map((entry, index) =>
    `<tr><td><button data-source="${index}">${escape(entry.locator)}</button><div class="address">${escape(entry.host)}</div></td><td>${escape(entry.content_type)}</td><td>${number(entry.raw_bytes)}</td><td>${entry.indexed ? 'Indexed' : 'Pending'}</td></tr>`).join('') + '</tbody></table>';
  $('#catalog-results').querySelectorAll('button').forEach(button => button.addEventListener('click', () => {
    const entry = data.entries[Number(button.dataset.source)];
    void action(() => inspect(`inseam://${entry.host}/${entry.locator}`));
  }));
}
async function inspect(address, focusFragment) {
  const data = await api('expand', { address }); state.source = address;
  $('#source-address').textContent = address; $('#source-dialog').showModal();
  const fragments = [...data.fragments, ...data.neighbors].slice(0, 200);
  const show = node => fragmentDetail(fragments.find(fragment => String(fragment.id) === String(node.id)));
  state.sourceZoom = graph($('#source-graph'), fragments.map(fragment => ({ id: String(fragment.id), label: `#${fragment.id} ${fragment.mimetype}` })),
    data.relations.map(relation => ({ source: String(relation.from), target: String(relation.to), kind: relation.kind })), show);
  $('#fragment-detail').innerHTML = `<p class="quiet">${data.fragments.length} source fragments · ${data.neighbors.length} neighbors · ${data.relations.length} relations</p>`;
  $('#fragment-list').innerHTML = fragments.map((fragment, index) => `<button class="fragment-button" data-fragment="${index}">#${fragment.id} ${escape(fragment.mimetype)}</button>`).join('');
  $('#fragment-list').querySelectorAll('button').forEach(button => button.addEventListener('click', () => fragmentDetail(fragments[Number(button.dataset.fragment)])));
  $('#relation-list').innerHTML = '<pre>' + escape(data.relations.slice(0, 400).map(relation => `#${relation.from} → ${relation.kind} → #${relation.to}`).join('\n')) + '</pre>';
  $('#scan-output').textContent = '';
  const focused = fragments.find(fragment => fragment.id === focusFragment);
  if (focused) fragmentDetail(focused);
}
function fragmentDetail(fragment) {
  $('#fragment-detail').innerHTML = `<h3>#${fragment.id} · ${escape(fragment.mimetype)}</h3><p class="quiet">${escape(JSON.stringify(fragment.extent ?? {}))}</p><pre>${escape(fragment.text ?? 'This fragment references content rather than storing text.')}</pre>` +
    (fragment.source ? `<button id="neighbor-source">Open neighboring source ↗</button>` : '');
  if (fragment.source) bind('#neighbor-source', () => inspect(fragment.source));
}

async function create() {
  const kind = $('#source-kind').value;
  const body = { name: $('#index-name').value };
  if (kind === 'benchmark') body.run = $('#benchmark-run').value;
  if (kind === 'existing') { body.data = $('#data-path').value; body.composition = $('#config-path').value; }
  if (kind === 'benchmark' && !body.run) throw new Error('Choose an available benchmark run');
  if (kind === 'existing' && !body.data) throw new Error('Enter the index data directory');
  await api('create', body); $('#library-dialog').close(); void pollJob();
}
async function pollJob() {
  if (state.polling) return;
  state.polling = true;
  try {
    for (let attempt = 0; attempt < 21610; attempt++) {
      const { job } = await api('job');
      if (!job) break;
      $('#job-panel').hidden = false; $('#job-title').textContent = `${job.label} · ${job.state}`;
      $('#job-log').textContent = job.error ?? job.log ?? '';
      $('#cancel-job').hidden = job.state !== 'running';
      if (job.state !== 'running') {
        if (job.state === 'completed') {
          await refreshLibrary();
          if (job.result?.id) { state.tab = 'indexing'; await selectIndex(job.result.id); await tab('indexing'); }
          else await loadTab();
        } else notice(job.error, true);
        break;
      }
      await new Promise(resolve => setTimeout(resolve, 1000));
    }
  } catch (error) { notice(error.message, true); }
  finally { state.polling = false; }
}

bind('#add-index', () => $('#library-dialog').showModal());
bind('#empty-add', () => $('#library-dialog').showModal());
bind('#close-library', () => $('#library-dialog').close());
bind('#close-source', () => $('#source-dialog').close());
bind('#refresh', refreshLibrary); bind('#reload-map', overview);
bind('#source-zoom-in', () => state.sourceZoom?.(1.3)); bind('#source-zoom-out', () => state.sourceZoom?.(1 / 1.3)); bind('#source-zoom-reset', () => state.sourceZoom?.(0));
bind('#zoom-in', () => state.zoom(1.3)); bind('#zoom-out', () => state.zoom(1 / 1.3)); bind('#zoom-reset', () => state.zoom(0));
bind('#previous', async () => { state.offset = Math.max(0, state.offset - 100); await catalog(); });
bind('#next', async () => { state.offset += 100; await catalog(); });
bind('#save-config', async () => { await api('save-config', { text: $('#composition').value }); $('#composition').value = (await api('config')).text; });
bind('#cancel-job', () => api('cancel'));
form('#search-form', search); form('#create-form', create);
form('#catalog-form', async () => { state.offset = 0; await catalog(); });
form('#scan-form', async () => { const data = await api('scan', { address: state.source, start: $('#scan-start').value, end: $('#scan-end').value }); $('#scan-output').textContent = `Lines ${data.start}–${data.end} of ${data.lines_total ?? '?'}\n\n${data.text}`; });
form('#index-form', async () => { await api('index', { root: $('#root').value, host: $('#host').value,
  maxSources: $('#max-sources').value, rebuild: $('#rebuild').checked, catalogOnly: $('#catalog-only').checked, batch: $('#batch').checked }); void pollJob(); });
$('#source-kind').addEventListener('change', () => { $('#benchmark-fields').hidden = $('#source-kind').value !== 'benchmark'; $('#existing-fields').hidden = $('#source-kind').value !== 'existing'; });
$('#benchmark-run').addEventListener('change', runDetail);
$('#index-select').addEventListener('change', () => void action(() => selectIndex($('#index-select').value)));
document.querySelectorAll('[data-tab]').forEach(button => button.addEventListener('click', () => void action(() => tab(button.dataset.tab))));
void action(async () => { await refreshLibrary(); const selected = localStorage.getItem('inseam-explorer-index'); if (state.indexes.some(index => index.id === selected)) await selectIndex(selected); const { job } = await api('job'); if (job?.state === 'running') void pollJob(); });
