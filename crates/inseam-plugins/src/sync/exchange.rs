//! One exchange with one peer, and the handler that answers one: the two
//! halves of `inseam/sync/1` (`design/address-sync.md`). The requester
//! opens with its vector and nothing else, learns the responder's vector
//! from the reply, and from then on each round ships the suffix the
//! responder lacks beside its own vector, until neither side has news.
//! Every round is bounded in entries, every exchange in rounds, and every
//! request in time.

use std::sync::Arc;

use inseam_kernel::network::{LogEntry, NodeId, VersionVector, LOG_ENTRIES_PER_BATCH_MAX};
use inseam_seams::roster::RosterChanged;
use inseam_seams::transport::{PeerAddress, ProtocolName, RequestHandler};
use inseam_seams::SeamError;

use super::protocol::{
    carries_roster_record, check_batch_bound, decode, encode, protocol_name, SyncRequest,
    SyncResponse,
};
use super::Inner;

/// Rounds one exchange may take before it yields to the next scheduled
/// one: at one batch per direction per round, this moves 128,000 entries
/// each way, which a peer far behind covers over a few rounds of the
/// timer rather than one exchange that never ends.
pub const ROUNDS_MAX: u32 = 64;
const _: () = assert!(ROUNDS_MAX > 1, "an exchange needs a second round to ship anything");
const _: () = assert!(
    (ROUNDS_MAX as usize) * LOG_ENTRIES_PER_BATCH_MAX == 128_000,
    "the per-exchange ceiling is the product of the two bounds"
);

/// What one exchange moved.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Exchanged {
    /// Entries applied from the peer.
    pub received: u64,
    /// Entries shipped to the peer.
    pub sent: u64,
    pub rounds: u32,
}

/// Drive one exchange to quiescence or the round bound. A round makes no
/// progress when the peer applied nothing of ours (its vector stands
/// still) and shipped nothing new; the exchange stops there rather than
/// spending the remaining rounds re-shipping what the peer refuses.
pub(super) async fn exchange(inner: &Inner, peer: &PeerAddress) -> Result<Exchanged, SeamError> {
    let protocol = protocol_name();
    let mut outbound: Vec<LogEntry> = Vec::new();
    let mut previous_vector: Option<VersionVector> = None;
    let mut totals = Exchanged::default();
    for round in 1..=ROUNDS_MAX {
        totals.rounds = round;
        let request = SyncRequest {
            vector: inner.store.version_vector(&inner.local).await?,
            entries: std::mem::take(&mut outbound),
        };
        let sent = u64::try_from(request.entries.len()).expect("a bounded batch fits u64");
        let response = round_trip(inner, peer, &protocol, &request).await?;
        let applied = apply_batch(inner, &response.entries).await?;
        totals.sent = totals.sent.saturating_add(sent);
        totals.received = totals.received.saturating_add(applied);
        outbound = inner
            .store
            .log_after(&inner.local, &response.vector, LOG_ENTRIES_PER_BATCH_MAX)
            .await?;
        assert!(outbound.len() <= LOG_ENTRIES_PER_BATCH_MAX);
        let quiet = applied == 0 && outbound.is_empty();
        let stalled = applied == 0 && previous_vector.as_ref() == Some(&response.vector);
        if quiet || stalled {
            return Ok(totals);
        }
        previous_vector = Some(response.vector);
    }
    tracing::debug!(
        peer = %peer.id.short(),
        "sync exchange used all {ROUNDS_MAX} rounds; the next round continues it"
    );
    Ok(totals)
}

/// One request under the configured timeout, applied both at the
/// transport and around it: the transport's own deadline is the peer's to
/// honor, the outer one is ours.
async fn round_trip(
    inner: &Inner,
    peer: &PeerAddress,
    protocol: &ProtocolName,
    request: &SyncRequest,
) -> Result<SyncResponse, SeamError> {
    let body = encode(request, "sync request")?;
    let reply = tokio::time::timeout(
        inner.request_timeout,
        inner
            .transport
            .request(peer, protocol, body, inner.request_timeout),
    )
    .await
    .map_err(|_| {
        SeamError::failed(format!(
            "sync with {} timed out after {}s",
            peer.id.short(),
            inner.request_timeout.as_secs()
        ))
    })??;
    let response: SyncResponse = decode(&reply, "sync response")?;
    check_batch_bound(response.entries.len())?;
    Ok(response)
}

/// Hold and materialize a peer's batch, announcing a roster change when
/// one could have landed. Returns how many entries were applied.
pub(super) async fn apply_batch(inner: &Inner, entries: &[LogEntry]) -> Result<u64, SeamError> {
    check_batch_bound(entries.len())?;
    if entries.is_empty() {
        return Ok(0);
    }
    let report = inner.store.apply_remote(&inner.local, entries).await?;
    if report.refused > 0 {
        tracing::debug!(refused = report.refused, "sync batch carried entries the store refused");
    }
    if report.applied > 0 && carries_roster_record(entries) {
        inner.bus.emit(&RosterChanged);
    }
    Ok(report.applied)
}

/// The responder's half: apply what the peer shipped, answer with the
/// vector afterwards and the suffix the peer's vector lacks.
pub(super) struct Handler {
    pub inner: Arc<Inner>,
}

#[async_trait::async_trait]
impl RequestHandler for Handler {
    async fn handle(&self, peer: NodeId, body: Vec<u8>) -> Result<Vec<u8>, SeamError> {
        assert_ne!(peer, self.inner.local, "the transport never hands a node its own request");
        let request: SyncRequest = decode(&body, "sync request")?;
        check_batch_bound(request.entries.len())?;
        apply_batch(&self.inner, &request.entries).await?;
        let response = SyncResponse {
            vector: self.inner.store.version_vector(&self.inner.local).await?,
            entries: self
                .inner
                .store
                .log_after(&self.inner.local, &request.vector, LOG_ENTRIES_PER_BATCH_MAX)
                .await?,
        };
        assert!(response.entries.len() <= LOG_ENTRIES_PER_BATCH_MAX);
        encode(&response, "sync response")
    }
}
