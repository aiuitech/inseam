//! The rungs of the discovery ladder (`design/finder.md`) as pure-ish
//! functions over the store, the finder, and a reader of source content.
//! Two callers climb them: the operations service, serving its own
//! transports, and the routing handler, serving a peer's routed request —
//! so a `scan` answered over the network is the very same scan answered
//! locally, with one set of range checks, one clamp, and one media
//! fallback. Nothing here decides *where* content comes from; the caller
//! hands in a [`Reader`], and the rung reads through it.

use std::collections::HashMap;
use std::sync::Arc;

use inseam_kernel::address::{Address, ContentLength};
use inseam_kernel::fragment::Mimetype;
use inseam_kernel::store::{IndexStore, SourceId, StoredFragment, StoredSource};
use inseam_seams::SeamError;
use inseam_seams::connection::Connection;
use inseam_seams::dates::ymd;
use inseam_seams::finder::{Finder, FinderRequest, QueryTrace, RankedFragment, RankedSource};
use inseam_seams::operations::{
    EnvelopeView, ExpandResponse, FETCH_BYTES_MAX, FetchBytesResponse, FetchResponse, FileBytes,
    FragmentHint, FragmentView, QueryResult, RelationView, SCAN_LINES_MAX, ScanResponse,
};
use inseam_seams::routing::Routing;
use inseam_seams::text::{check_line_range, count_lines, is_indexable_text, preview, slice_lines};

/// Characters of fragment text shown in hints and expand views.
pub(crate) const PREVIEW_CHARS: usize = 280;
/// Most results one query serves, on this node or on a peer's behalf; a
/// request past it is clamped, never refused.
pub(crate) const QUERY_LIMIT_MAX: usize = 50;
const _: () = assert!(QUERY_LIMIT_MAX >= 1, "a query serves at least one result");

/// Where a rung reads source content from: the steward's own connection,
/// or the routing seam when the steward is another node. The rung is the
/// same either way; only the reader differs.
pub(crate) enum Reader {
    Connection(Arc<dyn Connection>),
    Routing(Arc<dyn Routing>),
}

impl Reader {
    async fn read_text(&self, address: &Address) -> Result<String, SeamError> {
        match self {
            Self::Connection(connection) => connection.read_text(address).await,
            Self::Routing(routing) => routing.read_text(address).await,
        }
    }

    async fn read_lines(
        &self,
        address: &Address,
        start: u64,
        end: u64,
    ) -> Result<String, SeamError> {
        match self {
            Self::Connection(connection) => connection.read_lines(address, start, end).await,
            Self::Routing(routing) => routing.read_lines(address, start, end).await,
        }
    }

    async fn read_bytes(&self, address: &Address) -> Result<Vec<u8>, SeamError> {
        match self {
            Self::Connection(connection) => connection.read_bytes(address).await,
            Self::Routing(routing) => routing.read_bytes(address).await,
        }
    }
}

/// The catalog row an address names, or the typed refusal.
pub(crate) async fn source_at(
    store: &IndexStore,
    address: &Address,
) -> Result<StoredSource, SeamError> {
    store
        .source_by_address(address)
        .await?
        .ok_or_else(|| SeamError::UnknownSource(address.clone()))
}

/// The limit a query serves: at least one, at most [`QUERY_LIMIT_MAX`].
pub(crate) fn clamp_query_limit(limit: usize) -> usize {
    let clamped = limit.clamp(1, QUERY_LIMIT_MAX);
    assert!(clamped >= 1);
    assert!(clamped <= QUERY_LIMIT_MAX);
    clamped
}

/// Rung one on this node's index: the finder's ranked sources rendered as
/// results, `via` unset because they are this node's own.
pub(crate) async fn query(
    finder: &dyn Finder,
    request: &FinderRequest,
) -> Result<(Vec<QueryResult>, QueryTrace), SeamError> {
    assert!(request.limit >= 1);
    assert!(request.limit <= QUERY_LIMIT_MAX);
    let discovery = finder.discover(request).await?;
    let results: Vec<QueryResult> = discovery.ranked.into_iter().map(query_result).collect();
    assert!(
        results.len() <= request.limit,
        "the finder honors its limit"
    );
    Ok((results, discovery.trace))
}

fn query_result(ranked: RankedSource) -> QueryResult {
    QueryResult {
        address: ranked.source.address.clone(),
        score: round3(ranked.score),
        summary: ranked.summary,
        envelope: envelope_view(&ranked.source),
        hints: ranked.hints.iter().map(hint_view).collect(),
        replicas: ranked.replicas,
        via: None,
    }
}

