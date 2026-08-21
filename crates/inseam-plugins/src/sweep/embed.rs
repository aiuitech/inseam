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

use inseam_kernel::fragment::FragmentId;
use inseam_kernel::store::{IndexStore, SearchRow, SourceCompletion, SourceId};
use inseam_seams::embedder::Embedder;
use inseam_seams::SeamError;

/// Search rows per embed+land batch.
pub(super) const FLUSH_AT: usize = 128;
/// Batches embedding at once; landing stays sequential and ordered.
const EMBED_IN_FLIGHT: usize = 4;

/// A text-bearing fragment bound for the search tables.
#[derive(Debug, Clone)]
pub(super) struct PendingRow {
    pub(super) fragment: FragmentId,
    pub(super) source: Option<SourceId>,
    pub(super) text: String,
}

/// One unit of stage work: rows to embed and land, and the sources whose
/// last rows are in this batch or an earlier one.
#[derive(Debug, Default)]
pub(super) struct Batch {
    pub(super) rows: Vec<PendingRow>,
    pub(super) completed: Vec<SourceCompletion>,
}

/// The writer-side buffer: rows accumulate until a batch is full;
/// completions are released only once every buffered row has been
/// dispatched ahead of them.
#[derive(Debug, Default)]
pub(super) struct RowBuffer {
    rows: Vec<PendingRow>,
    completed: Vec<SourceCompletion>,
}

impl RowBuffer {
    pub(super) fn push_rows(&mut self, rows: impl IntoIterator<Item = PendingRow>) {
        self.rows.extend(rows);
    }

    pub(super) fn push_completion(&mut self, completion: SourceCompletion) {
        self.completed.push(completion);
    }

    /// Full batches ready to dispatch, in order. Completions ride only when
    /// no rows remain buffered behind them.
    pub(super) fn drain_ready(&mut self) -> Vec<Batch> {
        let mut batches = Vec::new();
        while self.rows.len() >= FLUSH_AT {
            let rest = self.rows.split_off(FLUSH_AT);
            let rows = std::mem::replace(&mut self.rows, rest);
            batches.push(Batch {
                rows,
                completed: Vec::new(),
            });
        }
        if self.rows.is_empty()
            && !self.completed.is_empty()
            && let Some(last) = batches.last_mut()
        {
            last.completed = std::mem::take(&mut self.completed);
        }
        batches
    }

    /// Everything left, completions on the final batch (a rows-less batch
    /// if that is all that remains).
    pub(super) fn drain_all(&mut self) -> Vec<Batch> {
        let mut batches = self.drain_ready();
        let rows = std::mem::take(&mut self.rows);
        let completed = std::mem::take(&mut self.completed);
        if !rows.is_empty() || !completed.is_empty() {
            batches.push(Batch { rows, completed });
        }
        assert!(self.rows.is_empty());
        assert!(self.completed.is_empty());
        batches
    }
}

/// A running embedding stage. Submit batches, then `finish` to get the
/// number of rows embedded (or the stage's first error).
pub(super) struct EmbedStage {
    sender: Option<mpsc::Sender<Batch>>,
    task: JoinHandle<Result<usize, SeamError>>,
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
        let sender = self
            .sender
            .as_ref()
            .expect("submit before finish");
        sender
            .send(batch)
            .await
            .map_err(|_| SeamError::failed("embedding stage stopped early"))
    }

    /// Close the stage and wait for every queued batch to land.
    pub(super) async fn finish(mut self) -> Result<usize, SeamError> {
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
) -> Result<usize, SeamError> {
    let batches = stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|batch| (batch, receiver))
    });
    let landed = batches
        .map(|batch| {
            let embedder = Arc::clone(&embedder);
            tokio::spawn(async move { embed_batch(embedder.as_ref(), batch).await })
        })
        .buffered(EMBED_IN_FLIGHT);
    futures_util::pin_mut!(landed);
    let mut embedded: usize = 0;
    while let Some(joined) = landed.next().await {
        let (rows, completed, embedded_now) =
            joined.map_err(|e| SeamError::failed(format!("embedding task failed: {e}")))?;
        store.land_search_rows(&rows, &completed).await?;
        embedded += embedded_now;
    }
    Ok(embedded)
}

/// Embed one batch's rows. Never fails: an embedding error leaves the rows
/// text-searchable only, with a warning, rather than gating the sweep.
async fn embed_batch(
    embedder: &dyn Embedder,
    batch: Batch,
) -> (Vec<SearchRow>, Vec<SourceCompletion>, usize) {
    let Batch { rows, completed } = batch;
    let vectors: Vec<Option<Vec<f32>>> = if embedder.dimensions().is_some() && !rows.is_empty() {
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        match embedder.embed(&texts).await {
            Ok(vectors) => {
                assert_eq!(vectors.len(), rows.len(), "embedder returns one vector per text");
                vectors.into_iter().map(Some).collect()
            }
            Err(e) => {
                tracing::warn!("embedding failed; rows stay text-searchable only: {e}");
                vec![None; rows.len()]
            }
        }
    } else {
        vec![None; rows.len()]
    };
    let embedded = vectors.iter().filter(|v| v.is_some()).count();
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
    (search_rows, completed, embedded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(n: i64) -> PendingRow {
        PendingRow {
            fragment: FragmentId(n),
            source: Some(SourceId(1)),
            text: format!("row {n}"),
        }
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
        assert!(ready[0].completed.is_empty(), "3 rows still buffered behind the completion");
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
