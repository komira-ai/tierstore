//! The sweep of the stage, which a frontend runs every 60 seconds: it removes
//!
//! - the staged files whose age from their create has reached the stage TTL (`fs.put.stage_ttl_secs`,
//!   longer than any job), unless a commit of one is running;
//! - the committed names whose age from their commit has reached the committed TTL
//!   (`fs.put.committed_ttl_secs`), which only shows them;
//! - the files a start found with no staged entry ([`ORPHAN_GRACE`] after that start: the time a
//!   process that handed the mount over may still write the line of a file it made);
//!
//! and rewrites the journal with the live entries alone once it has more than twice as many lines,
//! plus [`COMPACT_SLACK`].

use std::time::Duration;

use super::journal::{Journal, Record};
use super::replay::live_records;
use super::store::StagedStore;
use crate::server::treefs::locked;

/// How long after a start a file it found with no entry is removed.
pub(super) const ORPHAN_GRACE: Duration = Duration::from_secs(600);

/// The lines over twice the live entries' that the journal may have before a sweep rewrites it.
pub(super) const COMPACT_SLACK: u64 = 64;

/// A file a start found with no staged entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) struct Orphan {
    /// The number of the file.
    pub id: u64,
    /// When the start found it, in milliseconds since the Unix epoch.
    pub found_ms: u64,
}

/// What a sweep removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::server) struct SweepReport {
    /// Staged files past the stage TTL.
    pub parts: u64,
    /// Committed names past the committed TTL.
    pub committed: u64,
    /// Files a start found with no entry.
    pub orphans: u64,
}

/// Milliseconds of `duration`, at most `u64::MAX`.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl StagedStore {
    /// Removes what is due (see the module) and says how much of each.
    pub(in crate::server) fn sweep(&self) -> SweepReport {
        let now = self.clock.now();
        let stage_ttl = millis(self.settings.stage_ttl);
        let committed_ttl = millis(self.settings.committed_ttl);
        let mut report = SweepReport::default();
        let mut table = locked(&self.table);
        let due: Vec<u64> = table
            .entries
            .iter()
            .filter(|(id, entry)| {
                let ttl = entry.committed.map_or(stage_ttl, |_| committed_ttl);
                now.saturating_sub(entry.ms) >= ttl && !table.uploading.contains_key(id)
            })
            .map(|(id, _)| *id)
            .collect();
        let mut files = Vec::new();
        for id in due {
            let entry = table.take(id);
            table.note(&Record::Remove { id });
            match entry.and_then(|entry| entry.committed) {
                Some(_) => report.committed += 1,
                None => {
                    report.parts += 1;
                    files.push(id);
                }
            }
        }
        let grace = millis(ORPHAN_GRACE);
        let (old, young): (Vec<Orphan>, Vec<Orphan>) = table
            .orphans
            .iter()
            .partition(|orphan| now.saturating_sub(orphan.found_ms) >= grace);
        table.orphans = young;
        report.orphans = old.len() as u64;
        files.extend(old.iter().map(|orphan| orphan.id));
        let live = table.entries.len() as u64;
        if table.journal.lines() > 2 * live + COMPACT_SLACK {
            match Journal::rewrite(&self.stage, &live_records(&table.entries)) {
                Ok(journal) => table.journal = journal,
                Err(error) => tracing::warn!(
                    "tierstore fs: the put journal in {} is not rewritten: {error}",
                    self.stage.display()
                ),
            }
        }
        drop(table);
        for id in files {
            self.remove_stage_file(id);
        }
        report
    }
}
