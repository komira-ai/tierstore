//! Recovery after a restart (10.2, steps 1 to 4): the index of a node is made whole again from
//! what is on its disks, by a replay from the mark or, when there is no usable store, by a rebuild.
//!
//! [`Recovery::run`] replays from the mark when the index holds one and rebuilds otherwise; neither
//! calls [`KvIndex::set_ready`], the caller does after its tombstone pull.
//!
//! **Replay from the mark.** For each open segment the mark lists, the records of `seg/<id>.seg`
//! from the marked length to the last record whose checksums verify are read in windows of
//! [`Recovery::window`] bytes (a record is read whole, so a window is at least one record). A
//! torn tail of such a scan, bytes after the last valid record that are not zero, is cut off the
//! file and reported. A segment sealed since (its `seg/<id>.idx` exists, or its `.seg` ends in a
//! valid trailer), which the mark lists as open or which is not in the mark and is newer than every
//! block the mark lists for its node, is read by its trailer alone; a segment not in the mark with
//! no trailer is scanned from 0. The large files under `big/` are read (the layout below), and the
//! tail of every origin's tomb log after the index's cursor. Every fact goes through
//! [`Index::replay`], one call per segment, so the memory in use is one segment's facts.
//!
//! **Rebuild.** First the block table: every `stripes/*.desc` is put and then every
//! `stripes/*.free` applied, and the blocks named by a block drop in a tomb log are noted; the
//! trailers of those blocks and of the blocks of a freed stripe are skipped. Then facts are read,
//! each block once, from the first source that has it: the hot sealed segments (`seg/<id>.idx`,
//! else the trailer at the tail of `seg/<id>.seg`), the open segments (a record scan from 0),
//! `blk/<id>.idx`, and the trailer at the end of the data region of a data shard or copy file on a
//! cold drive (one footer read and one trailer read per file; a parity shard, or a data shard of a
//! block already read, is not read). Large files follow, and the tomb logs last. Each source is a
//! run sorted by key; the runs are merged with a k-way merge over their heads and fed to one
//! [`Index::replay`], so the index is loaded in key order. Memory is the decoded trailers and the
//! open-segment records of all runs at once, consumed as the merge goes: bounded by the entries of
//! the node, which is what this change accepts (the design's bulk load streams each trailer).
//!
//! **Chains.** A trailer of kind [`BlockKind::ChainSlice`] contributes one slice. The slices of
//! one `(key, generation, stamp)` found in the blocks read are checked against the geometry (the
//! blocks of one chain are consecutive and each slice has its length) and make one record whose
//! location is the chain's first block and whose length is the sum of the slices; a chain with a
//! slice missing, because its block is dropped, freed or unreadable, is not indexed and is counted.
//!
//! **Large files.** The layout recovery reads: a large file begins with a 64-byte record header
//! ([`tierstore_format::record::RecordHeader`]) and the key bytes, the data follows. One read of
//! the file's head gives both; the generation and size of the header must equal those in the file
//! name and the hash of the key must equal the key hash of the name, else the file is counted in
//! [`RecoveryReport::bad_large_files`] and skipped, never indexed. The record is made with the
//! header's stamp and flags and [`EntryLocation::Large`].

mod facts;
mod merge;
mod rebuild;
mod replay;
mod scan;

use std::io;

use tierstore_format::FormatError;
use tierstore_format::block::BlockGeometry;
use tierstore_format::ids::BlockId;
use tierstore_store_api::index::{Index, IndexError, ReplayStats};

use crate::backend::Backend;
use crate::fs::Fs;
use crate::kv::KvIndex;
use crate::tomblog::{LogError, TombLog};

/// Why recovery failed.
#[derive(Debug, thiserror::Error)]
pub enum RecoverError {
    /// The index refused a read or a write.
    #[error("recovery: {0}")]
    Index(#[from] IndexError),
    /// A file operation failed.
    #[error("recovery: {path}: {source}")]
    Io {
        /// The path the operation was on.
        path: String,
        /// The error.
        source: io::Error,
    },
    /// The bytes of a file are not the format they should be.
    #[error("recovery: {path}: {source}")]
    Format {
        /// The file.
        path: String,
        /// What is wrong with the bytes.
        source: FormatError,
    },
    /// The tomb log could not be read.
    #[error("recovery: {0}")]
    Log(#[from] LogError),
}

/// How the index was recovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// From the replay mark of the store.
    Replay,
    /// From the files alone.
    Rebuild,
}

/// An open segment that was scanned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenSegment {
    /// The segment's block.
    pub block: BlockId,
    /// The bytes up to the end of its last valid record: the length the writer resumes at.
    pub valid_len: u64,
    /// Whether bytes after `valid_len` were not zero and were cut off the file.
    pub torn: bool,
}

/// What a recovery did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Replay or rebuild.
    pub mode: Mode,
    /// Facts the index read, over every call of [`Index::replay`].
    pub facts: u64,
    /// Facts that changed an entry.
    pub applied: u64,
    /// The open segments scanned, in the order they were.
    pub open_segments: Vec<OpenSegment>,
    /// Trailers read from the tail of a hot `seg/<id>.seg`.
    pub trailers: u64,
    /// `seg/<id>.idx` and `blk/<id>.idx` files read.
    pub idx_files: u64,
    /// Trailers read from cold shard and copy files.
    pub shard_trailers: u64,
    /// Large files indexed.
    pub large_files: u64,
    /// Large files whose head does not match their name: reported and not indexed.
    pub bad_large_files: u64,
    /// Chains not indexed because a slice is missing or the slices do not fit the geometry.
    pub incomplete_chains: u64,
    /// Entries of the tomb logs read.
    pub log_entries: u64,
    /// Blocks not read because they are dropped.
    pub skipped_dropped: u64,
    /// Blocks not read because their stripe is freed.
    pub skipped_freed: u64,
}

impl RecoveryReport {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            facts: 0,
            applied: 0,
            open_segments: Vec::new(),
            trailers: 0,
            idx_files: 0,
            shard_trailers: 0,
            large_files: 0,
            bad_large_files: 0,
            incomplete_chains: 0,
            log_entries: 0,
            skipped_dropped: 0,
            skipped_freed: 0,
        }
    }

    fn add(&mut self, stats: ReplayStats) {
        self.facts += stats.facts;
        self.applied += stats.applied;
    }
}

/// The files recovery reads and how.
pub struct Recovery<'a> {
    /// The node's hot directory.
    pub hot: &'a dyn Fs,
    /// Each drive's `ts-cold` directory.
    pub cold: Vec<&'a dyn Fs>,
    /// The tomb log files under `hot`, opened by the caller (which cut a torn tail on opening),
    /// under the majority-ledger coordination model ([`crate::ledger`]); `None` under a model
    /// without them: recovery then reads no log, and tombstones and block drops come from the
    /// model's own source (applied as facts or with `delete`).
    pub tomb: Option<&'a TombLog>,
    /// The block geometry, to check a chain's slices.
    pub geometry: BlockGeometry,
    /// The bytes read per step of the scan of an open segment.
    pub window: u64,
}

impl Recovery<'_> {
    /// Recovers `index`: a replay from its mark when it has one, else a rebuild.
    pub fn run<B: Backend>(&self, index: &KvIndex<B>) -> Result<RecoveryReport, RecoverError> {
        match index.replay_mark()? {
            Some(mark) => self.replay(index, &mark),
            None => self.rebuild(index),
        }
    }
}
