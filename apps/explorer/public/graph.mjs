const namespace = 'http://www.w3.org/2000/svg';
function element(tag, attributes, text) {
  const node = document.createElementNS(namespace, tag);
  for (const [key, value] of Object.entries(attributes)) node.setAttribute(key, String(value));
  if (text !== undefined) node.textContent = text;
  return node;
}

export function graph(container, nodes, edges, select) {
  const svg = element('svg', { viewBox: '0 0 800 500', role: 'group', 'aria-label': 'Interactive relation graph' });
  const definitions = element('defs', {});
  const marker = element('marker', { id: `${container.id}-arrow`, viewBox: '0 0 10 10', refX: 9, refY: 5, markerWidth: 5, markerHeight: 5, orient: 'auto-start-reverse' });
  marker.append(element('path', { d: 'M 0 0 L 10 5 L 0 10 z', fill: '#a5bc74' }));
  definitions.append(marker); svg.append(definitions);
  svg.dataset.arrow = `${container.id}-arrow`;
  const positions = new Map();
  nodes.slice(0, 200).forEach((node, index) => {
    const angle = index * 2 * Math.PI / Math.max(nodes.length, 1) - Math.PI / 2;
    const radius = nodes.length === 1 ? 0 : 155 + (index % 2) * 30;
    positions.set(String(node.id), { ...node, x: 400 + Math.cos(angle) * radius * 1.55, y: 250 + Math.sin(angle) * radius });
  });
  for (const edge of edges.slice(0, 400)) drawEdge(svg, positions, edge);
  for (const node of positions.values()) drawNode(svg, node, select);
  if (nodes.length > 40) svg.classList.add('dense');
  container.replaceChildren(svg);
  const view = [0, 0, 800, 500];
  const apply = () => svg.setAttribute('viewBox', view.join(' '));
  const zoom = multiplier => {
    if (multiplier === 0) view.splice(0, 4, 0, 0, 800, 500);
    else {
      const width = Math.max(200, Math.min(2000, view[2] / multiplier));
      const height = width * 500 / 800;
      view.splice(0, 4, view[0] + (view[2] - width) / 2, view[1] + (view[3] - height) / 2, width, height);
    }
    apply();
  };
  let drag;
  svg.addEventListener('pointerdown', event => {
    if (event.target.closest('.node')) return;
    drag = [event.clientX, event.clientY, view[0], view[1]];
    svg.setPointerCapture(event.pointerId);
  });
  svg.addEventListener('pointermove', event => {
    if (!drag) return;
    const bounds = svg.getBoundingClientRect();
    view[0] = drag[2] - (event.clientX - drag[0]) * view[2] / bounds.width;
    view[1] = drag[3] - (event.clientY - drag[1]) * view[3] / bounds.height;
    apply();
  });
  svg.addEventListener('pointerup', () => { drag = undefined; });
  svg.addEventListener('pointercancel', () => { drag = undefined; });
  return zoom;
}

function drawEdge(svg, positions, edge) {
  const from = positions.get(String(edge.source)), to = positions.get(String(edge.target));
  if (!from || !to) return;
  const self = from.id === to.id;
  const distance = Math.max(1, Math.hypot(to.x - 400, to.y - 250));
  const offset = 10 + Math.min(22, Math.log2((to.count ?? 1) + 1) * 2);
  const endX = to.x - (to.x - 400) / distance * offset;
  const endY = to.y - (to.y - 250) / distance * offset;
  const path = self ? `M ${from.x} ${from.y} c -75 -85 75 -85 0 0`
    : `M ${from.x} ${from.y} Q 400 250 ${endX} ${endY}`;
  const line = element('path', { d: path, class: 'edge', 'marker-end': `url(#${svg.dataset.arrow})`, 'data-from': edge.source, 'data-to': edge.target, 'stroke-width': Math.min(5, 1 + Math.log10(edge.count ?? 1)) });
  line.append(element('title', {}, `${edge.source} → ${edge.kind} → ${edge.target}, ${edge.count ?? 1}`));
  svg.append(line);
  if (positions.size <= 18) svg.append(element('text', {
    x: self ? from.x : (from.x + 800 + to.x) / 4,
    y: self ? from.y - 55 : (from.y + 500 + to.y) / 4,
    'text-anchor': 'middle', class: 'edge-label', 'data-from': edge.source, 'data-to': edge.target,
  }, edge.kind));
}

function drawNode(svg, node, select) {
  const group = element('g', { class: 'node', tabindex: 0, role: 'button',
    'aria-label': node.label, transform: `translate(${node.x},${node.y})` });
  const radius = 7 + Math.min(22, Math.log2((node.count ?? 1) + 1) * 2);
  group.append(element('circle', { r: radius }));
  group.append(element('title', {}, `${node.label}, ${node.count ?? 1} fragments`));
  group.append(element('text', { y: radius + 18, 'text-anchor': 'middle' }, node.label.length > 34 ? node.label.slice(0, 31) + '…' : node.label));
  const activate = () => {
    svg.querySelectorAll('.selected').forEach(node => node.classList.remove('selected'));
    group.classList.add('selected');
    svg.classList.add('has-selection');
    svg.querySelectorAll('[data-from]').forEach(edge => edge.classList.toggle('related', edge.dataset.from === String(node.id) || edge.dataset.to === String(node.id)));
    select(node);
  };
  group.addEventListener('click', activate);
  group.addEventListener('keydown', event => { if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); activate(); } });
  svg.append(group);
}
