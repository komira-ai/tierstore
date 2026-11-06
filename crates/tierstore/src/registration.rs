//! The body of a proxy's registration, `_tiering/proxies/<proxy id>`.
//!
//! A registration was an empty object, whose name told the reconciler that the proxy has a
//! journal. It is now a small JSON object that the proxy rewrites every heartbeat, so that the
//! reconciler, which compares nothing across machines, sees the object change while its owner is
//! alive. Unknown fields are ignored, and a body that is empty or is not this object (a
//! registration of a release before this one) reads as none, so both directions of a rolling
//! update work: an older reconciler lists the names and never reads the body.
//!
//! `peers` is how a restart is told from two processes that run side by side under one proxy id (a
//! hand-off, a new process that takes the sockets of an old one that drains): both write this
//! object, each over the other, so it names one `boot` at a time. A process names the boots of the
//! others it has reason to think alive in `peers`, so that every body names all of them (see
//! `tiered/peers.rs`), and the reconciler reaps the entry of an owner only when the boot that made
//! it is named by neither `boot` nor `peers`. A body without `peers` (written by a release before
//! it) names only its `boot` and gives no ground to reap by.

use std::collections::BTreeSet;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::journal::to_body;

/// The `proto` of the registrations this release writes: 2, the protocol of one journal entry for
/// each key (the registrations of the release before, which heartbeats but writes journals, say 1;
/// `tierstore format-epoch --activate` refuses while one of them is live).
pub(crate) const PROTO: u32 = 2;

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
    /// The boots of the other processes of this proxy id that may be alive, as the writer knows
    /// them; empty when it knows none. Absent from a body written by a release before this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) peers: Option<Vec<String>>,
}

impl Registration {
    /// The bytes to store.
    pub(crate) fn encode(&self) -> Bytes {
        to_body(self)
    }

    /// The boots this registration names as alive (its own and its peers'), or `None` for a body
    /// that does not say who its peers are.
    pub(crate) fn named(&self) -> Option<BTreeSet<String>> {
        let peers = self.peers.as_ref()?;
        Some(peers.iter().chain([&self.boot]).cloned().collect())
    }

    /// The registration `body` holds, if it holds one.
    pub(crate) fn decode(body: &[u8]) -> Option<Self> {
        serde_json::from_slice(body).ok()
    }
}
