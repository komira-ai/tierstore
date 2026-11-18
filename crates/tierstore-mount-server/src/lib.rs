//! The server side of the mount-to-tierstore protocol (`docs/design/fetch-cache-serve.md`, 5.1
//! and 5.4; section 18, row T5). Linux only: the protocol passes descriptors with `SCM_RIGHTS`.
//!
//! A mount (the client, T6) keeps one long-lived connection to the server over a unix stream
//! socket and multiplexes its requests on it by request id; the frames are those of
//! [`tierstore_fetch_api::wire`]. What this crate does with them:
//!
//! - **A task per request.** `open-pack`, `wait` and `ensure` each run in their own task, so a
//!   `wait` parked on a fill never holds up another request, and replies leave in the order they
//!   complete. A connection runs at most [`Config::max_in_flight`] of them at once and stops
//!   reading while it is at that bound. The handshake and `release` change what the connection is
//!   bound to, so they run in order, before the next frame is read.
//! - **Epoch.** The server draws a random non-zero `u64` at start ([`random_epoch`]) and stamps
//!   every reply with it. A request other than the handshake must carry it: a `wait` or `ensure`
//!   stamped with another epoch names a handle of a previous server and is refused
//!   `UnknownHandle`; an `open-pack` or `release` is refused `NotHandshaken`. Handles are numbered
//!   per server, so a handle of a previous server is never mistaken for one of this server's.
//! - **Descriptors.** The handshake brings the mount's counter memfd and its end of the lifeline;
//!   `open-pack` sends back the pack's data fd and sealed state memfd, both duplicated from the
//!   entry ([`tierstore_fetch_api::entry::Entry::fds`]). Descriptors arrive with the bytes of
//!   their frame: the server reads one frame at a time, so every descriptor received while it
//!   reads a frame's bytes is that frame's, and a frame with another number of them than its kind
//!   carries ends the connection.
//! - **Counter memfd.** The server refuses a counter memfd that is not sealed against shrinking
//!   ([`COUNTER_MEMFD_SEALS`]), so a mount can never truncate pages out from under the server's
//!   read-only mapping. It hands the memfd to a [`CounterMapper`] (the Linux entry store, T1, maps
//!   it) and registers the holder with the entry store. `open-pack` attaches the holder's region
//!   with [`tierstore_fetch_api::entry::EntryStore::attach`], which remaps the memfd before it
//!   returns, so the reply is sent only after the server's mapping covers the region.
//! - **Lifeline.** Only the lifeline's `POLLHUP` ends a holder that did not `release`: the server
//!   then drops the holder's counters, regions, handles and holds
//!   ([`tierstore_fetch_api::entry::EntryStore::drop_holder`]). A connection that closes ends
//!   nothing but its own requests: a mount that reconnects with the same holder id keeps its
//!   counters, regions, holds and handles, and a re-sent `open-pack` returns the same handle and
//!   region.
//!
//! Section 14 rows: "Mount killed (lifeline hangup): counters dropped, eviction proceeds; mount
//! whose protocol connection flaps keeps counters, regions and holds and is never zeroed while
//! its lifeline is open"; "A mount grows its counter memfd past the server's mapping: the server
//! remaps before acknowledging `open-pack`"; "More than 128 holders on one pack are all served";
//! "Eviction removes an unheld entry's path while a mount holds the data fd: reads return that
//! entry's bytes"; "Server restart with a stale-epoch reply in flight: dropped; the `wait`
//! re-issued" (the server's half: every reply carries its epoch, and an old epoch's handle is
//! refused).
//!
//! The blocking calls of the entry store and of the [`CounterMapper`] run on tokio's blocking
//! threads, never on an async worker.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

mod epoch;
mod framing;
mod lifeline;
mod registry;
mod server;
mod stats;
mod trees;

pub use epoch::random_epoch;
pub use registry::{COUNTER_MEMFD_SEALS, CounterMapper};
pub use server::{Config, MountServer, Parts};
pub use stats::Stats;
pub use trees::{TreeError, Trees};

/// Why the server could not start or stopped serving.
#[derive(Debug, thiserror::Error)]
pub enum MountServerError {
    /// The system's random source failed while drawing the epoch.
    #[error("drawing the server epoch: {0}")]
    Random(#[source] std::io::Error),
    /// Accepting a connection on the listening socket failed.
    #[error("accepting a mount connection: {0}")]
    Accept(#[source] std::io::Error),
}
