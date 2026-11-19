//! A tree of the tierstore read path, read by path (`docs/design/fetch-cache-serve.md`, 5.1,
//! 5.3 and section 23): the layer the fronts are adapters on. The FUSE front's `TreeFs` in the
//! daemon is one; this crate has no FUSE and no NFS of its own.
//!
//! - [`Image`]: a tree's L2 list (`tierstore-list`), checked once at open against the tree id,
//!   then mapped read-only (`PROT_READ`, `MAP_PRIVATE`) and never written. The record sections
//!   (the entries and the names) are advised `WILLNEED` at open, and for a hot mount
//!   ([`ImageOptions::lock`]) also mapped a second time `MAP_LOCKED`, so a first touch of a record
//!   does not wait on the cache device (5.3). Every read copies the bytes it needs out of the
//!   mapping: a LOOKUP is a binary search of the sorted children's records, a node is its record,
//!   a name or a link's target is copied from the names. File data is not mapped.
//! - [`Tree`]: the image and its packs, each opened once through the mount protocol
//!   (`tierstore-mount-client`, `open-pack`) when the tree is opened. [`Tree::read`] runs the
//!   reader protocol of 5.1 for a range of a file: the group counters are raised and every
//!   covered cell's state loaded (`begin_read`); if all are `present` the caller's [`Stage`]
//!   reads the bytes from the pack's data descriptor; then every cell is loaded again (the
//!   post-read recheck) and only if all are still `present` is the read committed, before the
//!   counters are lowered. Bytes read over a cell that changed are discarded, never served.
//! - Not ready: [`ReadStep::Park`] hands the stage back with a [`Wait`]; [`Tree::wait`] waits on
//!   the mount protocol (`wait`), and the caller reads again from the start after
//!   [`Waited::Retry`]. A `bad` cell fails the read with EIO at once.
//! - Lost (4.6): a `wait` answered `lost` marks the whole tree lost. Every parked
//!   [`Tree::wait`] of the tree then ends with [`Waited::Lost`], and every later read fails with
//!   [`ReadFailure::Lost`], until the tree is dropped (the mount's release). [`Tree::lost`]
//!   resolves when it happens, for whoever reports the jobs that hold the tree as failed with the
//!   retriable cause `tree-lost` (`tierstore_fetch_api::TREE_LOST_CAUSE`).
//!
//! No clock and no timer: nothing here measures time, and a parked read waits for the protocol's
//! answer or the tree's loss. Linux only, as the mount protocol is.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod error;
mod image;
mod read;
#[cfg(feature = "testing")]
pub mod testing;
mod tree;

pub use error::TreeError;
pub use image::{Advice, Image, ImageOptions, ImagePack, NAME_MAX, Node, NodeKind};
pub use read::{ReadFailure, ReadStep, Stage, Staged, Wait, Waited};
pub use tree::{Tree, TreeConfig, TreeStats};
