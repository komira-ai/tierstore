//! [`EntryJournal`]: the journal and snapshot of one entry, and the order of its writes
//! (section 8).

use std::collections::BTreeSet;
use std::io;
use std::sync::{Mutex, MutexGuard, PoisonError};

use tierstore_fetch_api::cell::{CellByte, CellState};
use tierstore_fetch_api::entry::{CommitReport, EntryError, EntrySpec, PunchReport, Settle};
use tierstore_fetch_api::ids::{CellRun, PackId};
use tierstore_fetch_api::page::CELL_SHIFT;

use crate::boot::{BootId, same_boot};
use crate::cells::{CellSet, CellStates, runs_of};
use crate::files::{EntryFiles, FileKind};
use crate::format::{
    FormatError, HEADER_LEN, JournalHeader, JournalImage, RECORD_LEN, Record, RecordKind, Snapshot,
    cell_count,
};

/// The journal is folded into a new snapshot once this many records were appended since the
/// last fold (section 8: "every 64 records"). Constant for both deployment profiles: it bounds
/// the replay of one entry, not a device's throughput.
pub const FOLD_EVERY: u32 = 64;

/// Whether the cells an open restored were written in this boot (section 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// The snapshot and the journal replayed were written in this boot (a process crash, or a
    /// clean close): committed cells are restored `present`, durable, with no re-hash.
    SameBoot,
    /// Another boot, or no boot id on either side (a power loss, a kernel crash): every restored
    /// cell is `recheck`, which the mount does not serve until a re-hash flips it.
    Recheck,
}

/// Why an open restored nothing from an entry's files and started it empty (every cell
/// `absent`, the data file punched whole). None is an error: the cells are fetched again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscardReason {
    /// The snapshot could not be read as one: torn, flipped, another format or version.
    Snapshot(FormatError),
    /// The snapshot is another pack's, or of another size.
    Identity,
    /// The journal follows a later snapshot than the one on disk (the rename of that snapshot
    /// did not reach the disk while the journal's reset did; the ordering of
    /// [`EntryJournal::fold`] rules it out short of a device that reorders a directory fsync).
    JournalAhead {
        /// The journal's epoch.
        journal: u64,
        /// The snapshot's epoch.
        snapshot: u64,
    },
    /// A record of a kind this release does not read (written by a later release): what it did
    /// to the cells is unknown, so none of them is trusted.
    UnknownRecord(u8),
    /// A record whose run leaves the pack.
    RunOutsidePack(CellRun),
}

/// What [`EntryJournal::open`] restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovery {
    /// The byte of every cell, to initialise the state page with: `present + durable` for a cell
    /// committed in this boot, `recheck` for one whose bytes may not be what was committed,
    /// `absent` for every other.
    pub cells: Vec<CellByte>,
    /// The last use the snapshot recorded (0 for a new entry).
    pub last_use: u64,
    /// Whether the restored cells were trusted: from the boot ids of the snapshot and of the
    /// journal replayed. `SameBoot` when nothing was replayed (a new or a discarded entry).
    pub trust: Trust,
    /// No snapshot existed: a new entry (or one whose creation did not reach the disk).
    pub fresh: bool,
    /// Why the files were discarded, if they were.
    pub discarded: Option<DiscardReason>,
    /// The journal records replayed.
    pub records: u32,
    /// The bytes of the journal after its last good record: a torn append, or a stale tail.
    pub tail_bytes: u64,
    /// Whether the open folded the state into a new snapshot and journal (every open but that of
    /// an entry closed cleanly in this boot).
    pub folded: bool,
}

impl Recovery {
    /// The number of cells restored in `state`.
    #[must_use]
    pub fn count(&self, state: CellState) -> u32 {
        // One byte per cell of a pack of at most u32::MAX cells.
        self.cells
            .iter()
            .filter(|b| b.state() == Some(state))
            .count() as u32
    }
}

#[derive(Debug, Default)]
struct Pending {
    /// Cells published (or re-hashed `present`) since the last commit took the set.
    added: BTreeSet<u32>,
    /// Cells whose re-hash failed: no longer to be rechecked after a restart.
    dropped: BTreeSet<u32>,
}

