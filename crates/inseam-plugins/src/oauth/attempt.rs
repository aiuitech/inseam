//! One authorization attempt between "the owner is sent to the provider"
//! and "the tokens are on file": the `state` that names it, the PKCE
//! verifier the exchange needs back, and its outcome, which transports wait
//! on. Two ways for the browser to come back share this record
//! (`design/connections.md`): the loopback listener this file runs for
//! local transports, and the redirect a remote transport serves itself and
//! delivers through the service.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use inseam_kernel::address::Timestamp;
use inseam_seams::SeamError;
use inseam_seams::oauth::{AuthorizationCallback, GrantId};

use super::flow::{REQUEST_HEAD_BYTES_MAX, http_response, parse_callback};
use super::grant::GrantHandle;

/// Authorizations one node keeps in flight at once: an owner signing in
/// from two clients, never a queue.
pub const ATTEMPTS_MAX: usize = 8;

/// Loopback connections one authorization attempt will look at before
/// giving up: browsers fetch favicons and prefetch, so the callback is
/// rarely the first connection, but it is never the fortieth.
pub const CALLBACK_CONNECTIONS_MAX: u32 = 32;

/// How an attempt ended. `Clone` so every waiter gets its own copy;
/// rebuilt into a [`SeamError`] at the seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Authorized,
    Refused(String),
    Failed(String),
}

impl Outcome {
    pub fn into_result(self, grant: &GrantId) -> Result<GrantId, SeamError> {
        match self {
            Self::Authorized => Ok(grant.clone()),
            Self::Refused(reason) => Err(SeamError::Refused(reason)),
            Self::Failed(reason) => Err(SeamError::Failed(reason)),
        }
    }

    fn from_error(error: SeamError) -> Self {
        match error {
            SeamError::Refused(reason) => Self::Refused(reason),
            other => Self::Failed(other.to_string()),
        }
    }
}

pub struct Attempt {
    pub state: String,
    pub grant: GrantHandle,
    pub redirect_uri: String,
    verifier: String,
    /// Epoch seconds after which the attempt is dead, by the provider's
    /// clock; pruned by the service.
    pub deadline: Timestamp,
    outcome: watch::Sender<Option<Outcome>>,
}

impl Attempt {
    pub fn new(
        state: String,
        grant: GrantHandle,
        redirect_uri: String,
        verifier: String,
        deadline: Timestamp,
    ) -> Self {
        let (outcome, _) = watch::channel(None);
        Self {
            state,
            grant,
            redirect_uri,
            verifier,
            deadline,
            outcome,
        }
    }

    pub fn is_expired(&self, now: Timestamp) -> bool {
        now > self.deadline
    }

    /// The outcome so far, if the attempt has ended.
    pub fn outcome(&self) -> Option<Outcome> {
        self.outcome.borrow().clone()
    }

    /// A waiter's view; see [`wait`].
    pub fn subscribe(&self) -> watch::Receiver<Option<Outcome>> {
        self.outcome.subscribe()
    }

    /// Check the browser's return against this attempt (RFC 6749 §4.1.2):
    /// the state must match, a provider error is a refusal, and a code must
    /// be present. The returned code is ready to exchange.
    pub fn accept(&self, callback: &AuthorizationCallback) -> Result<String, SeamError> {
        if callback.state.as_deref() != Some(self.state.as_str()) {
            return Err(SeamError::Refused(
                "authorization redirect carried the wrong state; possible CSRF, attempt abandoned"
                    .to_string(),
            ));
        }
        if let Some(error) = &callback.error {
            let description = callback.error_description.clone().unwrap_or_default();
            return Err(SeamError::Refused(format!(
                "provider answered `{error}`: {description}"
            )));
        }
        match &callback.code {
            Some(code) if !code.is_empty() => Ok(code.clone()),
            _ => Err(SeamError::failed("authorization redirect carried no code")),
        }
    }

    /// Exchange the code and record how it went; an attempt finishes once.
    pub async fn finish(&self, code: &str) -> Outcome {
        let outcome = match self
            .grant
            .finish(code, &self.redirect_uri, &self.verifier)
            .await
        {
            Ok(()) => Outcome::Authorized,
            Err(e) => Outcome::from_error(e),
        };
        self.resolve(outcome.clone());
        outcome
    }

