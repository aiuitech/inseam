//! Addresses and envelopes: the founding constraint is that source data never
//! moves, only addresses do (`design/addressing.md`). An address is
//! `host identity + locator within that host`; the envelope is the small,
//! size-bounded metadata record that travels with it.

use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::fragment::Mimetype;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AddressError {
    #[error("host id may not be empty")]
    EmptyHost,
    #[error("host id `{0}` may not contain `/`, `:` or whitespace")]
    InvalidHost(String),
    #[error("locator may not be empty")]
    EmptyLocator,
    #[error("locator `{0}` may not start with `/`")]
    AbsoluteLocator(String),
    #[error("`{0}` is not an address of the form inseam://<host>/<locator>")]
    Unparseable(String),
    #[error("`{0}` is not a content digest (64 lowercase hex characters)")]
    InvalidDigest(String),
}

/// Identity of a host — where sources live. Hosts have no inseam machinery;
/// the id exists so addresses can name them and stewards can route to them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostId(String);

impl HostId {
    pub fn new(id: impl Into<String>) -> Result<Self, AddressError> {
        let id = id.into();
        if id.is_empty() {
            return Err(AddressError::EmptyHost);
        }
        if id.contains(['/', ':']) || id.chars().any(char::is_whitespace) {
            return Err(AddressError::InvalidHost(id));
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for HostId {
    type Error = AddressError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<HostId> for String {
    fn from(h: HostId) -> String {
        h.0
    }
}

impl fmt::Display for HostId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The within-host part of an address. Opaque to the core; only the host's
/// connection interprets it. Never starts with `/` so that
/// `inseam://<host>/<locator>` round-trips unambiguously.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Locator(String);

impl Locator {
    pub fn new(locator: impl Into<String>) -> Result<Self, AddressError> {
        let locator = locator.into();
        if locator.is_empty() {
            return Err(AddressError::EmptyLocator);
        }
        if locator.starts_with('/') {
            return Err(AddressError::AbsoluteLocator(locator));
        }
        Ok(Self(locator))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Locator {
    type Error = AddressError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<Locator> for String {
    fn from(l: Locator) -> String {
        l.0
    }
}

/// The global name of a source: host identity + locator within that host.
/// Rendered as `inseam://<host>/<locator>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Address {
    pub host: HostId,
    pub locator: Locator,
}

impl Address {
    pub fn new(host: HostId, locator: Locator) -> Self {
        Self { host, locator }
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "inseam://{}/{}", self.host, self.locator.as_str())
    }
}

impl FromStr for Address {
    type Err = AddressError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rest = s
            .strip_prefix("inseam://")
            .ok_or_else(|| AddressError::Unparseable(s.to_string()))?;
        let (host, locator) = rest
            .split_once('/')
            .ok_or_else(|| AddressError::Unparseable(s.to_string()))?;
        Ok(Self {
            host: HostId::new(host)?,
            locator: Locator::new(locator)?,
        })
    }
}

impl TryFrom<String> for Address {
    type Error = AddressError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Address> for String {
    fn from(a: Address) -> String {
        a.to_string()
    }
}

/// Unix-epoch seconds. All the calendar precision inseam needs; rendering
/// one as a date is a plugin convention (`inseam_seams::dates`), not a
/// kernel one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(pub i64);

impl From<SystemTime> for Timestamp {
    fn from(t: SystemTime) -> Self {
        // Seconds since (or before) the epoch always fit: `i64` covers
        // ±292 billion years, so the saturation is a formality.
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => Self(i64::try_from(d.as_secs()).unwrap_or(i64::MAX)),
            Err(e) => Self(i64::try_from(e.duration().as_secs()).map_or(i64::MIN, |s| -s)),
        }
    }
}

/// Content length as the envelope records it: lines for text, bytes otherwise.
/// Lines are what let callers `scan` a range instead of fetching whole sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "unit", content = "value", rename_all = "lowercase")]
pub enum ContentLength {
    Lines(u64),
    Bytes(u64),
}

impl ContentLength {
    pub fn unit(&self) -> &'static str {
        match self {
            Self::Lines(_) => "lines",
            Self::Bytes(_) => "bytes",
        }
    }

    pub fn value(&self) -> u64 {
        match self {
            Self::Lines(n) | Self::Bytes(n) => *n,
        }
    }
}

impl fmt::Display for ContentLength {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.value(), self.unit())
    }
}

/// BLAKE3 over a source's raw bytes (`design/addressing.md`): the cross-host
/// dedup key. Its one job is to be a merge key — the same file living on two
/// hosts carries equal digests, and the finder collapses the copies into one
/// result. Never a name: identity stays location-addressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContentDigest([u8; 32]);

impl ContentDigest {
    /// Digest a source's complete raw bytes. Whole-content by design: the
    /// caller already holds the bytes it read for indexing, so there is no
    /// incremental path to keep in sync with this one.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

impl FromStr for ContentDigest {
    type Err = AddressError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Strict form on both paths: exactly what `to_hex` renders, so a
        // digest round-trips byte-identically through its string form.
        if s.len() != 64 {
            return Err(AddressError::InvalidDigest(s.to_string()));
        }
        if !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(AddressError::InvalidDigest(s.to_string()));
        }
        let mut bytes = [0u8; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                .map_err(|_| AddressError::InvalidDigest(s.to_string()))?;
        }
        Ok(Self(bytes))
    }
}

