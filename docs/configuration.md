# Configuration

## The LLM endpoint

All LLM work — embeddings, summary/entity transforms, the agent demo — goes through one OpenAI-compatible endpoint, configured in the profile:

```toml
[endpoint]
base_url = "https://openrouter.ai/api/v1"   # the default; OpenAI, Ollama, vLLM, ... all work
api_key_env = "OPENROUTER_API_KEY"          # which env var holds the key
```

Swapping providers is configuration, not code. The `inseam models` catalog listing uses OpenRouter-specific endpoints and may 404 elsewhere.

## Secrets

The endpoint API key is the only secret, read from whatever environment variable `api_key_env` names — a plain exported variable, set in your shell profile or injected by your runner:

```sh
export OPENROUTER_API_KEY=sk-or-v1-...
```

Secrets never live in the profile: profiles are shareable, copyable documents, and the data dir is deletable derived state. There is no `.env` loading — behavior doesn't depend on the directory you invoke `inseam` from. Without a key: embeddings with `provider = "endpoint"` refuse to run (clear error), LLM summaries/entities silently fall back to their offline forms, and `inseam agent` / `inseam models` bail with instructions.

## Data directory

The node's index and catalog live in one directory:

1. `--data-dir <path>` or `INSEAM_DATA_DIR`
2. otherwise the platform data dir, e.g. `~/Library/Application Support/inseam`

Deleting the directory deletes derived state only; the index is rebuildable from sources at any time.

## Profile

The [index profile](index/profiles.md) is TOML, resolved in order:

1. `--profile <path>` or `INSEAM_PROFILE`
2. `<data-dir>/profile.toml` if present
3. built-in defaults

Every field has a default; a partial file overrides only what it names. Unknown keys are rejected (typo protection).

## Logging

`INSEAM_LOG` (or nothing: `inseam=info`) with `tracing_subscriber` env-filter syntax; logs go to stderr so stdout stays pipeable JSON/text.
