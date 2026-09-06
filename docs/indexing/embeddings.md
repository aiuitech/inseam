# Embeddings

The `embedder` entry decides which vectors the index keeps: the provider, the model, the vector width, and which fragments get a vector at all. Together those are the **embedding identity** the search surface is bound to ([storage.md](storage.md)); change any of them and the next `inseam index` re-embeds in place ([maintenance.md](maintenance.md)).

```toml
[[entry]]
id = "embedder"
[entry.config]
provider = "endpoint"      # endpoint | hashed | none
model = "all-minilm:l6-v2"
# dimensions = 384         # unset: the model's native width
vectors = "summaries"      # all | summaries
```

## Provider

- `endpoint` — embeds through the `llm` entry's OpenAI-compatible endpoint (`/embeddings`). OpenRouter by default; a local ollama is a `base_url` away ([local models](#local-models-with-ollama)).
- `hashed` — deterministic local bag-of-words hashing: weak semantics, no network, no cost. The offline and test embedder. `dimensions` is any width (default 256).
- `none` — no vectors; discovery degrades to full-text seeding only.

## Model and dimensions

The width is the model's to decide, so `dimensions` is optional. Unset, the embedder takes the model's native width. Set, it is checked against what the model produces before anything is embedded — at activation, so a disagreement is a config error on boot, never a width mismatch discovered mid-index. Three things answer "what does this model produce", in order:

