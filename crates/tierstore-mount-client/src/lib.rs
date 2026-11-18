//! The mount's side of the mount-to-tierstore protocol (`docs/design/fetch-cache-serve.md`,
//! revision 7, 5.1 and 5.4; section 18, row T6), Linux only: the protocol passes descriptors with
//! `SCM_RIGHTS` and maps memfds. The server side is `tierstore-mount-server` (T5); the frames are
//! those of [`tierstore_fetch_api::wire`].
//!
//! [`PackClient`] is what a mount process (the tree `Fs` on `tsfuse`, T-F8) uses to read packs
//! from the node's cache:
//!
//! - **A runtime of its own.** The client runs on a current-thread runtime on a thread of its own;
//!   the readers run on their own threads and reach it by channels. A reader blocked in the kernel
//!   never stalls the `wait` completions other readers wait for. A request returns a [`Ticket`] at
//!   once.
//! - **The table of open packs** ([`PackClient::open`]): one [`Pack`] per pack id for the mount's
//!   life, with the data fd and the state memfd `open-pack` passed ([`PackView`]); a reconnect
//!   re-opens it and swaps the view.
//! - **State reads** ([`Pack::begin_read`]): the reader protocol of 5.1 on the state page, mapped
//!   `PROT_READ` from a memfd the mount refuses unless it is sealed for mounts: the counters of
//!   the covered groups are incremented, every covered cell is loaded `SeqCst`, the post-read
//!   recheck reloads them, and the counters are given back after the reply.
//! - **Counter regions** ([`PackCounters`]): one counter memfd for the mount's life, sealed
//!   against shrinking, sent at every handshake; a region of `u16` counters per open pack, grown
//!   by chunks so a reader's references are never unmapped.
//! - **Wait coalescing** ([`Pack::wait`]): at most one outstanding `wait` per 8 cells; a `wait`
//!   covers the rest of the file's span up to the end of its 8-cell window, and concurrent reads
//!   join it.
//! - **Reconnect**: on the end of the connection, a backoff of 50 ms doubling to 2 s with jitter
//!   (on an injected [`Timer`]), then the handshake with the same holder id, counter memfd and
//!   lifeline, `open-pack` for every pack, and every outstanding `wait` again. Replies of another
//!   epoch than the connection's are dropped.
//!
//! When the shared-cache crate of revision 8 (`tierstore-cache`) exists, this crate becomes its
//! `share::client` module, as the server becomes `share::server`; the protocol does not change.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod backoff;
mod client;
mod coalesce;
mod counters;
mod driver;
mod error;
mod framing;
mod link;
mod pack;
mod stats;
mod ticket;
mod view;

pub use client::{Config, PackClient};
pub use counters::PackCounters;
pub use error::{ClientError, StartError};
pub use link::{Connector, Sleep, SocketPath, Timer, TokioTimer};
pub use pack::{Attempt, OpenSpec, Pack, ReadGuard};
pub use stats::Stats;
pub use ticket::Ticket;
pub use view::{BadPackFds, PackView};

#[cfg(test)]
mod tests;
