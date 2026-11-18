//! A storage node's key index and block table (`docs/design/fetch-cache-serve.md`, section 10.2;
//! task T-S3 of section 18): the frozen [`tierstore_store_api::index::Index`] in an embedded ordered
//! store, with everything a node needs to keep it right across crashes, deletes and restarts.
//!
//! What is here, by module:
//! - [`backend`]: the ordered store under the index, as a small trait with three implementations:
//!   `fjall` (an LSM-tree, the design's choice), `redb` (a copy-on-write B-tree, the measured
//!   alternative) and an in-memory store that models a crash as the loss of the batches after the
//!   last persist.
//! - [`kv`]: [`KvIndex`], the `Index` on a backend: the liveness rule and "the newest fact about a
//!   key wins", create-only `put_if_absent`, tombstones, relocates, promoted-copy hints, the replay
//!   mark and replay, per-block live counts, and `indexing` until the node is built.
//! - [`tombs`]: the tombstones a node holds until their drop, what each names, and the one drop
//!   rule (10.2).
//! - [`blocks`]: the block table: block rows and stripe descriptors and frees.
//! - [`tomblog`]: the tombstone log files `tomb/<origin>/<first_seq>.log`: appends, pulls after a
//!   cursor, compaction.
//! - [`fs`]: the file access recovery and the tomb log use, on a real directory or on a fake file
//!   system that crashes on demand.
//! - [`recover`]: recovery after a restart: replay from the mark (open segments, segments sealed
//!   since, large files, the tomb log tail) or a rebuild as a bulk load in key order from hot
//!   trailers, `blk/*.idx` files and cold shard trailers (10.2 steps 1 to 4).
//! - [`sync`]: `index-sync`: a peer streams its index range for the keys this node should hold.
//! - [`list`]: the LIST of the S3 front over several index nodes: merge of sorted pages, duplicates
//!   collapsed, the delimiter roll-up with one seek past each rolled-up prefix, continuation.
//! - [`measure`]: the bytes-per-key, point-read and bulk-load measurement of 10.2, run by the
//!   `tierstore-index-measure` binary.

#![forbid(unsafe_code)]

pub mod backend;
pub mod blocks;
mod codec;
pub mod fs;
pub mod kv;
pub mod list;
pub mod measure;
pub mod recover;
mod rules;
pub mod sync;
pub mod tomblog;
pub mod tombs;

pub use kv::{IndexConfig, KvIndex, STORE_VERSION};