impl TryFrom<String> for ContentDigest {
    type Error = AddressError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<ContentDigest> for String {
    fn from(d: ContentDigest) -> String {
        d.to_hex()
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// How a trust property came to be believed (`design/access-control.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustLevel {
    Claimed,
    Verified,
}

/// A `key:value` trust claim attached to a host or a source's envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Property {
    pub key: String,
    pub value: String,
    pub trust: TrustLevel,
}

/// The metadata record that syncs alongside an address: the only
/// content-derived thing that ever leaves a host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Plugin-level kind of source ("file", "email", ...). Vocabulary belongs
    /// to connections, not the core.
    pub source_type: String,
    pub content_type: Mimetype,
    pub length: ContentLength,
    pub created: Option<Timestamp>,
    pub modified: Option<Timestamp>,
    pub observed: Timestamp,
    pub properties: Vec<Property>,
    /// Title-grade discovery hint the index can use without fetching.
    pub hint: Option<String>,
    /// BLAKE3 over the source's raw bytes, when the steward has one: set from
    /// service metadata at enumeration when the service provides that exact
    /// algorithm, else filled in the first time indexing reads the content.
    /// Best-effort by construction — a source with no stable byte form
    /// carries none and never collapses (`design/addressing.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_digest: Option<ContentDigest>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> Address {
        s.parse().expect("test address parses")
    }

    #[test]
    fn address_roundtrips_through_display() {
        let a = addr("inseam://fs-mba/Users/greg/Data/notes/reno.md");
        assert_eq!(a.host.as_str(), "fs-mba");
        assert_eq!(a.locator.as_str(), "Users/greg/Data/notes/reno.md");
        assert_eq!(addr(&a.to_string()), a);
    }

    #[test]
    fn address_locator_without_slashes_roundtrips() {
        let a = addr("inseam://gmail-greg/msg-18f2ab");
        assert_eq!(a.locator.as_str(), "msg-18f2ab");
        assert_eq!(addr(&a.to_string()), a);
    }

    #[test]
    fn rejects_missing_scheme() {
        assert!(matches!(
            "fs-mba/foo".parse::<Address>(),
            Err(AddressError::Unparseable(_))
        ));
    }

    #[test]
    fn rejects_host_with_separator_characters() {
        assert!(matches!(
            HostId::new("fs/mba"),
            Err(AddressError::InvalidHost(_))
        ));
        assert!(matches!(
            HostId::new("fs:mba"),
            Err(AddressError::InvalidHost(_))
        ));
        assert!(matches!(HostId::new(""), Err(AddressError::EmptyHost)));
    }

    #[test]
    fn digest_roundtrips_through_hex() {
        let d = ContentDigest::of_bytes(b"# hi\n");
        let hex = d.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(hex.parse::<ContentDigest>().expect("parses"), d);
    }

    #[test]
    fn equal_bytes_digest_equal_and_different_bytes_differ() {
        assert_eq!(
            ContentDigest::of_bytes(b"same"),
            ContentDigest::of_bytes(b"same")
        );
        assert_ne!(
            ContentDigest::of_bytes(b"same"),
            ContentDigest::of_bytes(b"other")
        );
    }

    #[test]
    fn rejects_malformed_digests() {
        for bad in ["", "abc", &"A".repeat(64), &"g".repeat(64)] {
            assert!(matches!(
                bad.parse::<ContentDigest>(),
                Err(AddressError::InvalidDigest(_))
            ));
        }
    }

    #[test]
    fn rejects_absolute_locators() {
        assert!(matches!(
            Locator::new("/etc/passwd"),
            Err(AddressError::AbsoluteLocator(_))
        ));
    }
}
