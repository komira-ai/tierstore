//! What a case asks of the node under test and of its upstream: the agent's three calls, a read of
//! a tree through the mount, what the node shows from outside (its placements on disk, its
//! resident memory, its queue gauges, whether it runs), and what the rig does to it (writes trees
//! into the upstream, stalls or fails the upstream, kills and starts the node again, fills its
//! disk), with the rig's clock. [`Live`](crate::live::Live) is the rig of a running node; the
//! cases' tests have a rig of their own, whose clock moves when a case pauses.

use std::path::{Path, PathBuf};
use std::time::Duration;

use objstore_s3::s3::fake::Serving;
use tierstore_trees_api::provider::v1::Source;

use crate::metrics::Gauges;
use crate::provider::Refusal;
use crate::tree::{Tree, TreeFault};

/// The rig of a case (module docs). Errors other than the provider's are text: they end the case.
#[expect(
    async_fn_in_trait,
    reason = "the cases and the tests of this crate are its only callers, and none sends a future to another thread"
)]
pub trait Rig {
    /// The agent's Prepare of `source` with the tree whose list is `tree`.
    async fn prepare(&mut self, source: &Source, tree: &str) -> Result<(), Refusal>;

    /// The agent's Mount of `source`: the directory of its tree.
    async fn mount(&mut self, source: &Source) -> Result<PathBuf, Refusal>;

    /// The agent's Release of `source`.
    async fn release(&mut self, source: &Source) -> Result<(), Refusal>;

    /// Reads `tree` at `at` as a job would ([`Tree::check`]).
    async fn read(&mut self, at: &Path, tree: &Tree) -> Result<(), TreeFault>;

    /// The placements the node keeps on disk.
    fn placements(&mut self) -> Result<usize, String>;

    /// The node's resident memory, in bytes.
    fn resident(&mut self) -> Result<u64, String>;

    /// The node's queue gauges.
    async fn gauges(&mut self) -> Result<Gauges, String>;

    /// Writes `tree` into the upstream.
    async fn put(&mut self, tree: &Tree) -> Result<(), String>;

    /// Makes the upstream serve as `serving` says.
    async fn serve_as(&mut self, serving: Serving) -> Result<(), String>;

    /// The requests a stall of the upstream has held so far.
    async fn stalled(&mut self) -> Result<u64, String>;

    /// Kills the node (SIGKILL) and starts it again on the same files.
    async fn restart(&mut self) -> Result<(), String>;

    /// Whether the node still runs.
    fn alive(&mut self) -> bool;

    /// Fills the disk of the node's cache: the bytes written.
    fn fill_disk(&mut self) -> Result<u64, String>;

    /// Gives back what [`Rig::fill_disk`] took.
    fn free_disk(&mut self) -> Result<(), String>;

    /// The time since the rig started.
    fn now(&self) -> Duration;

    /// Waits `time`.
    async fn pause(&mut self, time: Duration);

    /// Stops the node and its upstream.
    async fn stop(self) -> Result<(), String>;
}
