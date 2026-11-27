//! The requests the service holds for a test: the next AbortMultipartUpload, which it drops
//! unhandled when its caller goes away ([`FakeS3::hold_next_abort`](super::FakeS3::hold_next_abort)),
//! and every HEAD while a test holds them ([`FakeS3::hold_heads`]); and what a test asks of the
//! requests the service has begun and of an object's metadata.

use std::sync::{Mutex, PoisonError};

use tokio::io::AsyncRead;
use tokio::sync::Notify;

use super::{FakeS3, Request, State};
use crate::ObjectKey;

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