1. A **built-in catalog** of known embedding models (OpenAI, Google, Mistral, and the ollama library's common embedders — `all-minilm`, `nomic-embed-text`, `mxbai-embed-large`, `bge-*`, `snowflake-arctic-embed*`, `qwen3-embedding`, `embeddinggemma`, `granite-embedding`), matched by name across the ways endpoints spell them (`openai/text-embedding-3-small`, `text-embedding-3-small`; `all-minilm:l6-v2`, `all-minilm`).
2. **The endpoint itself**, when it can introspect its models. A local ollama answers `/api/show`: the model's capabilities (an embedder or not) and its width. Asking ollama to embed with a chat model (`qwen3.5:9b`) is refused by name.
3. **Explicit config.** A model neither knows needs `dimensions` set; the value is taken on trust and the first embedding is the check.

A `dimensions` smaller than the native width is accepted only for models trained for it (Matryoshka — `text-embedding-3-*`, `nomic-embed-text` v1.5, `qwen3-embedding`, `embeddinggemma`, `gemini-embedding-001`), and is then sent as the request's `dimensions`. Every OpenAI-compatible server truncates any model's vectors on request — ollama returns 128 of all-minilm's 384 without complaint — and a truncated non-Matryoshka vector is noise, so that is refused. Larger than native is refused too.

`inseam models --embeddings` lists what the endpoint offers; against ollama it lists installed models with their widths.

## Which fragments get vectors

Vectors are the storage bulk of an index — one `f32` per dimension per text fragment, against a few bytes per fragment for the graph. `vectors` chooses:

- `all` (default) — every text-bearing fragment (sections, chunks, summaries, entities) gets a vector.
- `summaries` — only summary fragments get vectors: one bounded row per source. Every fragment still enters the full-text index, so exact-term queries reach into sections and chunks; only vector seeding runs over summaries.

For the leanest index, pair `vectors = "summaries"` with the structural transforms switched off ([transforms.md](transforms.md)): with `markdown` and `chunker` disabled the subtree is the source and its summary and nothing else, and the summarizer's `target_chars` bounds exactly how much text each source contributes. Three sources index to six fragments and three search rows.

Switching `vectors` is an identity change: the next index run re-embeds in place from stored text — no transforms re-run, no LLM spend, and in the `summaries` direction a fraction of the embedding calls.

## Local models with ollama

Ollama speaks the OpenAI-compatible API on `http://localhost:11434/v1`, needs no key, and its native API (`/api/tags`, `/api/show`) tells inseam which installed models embed, which chat, and what width each embedder produces — `inseam models` and `inseam models --embeddings` use it.

```toml
[[entry]]
id = "llm"
[entry.config]
base_url = "http://localhost:11434/v1"
api_key_env = ""                       # keyless endpoint
transform_model = "qwen3.5:9b"
transform_reasoning_effort = "none"    # a thinking model must be told not to
agent_model = "qwen3.5:9b"

[[entry]]
id = "embedder"
[entry.config]
provider = "endpoint"
model = "all-minilm:l6-v2"
vectors = "summaries"
```

`transform_reasoning_effort` matters for thinking models: left to itself, qwen3.5 spends the whole reply thinking and returns an empty summary (the summarizer then falls back to extractive — visible in the index report's `summaries:` line). `none` makes it answer directly; the value is whatever the endpoint spells (`none`, `low`, `minimal`, …) and is sent only when set, since endpoints reject values their models don't know. OpenRouter receives its native nested `reasoning` object and `exclude = true`, because transforms consume the answer but never the reasoning trace. Other OpenAI-compatible endpoints retain the top-level `reasoning_effort` spelling.

## The batch lane

Transform LLM calls ride one of two lanes. The **interactive** lane (default) answers each call with its own request. The **batch** lane parks calls and submits them together through the endpoint's batch API — OpenRouter's `/api/beta/batches`, billed at half the model's price, answered within a 24-hour window. It is for large, time-insensitive runs: a first index of a big catalog, a full `--rebuild`. A few changed files would wait minutes for a job that carries three requests.

Ask for it per run or per transform:

```sh
inseam index ~/Data --batch          # every LLM-using transform rides the batch lane this run
```

```toml
[[entry]]
id = "summarizer"
[entry.config]
llm_lane = "batch"                   # summaries always ride the batch lane (entities: same key)
llm_call_budget = 100000             # the default 500 would cap a large run's LLM summaries
```

What happens on the batch lane:

- The endpoint plugin declares `transform_batch_model` — `<transform_model>:batch`, OpenRouter's batch variant of the same model (set `transform_batch_model` to name another). An endpoint without a batch API (ollama, vLLM) declares none, and a batch request is served interactively with a warning.
- The sweep plans `batch_concurrency` sources at once (4,096 instead of `concurrency`'s 8). Each planner parks on its summary call, so the parked set is what one job gathers. The dial also bounds the parked sources' content held in memory.
- The endpoint's batch lane submits a job when it is full (`batch_requests_max`, 5,000 by default and at most 5,000 on OpenRouter, or 64 MiB of requests), when no call has arrived for two seconds, or when the oldest parked call has waited two minutes. Jobs run concurrently (eight at most) while later planners keep parking. Each job is polled every five seconds for its first minute, then every thirty. A 429 or server error is retried up to eight times; provider reset headers set the wait, bounded at five minutes per retry.
- Results return to their callers by stable id. A failed item fails only its own call — that source's summary falls back to extractive — and a failed job fails every call in it the same way; the run continues.
- The index report counts jobs: `llm batch lane: N jobs`.

The shape stamp names the base model, never the lane: switching a composition between lanes, or running `--batch` once, re-indexes nothing. A `transform_model` that already ends in `:batch` is itself the batch lane for every call.

## Wire format

Embeddings are requested as base64 (`encoding_format: "base64"`): each vector arrives as one packed little-endian `f32` array instead of hundreds of decimal literals — less than half the bytes and a straight byte copy instead of float parsing. Endpoints that ignore the parameter (some OpenAI-compatible servers) still answer with JSON floats; both shapes are read. The client packs up to 128 vector-covered rows into each endpoint request. The sweep hands it 256 search rows at a time, so the common source-plus-summary shape fills one request with about 128 summary vectors; four sweep batches run concurrently, bounding the vector work at about 512 inputs in flight ([maintenance.md](maintenance.md)).