#[derive(Debug)]
struct Log<F> {
    files: F,
    pack: PackId,
    size: u64,
    cells: u32,
    boot: Option<BootId>,
    /// The epoch of the snapshot on disk and of the journal after it.
    epoch: u64,
    /// The journal's length: where the next record goes.
    len: u64,
    /// Whether the journal's name is known to be in the directory durably.
    journal_named: bool,
    /// Records appended since the last fold.
    since_fold: u32,
    /// Cells committed in this boot (what replay would restore `present`).
    trusted: CellSet,
    /// Cells whose bytes are on disk but must be re-hashed (what replay would restore `recheck`).
    untrusted: CellSet,
    /// An I/O error of the journal left its state on disk unknown: no further commit, punch or
    /// fold, so nothing claims a durability the disk may not have. The entry is reopened.
    failed: bool,
}

/// The journal and snapshot of one entry.
///
/// The entry calls [`EntryJournal::published`] after each `claimed -> present`, and its
/// `Entry::commit_batch` and `Entry::punch` call [`EntryJournal::commit_batch`] and
/// [`EntryJournal::punch`] and forward the reports. The journal keeps, beside the files, the
/// cells a replay would restore, so a fold writes them without reading the state page.
///
/// **Locks.** One lock orders the journal's I/O (commit, punch, fold); it is never taken by
/// [`EntryJournal::published`] or [`EntryJournal::settled`], which take only the short lock of
/// the pending set and do no I/O, so a fill or a FUSE request never waits on the journal's I/O.
///
/// **Failure.** An I/O error of the data file's sync, the journal's append or sync, or a fold
/// fails the call with [`EntryError::Io`] and leaves the journal failed: every later commit,
/// punch and fold fails too (after a failed `fsync` the kernel may have dropped the dirty pages,
/// so a retry would claim a durability the disk does not have). The store drops and reopens the
/// entry, whose open replays what reached the disk. A failed `fallocate` of a punch fails that
/// call only: the `PUNCH` records are durable, so the cells are not restored, and their blocks
/// leak until the cells are written again.
#[derive(Debug)]
pub struct EntryJournal<F> {
    log: Mutex<Log<F>>,
    pending: Mutex<Pending>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Each field is updated only after its I/O succeeded, so a panic while a lock was held
    // leaves state that is still usable; it is used as it is.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn io_error(pack: &PackId, op: &'static str, source: io::Error) -> EntryError {
    EntryError::Io {
        pack: pack.clone(),
        op,
        source,
    }
}

/// The byte range of `run` in a pack of `size` bytes (the last cell may be short).
fn byte_range(run: CellRun, size: u64) -> (u64, u64) {
    let start = (u64::from(run.first) << CELL_SHIFT).min(size);
    let end = ((u64::from(run.first) + u64::from(run.count)) << CELL_SHIFT).min(size);
    (start, end - start)
}

impl<F: EntryFiles> Log<F> {
    fn io(&mut self, op: &'static str, source: io::Error) -> EntryError {
        self.failed = true;
        io_error(&self.pack, op, source)
    }

    fn check_ok(&self) -> Result<(), EntryError> {
        if self.failed {
            return Err(io_error(
                &self.pack,
                "journal",
                io::Error::other(
                    "an earlier I/O error left the journal unusable; reopen the entry",
                ),
            ));
        }
        Ok(())
    }

    fn apply_dropped(&mut self, dropped: BTreeSet<u32>) {
        for c in dropped {
            self.untrusted.remove(c);
        }
    }

    /// Appends one record per run and syncs the journal once.
    fn append(&mut self, kind: RecordKind, runs: &[CellRun]) -> Result<(), EntryError> {
        let mut bytes = Vec::with_capacity(runs.len() * RECORD_LEN);
        for run in runs {
            bytes.extend_from_slice(&Record { kind, run: *run }.encode(self.epoch));
        }
        if let Err(e) = self.files.write_at(FileKind::Journal, self.len, &bytes) {
            return Err(self.io("append to the journal", e));
        }
        if let Err(e) = self.files.sync_data(FileKind::Journal) {
            return Err(self.io("fdatasync the journal", e));
        }
        self.len += bytes.len() as u64;
        // A batch holds at most one run per cell of the pack.
        self.since_fold = self.since_fold.saturating_add(runs.len() as u32);
        Ok(())
    }

