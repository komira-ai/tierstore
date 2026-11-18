//! Why the client could not start, and why a request of a reader ended without an outcome.

use std::io;

use tierstore_fetch_api::ids::PackId;
use tierstore_fetch_api::page::CounterRegion;
use tierstore_fetch_api::wire::RefusalCode;

use crate::view::BadPackFds;

/// Why [`crate::PackClient::start`] failed.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The counter memfd could not be created or sealed.
    #[error("creating the counter memfd: {0}")]
    Counters(#[source] io::Error),
    /// The lifeline socketpair could not be created.
    #[error("creating the lifeline: {0}")]
    Lifeline(#[source] io::Error),
    /// The system's random source failed while seeding the backoff's jitter.
    #[error("seeding the reconnect jitter: {0}")]
    Random(#[source] io::Error),
    /// The client's runtime or its thread could not be started.
    #[error("starting the client's runtime: {0}")]
    Runtime(#[source] io::Error),
}

/// Why a request of a reader (an open, a wait, a release) ended without an outcome. Cloned to
/// every reader a coalesced request answers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// The client's runtime has stopped: every [`crate::PackClient`] and [`crate::Pack`] was
    /// dropped, or its thread ended.
    #[error("the mount client has stopped")]
    Stopped,
    /// The holder was released (a clean unmount): no request is served any more.
    #[error("the holder was released")]
    Released,
    /// The server refused the handshake: the client serves no request any more.
    #[error("the server refused the handshake: {0:?}")]
    HandshakeRefused(RefusalCode),
    /// The server refused `release`.
    #[error("the server refused release: {0:?}")]
    ReleaseRefused(RefusalCode),
    /// The server refused `open-pack` (an unknown tree or pack, a list that is not L2, a pack of
    /// another cell size, a region it would not attach): the reader's miss path.
    #[error("the server refused open-pack of {pack:?}: {code:?}")]
    OpenRefused {
        /// The pack.
        pack: PackId,
        /// Why.
        code: RefusalCode,
    },
    /// The server opened the pack with another size than the tree's list gives it.
    #[error("{pack:?}: open-pack answered {got} bytes, the list says {expected}")]
    SizeMismatch {
        /// The pack.
        pack: PackId,
        /// The list's size.
        expected: u64,
        /// The server's.
        got: u64,
    },
    /// The server reads the pack's counters at another region than the one the mount allocated.
    #[error("{pack:?}: open-pack answered region {got:?}, the mount allocated {asked:?}")]
    RegionMismatch {
        /// The pack.
        pack: PackId,
        /// The mount's region.
        asked: CounterRegion,
        /// The server's.
        got: CounterRegion,
    },
    /// The descriptors of `open-pack` are not a state page the mount can map read-only.
    #[error("{pack:?}: {source}")]
    BadFds {
        /// The pack.
        pack: PackId,
        /// Why.
        #[source]
        source: BadPackFds,
    },
    /// The pack has more cells than a state page counts.
    #[error("{pack:?}: a pack of {size} bytes is too large")]
    TooLarge {
        /// The pack.
        pack: PackId,
        /// Its size.
        size: u64,
    },
    /// The pack's region of the counter memfd could not be allocated.
    #[error("{pack:?}: allocating the counter region: {reason}")]
    Counters {
        /// The pack.
        pack: PackId,
        /// Why.
        reason: String,
    },
    /// The request cannot be encoded as a frame (a tree reference longer than the frame allows).
    #[error("{pack:?}: the request cannot be sent: {reason}")]
    Encode {
        /// The pack.
        pack: PackId,
        /// Why.
        reason: String,
    },
}