/// Rung two: one source's subtree and cross-links from this node's index.
pub(crate) async fn expand(
    store: &IndexStore,
    finder: &dyn Finder,
    source: &StoredSource,
) -> Result<ExpandResponse, SeamError> {
    let expansion = finder.expand(source).await?;
    let mut addresses: HashMap<SourceId, Address> = HashMap::new();
    let mut neighbors = Vec::with_capacity(expansion.neighbors.len());
    for fragment in &expansion.neighbors {
        let address = match fragment.source {
            Some(owner) => neighbor_address(store, &mut addresses, owner).await,
            None => None,
        };
        neighbors.push(fragment_view(fragment, address));
    }
    Ok(ExpandResponse {
        address: source.address.clone(),
        summary: store.summary_of(source.id).await?,
        fragments: expansion
            .fragments
            .iter()
            .map(|fragment| fragment_view(fragment, None))
            .collect(),
        relations: expansion.relations.iter().map(RelationView::from).collect(),
        neighbors,
    })
}

/// A neighbor's source address, read once per source: a keyed fragment's
/// neighbors cluster by source, and a lookup failure only leaves the hop
/// unaddressed, never fails the expansion.
async fn neighbor_address(
    store: &IndexStore,
    cache: &mut HashMap<SourceId, Address>,
    owner: SourceId,
) -> Option<Address> {
    if let Some(address) = cache.get(&owner) {
        return Some(address.clone());
    }
    let address = store
        .source(owner)
        .await
        .ok()
        .flatten()
        .map(|s| s.address)?;
    cache.insert(owner, address.clone());
    Some(address)
}

/// A checked, clamped scan range: 1-based inclusive, held to
/// [`SCAN_LINES_MAX`] lines. Built before anything is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScanWindow {
    pub start: u64,
    pub end: u64,
}

pub(crate) fn scan_window(start: u64, end: u64) -> Result<ScanWindow, SeamError> {
    check_line_range(start, end)?;
    let end = clamp_scan_end(start, end);
    assert!(start >= 1);
    assert!(end >= start);
    assert!(end - start < SCAN_LINES_MAX);
    Ok(ScanWindow { start, end })
}

/// Rung three: lines of a text source through the reader, or of the
/// largest text descendant of anything else. The reader arrives as a
/// result because only a text source needs one — a scan of a video served
/// from its transcript in this node's index must not fail for want of a
/// path to the video's host.
pub(crate) async fn scan(
    store: &IndexStore,
    source: StoredSource,
    window: ScanWindow,
    reader: Result<Reader, SeamError>,
) -> Result<ScanResponse, SeamError> {
    // Scan reads what the index reads as text — `text/*` and the
    // structured application types — one list shared with `fetch` and
    // the chunker, so the three never disagree. Everything else (media,
    // PDFs) is served through a text descendant.
    if serves_text(&source.envelope.content_type) {
        let text = reader?
            .read_lines(&source.address, window.start, window.end)
            .await?;
        return Ok(scan_text_response(source, window, text));
    }
    scan_stand_in_response(store, source, window).await
}

fn scan_text_response(source: StoredSource, window: ScanWindow, text: String) -> ScanResponse {
    let lines_total = match source.envelope.length {
        ContentLength::Lines(n) => Some(n),
        ContentLength::Bytes(_) => None,
    };
    ScanResponse {
        address: source.address,
        mimetype: source.envelope.content_type.to_string(),
        start: window.start,
        end: lines_total.map_or(window.end, |total| window.end.min(total)),
        lines_total,
        text,
        served_from_fragment: None,
    }
}

/// Scanning anything that is not text means reading lines of its text
/// descendants — the transcript case. The largest one stands in.
async fn scan_stand_in_response(
    store: &IndexStore,
    source: StoredSource,
    window: ScanWindow,
) -> Result<ScanResponse, SeamError> {
    let fragments = store.fragments_of(source.id).await?;
    let Some((fragment, text)) = scan_stand_in(&fragments) else {
        return Err(SeamError::NothingToScan(source.address));
    };
    let sliced = slice_lines(text, window.start, window.end)?;
    let lines_total = count_lines(text);
    Ok(ScanResponse {
        address: source.address,
        mimetype: fragment.mimetype.to_string(),
        start: window.start,
        end: window.end.min(lines_total),
        lines_total: Some(lines_total),
        text: sliced,
        served_from_fragment: Some(fragment.id),
    })
}

