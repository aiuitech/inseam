# Vocabulary

Implements [design/vocabulary.md](../../design/vocabulary.md): the corpus's own words as rows in the index, grouped into clusters, so a document and a question are both read in the corpus's terms. The sweep builds it in a **vocabulary pass** after every file has landed and before folders ([maintenance.md](maintenance.md)); the finder reads it at query time ([../finder/algorithm.md](../finder/algorithm.md)).

## Rows

Every vocabulary row is a keyed fragment ([transforms.md](transforms.md)): it belongs to no source, dedupes index-wide under its key, carries no address, conducts relevance, and never ranks as a result. It is a lexical search row and never carries a vector. Beside the fragment the store files the row's kind, origin, normalized spelling (lowercase, whitespace collapsed), document frequency (sources anchored to it), cluster, and gloss.

| Kind | Key | Mimetype | Anchored | Comes from |
| --- | --- | --- | --- | --- |
| term | `term:<normalized>` | `text/x-inseam-term` | `mentions` from every content fragment that spells it | mined from text; the hints transform |
| identifier | `identifier:<normalized>` | `text/x-inseam-identifier` | `mentions` | mined from text (digits, `_`, `/`); the hints transform |
| entity | `entity:<kind>:<normalized>` | `text/x-inseam-entity;kind=…` | `mentions`; `authored` from a root when the envelope names an author | the entity and hints transforms; envelope facets |
| alias | `alias:<normalized>` | `text/x-inseam-alias` | `aliases` into the row it stands for | cluster grounding |
| facet | `facet:<key>:<normalized>` | `text/x-inseam-facet;key=…` | `faceted` from the source's root | envelope facets; the host kind, planted for every source |

`inseam vocabulary` lists rows most frequent first (`--kind` narrows), `--show <spelling>` prints one row with its gloss, aliases, cluster, and anchored sources, and `--clusters` lists clusters. `inseam status` prints the counts by kind, the cluster count, and the generation.

## The pass

The pass runs when a sweep landed or removed a source, or when its own configuration digest changed; otherwise it reports `skipped (unchanged)`. It reads the store and never a host. In order:

1. **Adopt.** Keyed fragments the transforms planted (entities, terms, identifiers) get a vocabulary row if they have none.
2. **Mine.** Every content fragment is tokenized (runs of alphanumerics with inner `-_./'` kept, so `eu-central-1` and `stream.timebox_finalized` survive) and every token that carries a mark of internal naming — digits or code marks (an identifier), inner capitals, hyphens, dots, or a plain word of four letters or more that is not a function word (a term) — is counted once per source. Keyword rows from the summarizer propose multi-word phrases, which count through matching. The table is bounded by `candidates_max`; shape-marked tokens may double it.
3. **Band and plant.** Candidates with a document frequency from `term_df_min` up to `df_max` — `term_df_max_percent` of the content sources, never below `term_df_max_floor` — become rows. Mined rows the band no longer names are unanchored and collected at the end of the sweep.
4. **Match and anchor.** One more walk over every content fragment: each row's spelling — mined or extracted, single token or phrase up to four tokens — is matched on word boundaries by n-gram lookup and anchored `mentions`. This is why a document indexed before a term was known still gets its edge.
5. **Facets.** Every rooted source of the swept host is anchored `faceted` to `facet:host:<kind>`; every source whose envelope carries facets gets its own rows: `author` lands on the entity row via `authored`, any other key on `facet:<key>:<value>` via `faceted`.
6. **Cluster.** Rows without a cluster are assigned by co-occurrence, most frequent first: a row joins the cluster present in at least `cluster_join_min` of its sources with room under `cluster_terms_max`, and founds one otherwise, up to `clusters_max`. Assignments made earlier in the same pass are candidate homes for later rows. At most `cluster_assignments_per_sweep_max` rows per pass.
7. **Embed.** Every cluster whose membership changed is embedded from its members' spellings and glosses in a stable order, only when that text's digest moved; clusters whose vectors clear `cluster_merge_cosine` merge, smaller into larger, at most once per pass each.
8. **Ground.** With an `llm` mounted and `cluster_llm_budget` left, one call per changed cluster, with the members and short excerpts of where the most frequent ones appear. The model returns merges (applied by re-keying the loser's anchors to the survivor), a gloss per row, and up to `aliases_per_row_max` search phrases per row, which become alias rows. Calls are metered under `vocabulary` like a transform's; a spent budget stops grounding, never the pass.

The pass reports what it did in the index report — sources walked, candidates mined and kept, rows planted, anchors added, rows retracted, facet rows, clusters founded, joined, merged, embedded, and grounded, aliases and glosses written, model calls, time per step — and names the twenty most frequent rows, so the first thing to do after a pass is read that list: if it is full of industry words, the shape rule is wrong before any query is run.

## Configuration

Under the `sweep` entry, `[entry.config.vocabulary]` ([../configuration.md](../configuration.md)):

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | true | run the pass at all |
| `term_df_min` | 2 | fewest sources a candidate must recur in |
| `term_df_max_percent` | 2 | most sources, as a percentage of content sources |
| `term_df_max_floor` | 50 | the percentage is never taken below this count |
| `candidates_max` | 200000 | distinct candidates the mining table keeps |
| `clusters_max` | 10000 | clusters the pass may found |
| `cluster_terms_max` | 64 | members a cluster may hold |
| `cluster_join_min` | 0.5 | fraction of a row's sources a cluster must be present in |
| `cluster_merge_cosine` | 0.92 | clusters at or over this cosine merge |
| `cluster_assignments_per_sweep_max` | 50000 | rows assigned per pass |
| `cluster_llm_budget` | 500 | grounding calls per run; 0 grounds nothing |
| `llm_lane` | interactive | the lane grounding calls ride |
| `aliases_per_row_max` | 4 | aliases the model may give one row |

The mining dials (`term_df_*`, `candidates_max`) ride the pass's own digest: changing one re-runs the pass in full on the next sweep and never re-runs a per-source transform. The cluster and budget dials are run-metering.

## Facets from hosts

A connection may put descriptive facets on an envelope — `author`, `label`, `channel`, `thread`, `repository` — in its own vocabulary; they are a field apart from trust properties and never feed an access decision. The Gmail host fills `author` from the `From` header (the display name when there is one) and one `label` per Gmail label ([google-workspace.md](google-workspace.md)). A path host declares no container facet: its folder is the container, and folder entries relate `contains` to their children's roots so membership conducts in the walk ([transforms.md](transforms.md#folders)). Facet values filter queries: `inseam query … --facet label:INBOX --facet author:"Dana Reyes"` keeps only sources anchored to every value named ([../finder/operations.md](../finder/operations.md)).

## Storage

Two catalog tables beside the graph ([storage.md](storage.md)): `vocabulary_rows` (fragment, kind, origin, normalized spelling, document frequency, cluster, gloss) and `clusters` (label, the digest of the embedded text, the vector, member count, document frequency, the pass generation that last changed it). Both are created beside the catalog with no schema-version bump, so a node from before the vocabulary fills them on its next sweep; the `sources` table gains a `facets` column in place the same way. The generation counter in `meta` tells the finder's cluster cache when to reload.