    /// Writes the snapshot of the next epoch and starts its journal (section 8: write-temp,
    /// fsync, rename, directory fsync; then the journal's reset).
    fn fold(&mut self, epoch: u64, last_use: u64) -> Result<(), EntryError> {
        let snapshot = Snapshot {
            epoch,
            boot: self.boot,
            pack: self.pack.clone(),
            size: self.size,
            last_use,
            present: self.trusted.clone(),
            recheck: self.untrusted.clone(),
        };
        let header = JournalHeader {
            epoch,
            boot: self.boot,
        }
        .encode();
        if let Err((op, e)) = write_fold(
            &self.files,
            &snapshot.encode(),
            &header,
            !self.journal_named,
        ) {
            return Err(self.io(op, e));
        }
        self.journal_named = true;
        self.epoch = epoch;
        self.len = HEADER_LEN as u64;
        self.since_fold = 0;
        Ok(())
    }

    /// Folds when [`FOLD_EVERY`] records were appended since the last fold. It runs after the
    /// call's own work is durable, so its failure does not fail that call: it leaves the journal
    /// failed ([`Log::io`]), and the next call reports it.
    fn fold_if_due(&mut self, last_use: u64) {
        if self.since_fold >= FOLD_EVERY {
            // The error is kept as the failed state; this call's records are already durable.
            let _failed = self.fold(self.epoch + 1, last_use).is_err();
        }
    }
}

/// The writes of a fold, in their order: the snapshot by write-temp, `fdatasync`, rename and
/// directory `fsync` (so the rename is durable before the journal is reset: a reset journal
/// never follows an older snapshot), then the journal's reset to `header` and its `fdatasync`,
/// and a directory `fsync` again when the journal's name is new. The failing step's name and
/// error.
fn write_fold<F: EntryFiles>(
    files: &F,
    snapshot: &[u8],
    header: &[u8; HEADER_LEN],
    name_journal: bool,
) -> Result<(), (&'static str, io::Error)> {
    let step = |op: &'static str, r: io::Result<()>| r.map_err(|e| (op, e));
    step(
        "truncate the snapshot temp",
        files.set_len(FileKind::SnapshotTemp, 0),
    )?;
    step(
        "write the snapshot temp",
        files.write_at(FileKind::SnapshotTemp, 0, snapshot),
    )?;
    step(
        "fdatasync the snapshot temp",
        files.sync_data(FileKind::SnapshotTemp),
    )?;
    step("rename the snapshot", files.replace_snapshot())?;
    step("fsync the entry directory", files.sync_dir())?;
    step("truncate the journal", files.set_len(FileKind::Journal, 0))?;
    step(
        "write the journal header",
        files.write_at(FileKind::Journal, 0, header),
    )?;
    step("fdatasync the journal", files.sync_data(FileKind::Journal))?;
    if name_journal {
        step("fsync the entry directory", files.sync_dir())?;
    }
    Ok(())
}

/// What the files of an entry say, before it is decided whether to trust them.
struct Replayed {
    trusted: CellSet,
    untrusted: CellSet,
    last_use: u64,
    trust: Trust,
    records: u32,
    tail_bytes: u64,
    /// The snapshot's epoch, and whether its journal is exactly a clean header of it written in
    /// this boot (then nothing needs folding).
    epoch: u64,
    clean: bool,
}

/// Replays `snapshot` and the journal `image` for an open in boot `boot`.
fn replay(
    snapshot: &Snapshot,
    image: &JournalImage,
    boot: Option<BootId>,
) -> Result<Replayed, DiscardReason> {
    let cells = snapshot.present.cell_count();
    let snap_trusted = same_boot(snapshot.boot, boot);
    let mut trusted = CellSet::new(cells);
    let mut untrusted = snapshot.recheck.clone();
    for c in snapshot.present.iter() {
        if snap_trusted {
            trusted.insert(c);
        } else {
            untrusted.insert(c);
        }
    }
    let mut trust = if snap_trusted {
        Trust::SameBoot
    } else {
        Trust::Recheck
    };
    let mut records: &[Record] = &[];
    let mut clean = false;
    if let Ok(h) = &image.header {
        if h.epoch > snapshot.epoch {
            return Err(DiscardReason::JournalAhead {
                journal: h.epoch,
                snapshot: snapshot.epoch,
            });
        }
        if h.epoch == snapshot.epoch {
            if let Some(kind) = image.unknown {
                return Err(DiscardReason::UnknownRecord(kind));
            }
            records = &image.records;
            clean = snap_trusted
                && h.boot == snapshot.boot
                && records.is_empty()
                && image.tail_len == 0;
        }
        let journal_trusted = same_boot(h.boot, boot);
        if !records.is_empty() && !journal_trusted {
            trust = Trust::Recheck;
        }
        for r in records {
            let end = u64::from(r.run.first) + u64::from(r.run.count);
            if end > u64::from(cells) {
                return Err(DiscardReason::RunOutsidePack(r.run));
            }
            for c in r.run.cells() {
                match r.kind {
                    RecordKind::Add if journal_trusted => {
                        trusted.insert(c);
                        untrusted.remove(c);
                    }
                    RecordKind::Add => {
                        untrusted.insert(c);
                        trusted.remove(c);
                    }
                    RecordKind::Punch => {
                        trusted.remove(c);
                        untrusted.remove(c);
                    }
                }
            }
        }
    }
    Ok(Replayed {
        trusted,
        untrusted,
        last_use: snapshot.last_use,
        trust,
        // At most one record per 16 bytes of a journal folded every 64 records.
        records: records.len() as u32,
        tail_bytes: image.tail_len,
        epoch: snapshot.epoch,
        clean,
    })
}

