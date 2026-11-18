//! The facts recovery makes: a trailer's entries, the records of an open segment, the chains the
//! slices of trailers add up to, the large files, and the blocks that are skipped.

use std::collections::{BTreeMap, BTreeSet};

use tierstore_format::block::BlockGeometry;
use tierstore_format::ids::{BlockId, Generation, KeyHash};
use tierstore_format::names::HotName;
use tierstore_format::record::{RECORD_HEADER_LEN, RecordFlags, RecordHeader};
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::LargeFile;
use tierstore_format::trailer::{BlockKind, Trailer};
use tierstore_store_api::index::{EntryLocation, Fact, IndexEntry, IndexError};

use super::scan::{Scanned, io_err};
use super::{RecoverError, RecoveryReport};
use crate::backend::Backend;
use crate::blocks::BlockState;
use crate::fs::Fs;
use crate::kv::{KvIndex, SCAN_PAGE};

/// A run of records sorted by key and stamp: one source of the merge.
pub(super) type Run = Vec<(Vec<u8>, IndexEntry)>;

/// The facts of a run, in its order.
pub(super) fn facts_of(run: Run) -> impl Iterator<Item = Fact> {
    run.into_iter()
        .map(|(key, entry)| Fact::Record { key, entry })
}

fn entry_in_block(
    block: BlockId,
    offset: u64,
    generation: Generation,
    length: u64,
    stamp: Stamp,
    flags: RecordFlags,
) -> IndexEntry {
    IndexEntry {
        generation,
        location: EntryLocation::Block { block, offset },
        length,
        stamp,
        flags,
        prior_blocks: Vec::new(),
        promoted: None,
    }
}

/// The records of a trailer of `block`: its entries in its order, a record each with the entry's
/// flags (a pack block's entries carry the relocate flag); a chain slice goes to `chains`.
pub(super) fn trailer_run(block: BlockId, t: &Trailer, chains: &mut Chains) -> Run {
    let mut run = Vec::new();
    for e in &t.entries {
        if t.kind == BlockKind::ChainSlice {
            chains.add(&e.key, e.generation, e.stamp, block, e.length);
        } else {
            run.push((
                e.key.clone(),
                entry_in_block(block, e.offset, e.generation, e.length, e.stamp, e.flags),
            ));
        }
    }
    run
}

/// The records of a scan of an open segment of `block`, sorted by key and stamp.
pub(super) fn scanned_run(block: BlockId, records: Vec<Scanned>) -> Run {
    let mut run: Run = records
        .into_iter()
        .map(|s| {
            let h = s.header;
            let entry = entry_in_block(block, s.offset, h.generation, h.data_len, h.stamp, h.flags);
            (s.key, entry)
        })
        .collect();
    run.sort_by(|a, b| (&a.0, a.1.stamp).cmp(&(&b.0, b.1.stamp)));
    run
}

/// What names one object's chain: its key, generation and stamp.
type ChainKey = (Vec<u8>, Generation, Stamp);

/// The slices of chains seen in trailers, by chain.
#[derive(Default)]
pub(super) struct Chains {
    slices: BTreeMap<ChainKey, Vec<(BlockId, u64)>>,
}

impl Chains {
    fn add(&mut self, key: &[u8], generation: Generation, stamp: Stamp, block: BlockId, len: u64) {
        self.slices
            .entry((key.to_vec(), generation, stamp))
            .or_default()
            .push((block, len));
    }

    /// One record per chain whose slices are all there, sorted by key and stamp, and the number
    /// of chains that are not.
    pub(super) fn finish(self, geometry: BlockGeometry) -> (Run, u64) {
        let mut run: Run = Vec::new();
        let mut incomplete = 0;
        for ((key, generation, stamp), mut slices) in self.slices {
            slices.sort();
            let first = slices[0].0;
            let total: u64 = slices.iter().map(|s| s.1).sum();
            let whole = geometry.chain(first, total).is_ok_and(|chain| {
                chain.blocks() == slices.len() as u64
                    && slices.iter().enumerate().all(|(i, (block, len))| {
                        chain
                            .slice(i as u64)
                            .is_some_and(|s| s.block == *block && s.len == *len)
                    })
            });
            if whole {
                let entry = IndexEntry {
                    generation,
                    location: EntryLocation::Chain { first },
                    length: total,
                    stamp,
                    flags: RecordFlags(0),
                    prior_blocks: Vec::new(),
                    promoted: None,
                };
                run.push((key, entry));
            } else {
                incomplete += 1;
            }
        }
        run.sort_by(|a, b| (&a.0, a.1.stamp).cmp(&(&b.0, b.1.stamp)));
        (run, incomplete)
    }
}

