//! The hot tier of a tierstore storage node (`docs/design/fetch-cache-serve.md` 10.3; task T-S4
//! of section 18): [`HotNode`], the frozen [`tierstore_store_api::hot::HotStore`] on the node's
//! NVMe (LATER) or SSD partition (NOW).
//!
//! What is here, by module:
//! - [`config`]: the `[hot]` settings and their checks.
//! - [`dev`]: the hot device's file operations, and the real directory behind them;
//!   [`devfs`] shows the device to the index's recovery.
//! - [`sched`]: `HotSched`, the device's I/O queue: depth, class order, stall watchdog.
//! - [`peers`]: what the hot store needs from the other storage nodes (replication batches,
//!   seal messages, large-file streams, fetches), and the spawner of its background tasks.
//! - [`index`]: the part of the node's index and block table the store uses.
//! - [`choose`]: the Choose rule of the majority seal.
//! - [`node`]: the store: write segments with group commit and the ack rule, a replica's side
//!   of replication, the voter and the sealer of the majority seal, converge, seal-and-roll and
//!   take-over, the seal triggers, large files teed to every replica with the trailer check, the
//!   startup sweep and recovery, hot scrub, reclaim and the dirty marks, reads.
//!
//! Section 14 rows this crate's tests prove: group commit; replication under crashes and message
//! loss; seal and roll; two sealers under a partition; seal triggers; large object PUT; weaker
//! acknowledgement; the hot part of backpressure; the NVMe `HotSched` honours its depth and
//! class order.

#![forbid(unsafe_code)]

pub mod choose;
pub mod config;
pub mod dev;
pub mod devfs;
pub mod index;
pub mod node;
pub mod peers;
pub mod sched;

pub use node::{HotNode, HotParts};
