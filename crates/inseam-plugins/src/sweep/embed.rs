//! The embedding stage: search rows buffered into batches, batches embedded
//! concurrently, and landed **in order** together with the `indexed` marks
//! of the sources whose rows they complete. Ordered landing is what lets a
//! mark ride with a batch: every row of a completed source is in that batch
//! or an earlier one, so once the mark is written the source is searchable
//! in full (`docs/indexing/storage.md`).

use std::sync::Arc;

use futures_util::stream::{self, StreamExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use inseam_kernel::address::ContentDigest;
use inseam_kernel::fragment::FragmentId;
use inseam_kernel::store::{IndexStore, SearchRow, SourceCompletion, SourceId};
use inseam_seams::SeamError;
use inseam_seams::embedder::Embedder;

/// Search rows per embed+land batch. The lean source+summary shape yields
/// one vector per two rows, filling a 128-input endpoint request.
pub(super) const FLUSH_AT: usize = 256;
/// Batches embedding at once; landing stays sequential and ordered.
const EMBED_IN_FLIGHT: usize = 4;

/// A text-bearing fragment bound for the search tables. Every row enters
/// the full-text index; whether it also gets a vector is the embedder's
/// vector scope's call, and `is_summary` is the one fact that call needs.
#[derive(Debug, Clone)]
pub(super) struct PendingRow {
    pub(super) fragment: FragmentId,
    pub(super) source: Option<SourceId>,
    pub(super) text: String,
    pub(super) is_summary: bool,
}

/// One unit of stage work: rows to embed and land, and the sources whose
/// last rows are in this batch or an earlier one.
#[derive(Debug, Default)]
pub(super) struct Batch {
    pub(super) rows: Vec<PendingRow>,
    pub(super) completed: Vec<SourceCompletion>,
}

/// The writer-side buffer: rows accumulate until a batch is full;
/// each completion remembers how many preceding rows still need dispatch.
#[derive(Debug, Default)]
pub(super) struct RowBuffer {
    rows: Vec<PendingRow>,
    completed: Vec<BufferedCompletion>,
}

/// A source completion's position in the ordered row stream. Once a drain
/// crosses this boundary, every search row for that source has been sent.
#[derive(Debug)]
struct BufferedCompletion {
    row_count_before: usize,
    completion: SourceCompletion,
}

impl RowBuffer {
    pub(super) fn push_rows(&mut self, rows: impl IntoIterator<Item = PendingRow>) {
        self.rows.extend(rows);
    }

    pub(super) fn push_completion(&mut self, completion: SourceCompletion) {
        self.completed.push(BufferedCompletion {
            row_count_before: self.rows.len(),
            completion,
        });
    }

    /// Full batches ready to dispatch, in order. A completion rides the
    /// first batch that crosses its source's final-row boundary.
    pub(super) fn drain_ready(&mut self) -> Vec<Batch> {
        let batch_count = self.rows.len() / FLUSH_AT;
        let mut batches = Vec::with_capacity(batch_count);
        for _batch_index in 0..batch_count {
            batches.push(self.drain_batch(FLUSH_AT));
        }
        batches
    }

    /// Everything left, completions on the final batch (a rows-less batch
    /// if that is all that remains).
    pub(super) fn drain_all(&mut self) -> Vec<Batch> {
        let mut batches = self.drain_ready();
        if self.rows.is_empty() {
            let completed: Vec<SourceCompletion> = self
                .completed
                .drain(..)
                .map(|buffered| buffered.completion)
                .collect();
            if !completed.is_empty() {
                batches.push(Batch {
                    rows: Vec::new(),
                    completed,
                });
            }
        } else {
            batches.push(self.drain_batch(self.rows.len()));
        }
        assert!(self.rows.is_empty());
        assert!(self.completed.is_empty());
        batches
    }

    fn drain_batch(&mut self, row_count: usize) -> Batch {
        assert!(row_count > 0);
        assert!(row_count <= self.rows.len());
        let rest = self.rows.split_off(row_count);
        let rows = std::mem::replace(&mut self.rows, rest);
        let completion_count = self
            .completed
            .partition_point(|buffered| buffered.row_count_before <= row_count);
        let completed = self
            .completed
            .drain(..completion_count)
            .map(|buffered| buffered.completion)
            .collect();
        for buffered in &mut self.completed {
            assert!(buffered.row_count_before > row_count);
            buffered.row_count_before -= row_count;
        }
        Batch { rows, completed }
    }
}

/// What the stage produced: vectors the endpoint computed and vectors the
/// digest-keyed cache already held (`design/indexing.md`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct EmbedTotals {
    pub(super) embedded: usize,
    pub(super) reused: usize,
}

