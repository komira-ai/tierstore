//! The rewrite of the journal with the live entries alone, which a sweep makes once the journal has
//! more than twice as many lines as live entries, plus [`COMPACT_SLACK`]. The file work (the
//! write and the syncs of `journal.new`) is done outside the table's lock, so that no request of
//! the put directory waits for it:
//!
//! 1. Under the lock: the live records are snapshotted, and the table starts keeping every record
//!    appended from then on ([`Table::captured`]).
//! 2. Without the lock: the snapshot is written to `journal.new` and synced. Requests go on and
//!    append to `journal` as before; `journal` is the journal a start reads until the rename.
//! 3. Under the lock: the records appended meanwhile are appended to `journal.new`, which is renamed
//!    over `journal`, and the table appends to it from then on. Each is one `write(2)` without
//!    `fsync`, as every append is, and a rename.
//! 4. Without the lock: the directory is synced.
//!
//! A crash before step 3's rename leaves `journal` as it was (and a `journal.new` that a start
//! removes); after it, `journal` holds the snapshot and then the records in the order they were
//! appended, which is the order per staged file that a start replays. A failure at any step leaves
//! `journal` as it was, still appended to, and is said.

use super::journal::{Journal, sync_dir};
use super::replay::live_records;
use super::store::StagedStore;
use super::sweep::COMPACT_SLACK;
use crate::server::treefs::locked;

impl StagedStore {
    /// Rewrites the journal if it has more than twice the live entries' lines plus
    /// [`COMPACT_SLACK`]; a failure is said and the journal stays.
    pub(super) fn compact_journal(&self) {
        let _rewriting = locked(&self.rewriting);
        let snapshot = {
            let mut table = locked(&self.table);
            let live = table.entries.len() as u64;
            if table.journal.lines() <= 2 * live + COMPACT_SLACK {
                return;
            }
            table.captured = Some(Vec::new());
            live_records(&table.entries)
        };
        if let Err(error) = self.swap_journal(&snapshot) {
            locked(&self.table).captured = None;
            let _ = std::fs::remove_file(self.stage.join(super::journal::NEW_FILE));
            tracing::warn!(
                "tierstore fs: the put journal in {} is not rewritten: {error}",
                self.stage.display()
            );
        }
    }

    /// Writes `snapshot` to the journal's new file outside the table's lock, and under it puts
    /// that file, with the records appended meanwhile, in place of the journal.
    fn swap_journal(&self, snapshot: &[super::journal::Record]) -> std::io::Result<()> {
        let mut new = Journal::begin_rewrite(&self.stage, snapshot)?;
        #[cfg(test)]
        {
            let hook = locked(&self.while_rewriting).clone();
            if let Some(hook) = hook {
                hook();
            }
        }
        {
            let mut table = locked(&self.table);
            let captured = table.captured.take().unwrap_or_default();
            new.finish_rewrite(&captured)?;
            table.journal = new;
        }
        sync_dir(&self.stage)
    }
}
