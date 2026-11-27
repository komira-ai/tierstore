//! The requests the service holds for a test: the next AbortMultipartUpload, which it drops
//! unhandled when its caller goes away ([`FakeS3::hold_next_abort`](super::FakeS3::hold_next_abort)),
//! and every HEAD while a test holds them ([`FakeS3::hold_heads`](super::FakeS3::hold_heads)).

use std::sync::{Mutex, PoisonError};

use tokio::io::AsyncRead;
use tokio::sync::Notify;

use super::{Request, State};

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

/// Waits until HEADs are not held.
pub(super) async fn heads_let_go(state: &Mutex<State>, changed: &Notify) {
    loop {
        // Registered before the look, so a release between the two is not lost.
        let mut released = std::pin::pin!(changed.notified());
        released.as_mut().enable();
        if !state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .hold_heads
        {
            return;
        }
        released.await;
    }
}