/// A running embedding stage. Submit batches, then `finish` to get the
/// totals (or the stage's first error).
pub(super) struct EmbedStage {
    sender: Option<mpsc::Sender<Batch>>,
    task: JoinHandle<Result<EmbedTotals, SeamError>>,
}

impl EmbedStage {
    pub(super) fn start(embedder: Arc<dyn Embedder>, store: Arc<IndexStore>) -> Self {
        let (sender, receiver) = mpsc::channel::<Batch>(EMBED_IN_FLIGHT);
        let task = tokio::spawn(run(embedder, store, receiver));
        Self {
            sender: Some(sender),
            task,
        }
    }

    /// Queue a batch; waits while `EMBED_IN_FLIGHT` batches are already
    /// queued. A stage that has stopped refuses — `finish` has its error.
    pub(super) async fn submit(&self, batch: Batch) -> Result<(), SeamError> {
        let sender = self.sender.as_ref().expect("submit before finish");
        sender
            .send(batch)
            .await
            .map_err(|_| SeamError::failed("embedding stage stopped early"))
    }

    /// Close the stage and wait for every queued batch to land.
    pub(super) async fn finish(mut self) -> Result<EmbedTotals, SeamError> {
        drop(self.sender.take());
        self.task
            .await
            .map_err(|e| SeamError::failed(format!("embedding stage task failed: {e}")))?
    }
}

/// The stage body: embed up to `EMBED_IN_FLIGHT` batches concurrently, land
/// each in submission order.
async fn run(
    embedder: Arc<dyn Embedder>,
    store: Arc<IndexStore>,
    receiver: mpsc::Receiver<Batch>,
) -> Result<EmbedTotals, SeamError> {
    let batches = stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|batch| (batch, receiver))
    });
    let landed = batches
        .map(|batch| {
            let embedder = Arc::clone(&embedder);
            let store = Arc::clone(&store);
            tokio::spawn(async move { embed_batch(embedder.as_ref(), &store, batch).await })
        })
        .buffered(EMBED_IN_FLIGHT);
    futures_util::pin_mut!(landed);
    let mut totals = EmbedTotals::default();
    while let Some(joined) = landed.next().await {
        let embedded =
            joined.map_err(|e| SeamError::failed(format!("embedding task failed: {e}")))?;
        store
            .land_search_rows(&embedded.rows, &embedded.completed)
            .await?;
        totals.embedded += embedded.embedded;
        totals.reused += embedded.reused;
    }
    Ok(totals)
}

/// One batch after embedding: the rows to land, the completions riding
/// them, and where the vectors came from.
struct EmbeddedBatch {
    rows: Vec<SearchRow>,
    completed: Vec<SourceCompletion>,
    embedded: usize,
    reused: usize,
}