    /// Record an outcome reached without an exchange (a refused redirect, a
    /// timeout).
    pub fn resolve(&self, outcome: Outcome) {
        self.outcome.send_if_modified(|slot| {
            if slot.is_some() {
                return false;
            }
            *slot = Some(outcome);
            true
        });
    }
}

/// Wait for an attempt's outcome, bounded. A timeout is reported as the
/// failure it is; the attempt itself stays pending until its deadline.
pub async fn wait(
    mut outcome: watch::Receiver<Option<Outcome>>,
    timeout: Duration,
) -> Result<Outcome, SeamError> {
    let settled = async {
        loop {
            if let Some(outcome) = outcome.borrow_and_update().clone() {
                return Ok(outcome);
            }
            if outcome.changed().await.is_err() {
                return Err(SeamError::failed("the authorization attempt was dropped"));
            }
        }
    };
    tokio::time::timeout(timeout, settled).await.map_err(|_| {
        SeamError::failed(format!(
            "no authorization redirect arrived within {} seconds",
            timeout.as_secs()
        ))
    })?
}

/// Run the loopback side of an attempt to its end: accept the redirect,
/// exchange the code, and tell the tab what happened — the owner is looking
/// at it, not at a terminal.
pub async fn serve_loopback(attempt: Arc<Attempt>, listener: TcpListener, timeout: Duration) {
    let arrived = tokio::time::timeout(timeout, accept_callback(&listener, &attempt)).await;
    let (code, mut stream) = match arrived {
        Ok(Ok(arrived)) => arrived,
        Ok(Err(e)) => {
            attempt.resolve(Outcome::from_error(e));
            return;
        }
        Err(_) => {
            attempt.resolve(Outcome::Failed(format!(
                "no authorization redirect arrived within {} seconds",
                timeout.as_secs()
            )));
            return;
        }
    };
    let outcome = attempt.finish(&code).await;
    let page = match &outcome {
        Outcome::Authorized => http_response(
            200,
            "OK",
            "Authorized",
            "inseam received the grant. You can close this tab.",
        ),
        Outcome::Refused(e) | Outcome::Failed(e) => {
            http_response(502, "Bad Gateway", "Authorization failed", e)
        }
    };
    let _ = stream.write_all(&page).await;
    let _ = stream.shutdown().await;
}

/// Wait for the browser to land on `/callback` with our `state`. Other
/// requests on the port (favicons, stray tabs) are answered 404 and
/// skipped, up to [`CALLBACK_CONNECTIONS_MAX`] of them.
async fn accept_callback(
    listener: &TcpListener,
    attempt: &Attempt,
) -> Result<(String, TcpStream), SeamError> {
    let mut connections: u32 = 0;
    while connections < CALLBACK_CONNECTIONS_MAX {
        connections += 1;
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| SeamError::failed(format!("loopback accept failed: {e}")))?;
        let head = read_request_head(&mut stream).await?;
        let callback = match parse_callback(&head) {
            Ok(callback) => callback,
            Err(_) => {
                answer(&mut stream, 404, "Not Found", "Not found", "").await;
                continue;
            }
        };
        if callback.path != "/callback" {
            answer(&mut stream, 404, "Not Found", "Not found", "").await;
            continue;
        }
        let callback = AuthorizationCallback {
            state: callback.state,
            code: callback.code,
            error: callback.error,
            error_description: callback.error_description,
        };
        return match attempt.accept(&callback) {
            Ok(code) => Ok((code, stream)),
            Err(e) => {
                answer(&mut stream, 400, "Bad Request", "Rejected", &e.to_string()).await;
                Err(e)
            }
        };
    }
    Err(SeamError::failed(format!(
        "{CALLBACK_CONNECTIONS_MAX} loopback connections arrived and none was the authorization redirect"
    )))
}

/// Read until the end of the request head or the size cap.
async fn read_request_head(stream: &mut TcpStream) -> Result<String, SeamError> {
    let mut buffer: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while buffer.len() < REQUEST_HEAD_BYTES_MAX {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|e| SeamError::failed(format!("loopback read failed: {e}")))?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

/// Best-effort answer to a loopback tab; a browser that hung up is not an
/// error in the flow.
async fn answer(stream: &mut TcpStream, status: u16, reason: &str, title: &str, body: &str) {
    let _ = stream
        .write_all(&http_response(status, reason, title, body))
        .await;
    let _ = stream.shutdown().await;
}
