//! The body of a proxy's registration, `_tiering/proxies/<proxy id>`.
//!
//! A registration was an empty object, whose name told the reconciler that the proxy has a
//! journal. It is now a small JSON object that the proxy rewrites every heartbeat, so that the
//! reconciler, which compares nothing across machines, sees the object change while its owner is
//! alive. Unknown fields are ignored, and a body that is empty or is not this object (a
//! registration of a release before this one) reads as none, so both directions of a rolling
//! update work: an older reconciler lists the names and never reads the body.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::journal::to_body;

/// The `proto` of the registrations this release writes.
pub(crate) const PROTO: u32 = 1;

/// What a registration says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Registration {
    /// The version of this body.
    pub(crate) proto: u32,
    /// Random for each start of the process: two processes of one proxy id differ in it.
    pub(crate) boot: String,
    /// The number of heartbeats written since the start, so every body differs from the one
    /// before. Zero is the registration of a process that does not heartbeat (a library user that
    /// never started the heartbeat): it never changes, and the reconciler must not take it for a
    /// process that has left.
    pub(crate) hb: u64,
}

impl Registration {
    /// The bytes to store.
    pub(crate) fn encode(&self) -> Bytes {
        to_body(self)
    }

    /// The registration `body` holds, if it holds one.
    pub(crate) fn decode(body: &[u8]) -> Option<Self> {
        serde_json::from_slice(body).ok()
    }

    /// Whether the owner heartbeats, and so is gone when the object stops changing.
    pub(crate) fn beats(&self) -> bool {
        self.hb > 0
    }
}