/// Embed one batch's rows — those the vector scope covers; the rest land
/// text-only by design. A row whose text the cache has already embedded
/// under this model takes its vector from there. Never fails: an embedding
/// error leaves the rows text-searchable only, with a warning, rather than
/// gating the sweep.
async fn embed_batch(embedder: &dyn Embedder, store: &IndexStore, batch: Batch) -> EmbeddedBatch {
    let Batch { rows, completed } = batch;
    let scope = embedder.vectors();
    let wanted: Vec<usize> = if embedder.dimensions().is_some() {
        rows.iter()
            .enumerate()
            .filter(|(_, row)| scope.covers(row.is_summary))
            .map(|(position, _)| position)
            .collect()
    } else {
        Vec::new()
    };
    let mut vectors: Vec<Option<Vec<f32>>> = vec![None; rows.len()];
    let mut reused: usize = 0;
    if !wanted.is_empty() {
        let texts: Vec<&str> = wanted.iter().map(|&p| rows[p].text.as_str()).collect();
        let vectored = embed_texts(embedder, store, &texts).await;
        reused = vectored.reused;
        for (position, vector) in wanted.iter().zip(vectored.vectors) {
            vectors[*position] = vector;
        }
    }
    let with_vector = vectors.iter().filter(|v| v.is_some()).count();
    assert!(with_vector <= rows.len());
    assert!(reused <= with_vector);
    let search_rows: Vec<SearchRow> = rows
        .into_iter()
        .zip(vectors)
        .map(|(row, vector)| SearchRow {
            fragment: row.fragment,
            source: row.source,
            text: row.text,
            vector,
        })
        .collect();
    EmbeddedBatch {
        rows: search_rows,
        completed,
        embedded: with_vector - reused,
        reused,
    }
}

/// Vectors for `texts`, in order: cache hits by text digest first, the
/// endpoint for the rest. A cache read failure is logged and treated as a
/// full miss; an endpoint failure leaves the misses without a vector.
struct Vectored {
    vectors: Vec<Option<Vec<f32>>>,
    reused: usize,
}

