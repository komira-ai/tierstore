//! The body of a proxy's registration, `_tiering/proxies/<proxy id>`.
//!
//! A registration was an empty object, whose name told the reconciler that the proxy has a
//! journal. It is now a small JSON object that the proxy rewrites every heartbeat, so that the
//! reconciler, which compares nothing across machines, sees the object change while its owner is
//! alive. Unknown fields are ignored, and a body that is empty or is not this object (a
//! registration of a release before this one) reads as none, so both directions of a rolling
//! update work: an older reconciler lists the names and never reads the body. The two optional
//! fields `phase` and `writes_in_flight` are left out of the body when they are not set, so that a
//! registration that does not set them is the bytes it always was.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::format::FormatPhase;
use crate::journal::to_body;

/// The `proto` of the registrations of a proxy with no marks bucket: 2, the protocol of one journal entry for
/// each key (the registrations of the release before, which heartbeats but writes journals, say 1;
/// `tierstore format-epoch --activate` refuses while one of them is live).
pub(crate) const PROTO: u32 = 2;

/// The `proto` of a proxy that has a marks bucket, so reads the marker of a key on a hot miss in
/// every phase from which a marker can be in force (`docs/design/put-without-lock.md`, 5.7 and
/// 8.1): `tierstore format-epoch --activate-marks` takes 3 to mean that. A proxy with no marks
/// bucket (a library user's, and a server over a `dir` hot tier) says [`PROTO`], because it reads
/// no marker.
pub(crate) const PROTO_MARKS: u32 = 3;

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
    /// The format phase the proxy acts on, as it last read the format object: what the operator's
    /// rollback waits for (`drained`) before it empties the marks bucket. Absent in the
    /// registration of a release that does not write it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) phase: Option<FormatPhase>,
    /// The number of writes the proxy has begun and not ended, so that the rollback can tell that
    /// none is in flight. Absent in the registration of a release that does not write it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) writes_in_flight: Option<u64>,
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
