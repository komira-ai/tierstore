//! Replay from the mark, and the reading of segments and tomb logs that the rebuild shares.

use std::collections::BTreeMap;

use tierstore_format::ids::{BlockId, NodeId};
use tierstore_format::names::HotName;
use tierstore_format::trailer::Trailer;
use tierstore_store_api::index::{Fact, Index, ReplayMark};

use super::facts::{Chains, Run, Skips, facts_of, large_run, parse_hot, scanned_run, trailer_run};
use super::scan::{Tail, io_err, read_all, scan_open, trailer_at};
use super::{Mode, OpenSegment, RecoverError, Recovery, RecoveryReport};
use crate::backend::Backend;
use crate::kv::KvIndex;

/// Entries of a tomb log read per call.
const LOG_PAGE: usize = 4096;

/// Which files of a segment exist.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct SegFiles {
    seg: bool,
    idx: bool,
}

impl Recovery<'_> {
    /// Replays from `mark`.
    pub(super) fn replay<B: Backend>(
        &self,
        index: &KvIndex<B>,
        mark: &ReplayMark,
    ) -> Result<RecoveryReport, RecoverError> {
        let mut report = RecoveryReport::new(Mode::Replay);
        let mut skips = Skips::load(index)?;
        let listed: BTreeMap<BlockId, u64> = mark.open_segments.iter().copied().collect();
        let mut newest: BTreeMap<NodeId, u64> = BTreeMap::new();
        for block in listed.keys() {
            let n = newest.entry(block.node()).or_default();
            *n = (*n).max(block.counter());
        }
        let mut chains = Chains::default();
        for (block, files) in self.segments()? {
            let from = listed.get(&block).copied();
            let fresh = newest
                .get(&block.node())
                .is_none_or(|n| block.counter() > *n);
            if (from.is_none() && !fresh) || skips.skip(block) {
                continue;
            }
            let run = self.segment(block, files, from.unwrap_or(0), &mut report, &mut chains)?;
            self.apply(index, facts_of(run), &mut report)?;
        }
        let (run, incomplete) = chains.finish(self.geometry);
        report.incomplete_chains += incomplete;
        self.apply(index, facts_of(run), &mut report)?;
        let run = large_run(self.hot, &mut report)?;
        self.apply(index, facts_of(run), &mut report)?;
        self.replay_logs(index, &mut report)?;
        skips.report(&mut report);
        Ok(report)
    }

    /// Applies `facts` with one call of the index.
    pub(super) fn apply<B: Backend>(
        &self,
        index: &KvIndex<B>,
        mut facts: impl Iterator<Item = Fact>,
        report: &mut RecoveryReport,
    ) -> Result<(), RecoverError> {
        report.add(index.replay(&mut facts)?);
        Ok(())
    }

    /// The segments under `seg/`, by block.
    pub(super) fn segments(&self) -> Result<BTreeMap<BlockId, SegFiles>, RecoverError> {
        let mut out: BTreeMap<BlockId, SegFiles> = BTreeMap::new();
        for name in io_err("seg", self.hot.list("seg"))? {
            match parse_hot("seg", &name) {
                Some(HotName::Segment(b)) => out.entry(b).or_default().seg = true,
                Some(HotName::SegmentIndex(b)) => out.entry(b).or_default().idx = true,
                _ => {}
            }
        }
        Ok(out)
    }

    /// The records of one segment: those of its trailer, from its `.idx` file or else the tail of
    /// its `.seg`, or, when it has none, those of a scan of its `.seg` from `from`.
    pub(super) fn segment(
        &self,
        block: BlockId,
        files: SegFiles,
        from: u64,
        report: &mut RecoveryReport,
        chains: &mut Chains,
    ) -> Result<Run, RecoverError> {
        if files.idx {
            let path = HotName::SegmentIndex(block).path();
            if let Ok(t) = Trailer::decode(&read_all(self.hot, &path)?) {
                report.idx_files += 1;
                return Ok(trailer_run(block, &t, chains));
            }
        }
        if !files.seg {
            return Ok(Vec::new());
        }
        let path = HotName::Segment(block).path();
        let len = io_err(&path, self.hot.len(&path))?;
        if let Tail::Found(t) = trailer_at(self.hot, &path, len)? {
            report.trailers += 1;
            return Ok(trailer_run(block, &t, chains));
        }
        let scan = scan_open(self.hot, &path, len, from, self.window)?;
        report.open_segments.push(OpenSegment {
            block,
            valid_len: scan.valid_len,
            torn: scan.torn,
        });
        Ok(scanned_run(block, scan.records))
    }

    /// Replays the entries of every origin's tomb log after the index's cursor.
    pub(super) fn replay_logs<B: Backend>(
        &self,
        index: &KvIndex<B>,
        report: &mut RecoveryReport,
    ) -> Result<(), RecoverError> {
        let Some(tomb) = self.tomb else {
            return Ok(());
        };
        for origin in tomb.origins() {
            let mut after = index.tomb_cursor(origin)?;
            loop {
                let (records, more) = tomb.read(origin, after, LOG_PAGE)?;
                after = records.last().map_or(after, |r| r.seq);
                report.log_entries += records.len() as u64;
                self.apply(index, records.into_iter().map(Fact::Log), report)?;
                if !more {
                    break;
                }
            }
        }
        Ok(())
    }
}
