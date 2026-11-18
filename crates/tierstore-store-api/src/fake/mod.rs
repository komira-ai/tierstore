//! A fake storage node with scripted service times, hangs, errors and crashes, on an injected
//! clock (section 14: "a fake drive and a fake storage node"), so every task of the storage tier
//! and the read path's `StoreUpstream` can run against the peer protocol before the real nodes
//! exist. Everything is in memory and implements [`crate::peer::PeerClient`].
//!
//! - [`FakeCluster`]: the nodes by id; it implements `PeerClient` by routing each call to the
//!   node it names, and an id it does not hold is `Unreachable`.
//! - [`FakeStorageNode`]: one node. A test loads objects (`put_hot`, `put_cold`, `delete`), its
//!   index state (`set_indexing`, `set_mem_epoch`), its ping (`set_drive`, `set_hot_queued`) and
//!   its tombstone logs (`append_tomb`), and scripts each route ([`Route`]) with [`Step`]s.
//! - [`Step`] and [`Outcome`]: what one call does: after `service_ms` on the injected
//!   [`tierstore_fetch_api::clock::ManualClock`] it answers, fails with a `PeerError`, or hangs
//!   forever, or crashes the node. A route with no scripted step answers at once.
//!
//! The node waits on the manual clock before it answers, so a test moves time by hand and uses
//! `ManualClock::sleepers()` as the progress event that tells it the call is waiting.
//!
//! The node keeps a plain `u64` as its cluster stamp: every call ticks it, an envelope's stamp is
//! merged first (`max(own, theirs) + 1`), and every answer carries the value after the tick. That
//! is the fake's simple stand-in for the cluster stamp, not the hybrid logical clock of the real
//! nodes.
//!
//! A crash loses nothing durable: objects, copies, seal states, commits and tombstone logs stay
//! across [`FakeStorageNode::restart`]; only the crashed flag is cleared.

mod cluster;
mod handlers;
mod node;
mod script;

pub use cluster::FakeCluster;
pub use node::FakeStorageNode;
pub use script::{Outcome, Route, Step};

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks a fake's mutex; a test that panicked while holding it has already failed, so the state
/// is used as it is.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
