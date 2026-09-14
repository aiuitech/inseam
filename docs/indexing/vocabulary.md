# Vocabulary

Implements [design/vocabulary.md](../../design/vocabulary.md): the corpus's own words as rows in the index, grouped into clusters, so a document and a question are both read in the corpus's terms. The sweep builds it in a **vocabulary pass** after every file has landed and before folders ([maintenance.md](maintenance.md)); the finder reads it at query time ([../finder/algorithm.md](../finder/algorithm.md)).

## Rows

Every vocabulary row is a keyed fragment ([transforms.md](transforms.md)): it belongs to no source, dedupes index-wide under its key, carries no address, conducts relevance, and never ranks as a result. It is a lexical search row and never carries a vector. Beside the fragment the store files the row's kind, origin, normalized spelling (lowercase, whitespace collapsed), document frequency, cluster, and gloss.

A **mined** row stores no edges: its anchors are the content fragments whose text spells it, and the prose full-text index already holds those postings, so the Finder reads them at query time ([../finder/algorithm.md](../finder/algorithm.md)). Its document frequency is the number of sources the pass matched it in. Every other row's anchors are relations, because nothing else holds them.

| Kind | Key | Mimetype | Anchored | Comes from |
| --- | --- | --- | --- | --- |
| term | `term:<normalized>` | `text/x-inseam-term` | the full-text index (mined); `mentions` relations (extracted) | mined from text; the hints transform |
| identifier | `identifier:<normalized>` | `text/x-inseam-identifier` | the full-text index (mined); `mentions` relations (extracted) | mined from text (digits, `_`, `/`); the hints transform |
| entity | `entity:<kind>:<normalized>` | `text/x-inseam-entity;kind=…` | `mentions` relations; `authored` from a root when the envelope names an author | the entity and hints transforms; envelope facets |
| alias | `alias:<normalized>` | `text/x-inseam-alias` | `aliases` into the row it stands for | cluster grounding; a row the model merged into another |
| facet | `facet:<key>:<normalized>` | `text/x-inseam-facet;key=…` | `faceted` from the source's root | envelope facets |

`inseam vocabulary` lists rows most frequent first (`--kind` narrows), `--show <spelling>` prints one row with its gloss, aliases, cluster, and the sources it reaches, and `--clusters` lists clusters. `inseam status` prints the counts by kind, the cluster count, and the generation.

## The pass

The pass runs when a sweep landed or removed a source, or when its own configuration digest changed; otherwise it reports `skipped (unchanged)`. It reads the store and never a host. Content is every text fragment of a non-folder source that is not inseam-derived, plus verbatim summaries (the text itself, kept whole); a folder's text is its entries' hints and is neither mined nor an anchor. In order:

1. **Adopt.** Keyed fragments the transforms planted (entities, terms, identifiers) get a vocabulary row if they have none. Stored `mentions` edges of mined rows from an earlier pass are dropped.
2. **Mine.** Every content fragment is tokenized (runs of alphanumerics with inner `-_./'` kept, so `eu-central-1` and `stream.timebox_finalized` survive) and every token with a letter is counted **by its normalized spelling**, once per source: a bit in a seen-filter on first sight, a count from the second, so the singletons that make up most of a corpus's distinct tokens cost a bit each. A token's spelling is kept once it has recurred with a mark of internal naming — digits or code marks (an identifier), inner capitals, hyphens, dots, or a plain word of four letters or more (a term) — and a function word is no candidate under any capitalization: `FOR` in a heading and `for` in a sentence are one token, and neither is a term. Keyword rows from the summarizer propose multi-word phrases, which count through matching. The count table is bounded by `recurring_tokens_max`, the spellings by `candidates_max`.
3. **Band.** Candidates with a document frequency from `term_df_min` up to `df_max` — `term_df_max_percent` of the content sources, never below `term_df_max_floor` — are the pass's candidates. Mined rows the band no longer names are retracted outright (row, fragment, search row).
4. **Match and count.** One more walk over every content fragment: every candidate's and every existing row's spelling — single token or phrase up to four tokens — is matched on word boundaries by n-gram lookup, and each hit counts the source for the row, once per source, keeping a sample of up to 256 sources per row for the cluster decision. Nothing is written per hit. Candidates the walk confirms in the band are planted with their frequency; known rows get theirs recorded.
5. **Facets.** Every source whose envelope carries facets gets its rows: `author` lands on the entity row via `authored`, any other key on `facet:<key>:<value>` via `faceted`, and their frequencies are recounted from those relations. The host is not a facet: it is a column on every source and a query filter.
6. **Cluster.** In memory, most frequent row first: a row already in a cluster marks its sources with it; a row without one joins the cluster present in at least `cluster_join_min` of its sources with room under `cluster_terms_max`, and founds one otherwise, up to `clusters_max`. Clusters founded earlier in the same pass are homes for later rows. Founded clusters and joins are then written in batches. At most `cluster_assignments_per_sweep_max` rows per pass.
7. **Embed.** Every cluster whose membership changed is embedded from its members' spellings and glosses in a stable order, only when that text's digest moved; clusters whose vectors clear `cluster_merge_cosine` merge, smaller into larger, at most once per pass each.
8. **Ground.** With an `llm` mounted and `cluster_llm_budget` left, one call per changed cluster, eight in flight at once, with the members and one excerpt each for the most frequent. The model returns merges (the loser becomes an alias of the survivor and keeps its spelling), a gloss per row, and up to `aliases_per_row_max` search phrases per row, which become alias rows. Calls are metered under `vocabulary` like a transform's; a spent budget stops grounding, never the pass.

