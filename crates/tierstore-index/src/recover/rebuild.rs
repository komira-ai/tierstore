//! Rebuild: the index made from the files alone, as a bulk load in key order.

use std::collections::BTreeSet;

use tierstore_format::FormatError;
use tierstore_format::descriptor::StripeDescriptor;
use tierstore_format::footer::FooterSubject;
use tierstore_format::free::StripeFree;
use tierstore_format::ids::BlockId;
use tierstore_format::names::{ColdName, HotName};
use tierstore_format::tomb::LogEntry;
use tierstore_format::trailer::Trailer;

use super::facts::{Chains, Run, Skips, large_run, parse_hot, trailer_run};
use super::merge::Merge;
use super::scan::{Tail, fmt_err, footer_at, io_err, read_all, trailer_at};
use super::{Mode, RecoverError, Recovery, RecoveryReport};
use crate::backend::Backend;
use crate::fs::Fs;
use crate::kv::KvIndex;

/// What a rebuild has read so far.
struct Sources {
    skips: Skips,
    read: BTreeSet<BlockId>,
    chains: Chains,
    runs: Vec<Run>,
}

impl Sources {
    /// Whether `block` is to be left out: read already, dropped or freed.
    fn left_out(&mut self, block: BlockId) -> bool {
        self.read.contains(&block) || self.skips.skip(block)
    }
}

impl Recovery<'_> {
    /// Rebuilds `index` from the files: the stripes and block drops first, then each block's
    /// records from the first source that has its trailer, the large files, and the tomb logs.
    pub fn rebuild<B: Backend>(&self, index: &KvIndex<B>) -> Result<RecoveryReport, RecoverError> {
        let mut report = RecoveryReport::new(Mode::Rebuild);
        self.stripes(index)?;
        let mut skips = Skips::load(index)?;
        self.note_drops(&mut skips)?;
        let mut src = Sources {
            skips,
            read: BTreeSet::new(),
            chains: Chains::default(),
            runs: Vec::new(),
        };
        for (block, files) in self.segments()? {
            if src.skips.skip(block) {
                continue;
            }
            src.read.insert(block);
            let run = self.segment(block, files, 0, &mut report, &mut src.chains)?;
            src.runs.push(run);
        }
        self.block_indexes(&mut src, &mut report)?;
        for fs in &self.cold {
            self.cold_files(index, *fs, &mut src, &mut report)?;
        }
        let (run, incomplete) = src.chains.finish(self.geometry);
        report.incomplete_chains += incomplete;
        src.runs.push(run);
        src.runs.push(large_run(self.hot, &mut report)?);
        self.apply(index, Merge::new(src.runs), &mut report)?;
        self.replay_logs(index, &mut report)?;
        src.skips.report(&mut report);
        Ok(report)
    }

    /// Puts every stripe descriptor of `stripes/` and then applies every free.
    fn stripes<B: Backend>(&self, index: &KvIndex<B>) -> Result<(), RecoverError> {
        let mut descriptors = Vec::new();
        let mut frees = Vec::new();
        for name in io_err("stripes", self.hot.list("stripes"))? {
            match parse_hot("stripes", &name) {
                Some(HotName::Descriptor(_)) => descriptors.push(format!("stripes/{name}")),
                Some(HotName::Free(_)) => frees.push(format!("stripes/{name}")),
                _ => {}
            }
        }
        for path in descriptors {
            let bytes = read_all(self.hot, &path)?;
            index.put_stripe(&fmt_err(&path, StripeDescriptor::decode(&bytes))?)?;
        }
        for path in frees {
            let bytes = read_all(self.hot, &path)?;
            index.apply_stripe_free(&fmt_err(&path, StripeFree::decode(&bytes))?)?;
        }
        Ok(())
    }

    /// Notes the blocks named by a block drop in any origin's tomb log.
    fn note_drops(&self, skips: &mut Skips) -> Result<(), RecoverError> {
        let Some(tomb) = self.tomb else {
            return Ok(());
        };
        for origin in tomb.origins() {
            for record in tomb.read_all(origin)? {
                if let LogEntry::BlockDrop(drop) = record.entry {
                    skips.drop_block(drop.block);
                }
            }
        }
        Ok(())
    }

    /// The records of every `blk/<id>.idx` of a block not read. A file that is not a trailer
    /// leaves its block unread, for the cold drives.
    fn block_indexes(
        &self,
        src: &mut Sources,
        report: &mut RecoveryReport,
    ) -> Result<(), RecoverError> {
        for name in io_err("blk", self.hot.list("blk"))? {
            let Some(HotName::BlockIndex(block)) = parse_hot("blk", &name) else {
                continue;
            };
            if src.left_out(block) {
                continue;
            }
            let bytes = read_all(self.hot, &format!("blk/{name}"))?;
            if let Ok(t) = Trailer::decode(&bytes) {
                report.idx_files += 1;
                src.read.insert(block);
                let run = trailer_run(block, &t, &mut src.chains);
                src.runs.push(run);
            }
        }
        Ok(())
    }

    /// The trailers of the data shard and copy files of one cold drive.
    fn cold_files<B: Backend>(
        &self,
        index: &KvIndex<B>,
        fs: &dyn Fs,
        src: &mut Sources,
        report: &mut RecoveryReport,
    ) -> Result<(), RecoverError> {
        for hh in io_err("s", fs.list("s"))? {
            let dir = format!("s/{hh}");
            for name in io_err(&dir, fs.list(&dir))? {
                let path = format!("{dir}/{name}");
                let known = match ColdName::parse(&path) {
                    Some(ColdName::Copy(block)) => Some(block),
                    Some(ColdName::Shard {
                        stripe,
                        index: shard,
                    }) => match index.stripe(stripe)? {
                        // The descriptor says which block the shard holds, or that it is parity.
                        Some(row) => match row.descriptor.blocks.get(usize::from(shard)) {
                            Some((block, _)) => Some(*block),
                            None => continue,
                        },
                        None => None,
                    },
                    _ => continue,
                };
                if known.is_some_and(|block| src.left_out(block)) {
                    continue;
                }
                self.cold_file(fs, &path, known, src, report)?;
            }
        }
        Ok(())
    }

    /// The trailer of one cold file: its footer says the length of its data region and, when the
    /// block is not known from the descriptor, the block; the trailer ends the data region.
    fn cold_file(
        &self,
        fs: &dyn Fs,
        path: &str,
        known: Option<BlockId>,
        src: &mut Sources,
        report: &mut RecoveryReport,
    ) -> Result<(), RecoverError> {
        let len = io_err(path, fs.len(path))?;
        let footer = footer_at(fs, path, len)?;
        let block = match (known, &footer.subject) {
            (Some(block), _) => Some(block),
            (None, FooterSubject::Copy { block, .. }) => Some(*block),
            (None, FooterSubject::Stripe { stripe, index }) => {
                stripe.blocks.get(usize::from(*index)).map(|b| b.0)
            }
        };
        let Some(block) = block else {
            return Ok(());
        };
        if known.is_none() && src.left_out(block) {
            return Ok(());
        }
        let trailer = match trailer_at(fs, path, footer.data_len)? {
            Tail::Found(t) => t,
            Tail::Bad(source) => return fmt_err(path, Err(source)),
            Tail::Absent => {
                return fmt_err(
                    path,
                    Err(FormatError::Invalid {
                        what: "block trailer",
                        reason: "no trailer at the end of the data region",
                    }),
                );
            }
        };
        report.shard_trailers += 1;
        src.read.insert(block);
        let run = trailer_run(block, &trailer, &mut src.chains);
        src.runs.push(run);
        Ok(())
    }
}
