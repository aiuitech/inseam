//! The wire form of `inseam/sync/1`: one request carries the requester's
//! version vector and the entries it ships; one response carries the
//! responder's vector after applying them and the entries the requester
//! lacked. One round trip therefore moves knowledge both ways, and the
//! vectors let each side compute the other's next suffix without a
//! second negotiation. Bodies are JSON: entries are small and the
//! transport bounds a message, so a binary form buys nothing yet.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use inseam_kernel::network::{LogEntry, Record, VersionVector, LOG_ENTRIES_PER_BATCH_MAX};
use inseam_seams::transport::ProtocolName;
use inseam_seams::SeamError;

/// The protocol name registered with the transport; a breaking change to
/// the messages is a new name.
pub const PROTOCOL: &str = "inseam/sync/1";

/// [`PROTOCOL`] as the transport's validated type. The constant is
/// lowercase ASCII with slashes and a digit, within the length bound, so
/// the constructor cannot refuse it.
pub fn protocol_name() -> ProtocolName {
    ProtocolName::new(PROTOCOL).expect("the sync protocol name is a valid protocol name")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncRequest {
    /// What the requester holds, so the responder ships only what it lacks.
    pub vector: VersionVector,
    /// Entries the requester knows the responder lacks — empty on the first
    /// round, when it has not seen the responder's vector yet.
    #[serde(default)]
    pub entries: Vec<LogEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncResponse {
    /// What the responder holds after applying the request's entries.
    pub vector: VersionVector,
    /// Entries the request's vector lacked, at most one batch.
    #[serde(default)]
    pub entries: Vec<LogEntry>,
}

pub(super) fn encode<T: Serialize>(message: &T, what: &str) -> Result<Vec<u8>, SeamError> {
    serde_json::to_vec(message).map_err(|e| SeamError::failed(format!("encoding {what}: {e}")))
}

pub(super) fn decode<T: DeserializeOwned>(body: &[u8], what: &str) -> Result<T, SeamError> {
    serde_json::from_slice(body).map_err(|e| SeamError::failed(format!("{what} is not valid: {e}")))
}

/// Refuse a batch over the bound by name before anything is applied; the
/// store asserts the same bound, so a peer can never turn it into a crash.
pub(super) fn check_batch_bound(entries_len: usize) -> Result<(), SeamError> {
    if entries_len > LOG_ENTRIES_PER_BATCH_MAX {
        return Err(SeamError::Refused(format!(
            "sync batch carries {entries_len} entries; the bound is {LOG_ENTRIES_PER_BATCH_MAX}"
        )));
    }
    Ok(())
}

/// Whether applying `entries` could have changed the roster tables — the
/// condition for announcing `RosterChanged`.
pub(super) fn carries_roster_record(entries: &[LogEntry]) -> bool {
    entries.iter().any(|entry| match &entry.record {
        Record::Node(_)
        | Record::Host(_)
        | Record::Stewardship(_)
        | Record::StewardshipWithdrawn { .. }
        | Record::Expulsion { .. } => true,
        Record::Source { .. } | Record::SourceGone { .. } => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::network::{Epoch, NodeId, Sequence, VectorEntry};

    fn node(byte: u8) -> NodeId {
        NodeId::from_bytes([byte; 32])
    }

    #[test]
    fn the_protocol_name_is_valid() {
        assert_eq!(protocol_name().as_str(), "inseam/sync/1");
    }

    #[test]
    fn a_request_roundtrips_and_defaults_its_entries() {
        let request = SyncRequest {
            vector: VersionVector::new(vec![VectorEntry {
                origin: node(1),
                epoch: Epoch(3),
                seq: Sequence(9),
            }]),
            entries: Vec::new(),
        };
        let body = encode(&request, "sync request").expect("encodes");
        let back: SyncRequest = decode(&body, "sync request").expect("decodes");
        assert_eq!(back, request);
        let bare: SyncRequest = decode(br#"{"vector":[]}"#, "sync request").expect("decodes");
        assert!(bare.entries.is_empty());
        assert!(decode::<SyncRequest>(b"nonsense", "sync request").is_err());
    }

    #[test]
    fn the_batch_bound_is_checked_by_count() {
        assert!(check_batch_bound(LOG_ENTRIES_PER_BATCH_MAX).is_ok());
        assert!(matches!(
            check_batch_bound(LOG_ENTRIES_PER_BATCH_MAX + 1),
            Err(SeamError::Refused(_))
        ));
    }

    #[test]
    fn roster_kinds_are_told_apart_from_catalog_kinds() {
        let expulsion = LogEntry {
            origin: node(1),
            epoch: Epoch(1),
            seq: Sequence(1),
            record: Record::Expulsion { node: node(2) },
        };
        let gone = LogEntry {
            origin: node(1),
            epoch: Epoch(1),
            seq: Sequence(2),
            record: Record::SourceGone {
                address: "inseam://fs-1/a.md".parse().expect("valid"),
            },
        };
        assert!(carries_roster_record(&[gone.clone(), expulsion]));
        assert!(!carries_roster_record(&[gone]));
        assert!(!carries_roster_record(&[]));
    }
}
