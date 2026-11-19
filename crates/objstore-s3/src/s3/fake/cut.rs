//! A request whose body ended before its `Content-Length`: the connection closed, and by default
//! the request reaches nothing, as on a store that takes a request only whole. Each is counted
//! ([`FakeS3::cut_requests`](super::FakeS3::cut_requests)), taken or not.
//!
//! Once a test asks ([`FakeS3::keep_cut_bodies`](super::FakeS3::keep_cut_bodies)), a request cut
//! short that is not of a multipart upload (not a part, not a completion: a PUT of a whole object)
//! is taken as SeaweedFS 4.54 takes one without `Content-Type`: as a PUT
//! of the bytes that came. Its conditions and its checksum decide as for any PUT (a body that is
//! not its `x-amz-checksum-sha256` is refused and nothing is kept), and without a checksum the
//! bytes are kept as the object, 0 of them for a body cut before its first byte. No answer is
//! sent: the client is gone.

use std::sync::{Mutex, PoisonError};

use tokio::sync::Notify;

use super::{Request, State, handle};

/// Counts `request`, whose body is the bytes that came, and takes it if the service keeps cut
/// bodies and it is not of a multipart upload (module docs); ends the connection either way.
pub(super) fn taken(state: &Mutex<State>, changed: &Notify, request: &Request) -> Option<()> {
    let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
    state.cut += 1;
    if state.keep_cut && !request.query.contains_key("uploadId") {
        drop(handle(&mut state, request));
    }
    drop(state);
    changed.notify_waiters();
    None
}