The pass reports what it did in the index report — sources walked, candidates mined and kept, rows planted, row–source matches counted, relations written (facets, authors, aliases), rows retracted, facet rows, clusters founded, joined, merged, embedded, and grounded, aliases and glosses written, model calls, time per step — and names the twenty most frequent rows, so the first thing to do after a pass is read that list: if it is full of industry words, the shape rule is wrong before any query is run.

## Configuration

Under the `sweep` entry, `[entry.config.vocabulary]` ([../configuration.md](../configuration.md)):

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | true | run the pass at all |
| `term_df_min` | 2 | fewest sources a candidate must recur in |
| `term_df_max_percent` | 2 | most sources, as a percentage of content sources |
| `term_df_max_floor` | 50 | the percentage is never taken below this count |
| `candidates_max` | 200000 | distinct candidate spellings the mining table keeps |
| `recurring_tokens_max` | 8000000 | distinct recurring tokens it counts, about sixteen bytes each |
| `clusters_max` | 10000 | clusters the pass may found |
| `cluster_terms_max` | 64 | members a cluster may hold |
| `cluster_join_min` | 0.5 | fraction of a row's sources a cluster must be present in |
| `cluster_merge_cosine` | 0.92 | clusters at or over this cosine merge |
| `cluster_assignments_per_sweep_max` | 200000 | rows assigned per pass |
| `cluster_llm_budget` | 500 | grounding calls per run; 0 grounds nothing |
| `llm_lane` | interactive | the lane grounding calls ride |
| `aliases_per_row_max` | 4 | aliases the model may give one row |

The mining dials (`term_df_*`, `candidates_max`, `recurring_tokens_max`) ride the pass's own digest: changing one re-runs the pass in full on the next sweep and never re-runs a per-source transform. The cluster and budget dials are run-metering.

## Facets from hosts

A connection may put descriptive facets on an envelope — `author`, `label`, `channel`, `thread`, `repository` — in its own vocabulary; they are a field apart from trust properties and never feed an access decision. The Gmail host fills `author` from the `From` header (the display name when there is one) and one `label` per Gmail label ([google-workspace.md](google-workspace.md)). A path host declares no container facet: its folder is the container, and folder entries relate `contains` to their children's roots so membership conducts in the walk ([transforms.md](transforms.md#folders)). The host itself is never a facet row; `inseam query --host` filters by it. Facet values filter queries: `inseam query … --facet label:INBOX --facet author:"Dana Reyes"` keeps only sources anchored to every value named ([../finder/operations.md](../finder/operations.md)).

## Storage

Two catalog tables beside the graph ([storage.md](storage.md)): `vocabulary_rows` (fragment, kind, origin, normalized spelling, document frequency, cluster, gloss) and `clusters` (label, the digest of the embedded text, the vector, member count, the sum of the members' document frequencies, the pass generation that last changed it). Both are created beside the catalog with no schema-version bump, so a node from before the vocabulary fills them on its next sweep; the `sources` table gains a `facets` column in place the same way. The generation counter in `meta` tells the finder's cluster cache when to reload. A mined row adds one fragment, one keyed-fragment entry, one vocabulary row, and one lexical search row, and nothing to the relation table; the keyed-fragment collection at the end of a sweep leaves every fragment with a vocabulary row alone, and the pass retracts its own.