impl<F: EntryFiles> EntryJournal<F> {
    /// Opens the journal of the entry `spec` in boot `boot` (`None` when the boot id could not be
    /// read): loads the snapshot, replays the journal, and returns the byte of every cell to start
    /// the state page with. The entry's data file exists at the pack's size (the store creates
    /// it, makes its name durable, and `ftruncate`s it to the size at every open: a power loss
    /// can lose an `ftruncate` that was not synced).
    ///
    /// A new entry (no snapshot) starts empty. Files that cannot be trusted
    /// ([`DiscardReason`]) start the entry empty too, with the data file punched whole. Unless
    /// the entry was closed cleanly in this boot, the state is then folded into a new snapshot
    /// and journal of the next epoch, written in this boot: a torn tail is dropped, and cells
    /// another boot wrote stay `recheck` in the new snapshot.
    pub fn open(
        files: F,
        spec: &EntrySpec,
        boot: Option<BootId>,
    ) -> Result<(Self, Recovery), EntryError> {
        let cells = cell_count(spec.size).ok_or_else(|| EntryError::TooLarge {
            pack: spec.pack.clone(),
            size: spec.size,
        })?;
        let snapshot_bytes = files
            .read(FileKind::Snapshot)
            .map_err(|e| io_error(&spec.pack, "read the snapshot", e))?;
        let journal_bytes = files
            .read(FileKind::Journal)
            .map_err(|e| io_error(&spec.pack, "read the journal", e))?;
        // A journal file that exists does not prove its name is durable (a process killed between
        // its creation and the fold's directory sync leaves it visible): the first fold, or the
        // clean open below, syncs the directory once.
        let journal_named = false;
        let image = JournalImage::decode(journal_bytes.as_deref().unwrap_or_default());
        let header_epoch = image.header.as_ref().ok().map(|h| h.epoch);

        let mut fresh = false;
        let outcome = match snapshot_bytes {
            None => {
                fresh = true;
                Err(None)
            }
            Some(bytes) => match Snapshot::decode(&bytes) {
                Err(e) => Err(Some((DiscardReason::Snapshot(e), None))),
                Ok(s) if s.pack != spec.pack || s.size != spec.size => {
                    Err(Some((DiscardReason::Identity, Some(s.epoch))))
                }
                Ok(s) => replay(&s, &image, boot).map_err(|r| Some((r, Some(s.epoch)))),
            },
        };
        let mut log = Log {
            files,
            pack: spec.pack.clone(),
            size: spec.size,
            cells,
            boot,
            epoch: 0,
            len: 0,
            journal_named,
            since_fold: 0,
            trusted: CellSet::new(cells),
            untrusted: CellSet::new(cells),
            failed: false,
        };
        let mut recovery = Recovery {
            cells: Vec::new(),
            last_use: 0,
            trust: Trust::SameBoot,
            fresh,
            discarded: None,
            records: 0,
            tail_bytes: image.tail_len,
            folded: true,
        };
        match outcome {
            Ok(r) => {
                log.trusted = r.trusted;
                log.untrusted = r.untrusted;
                log.epoch = r.epoch;
                recovery.last_use = r.last_use;
                recovery.trust = r.trust;
                recovery.records = r.records;
                recovery.tail_bytes = r.tail_bytes;
                if r.clean {
                    log.len = HEADER_LEN as u64;
                    recovery.folded = false;
                    if let Err(e) = log.files.sync_dir() {
                        return Err(io_error(&spec.pack, "fsync the entry directory", e));
                    }
                    log.journal_named = true;
                } else {
                    log.fold(r.epoch + 1, r.last_use)?;
                }
            }
            Err(discard) => {
                // A new epoch above every one on disk, so no record of an older journal can
                // carry a crc seeded with it.
                let known = discard.as_ref().and_then(|(_, e)| *e).max(header_epoch);
                let epoch = known.map_or(0, |e| e + 1);
                if let Some((reason, _)) = discard {
                    if let Err(e) = log.files.punch_data(0, spec.size) {
                        return Err(io_error(&spec.pack, "punch the discarded data file", e));
                    }
                    recovery.discarded = Some(reason);
                }
                log.fold(epoch, 0)?;
            }
        }
        recovery.cells = (0..cells)
            .map(|c| {
                if log.trusted.contains(c) {
                    CellByte::new(CellState::Present, true)
                } else if log.untrusted.contains(c) {
                    CellByte::new(CellState::Recheck, false)
                } else {
                    CellByte::new(CellState::Absent, false)
                }
            })
            .collect();
        let journal = Self {
            log: Mutex::new(log),
            pending: Mutex::new(Pending::default()),
        };
        Ok((journal, recovery))
    }

