//! The mount provider of a build node: jobscheduler's `MountProvider` protocol
//! (`jobscheduler.provider.v1`, vendored in `tierstore-trees-api`) on a Unix socket, the
//! `mount-provider` interface. The design is jobscheduler's `docs/design/mount-providers.md`
//! (komira-ai/jobscheduler#896) for the contract and `docs/design/node-local-trees.md` for the
//! trees it makes.
//!
//! The agent of jobscheduler on the node hands a job's source to the provider as soon as it has the
//! job, and the provider makes the source's tree local while the agent readies the container:
//!
//! - **`Prepare(source, ref)`** checks the ref, which must be the key of a tree list
//!   (`cas/l/<64 hex>`, see [`parse_tree_list`](crate::parse_tree_list)), and answers at once. In
//!   the background it reads that one object through the cache, checks that it hashes to its key,
//!   builds the tree of exactly its entries, and places it in the tree store at the path of the
//!   source under the mount of `tierstore fs` (module `names`). Then it reads the bytes of every
//!   file into the cache, at most `trees.fill_concurrency` files at once across the node, each
//!   with the call an open of the mount makes for it (`TreeStore::fetch_span`), so that an open of
//!   a file still being read waits for that one read instead of starting another.
//! - **`Mount(source)`** answers the tree's directory once it is placed (its files may still be
//!   filling), or `UNAVAILABLE` "not ready" after [`MOUNT_WAIT`], and the agent asks again; a ref
//!   the provider cannot use (no such object, a malformed list, a tree the store refuses) is
//!   `FAILED_PRECONDITION` with the reason.
//! - **`Release(source)`** stops the source's reads and removes its placement; the tree goes when
//!   nothing else holds it, and its packs stay in the cache to be evicted in their turn.
//! - **`Sweep(agent, keep)`** releases every source of the agent whose run is not in `keep`, both
//!   those this process prepared and the placements it finds under the agent's path.
//!
//! Every call is idempotent by its source. Every read of the store has a bound,
//! `params.fetch_timeout`; a read that passes it fails that file (an open reads it again) or, for
//! the tree list, the preparation, which the next `Mount` starts again. A placement lasts
//! `trees.ttl_max` from its preparation unless it is released, so a source an agent never released
//! goes in its turn.

pub(super) mod config;
mod listener;
mod names;
mod prepare;
mod service;
mod sources;

#[cfg(test)]
mod names_tests;
#[cfg(test)]
mod prepare_tests;
#[cfg(test)]
mod serve_tests;
#[cfg(test)]
mod service_tests;
#[cfg(test)]
mod sources_tests;
#[cfg(test)]
mod testing;

use std::path::PathBuf;
use std::time::Duration;

pub(super) use listener::serve;
pub(super) use sources::Provider;

/// The mount of `tierstore fs` when the interface does not say: `fs.mountpoint`'s default.
pub(super) const DEFAULT_MOUNTPOINT: &str = "/run/tierstore/fs";

/// How long one read of the store may take when the interface does not say.
pub(super) const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a `Mount` waits for its tree before it answers "not ready": below the 30 s the contract
/// gives the call.
pub(super) const MOUNT_WAIT: Duration = Duration::from_secs(25);

/// The longest reason a refusal carries, in bytes, as the contract bounds it.
pub(super) const MAX_REASON_BYTES: usize = 256;

/// The settings of the provider, as the `mount-provider` interface makes them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ProviderFlags {
    /// The path of the socket.
    pub socket: PathBuf,
    /// The mount of `tierstore fs`, under which a source's directory is answered.
    pub mountpoint: PathBuf,
    /// How long one read of the store may take.
    pub fetch_timeout: Duration,
}

/// Why a call is not answered with success: the three other answers of the contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ProviderError {
    /// The request is wrong in itself: `INVALID_ARGUMENT`.
    Invalid(String),
    /// What the request names cannot be used, now or later: `FAILED_PRECONDITION`.
    Refused(String),
    /// Not now: `UNAVAILABLE`, and the agent asks again.
    Unavailable(String),
}
