import { parse, stringify } from 'smol-toml';

export function integer(value, minimum, maximum, label) {
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < minimum || number > maximum) {
    throw new Error(`${label} must be an integer between ${minimum} and ${maximum}`);
  }
  return number;
}

export function text(value, label, maximum = 4096) {
  if (typeof value !== 'string' || !value.trim() || value.length > maximum || value.includes('\0')) {
    throw new Error(`${label} must contain 1–${maximum} characters`);
  }
  return value.trim();
}

export function choice(value, choices, label) {
  if (!choices.includes(value)) throw new Error(`Invalid ${label}`);
  return value;
}

export function composition(source, overrides = {}) {
  const document = parse(source);
  document.entry ??= [];
  if (!Array.isArray(document.entry) || document.entry.length > 256) throw new Error('Invalid entry list');
  // A working copy must never join the original node's network.
  for (const id of ['transport', 'sync', 'routing', 'roster', 'google']) {
    let entry = document.entry.find(entry => entry.id === id);
    if (!entry) { entry = { id }; document.entry.push(entry); }
    entry.disabled = true;
  }
  for (const [id, config] of Object.entries(overrides)) {
    let entry = document.entry.find(entry => entry.id === id);
    if (!entry) { entry = { id }; document.entry.push(entry); }
    entry.config = { ...entry.config, ...config };
  }
  return stringify(document);
}

export function finderOptions(body) {
  const damping = Number(body.damping ?? 0.5);
  if (!Number.isFinite(damping) || damping < 0 || damping >= 1) throw new Error('Damping must be in [0, 1)');
  return {
    seeds: choice(body.mode ?? 'both', ['both', 'full-text', 'vector'], 'search mode'),
    seed_k: integer(body.seedK ?? 60, 1, 1000, 'Seed count'),
    graph_hops: integer(body.hops ?? 2, 0, 4, 'Graph hops'),
    graph_relation_limit: 20000, damping,
  };
}

export const defaultComposition = `[[entry]]
id = "llm"
disabled = true
[[entry]]
id = "hints"
disabled = true
[[entry]]
id = "embedder"
[entry.config]
provider = "none"
[[entry]]
id = "summarizer"
[entry.config]
llm_call_budget = 0
target_chars = 12000
[[entry]]
id = "entities"
disabled = true
`;