    /// `cell` went `claimed -> present` (its bytes are written), or an eviction of a cell that was
    /// not durable was aborted back to `present`: the next commit makes it durable. No I/O, and
    /// not the journal's lock. (The node's [`crate::Committer`] is told too, with
    /// [`crate::Committer::published`], so it visits the entry.)
    pub fn published(&self, cell: u32) {
        lock(&self.pending).added.insert(cell);
    }

    /// A `recheck` (or `bad`) cell settled ([`tierstore_fetch_api::entry::Entry::settle`] won).
    /// `RecheckOk`: the bytes on disk hash right, so the next commit journals the cell as
    /// committed in this boot. `RecheckFailed`: the cell is `absent` and is not rechecked after
    /// a restart. `ExpireBad` changes nothing the journal holds. No I/O.
    pub fn settled(&self, cell: u32, how: Settle) {
        let mut pending = lock(&self.pending);
        match how {
            Settle::RecheckOk => {
                pending.added.insert(cell);
            }
            Settle::RecheckFailed => {
                pending.dropped.insert(cell);
            }
            Settle::ExpireBad => {}
        }
    }

    /// Makes the cells published since the last commit durable (section 8; `Entry::commit_batch`):
    /// for the cells still `present` and not durable, one `fdatasync` of the data file, then the
    /// `durable` bit on each ([`CellStates::mark_durable`]), then one `ADD` record per contiguous
    /// run, then one `fdatasync` of the journal. A cell that is still `present` or `evicting` but
    /// was not marked durable (an evictor won it, and may abort back to `present`) stays pending
    /// for the next commit; any other cell is dropped (it was punched or failed). Nothing to
    /// commit is a report of zeros, with no sync. Folds when due; a failed fold leaves the journal
    /// failed but does not fail this commit, whose records are durable.
    pub fn commit_batch(&self, cells: &dyn CellStates) -> Result<CommitReport, EntryError> {
        let mut log = lock(&self.log);
        log.check_ok()?;
        let taken = std::mem::take(&mut *lock(&self.pending));
        log.apply_dropped(taken.dropped);
        let mut candidates = Vec::new();
        let mut requeue = Vec::new();
        for c in taken.added {
            match cells.load(c) {
                Some(b) if b.durable() => {}
                Some(b) if b.state() == Some(CellState::Present) => candidates.push(c),
                Some(b) if b.state() == Some(CellState::Evicting) => requeue.push(c),
                _ => {}
            }
        }
        let mut report = CommitReport::default();
        let mut durable = Vec::new();
        if !candidates.is_empty() {
            if let Err(e) = log.files.sync_data(FileKind::Data) {
                return Err(log.io("fdatasync the data file", e));
            }
            report.data_syncs = 1;
            for c in candidates {
                if cells.mark_durable(c) {
                    durable.push(c);
                } else if cells.load(c).is_some_and(|b| {
                    !b.durable()
                        && matches!(b.state(), Some(CellState::Present | CellState::Evicting))
                }) {
                    requeue.push(c);
                }
            }
        }
        lock(&self.pending).added.extend(requeue);
        if durable.is_empty() {
            return Ok(report);
        }
        let runs = runs_of(&durable);
        log.append(RecordKind::Add, &runs)?;
        for &c in &durable {
            log.trusted.insert(c);
            log.untrusted.remove(c);
        }
        // At most one cell per u32 of the pack.
        report.cells_durable = durable.len() as u32;
        report.records = runs.len() as u32;
        log.fold_if_due(cells.last_use());
        Ok(report)
    }

