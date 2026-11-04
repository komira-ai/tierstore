//! The counts of the requests in flight and of the connections open, each held by a guard.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::Server;

/// A request in flight, counted until it is dropped.
pub(super) struct Flight<'a>(&'a Server);

impl<'a> Flight<'a> {
    pub(super) fn enter(server: &'a Server) -> Self {
        server.inflight.fetch_add(1, Ordering::SeqCst);
        server.drain.idle.notify_waiters();
        Self(server)
    }
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::SeqCst);
        self.0.drain.idle.notify_waiters();
    }
}

/// A connection that is open, counted until it is dropped.
pub(super) struct Connected(Arc<Server>);

impl Connected {
    pub(super) fn enter(server: &Arc<Server>) -> Self {
        server.open.fetch_add(1, Ordering::SeqCst);
        server.drain.idle.notify_waiters();
        Self(Arc::clone(server))
    }
}

impl Drop for Connected {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::SeqCst);
        self.0.drain.idle.notify_waiters();
    }
}
