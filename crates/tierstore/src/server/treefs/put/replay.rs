//! The start of the store: the stage directory checked and opened, the journal replayed against the
//! staged files on disk, and the journal rewritten with the live entries alone.
//!
//! - A staged entry whose file is there is staged again, with the file's length as its size and its
//!   digest unknown (the commit reads the file to hash it). No handle is open.
//! - A staged entry whose file is gone is dropped.
//! - A committed entry is shown again until its committed TTL.
//! - A file named like a staged file that has no staged entry (the process stopped between the
//!   file and its line, or a commit's file was not removed) is an orphan: the first sweep at least
//!   ten minutes after the start removes it. Files of other names are left alone.
//! - A line that cannot be read, or that names what cannot be (a name taken, a directory or name
//!   that is not one, a `.part` name committed, a staged entry whose name is final), is skipped
//!   and counted. A create of a final name followed by its commit is how a rewrite writes a
//!   committed entry.
//! - The next staged file is numbered after every number a line of the journal or a file of the
//!   stage names, so no number is used twice.

use std::collections::BTreeMap;
use std::fs::DirBuilder;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::Path;
use std::sync::{Arc, Mutex};

use rustix::fs::{Mode, OFlags};

use super::error::OpenError;
use super::hasher::InlineHash;
use super::journal::{self, Journal, Record};
use super::names::{PutDir, Sha256Digest, check_component, check_prefixes, is_part};
use super::store::{Body, DirUse, Entry, PutSettings, StagedStore, Table, open_beneath, stage_id};
use super::sweep::Orphan;
use crate::Clock;
use crate::server::local_cache::Disk;

/// What a start found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::server) struct Replayed {
    /// Staged files staged again.
    pub staged: u64,
    /// Committed names shown again.
    pub committed: u64,
    /// Staged entries dropped, their file gone.
    pub dropped: u64,
    /// Files with no staged entry, to be removed by a sweep ten minutes on.
    pub orphans: u64,
    /// Lines of the journal skipped.
    pub skipped: u64,
}

/// Fails unless the device numbers of the stage and of the cache are the same, so that a staged
/// file the server is handed can become a file of the cache without a copy (`fs.put.stage_dir` must
/// be on the cache volume).
pub(super) fn same_volume(stage: (&Path, u64), cache: (&Path, u64)) -> Result<(), OpenError> {
    if stage.1 != cache.1 {
        return Err(OpenError::OtherVolume {
            stage: stage.0.to_owned(),
            cache: cache.0.to_owned(),
        });
    }
    Ok(())
}

/// The entries the records of a journal leave, by number, and the lines skipped.
pub(super) fn apply(records: Vec<Record>) -> (BTreeMap<u64, Entry>, u64) {
    let mut entries: BTreeMap<u64, Entry> = BTreeMap::new();
    let mut skipped = 0;
    for record in records {
        if !apply_one(&mut entries, record) {
            skipped += 1;
        }
    }
    // A create's name is final only when a commit follows it (a rewritten committed entry); a
    // staged entry left with a final name is not one a job made.
    let before = entries.len();
    entries.retain(|_, entry| entry.committed.is_some() || is_part(&entry.name));
    skipped += (before - entries.len()) as u64;
    (entries, skipped)
}

/// The number of the entry named `name` in `dir`, if one is.
fn holder(entries: &BTreeMap<u64, Entry>, dir: &str, name: &str) -> Option<u64> {
    entries
        .iter()
        .find(|(_, entry)| entry.dir == dir && entry.name == name)
        .map(|(id, _)| *id)
}

/// Applies one record; false when it is skipped.
fn apply_one(entries: &mut BTreeMap<u64, Entry>, record: Record) -> bool {
    match record {
        Record::Create { id, dir, name, ms } => {
            let good = PutDir::parse(&dir).is_ok() && check_component(&name).is_ok();
            if !good || entries.contains_key(&id) || holder(entries, &dir, &name).is_some() {
                return false;
            }
            let entry = Entry {
                dir,
                name,
                ms,
                size: 0,
                committed: None,
            };
            let _ = entries.insert(id, entry);
            true
        }
        Record::Rename { id, name } => {
            let staged = entries.get(&id).filter(|entry| entry.committed.is_none());
            let Some(dir) = staged.map(|entry| entry.dir.clone()) else {
                return false;
            };
            if check_component(&name).is_err()
                || !is_part(&name)
                || holder(entries, &dir, &name).is_some()
            {
                return false;
            }
            entries.entry(id).and_modify(|entry| entry.name = name);
            true
        }
        Record::Commit {
            id,
            name,
            sha256,
            size,
            ms,
        } => {
            let staged = entries.get(&id).filter(|entry| entry.committed.is_none());
            let digest = Sha256Digest::from_hex(&sha256);
            let (Some(dir), Some(digest)) = (staged.map(|entry| entry.dir.clone()), digest) else {
                return false;
            };
            if check_component(&name).is_err() || is_part(&name) {
                return false;
            }
            if let Some(earlier) = holder(entries, &dir, &name) {
                let _ = entries.remove(&earlier);
            }
            let entry = Entry {
                dir,
                name,
                ms,
                size,
                committed: Some(digest),
            };
            let _ = entries.insert(id, entry);
            true
        }
        Record::Remove { id } => entries.remove(&id).is_some(),
    }
}