    /// Releases the bytes of `runs`, every cell of which is `evicting` (5.1 step 4;
    /// `Entry::punch`): one `PUNCH` record per non-empty run and **one** `fdatasync` of the
    /// journal for the whole call, then `fallocate(PUNCH_HOLE | KEEP_SIZE)` of each run. The
    /// cells' state is not changed (the caller then CASes each `evicting -> absent`). A cell
    /// outside the pack ([`EntryError::CellOutOfRange`]) or not `evicting`
    /// ([`EntryError::NotEvicting`]) fails the call before any effect.
    ///
    /// Any other error may come after the `PUNCH` records reached the journal or after some runs
    /// were released, so the caller then finishes the cells (`evicting -> absent`) and never
    /// aborts them back to `present` (their bytes may be zeros). A failed `fallocate` leaves the
    /// journal usable (its records are durable; the run's blocks leak until the cells are written
    /// again). Folds when due; a failed fold leaves the journal failed but does not fail this
    /// punch, whose work is done.
    pub fn punch(
        &self,
        cells: &dyn CellStates,
        runs: &[CellRun],
    ) -> Result<PunchReport, EntryError> {
        let mut log = lock(&self.log);
        log.check_ok()?;
        for cell in runs.iter().flat_map(|r| r.cells()) {
            match cells.load(cell) {
                None => {
                    return Err(EntryError::CellOutOfRange {
                        pack: log.pack.clone(),
                        cell,
                        cells: log.cells,
                    });
                }
                Some(b) if b.state() != Some(CellState::Evicting) => {
                    return Err(EntryError::NotEvicting {
                        pack: log.pack.clone(),
                        cell,
                    });
                }
                Some(_) => {}
            }
        }
        let runs: Vec<CellRun> = runs.iter().copied().filter(|r| r.count > 0).collect();
        if runs.is_empty() {
            return Ok(PunchReport::default());
        }
        log.append(RecordKind::Punch, &runs)?;
        for cell in runs.iter().flat_map(|r| r.cells()) {
            log.trusted.remove(cell);
            log.untrusted.remove(cell);
        }
        let mut report = PunchReport {
            journal_syncs: 1,
            records: runs.len() as u32,
            bytes_punched: 0,
        };
        for run in &runs {
            let (offset, len) = byte_range(*run, log.size);
            log.files
                .punch_data(offset, len)
                .map_err(|e| io_error(&log.pack, "punch the data file", e))?;
            report.bytes_punched += len;
        }
        log.fold_if_due(cells.last_use());
        Ok(report)
    }

    /// Folds the journal into a new snapshot with `last_use` (section 8: at entry close and
    /// after an eviction batch; [`EntryJournal::commit_batch`] and [`EntryJournal::punch`] fold
    /// by themselves every [`FOLD_EVERY`] records). Cells published but not committed are not in
    /// it: they stay pending for the next commit.
    pub fn fold(&self, last_use: u64) -> Result<(), EntryError> {
        let mut log = lock(&self.log);
        log.check_ok()?;
        let dropped = std::mem::take(&mut lock(&self.pending).dropped);
        log.apply_dropped(dropped);
        let next = log.epoch + 1;
        log.fold(next, last_use)
    }

    /// The cells a replay of what is on disk would restore `present` in this boot.
    #[must_use]
    pub fn committed(&self) -> CellSet {
        lock(&self.log).trusted.clone()
    }

    /// The cells a replay of what is on disk would restore `recheck`.
    #[must_use]
    pub fn to_recheck(&self) -> CellSet {
        lock(&self.log).untrusted.clone()
    }

    /// The epoch of the snapshot on disk.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        lock(&self.log).epoch
    }

    /// The cells published and not yet committed.
    #[must_use]
    pub fn pending(&self) -> usize {
        lock(&self.pending).added.len()
    }

    /// Whether an I/O error left the journal unusable.
    #[must_use]
    pub fn failed(&self) -> bool {
        lock(&self.log).failed
    }
}
