//! The wire shape of one exchange over one QUIC bi-stream. The request is a
//! JSON header line naming the protocol, a big-endian `u32` body length, and
//! the body bytes; the response is a status byte (ok or error), a `u32`
//! length, and the bytes — an error's bytes are its UTF-8 message. Bodies
//! travel raw so a routed `fetch_bytes` is never base64'd past the message
//! bound (`design/connections.md`, [`MESSAGE_BYTES_MAX`]). Every length is
//! checked against the bound before a buffer is allocated for it, on both
//! sides, and the header is bounded on its own so a peer cannot stream an
//! endless one.

use iroh::endpoint::{ReadExactError, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use inseam_seams::transport::{InvitationToken, MESSAGE_BYTES_MAX, ProtocolName};

/// The one ALPN every inseam protocol shares; the protocol name inside the
/// header is the routing key.
pub const ALPN: &[u8] = b"inseam/1";
/// The admission handshake, the first stream a dialer opens. Owned by the
/// transport; no handler may register it.
pub const HELLO_PROTOCOL: &str = "inseam/hello/1";
/// Longest request header line, newline included: a protocol name and its
/// JSON wrapping are tens of bytes.
pub const HEADER_BYTES_MAX: usize = 1024;

const STATUS_OK: u8 = 0;
const STATUS_ERROR: u8 = 1;
const LENGTH_BYTES: usize = 4;
const _: () = assert!(
    MESSAGE_BYTES_MAX <= u32::MAX as u64,
    "a body length must fit the u32 the frame carries"
);

#[derive(Debug, Error)]
pub enum WireError {
    #[error("message of {bytes} bytes exceeds the {limit}-byte bound")]
    TooLarge { bytes: u64, limit: u64 },
    #[error("request header exceeds {HEADER_BYTES_MAX} bytes")]
    HeaderTooLong,
    #[error("request header is not `{{\"protocol\": …}}`: {0}")]
    HeaderInvalid(String),
    #[error("response status byte {0} is neither ok nor error")]
    StatusInvalid(u8),
    #[error("stream ended before the message did")]
    Truncated,
    #[error("stream failed: {0}")]
    Stream(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct RequestHeader {
    protocol: ProtocolName,
}

/// The admission handshake's body: the one-time token a joining node
/// presents, or nothing for a node that expects to be in the roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub invitation: Option<InvitationToken>,
}

/// The hello protocol as a name; the literal is a valid name by
/// construction (lowercase, slashes, a digit), so this cannot fail.
pub fn hello_protocol() -> ProtocolName {
    ProtocolName::new(HELLO_PROTOCOL).expect("the hello protocol literal is a valid name")
}

/// A body's length as the frame carries it, refused before anything is
/// allocated or sent when it is over the bound.
pub fn check_body_bound(bytes: usize) -> Result<u32, WireError> {
    let bytes_u64 = u64::try_from(bytes).map_err(|_| WireError::TooLarge {
        bytes: u64::MAX,
        limit: MESSAGE_BYTES_MAX,
    })?;
    if bytes_u64 > MESSAGE_BYTES_MAX {
        return Err(WireError::TooLarge {
            bytes: bytes_u64,
            limit: MESSAGE_BYTES_MAX,
        });
    }
    // The bound is below `u32::MAX` (asserted above), so this cannot fail.
    u32::try_from(bytes_u64).map_err(|_| WireError::TooLarge {
        bytes: bytes_u64,
        limit: MESSAGE_BYTES_MAX,
    })
}

pub async fn write_request(
    send: &mut SendStream,
    protocol: &ProtocolName,
    body: &[u8],
) -> Result<(), WireError> {
    let length = check_body_bound(body.len())?;
    let header = serde_json::to_vec(&RequestHeader {
        protocol: protocol.clone(),
    })
    .map_err(|e| WireError::HeaderInvalid(e.to_string()))?;
    assert!(
        header.len() < HEADER_BYTES_MAX,
        "a bounded name renders to a bounded header"
    );
    assert!(
        !header.contains(&b'\n'),
        "JSON escapes newlines; the line terminator is unique"
    );
    let mut frame = Vec::with_capacity(header.len() + 1 + LENGTH_BYTES);
    frame.extend_from_slice(&header);
    frame.push(b'\n');
    frame.extend_from_slice(&length.to_be_bytes());
    write_all(send, &frame).await?;
    write_all(send, body).await?;
    send.finish()
        .map_err(|e| WireError::Stream(e.to_string()))?;
    Ok(())
}

pub async fn read_request(recv: &mut RecvStream) -> Result<(ProtocolName, Vec<u8>), WireError> {
    let line = read_header_line(recv).await?;
    let header: RequestHeader =
        serde_json::from_slice(&line).map_err(|e| WireError::HeaderInvalid(e.to_string()))?;
    let body = read_body(recv).await?;
    Ok((header.protocol, body))
}

pub async fn write_response(
    send: &mut SendStream,
    outcome: &Result<Vec<u8>, String>,
) -> Result<(), WireError> {
    let (status, bytes): (u8, &[u8]) = match outcome {
        Ok(body) => (STATUS_OK, body.as_slice()),
        Err(message) => (STATUS_ERROR, message.as_bytes()),
    };
    let length = check_body_bound(bytes.len())?;
    let mut frame = Vec::with_capacity(1 + LENGTH_BYTES);
    frame.push(status);
    frame.extend_from_slice(&length.to_be_bytes());
    write_all(send, &frame).await?;
    write_all(send, bytes).await?;
    send.finish()
        .map_err(|e| WireError::Stream(e.to_string()))?;
    Ok(())
}

pub async fn read_response(recv: &mut RecvStream) -> Result<Result<Vec<u8>, String>, WireError> {
    let mut status = [0u8; 1];
    read_exact(recv, &mut status).await?;
    let body = read_body(recv).await?;
    match status[0] {
        STATUS_OK => Ok(Ok(body)),
        STATUS_ERROR => Ok(Err(String::from_utf8_lossy(&body).into_owned())),
        other => Err(WireError::StatusInvalid(other)),
    }
}

/// The header up to its newline, one byte at a time: the header is tens of
/// bytes, and reading past the newline would swallow the length that
/// follows it.
async fn read_header_line(recv: &mut RecvStream) -> Result<Vec<u8>, WireError> {
    let mut line = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    for _ in 0..HEADER_BYTES_MAX {
        read_exact(recv, &mut byte).await?;
        if byte[0] == b'\n' {
            return Ok(line);
        }
        line.push(byte[0]);
    }
    Err(WireError::HeaderTooLong)
}

/// A length-prefixed body, its length checked against the bound before
/// the buffer exists.
async fn read_body(recv: &mut RecvStream) -> Result<Vec<u8>, WireError> {
    let mut length = [0u8; LENGTH_BYTES];
    read_exact(recv, &mut length).await?;
    let length = u32::from_be_bytes(length);
    let bytes = u64::from(length);
    if bytes > MESSAGE_BYTES_MAX {
        return Err(WireError::TooLarge {
            bytes,
            limit: MESSAGE_BYTES_MAX,
        });
    }
    let size = usize::try_from(length).map_err(|_| WireError::TooLarge {
        bytes,
        limit: MESSAGE_BYTES_MAX,
    })?;
    let mut body = vec![0u8; size];
    read_exact(recv, &mut body).await?;
    assert_eq!(body.len(), size);
    Ok(body)
}

async fn read_exact(recv: &mut RecvStream, buf: &mut [u8]) -> Result<(), WireError> {
    recv.read_exact(buf).await.map_err(|e| match e {
        ReadExactError::FinishedEarly(_) => WireError::Truncated,
        ReadExactError::ReadError(e) => WireError::Stream(e.to_string()),
    })
}

async fn write_all(send: &mut SendStream, bytes: &[u8]) -> Result<(), WireError> {
    send.write_all(bytes)
        .await
        .map_err(|e| WireError::Stream(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_bound_is_enforced_before_any_allocation() {
        assert_eq!(check_body_bound(0).expect("zero"), 0);
        let limit = usize::try_from(MESSAGE_BYTES_MAX).expect("fits");
        assert_eq!(
            u64::from(check_body_bound(limit).expect("at the bound")),
            MESSAGE_BYTES_MAX
        );
        assert!(matches!(
            check_body_bound(limit + 1),
            Err(WireError::TooLarge { bytes, limit: l }) if bytes == MESSAGE_BYTES_MAX + 1 && l == MESSAGE_BYTES_MAX
        ));
    }

    #[test]
    fn the_hello_body_is_plain_json() {
        let none = serde_json::to_string(&Hello { invitation: None }).expect("json");
        assert_eq!(none, r#"{"invitation":null}"#);
        let some = serde_json::to_string(&Hello {
            invitation: Some(InvitationToken::new("tok-1").expect("valid")),
        })
        .expect("json");
        assert_eq!(some, r#"{"invitation":"tok-1"}"#);
        let back: Hello = serde_json::from_str(&some).expect("parses");
        assert_eq!(back.invitation.expect("token").secret(), "tok-1");
        assert!(serde_json::from_str::<Hello>(r#"{"invitation":"has space"}"#).is_err());
    }

    #[test]
    fn the_hello_protocol_is_a_valid_name() {
        assert_eq!(hello_protocol().as_str(), HELLO_PROTOCOL);
    }
}