async fn embed_texts(embedder: &dyn Embedder, store: &IndexStore, texts: &[&str]) -> Vectored {
    let digests: Vec<String> = texts
        .iter()
        .map(|text| ContentDigest::of_bytes(text.as_bytes()).to_hex())
        .collect();
    let cached = match store.cached_embeddings(&digests).await {
        Ok(cached) => cached,
        Err(e) => {
            tracing::warn!("embedding cache read failed; embedding every row: {e}");
            std::collections::HashMap::new()
        }
    };
    let mut vectors: Vec<Option<Vec<f32>>> = digests
        .iter()
        .map(|digest| cached.get(digest).cloned())
        .collect();
    let reused = vectors.iter().filter(|v| v.is_some()).count();
    let misses: Vec<usize> = (0..texts.len()).filter(|&i| vectors[i].is_none()).collect();
    assert_eq!(reused + misses.len(), texts.len());
    if !misses.is_empty() {
        let missing_texts: Vec<&str> = misses.iter().map(|&i| texts[i]).collect();
        match embedder.embed(&missing_texts).await {
            Ok(embedded) => {
                assert_eq!(
                    embedded.len(),
                    misses.len(),
                    "embedder returns one vector per text"
                );
                for (position, vector) in misses.iter().zip(embedded) {
                    vectors[*position] = Some(vector);
                }
            }
            Err(e) => {
                tracing::warn!("embedding failed; rows stay text-searchable only: {e}");
            }
        }
    }
    Vectored { vectors, reused }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::store::VectorScope;

    fn row(n: i64) -> PendingRow {
        PendingRow {
            fragment: FragmentId(n),
            source: Some(SourceId(1)),
            text: format!("row {n}"),
            is_summary: n % 2 == 0,
        }
    }

    struct ScopedHashed {
        scope: VectorScope,
    }

    #[async_trait::async_trait]
    impl Embedder for ScopedHashed {
        fn dimensions(&self) -> Option<usize> {
            Some(8)
        }

        fn vectors(&self) -> VectorScope {
            self.scope
        }

        async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, SeamError> {
            Ok(texts.iter().map(|_| vec![1.0; 8]).collect())
        }
    }

    async fn store() -> (tempfile::TempDir, Arc<IndexStore>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = IndexStore::open(dir.path()).await.expect("opens");
        store
            .declare_embedding(inseam_kernel::store::EmbeddingIdentity {
                model: "test-model".into(),
                dimensions: 8,
                vectors: VectorScope::All,
            })
            .await
            .expect("declares");
        (dir, Arc::new(store))
    }

    fn batch(rows: std::ops::Range<i64>) -> Batch {
        Batch {
            rows: rows.map(row).collect(),
            completed: Vec::new(),
        }
    }

    #[tokio::test]
    async fn summaries_scope_embeds_only_summary_rows_and_keeps_the_rest_text_only() {
        let (_dir, store) = store().await;
        let embedded = embed_batch(
            &ScopedHashed {
                scope: VectorScope::Summaries,
            },
            &store,
            batch(0..6),
        )
        .await;
        assert_eq!(embedded.embedded, 3);
        assert_eq!(embedded.reused, 0);
        assert_eq!(embedded.rows.len(), 6);
        for (i, r) in embedded.rows.iter().enumerate() {
            assert_eq!(r.vector.is_some(), i % 2 == 0, "row {i}");
        }
    }

    #[tokio::test]
    async fn all_scope_embeds_every_row() {
        let (_dir, store) = store().await;
        let embedded = embed_batch(
            &ScopedHashed {
                scope: VectorScope::All,
            },
            &store,
            batch(0..6),
        )
        .await;
        assert_eq!(embedded.embedded, 6);
        assert!(embedded.rows.iter().all(|r| r.vector.is_some()));
    }

    #[tokio::test]
    async fn text_already_embedded_under_this_model_is_reused_not_re_embedded() {
        let (_dir, store) = store().await;
        let embedder = ScopedHashed {
            scope: VectorScope::All,
        };
        let first = embed_batch(&embedder, &store, batch(0..4)).await;
        store
            .land_search_rows(&first.rows, &first.completed)
            .await
            .expect("lands");
        assert_eq!(first.embedded, 4);

        // Rows 2..4 repeat texts the store has landed; 4..6 are new.
        let second = embed_batch(&embedder, &store, batch(2..6)).await;
        assert_eq!(second.reused, 2);
        assert_eq!(second.embedded, 2);
        assert!(second.rows.iter().all(|r| r.vector.is_some()));
    }

    fn completion(n: i64) -> SourceCompletion {
        SourceCompletion {
            source: SourceId(n),
            stamp: "s".into(),
            inventory: Vec::new(),
        }
    }

    #[test]
    fn completions_wait_until_their_rows_are_dispatched() {
        let mut buffer = RowBuffer::default();
        buffer.push_rows((0..FLUSH_AT as i64 + 3).map(row));
        buffer.push_completion(completion(1));
        let ready = buffer.drain_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].rows.len(), FLUSH_AT);
        assert!(
            ready[0].completed.is_empty(),
            "3 rows still buffered behind the completion"
        );
        let rest = buffer.drain_all();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].rows.len(), 3);
        assert_eq!(rest[0].completed.len(), 1);
    }

    #[test]
    fn completions_ride_the_batch_that_empties_the_buffer() {
        let mut buffer = RowBuffer::default();
        buffer.push_rows((0..FLUSH_AT as i64 * 2).map(row));
        buffer.push_completion(completion(1));
        let ready = buffer.drain_ready();
        assert_eq!(ready.len(), 2);
        assert!(ready[0].completed.is_empty());
        assert_eq!(ready[1].completed.len(), 1);
        assert!(buffer.drain_all().is_empty());
    }

    #[test]
    fn earlier_source_completes_while_later_source_rows_remain() {
        let mut buffer = RowBuffer::default();
        buffer.push_rows((0..10).map(row));
        buffer.push_completion(completion(1));
        buffer.push_rows((10..FLUSH_AT as i64 + 10).map(row));
        buffer.push_completion(completion(2));

        let ready = buffer.drain_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].rows.len(), FLUSH_AT);
        assert_eq!(ready[0].completed.len(), 1);
        assert_eq!(ready[0].completed[0].source, SourceId(1));

        let rest = buffer.drain_all();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].rows.len(), 10);
        assert_eq!(rest[0].completed.len(), 1);
        assert_eq!(rest[0].completed[0].source, SourceId(2));
    }

    #[test]
    fn a_rowless_source_still_completes() {
        let mut buffer = RowBuffer::default();
        buffer.push_completion(completion(7));
        assert!(buffer.drain_ready().is_empty());
        let all = buffer.drain_all();
        assert_eq!(all.len(), 1);
        assert!(all[0].rows.is_empty());
        assert_eq!(all[0].completed[0].source, SourceId(7));
    }
}