/// The refusal `fetch` makes before any read, local or routed: a source
/// that is not text is fetched as bytes, by name.
pub(crate) fn check_text_fetch(source: &StoredSource) -> Result<(), SeamError> {
    if serves_text(&source.envelope.content_type) {
        Ok(())
    } else {
        Err(SeamError::BinaryFetch(
            source.address.clone(),
            source.envelope.content_type.to_string(),
        ))
    }
}

/// Rung four: the whole source as text. The caller has already checked
/// the content type is one `fetch` serves (pair assertion below).
pub(crate) async fn fetch(
    reader: &Reader,
    source: StoredSource,
) -> Result<FetchResponse, SeamError> {
    assert!(
        serves_text(&source.envelope.content_type),
        "checked before the read"
    );
    let text = reader.read_text(&source.address).await?;
    Ok(FetchResponse {
        address: source.address,
        content_type: source.envelope.content_type.to_string(),
        text,
    })
}

/// What the catalog knows about the content at an address: a source's
/// content type and byte size when the address is cataloged, else the
/// mimetype of a fragment that references it (a linked image). Anything
/// else is unknown — a fetch never reaches for content the index has no
/// record of.
pub(crate) async fn content_at(
    store: &IndexStore,
    address: &Address,
) -> Result<(String, Option<u64>), SeamError> {
    if let Some(source) = store.source_by_address(address).await? {
        let known_bytes = match source.envelope.length {
            ContentLength::Bytes(n) => Some(n),
            ContentLength::Lines(_) => None,
        };
        return Ok((source.envelope.content_type.to_string(), known_bytes));
    }
    match store.fragment_referencing(address).await? {
        Some(fragment) => Ok((fragment.mimetype.to_string(), None)),
        None => Err(SeamError::UnknownSource(address.clone())),
    }
}

/// Refuse a byte fetch before reading when the catalog already knows the
/// size is past the bound; the read itself is the paired check.
pub(crate) fn check_bytes_bound(
    address: &Address,
    known_bytes: Option<u64>,
) -> Result<(), SeamError> {
    match known_bytes {
        Some(bytes) if bytes > FETCH_BYTES_MAX => Err(fetch_too_large(address, bytes)),
        Some(_) | None => Ok(()),
    }
}

/// The paired check after a read: what actually came back fits one
/// message, whatever the catalog claimed.
pub(crate) fn check_bytes_read(address: &Address, bytes: &[u8]) -> Result<(), SeamError> {
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if size > FETCH_BYTES_MAX {
        return Err(fetch_too_large(address, size));
    }
    Ok(())
}

/// Rung four for content that is not text: the bytes with their content
/// type, bounded per message. The caller has checked the catalog's size.
pub(crate) async fn fetch_bytes(
    reader: &Reader,
    address: Address,
    content_type: String,
) -> Result<FetchBytesResponse, SeamError> {
    let bytes = reader.read_bytes(&address).await?;
    check_bytes_read(&address, &bytes)?;
    Ok(FetchBytesResponse {
        address,
        content_type,
        bytes: FileBytes(bytes),
    })
}

/// What `fetch` and `scan` read through the connection as text: the
/// index's text types, plus folders — a folder has no bytes, and its host
/// serves its name listing as text (`design/indexing.md`, folders).
/// Folders stay out of `is_indexable_text` itself so no text transform
/// ever claims one.
pub(crate) fn serves_text(mimetype: &Mimetype) -> bool {
    is_indexable_text(mimetype) || mimetype.is_directory()
}

/// The end line a scan serves: the request's, held to [`SCAN_LINES_MAX`]
/// lines from `start`. The caller has already checked the range.
fn clamp_scan_end(start: u64, end: u64) -> u64 {
    assert!(start >= 1);
    assert!(end >= start);
    let span_end = start.saturating_add(SCAN_LINES_MAX - 1);
    let clamped = end.min(span_end);
    assert!(clamped >= start);
    clamped
}

/// The fragment a scan of a non-text source reads instead: its largest
/// text fragment that is source content rather than derived understanding
/// (no summaries, no entities).
fn scan_stand_in(fragments: &[StoredFragment]) -> Option<(&StoredFragment, &str)> {
    fragments
        .iter()
        .filter(|f| is_indexable_text(&f.mimetype))
        .filter(|f| !f.mimetype.is_inseam_defined())
        .filter_map(|f| f.text.as_deref().map(|t| (f, t)))
        .max_by_key(|(_, t)| t.len())
}

