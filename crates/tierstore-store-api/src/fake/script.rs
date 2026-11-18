//! The script of a fake storage node: which route a step is for and what it does.

use crate::peer::PeerError;

/// A route of the peer service a test can script.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Route {
    /// `POST /store/v1/locate`.
    Locate,
    /// A hot, cold or degraded GET.
    Get,
    /// `PUT /store/v1/hot/replica`.
    Replicate,
    /// `POST /store/v1/seal`.
    Seal,
    /// `POST /store/v1/key`.
    KeyCheck,
    /// `POST /store/v1/descriptor`.
    Commit,
    /// `GET /store/v1/tomb`.
    TombPull,
    /// `GET /store/v1/ping`.
    Ping,
}

/// What a scripted call does after its service time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Answer as the node's state says.
    Answer,
    /// Never resolve (no clock movement releases it).
    Hang,
    /// Fail with this error instead of answering.
    Fail(PeerError),
    /// The node crashes: this call and every later one is `Unreachable` until `restart`.
    Crash,
}

/// One scripted call: its service time on the injected clock and what it does after.
///
/// A `Hang` and a `Crash` act at once, without waiting for the service time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Milliseconds on the injected clock before the answer or the failure.
    pub service_ms: u64,
    /// What happens then.
    pub outcome: Outcome,
}

impl Step {
    /// An answer after `service_ms`.
    #[must_use]
    pub fn answer(service_ms: u64) -> Self {
        Self {
            service_ms,
            outcome: Outcome::Answer,
        }
    }

    /// A call that never resolves.
    #[must_use]
    pub fn hang() -> Self {
        Self {
            service_ms: 0,
            outcome: Outcome::Hang,
        }
    }

    /// A failure with `error` after `service_ms`.
    #[must_use]
    pub fn fail(service_ms: u64, error: PeerError) -> Self {
        Self {
            service_ms,
            outcome: Outcome::Fail(error),
        }
    }

    /// A crash of the node.
    #[must_use]
    pub fn crash() -> Self {
        Self {
            service_ms: 0,
            outcome: Outcome::Crash,
        }
    }
}
