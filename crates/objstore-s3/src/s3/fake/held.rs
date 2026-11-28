//! The requests the service holds for a test: the next AbortMultipartUpload, which it drops
//! unhandled when its caller goes away ([`FakeS3::hold_next_abort`](super::FakeS3::hold_next_abort)),
//! every HEAD while a test holds them ([`FakeS3::hold_heads`]), and every request under a key
//! prefix while a test stalls it ([`FakeS3::serve_as`] with [`Serving::Stalled`]); the requests it
//! answers with an error instead ([`Serving::Failing`]); and what a test asks of the requests the
//! service has begun and of an object's metadata.

use std::sync::{Mutex, PoisonError};

use tokio::io::AsyncRead;
use tokio::sync::Notify;

use super::{FakeS3, Request, State, error, tls};
use crate::ObjectKey;

/// What the service does with a request, by the key it names ([`FakeS3::serve_as`]): the path
/// after the bucket, so a request of the bucket itself (a listing) names the empty key, which only
/// the empty prefix matches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Serving {
    /// Every request is served.
    #[default]
    All,
    /// A request whose key starts with `prefix` is read and then held, unanswered, until the
    /// service serves otherwise: an upstream that stalled. Its connection stays open; a client
    /// that gives up and closes it finds the request answered to no one when it is let go.
    Stalled {
        /// The start of the keys held; the empty prefix holds every request.
        prefix: String,
    },
    /// A request whose key starts with `prefix` is answered with the HTTP status `status` and an
    /// S3 error body, and changes nothing the service holds: an upstream that answers only
    /// errors. 503 is `503 Slow Down` with the code `SlowDown`, 500 is `500 Internal Server Error`
    /// with `InternalError`, and any other status is `<status> Error` with `InternalError`.
    Failing {
        /// The start of the keys failed; the empty prefix fails every request.
        prefix: String,
        /// The status of the answer, such as 503 (`SlowDown`) or 500 (`InternalError`).
        status: u16,
    },
}

/// The service's [`Serving`], and how many requests a stall has held so far.
#[derive(Debug, Default)]
pub(super) struct Modes {
    now: Serving,
    stalled: u64,
}

/// Waits until `request` is let go: a HEAD that is not held ([`heads_let_go`]), then a request
/// that no stall holds. A request is counted once among the stalled however often it waits.
pub(super) async fn let_go(state: &Mutex<State>, changed: &Notify, request: &Request) {
    if request.method == "HEAD" {
        heads_let_go(state, changed).await;
    }
    let mut counted = false;
    loop {
        // Registered before the look, so a change between the two is not lost.
        let mut released = std::pin::pin!(changed.notified());
        released.as_mut().enable();
        {
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            let key = tls::key_of(&state, request);
            let held = match &state.serving.now {
                Serving::Stalled { prefix } => key.starts_with(prefix.as_str()),
                Serving::All | Serving::Failing { .. } => false,
            };
            if !held {
                return;
            }
            if !counted {
                state.serving.stalled += 1;
            }
        }
        // A test waiting for the count ([`FakeS3::wait_for`]) learns of it; this request's own
        // registration wakes too, once, and finds itself counted.
        if !counted {
            counted = true;
            changed.notify_waiters();
        }
        released.await;
    }
}

/// The error answer of `request` while the service fails its key ([`Serving::Failing`]); none
/// when the request is to be handled.
pub(super) fn failed(state: &State, request: &Request) -> Option<Vec<u8>> {
    let Serving::Failing { prefix, status } = &state.serving.now else {
        return None;
    };
    if !tls::key_of(state, request).starts_with(prefix.as_str()) {
        return None;
    }
    let (line, code) = match status {
        503 => ("503 Slow Down", "SlowDown"),
        500 => ("500 Internal Server Error", "InternalError"),
        other => return Some(error(&format!("{other} Error"), "InternalError")),
    };
    Some(error(line, code))
}

/// Whether `request` is the AbortMultipartUpload to hold: if it is, it waits until its caller
/// closes the connection and is then dropped, never having reached the store.
pub(super) async fn abort_dropped<S: AsyncRead + Unpin>(
    sock: &mut S,
    state: &Mutex<State>,
    changed: &Notify,
    request: &Request,
) -> bool {
    let held = {
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        let abort = request.method == "DELETE" && request.query.contains_key("uploadId");
        let held = abort && std::mem::take(&mut state.hold_abort);
        state.held_aborts += usize::from(held);
        held
    };
    if !held {
        return false;
    }
    changed.notify_waiters();
    // Until the caller closes the connection; then the request is lost.
    let _ = tokio::io::copy(sock, &mut tokio::io::sink()).await;
    let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
    state.held_aborts -= 1;
    drop(state);
    changed.notify_waiters();
    true
}

/// Waits until this HEAD is not held: it is one of those let through, or the HEADs are let go.
pub(super) async fn heads_let_go(state: &Mutex<State>, changed: &Notify) {
    loop {
        // Registered before the look, so a release between the two is not lost.
        let mut released = std::pin::pin!(changed.notified());
        released.as_mut().enable();
        {
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            match state.hold_heads {
                None => return,
                Some(0) => {}
                Some(passing) => {
                    state.hold_heads = Some(passing - 1);
                    return;
                }
            }
        }
        released.await;
    }
}

/// The calls of a test on what the service holds and has begun.
impl FakeS3 {
    /// How many requests so far the service has read the head of, whatever came of their body:
    /// for a test that must fail a body only once its request has reached the service.
    #[must_use]
    pub fn requests_begun(&self) -> u64 {
        self.lock().begun
    }

    /// Holds every HEAD from now on but the next `after`, read and not yet handled, until
    /// [`FakeS3::release_heads`]: for a test that must have the service take a request (a PUT cut
    /// short) before it answers a HEAD that the client sends after it, on another connection.
    pub fn hold_heads(&self, after: u32) {
        self.lock().hold_heads = Some(after);
    }

    /// Lets the HEADs held by [`FakeS3::hold_heads`] be handled, and every one after them.
    pub fn release_heads(&self) {
        self.lock().hold_heads = None;
        self.changed.notify_waiters();
    }

    /// From now on serves every request as `serving` says: a request a stall holds is let go
    /// when the service serves otherwise, and is then handled as the new mode says.
    pub fn serve_as(&self, serving: Serving) {
        self.lock().serving.now = serving;
        self.changed.notify_waiters();
    }

    /// What the service does with requests ([`FakeS3::serve_as`]).
    #[must_use]
    pub fn serving(&self) -> Serving {
        self.lock().serving.now.clone()
    }

    /// How many requests a stall has held so far ([`Serving::Stalled`]), each counted once.
    #[must_use]
    pub fn stalled_requests(&self) -> u64 {
        self.lock().serving.stalled
    }

    /// The metadata `name` (an `x-amz-meta-*` header, in lower case) of the object under `key`,
    /// if there is an object and it keeps one.
    #[must_use]
    pub fn object_meta(&self, key: &ObjectKey, name: &str) -> Option<String> {
        let state = self.lock();
        let meta = &state.objects.get(key.as_str())?.sums.meta;
        let found = meta.iter().find(|(held, _)| held == name);
        found.map(|(_, value)| value.clone())
    }
}