/// The records that say the entries `entries` and nothing else.
pub(super) fn live_records(entries: &BTreeMap<u64, Entry>) -> Vec<Record> {
    let mut records = Vec::new();
    for (&id, entry) in entries {
        records.push(Record::Create {
            id,
            dir: entry.dir.clone(),
            name: entry.name.clone(),
            ms: entry.ms,
        });
        if let Some(digest) = entry.committed {
            records.push(Record::Commit {
                id,
                name: entry.name.clone(),
                sha256: digest.to_string(),
                size: entry.size,
                ms: entry.ms,
            });
        }
    }
    records
}

/// Opens the directory `path` to make and remove files beneath it.
fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let directory = OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC;
    Ok(rustix::fs::open(path, directory, Mode::empty())?)
}

impl StagedStore {
    /// Opens the store over the stage directory `stage` (made, mode 0700, if it is not there), which
    /// must be on the volume of the cache directory `cache`, and replays its journal (see the
    /// module).
    ///
    /// # Errors
    /// - [`OpenError::Prefix`]: a prefix of the settings is not a key prefix ending in `/`.
    /// - [`OpenError::OtherVolume`]: the stage is not on the cache's volume.
    /// - [`OpenError::Io`]: the stage, the cache, the journal or a staged file could not be made,
    ///   read or written.
    pub(in crate::server) fn open(
        settings: PutSettings,
        stage: &Path,
        cache: &Path,
        clock: Arc<dyn Clock>,
        disk: Arc<dyn Disk>,
    ) -> Result<(Self, Replayed), OpenError> {
        check_prefixes(&settings.prefixes).map_err(OpenError::Prefix)?;
        let at = |path: &Path| {
            let path = path.to_owned();
            move |error: io::Error| OpenError::Io(path, error)
        };
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(stage)
            .map_err(at(stage))?;
        let stage_dev = std::fs::metadata(stage).map_err(at(stage))?.dev();
        let cache_dev = std::fs::metadata(cache).map_err(at(cache))?.dev();
        same_volume((stage, stage_dev), (cache, cache_dev))?;
        let stage_fd = open_dir(stage).map_err(at(stage))?;
        let leftover = stage.join(journal::NEW_FILE);
        match std::fs::remove_file(&leftover) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(at(&leftover)(error));
            }
            _ => {}
        }
        let bytes = Journal::read(stage).map_err(at(stage))?;
        let (records, unread) = journal::decode(&bytes);
        let most_recorded = records.iter().map(Record::id).max();
        let (entries, unapplied) = apply(records);
        let mut files = BTreeMap::new();
        for found in std::fs::read_dir(stage).map_err(at(stage))? {
            let found = found.map_err(at(stage))?;
            let name = found.file_name();
            if let Some(id) = name.to_str().and_then(stage_id) {
                let _ = files.insert(id, found.metadata().map_err(at(&found.path()))?.len());
            }
        }
        let now = clock.now();
        let mut replayed = Replayed {
            skipped: unread + unapplied,
            ..Replayed::default()
        };
        let most = files.keys().copied().chain(most_recorded).max();
        let mut kept = BTreeMap::new();
        let mut bodies = BTreeMap::new();
        for (id, mut entry) in entries {
            if entry.committed.is_some() {
                replayed.committed += 1;
                let _ = kept.insert(id, entry);
                continue;
            }
            let Some(len) = files.remove(&id) else {
                replayed.dropped += 1;
                continue;
            };
            let file = open_beneath(&stage_fd, id, OFlags::empty())
                .map_err(at(&stage.join(super::store::stage_name(id))))?;
            entry.size = len;
            let body = Body {
                file,
                hash: InlineHash::unknown(),
            };
            let _ = bodies.insert(id, Arc::new(Mutex::new(body)));
            let _ = kept.insert(id, entry);
            replayed.staged += 1;
        }
        let orphans: Vec<Orphan> = files
            .keys()
            .map(|&id| Orphan { id, found_ms: now })
            .collect();
        replayed.orphans = orphans.len() as u64;
        let journal = Journal::rewrite(stage, &live_records(&kept)).map_err(at(stage))?;
        let mut table = Table {
            entries: BTreeMap::new(),
            names: BTreeMap::new(),
            bodies,
            handles: BTreeMap::new(),
            uploading: BTreeMap::new(),
            dirs: BTreeMap::<String, DirUse>::new(),
            orphans,
            next_id: most.map_or(0, |most| most + 1),
            next_handle: 0,
            journal,
        };
        for (id, entry) in kept {
            table.insert(id, entry);
        }
        let store = Self {
            settings,
            stage: stage.to_owned(),
            stage_fd,
            clock,
            disk,
            table: Mutex::new(table),
        };
        Ok((store, replayed))
    }
}