/// The blocks whose trailers are not read: dropped by a block drop, or in a freed stripe.
pub(super) struct Skips {
    dropped: BTreeSet<BlockId>,
    freed: BTreeSet<BlockId>,
    hit: BTreeSet<BlockId>,
}

impl Skips {
    /// The blocks the block table marks dropped or freed.
    pub(super) fn load<B: Backend>(index: &KvIndex<B>) -> Result<Self, IndexError> {
        let mut skips = Self {
            dropped: BTreeSet::new(),
            freed: BTreeSet::new(),
            hit: BTreeSet::new(),
        };
        let mut after = None;
        loop {
            let page = index.blocks(after, SCAN_PAGE)?;
            for (block, info) in &page {
                match info.state {
                    BlockState::Dropped => skips.dropped.insert(*block),
                    BlockState::Freed => skips.freed.insert(*block),
                    _ => false,
                };
            }
            after = page.last().map(|(b, _)| *b);
            if page.len() < SCAN_PAGE {
                return Ok(skips);
            }
        }
    }

    /// Notes a block named by a block drop of a tomb log.
    pub(super) fn drop_block(&mut self, block: BlockId) {
        self.dropped.insert(block);
    }

    /// Whether `block` is not read; a skipped block is counted once.
    pub(super) fn skip(&mut self, block: BlockId) -> bool {
        let skipped = self.dropped.contains(&block) || self.freed.contains(&block);
        if skipped {
            self.hit.insert(block);
        }
        skipped
    }

    /// Puts the counts of the blocks skipped in `report`.
    pub(super) fn report(&self, report: &mut RecoveryReport) {
        report.skipped_dropped =
            self.hit.iter().filter(|b| self.dropped.contains(b)).count() as u64;
        report.skipped_freed = self.hit.len() as u64 - report.skipped_dropped;
    }
}

/// The large files under `big/`, read by their heads, as a run sorted by key and stamp. A file whose
/// head does not match its name is counted in `report` and left out.
pub(super) fn large_run(fs: &dyn Fs, report: &mut RecoveryReport) -> Result<Run, RecoverError> {
    let mut run: Run = Vec::new();
    for dir in io_err("big", fs.list("big"))? {
        if dir.len() != 2 {
            continue;
        }
        let dir = format!("big/{dir}");
        for name in io_err(&dir, fs.list(&dir))? {
            let path = format!("{dir}/{name}");
            let Some(HotName::Large(file)) = HotName::parse(&path) else {
                continue;
            };
            let head = io_err(
                &path,
                fs.read_at(&path, 0, (RECORD_HEADER_LEN + usize::from(u16::MAX)) as u64),
            )?;
            match large_entry(file, &head) {
                Some(kv) => {
                    report.large_files += 1;
                    run.push(kv);
                }
                None => report.bad_large_files += 1,
            }
        }
    }
    run.sort_by(|a, b| (&a.0, a.1.stamp).cmp(&(&b.0, b.1.stamp)));
    Ok(run)
}

/// The record a large file's head makes, when the head matches the file's name.
fn large_entry(file: LargeFile, head: &[u8]) -> Option<(Vec<u8>, IndexEntry)> {
    let header = RecordHeader::decode(head).ok()?;
    let key = head.get(RECORD_HEADER_LEN..RECORD_HEADER_LEN + usize::from(header.key_len))?;
    let matches = header.generation == file.generation
        && header.data_len == file.size
        && tierstore_format::crc32c(key) == header.key_crc
        && KeyHash::of(key) == file.key_hash;
    matches.then(|| {
        let entry = IndexEntry {
            generation: file.generation,
            location: EntryLocation::Large(file),
            length: file.size,
            stamp: header.stamp,
            flags: header.flags,
            prior_blocks: Vec::new(),
            promoted: None,
        };
        (key.to_vec(), entry)
    })
}

/// A file of the hot layout whose parse is wanted: a convenience for listing.
pub(super) fn parse_hot(dir: &str, name: &str) -> Option<HotName> {
    HotName::parse(&format!("{dir}/{name}"))
}