pub(crate) fn envelope_view(source: &StoredSource) -> EnvelopeView {
    let e = &source.envelope;
    EnvelopeView {
        source_type: e.source_type.clone(),
        content_type: e.content_type.to_string(),
        length: e.length,
        created: e.created.map(ymd),
        modified: e.modified.map(ymd),
        title: e.hint.clone(),
        content_digest: e.content_digest,
    }
}

fn hint_view(ranked: &RankedFragment) -> FragmentHint {
    let f = &ranked.fragment;
    FragmentHint {
        fragment: f.id,
        mimetype: f.mimetype.to_string(),
        score: round3(ranked.score),
        extent: f.extent,
        text: f
            .text
            .as_deref()
            .map(|t| preview(t, PREVIEW_CHARS))
            .unwrap_or_default(),
    }
}

fn fragment_view(f: &StoredFragment, source: Option<Address>) -> FragmentView {
    FragmentView {
        id: f.id,
        mimetype: f.mimetype.to_string(),
        extent: f.extent,
        text: f.text.as_deref().map(|t| preview(t, PREVIEW_CHARS)),
        content_address: f.content_address.clone(),
        source,
    }
}

fn fetch_too_large(address: &Address, bytes: u64) -> SeamError {
    SeamError::FetchTooLarge {
        address: address.clone(),
        bytes,
        limit: FETCH_BYTES_MAX,
    }
}

pub(crate) fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use inseam_kernel::fragment::FragmentId;

    use super::*;

    #[test]
    fn scan_end_is_held_to_the_span_bound() {
        assert_eq!(clamp_scan_end(1, 1), 1);
        assert_eq!(clamp_scan_end(5, 9), 9);
        assert_eq!(clamp_scan_end(1, SCAN_LINES_MAX), SCAN_LINES_MAX);
        assert_eq!(clamp_scan_end(1, SCAN_LINES_MAX + 1), SCAN_LINES_MAX);
        assert_eq!(clamp_scan_end(10, u64::MAX), 10 + SCAN_LINES_MAX - 1);
        assert_eq!(clamp_scan_end(u64::MAX, u64::MAX), u64::MAX);
    }

    #[test]
    fn scan_window_refuses_bad_ranges_before_clamping() {
        assert!(matches!(
            scan_window(0, 5),
            Err(SeamError::ScanRange { start: 0, end: 5 })
        ));
        assert!(matches!(
            scan_window(6, 5),
            Err(SeamError::ScanRange { start: 6, end: 5 })
        ));
        let window = scan_window(3, u64::MAX).expect("valid");
        assert_eq!(
            window,
            ScanWindow {
                start: 3,
                end: 3 + SCAN_LINES_MAX - 1
            }
        );
    }

    #[test]
    fn query_limit_is_clamped_to_one_through_the_max() {
        assert_eq!(clamp_query_limit(0), 1);
        assert_eq!(clamp_query_limit(7), 7);
        assert_eq!(clamp_query_limit(QUERY_LIMIT_MAX + 1), QUERY_LIMIT_MAX);
    }

    #[test]
    fn a_scan_stand_in_is_the_largest_text_fragment_that_is_source_content() {
        let fragment = |id: i64, mimetype: &str, text: &str| StoredFragment {
            id: FragmentId(id),
            source: None,
            mimetype: Mimetype::parse(mimetype).expect("valid"),
            text: Some(text.to_string()),
            extent: None,
            content_address: None,
        };
        let fragments = vec![
            fragment(
                1,
                "text/x-inseam-summary",
                "a very long summary of the video",
            ),
            fragment(2, "text/plain", "short"),
            fragment(3, "text/plain", "the transcript, longest"),
            fragment(
                4,
                "application/json",
                "{\"structured\": \"text counts too\"}",
            ),
            fragment(
                5,
                "image/png",
                "not text however long this reference text is",
            ),
        ];
        let (chosen, text) = scan_stand_in(&fragments).expect("a stand-in");
        assert_eq!(chosen.id, FragmentId(4));
        assert_eq!(text, "{\"structured\": \"text counts too\"}");
        assert!(
            scan_stand_in(&fragments[..1]).is_none(),
            "summaries never stand in"
        );
    }
}
