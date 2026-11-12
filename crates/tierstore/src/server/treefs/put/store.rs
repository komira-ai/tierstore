//! The staged-file store: the names of the put directory, the staged files under them, and the
//! calls a frontend makes on them (see the module above).
//!
//! One lock, `table`, holds the names, the numbers of the files, the open handles, the use of each
//! directory and the journal. Each staged file has a lock of its own, `body`, over its file and its
//! inline hash, held for the whole of a write (so the write and the hash agree) and for the hash at
//! a commit. A thread that holds a body may take the table; no thread takes a body while it holds
//! the table.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustix::fs::{AtFlags, Mode, OFlags};

use super::error::PutError;
use super::hasher::InlineHash;
use super::journal::{Journal, Record};
use super::names::{PutDir, Sha256Digest, check_component, is_part};
use super::sweep::Orphan;
use crate::Clock;
use crate::server::local_cache::Disk;
use crate::server::treefs::locked;

pub(in crate::server) use super::commit::Commit;

/// The ending of the name of a staged file in the stage directory.
const STAGE_SUFFIX: &str = ".stage";

/// The settings of the put directory (`fs.put.*`). A limit that is `None` is not applied.
#[derive(Clone, Debug)]
pub(in crate::server) struct PutSettings {
    /// The key prefixes a job may write under, each ending in `/` (`fs.put.prefixes`).
    pub prefixes: Vec<String>,
    /// The most bytes of one object (`fs.put.max_object_bytes`).
    pub max_object_bytes: Option<u64>,
    /// The most staged bytes of one directory (`fs.put.max_dir_bytes`).
    pub max_dir_bytes: Option<u64>,
    /// The most names of one directory, staged or committed (`fs.put.max_dir_files`).
    pub max_dir_files: Option<u64>,
    /// The free bytes of the stage's volume below which a create is refused
    /// (`fs.put.min_free_bytes`).
    pub min_free_bytes: Option<u64>,
    /// How long a staged file is kept from its create (`fs.put.stage_ttl_secs`).
    pub stage_ttl: Duration,
    /// How long a committed name is shown from its commit (`fs.put.committed_ttl_secs`).
    pub committed_ttl: Duration,
}

/// An open handle of a staged file, which a create gives and a release ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) struct Handle(pub(super) u64);

/// What a name of the put directory is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) enum Kind {
    /// A `.part` file, staged.
    Staged,
    /// A name committed upstream, with the SHA-256 of its bytes; shown, never read.
    Committed(Sha256Digest),
}

/// The attributes of a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) struct Attr {
    /// The bytes.
    pub size: u64,
    /// When it was made (staged) or committed, in milliseconds since the Unix epoch.
    pub ms: u64,
    /// Staged or committed.
    pub kind: Kind,
}

/// What a rename did.
#[derive(Debug)]
pub(in crate::server) enum Rename<'a> {
    /// A `.part` name has another `.part` name.
    Renamed,
    /// The commit: the caller puts the staged file upstream and finishes it.
    Commit(Commit<'a>),
}

/// A staged file's bytes and their hash.
pub(super) struct Body {
    /// `<id>.stage`, open to read and write.
    pub file: File,
    /// The hash of what was written in order.
    pub hash: InlineHash,
}

/// A name of the put directory.
#[derive(Debug)]
pub(super) struct Entry {
    /// The directory.
    pub dir: String,
    /// The name.
    pub name: String,
    /// When it was made (staged) or committed.
    pub ms: u64,
    /// The bytes.
    pub size: u64,
    /// The SHA-256 of a committed name; none while staged.
    pub committed: Option<Sha256Digest>,
}

/// What the names of one directory use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct DirUse {
    /// The names, staged and committed.
    pub files: u64,
    /// The bytes of its staged files.
    pub bytes: u64,
}

/// What the lock `table` guards.
pub(super) struct Table {
    /// Every name, by the number of its staged file.
    pub entries: BTreeMap<u64, Entry>,
    /// The number of each name, by directory and name.
    pub names: BTreeMap<(String, String), u64>,
    /// The body of each staged file.
    pub bodies: BTreeMap<u64, Arc<Mutex<Body>>>,
    /// The staged file of each open handle.
    pub handles: BTreeMap<u64, u64>,
    /// The key of each staged file whose commit is running.
    pub uploading: BTreeMap<u64, String>,
    /// What each directory uses.
    pub dirs: BTreeMap<String, DirUse>,
    /// The files a start found with no entry, removed ten minutes after it.
    pub orphans: Vec<Orphan>,
    /// The number the next staged file gets.
    pub next_id: u64,
    /// The number the next handle gets.
    pub next_handle: u64,
    /// The journal.
    pub journal: Journal,
}

impl Table {
    /// Adds the name `entry` as the number `id`.
    pub(super) fn insert(&mut self, id: u64, entry: Entry) {
        let use_ = self.dirs.entry(entry.dir.clone()).or_default();
        use_.files += 1;
        if entry.committed.is_none() {
            use_.bytes += entry.size;
        }
        let _ = self
            .names
            .insert((entry.dir.clone(), entry.name.clone()), id);
        let _ = self.entries.insert(id, entry);
    }

    /// Removes the name numbered `id`, its body and its handles, and gives back the entry (none
    /// if there was none). The caller writes the journal and removes the staged file.
    pub(super) fn take(&mut self, id: u64) -> Option<Entry> {
        let entry = self.entries.remove(&id)?;
        let _ = self.names.remove(&(entry.dir.clone(), entry.name.clone()));
        let _ = self.bodies.remove(&id);
        let _ = self.uploading.remove(&id);
        self.handles.retain(|_, of| *of != id);
        let use_ = self.dirs.entry(entry.dir.clone()).or_default();
        use_.files -= 1;
        if entry.committed.is_none() {
            use_.bytes -= entry.size;
        }
        if use_.files == 0 {
            let _ = self.dirs.remove(&entry.dir);
        }
        Some(entry)
    }

    /// The number of the name `name` of `dir`.
    pub(super) fn id_of(&self, dir: &PutDir, name: &str) -> Option<u64> {
        self.names
            .get(&(dir.as_str().to_owned(), name.to_owned()))
            .copied()
    }

    /// Appends `record` to the journal; a failure is said, and the change it records stands.
    pub(super) fn note(&mut self, record: &Record) {
        if let Err(error) = self.journal.append(record) {
            tracing::warn!("tierstore fs: the put journal is not written ({record:?}): {error}");
        }
    }
}

/// The staged files of the put directory, under one stage directory.
pub(in crate::server) struct StagedStore {
    /// The settings.
    pub(super) settings: PutSettings,
    /// The stage directory.
    pub(super) stage: PathBuf,
    /// The stage directory, open, which every staged file is made and removed beneath.
    pub(super) stage_fd: OwnedFd,
    /// The time, for the ages of the names.
    pub(super) clock: Arc<dyn Clock>,
    /// The free space of the stage's volume.
    pub(super) disk: Arc<dyn Disk>,
    /// The names, the handles and the journal.
    pub(super) table: Mutex<Table>,
}

impl std::fmt::Debug for StagedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedStore")
            .field("stage", &self.stage)
            .finish_non_exhaustive()
    }
}

/// The name of the staged file numbered `id` in the stage directory.
pub(super) fn stage_name(id: u64) -> String {
    format!("{id:020}{STAGE_SUFFIX}")
}

/// The number of the staged file called `name`, if it is one: 20 digits and `.stage`.
pub(super) fn stage_id(name: &str) -> Option<u64> {
    let digits = name.strip_suffix(STAGE_SUFFIX)?;
    if digits.len() != 20 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Opens the staged file numbered `id` of the stage directory `stage_fd` to read and write, with
/// `extra` flags: beneath that directory, and following no symbolic link (`openat2` with
/// `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS` on Linux, `O_NOFOLLOW` elsewhere). A new file has the mode
/// 0600.
pub(super) fn open_beneath(stage_fd: &OwnedFd, id: u64, extra: OFlags) -> io::Result<File> {
    let flags = extra | OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    // `openat2` refuses a mode without `O_CREAT`.
    let mode = if extra.contains(OFlags::CREATE) {
        Mode::RUSR | Mode::WUSR
    } else {
        Mode::empty()
    };
    let name = stage_name(id);
    #[cfg(target_os = "linux")]
    let fd = {
        use rustix::fs::ResolveFlags;
        let resolve =
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
        rustix::fs::openat2(stage_fd, name.as_str(), flags, mode, resolve)?
    };
    #[cfg(not(target_os = "linux"))]
    let fd = rustix::fs::openat(stage_fd, name.as_str(), flags, mode)?;
    Ok(File::from(fd))
}

impl StagedStore {
    /// The path of the staged file numbered `id`.
    pub(super) fn stage_path(&self, id: u64) -> PathBuf {
        self.stage.join(stage_name(id))
    }

    /// Makes the staged file numbered `id`, create-only, readable and writable by this process
    /// alone.
    fn make_stage_file(&self, id: u64) -> io::Result<File> {
        open_beneath(&self.stage_fd, id, OFlags::CREATE | OFlags::EXCL)
    }

    /// Removes the staged file numbered `id`; a failure is said and left to the next start.
    pub(super) fn remove_stage_file(&self, id: u64) {
        let name = stage_name(id);
        if let Err(error) = rustix::fs::unlinkat(&self.stage_fd, name.as_str(), AtFlags::empty()) {
            tracing::warn!("tierstore fs: cannot remove the staged file {name}: {error}");
        }
    }

    /// Creates the `.part` file `name` in `dir` (`O_CREAT|O_EXCL`), and gives its handle, open to
    /// write.
    ///
    /// # Errors
    /// - [`PutError::InvalidName`], [`PutError::NameTooLong`]: not a name.
    /// - [`PutError::NotPermitted`]: the name does not end in `.part`.
    /// - [`PutError::Exists`]: the name is staged in `dir`.
    /// - [`PutError::Quota`]: `dir` holds `max_dir_files` names.
    /// - [`PutError::NoSpace`]: the stage's volume has less than `min_free_bytes` free.
    /// - [`PutError::Io`]: the free space, the staged file or the journal could not be had.
    pub(in crate::server) fn create(&self, dir: &PutDir, name: &str) -> Result<Handle, PutError> {
        check_component(name)?;
        let _key = dir.key(name)?;
        if !is_part(name) {
            return Err(PutError::NotPermitted("a new file's name ends in .part"));
        }
        let mut table = locked(&self.table);
        if table.id_of(dir, name).is_some() {
            return Err(PutError::Exists);
        }
        let files = table.dirs.get(dir.as_str()).map_or(0, |use_| use_.files);
        if self.settings.max_dir_files.is_some_and(|max| files >= max) {
            return Err(PutError::Quota("files"));
        }
        if let Some(min) = self.settings.min_free_bytes {
            let free = self.disk.space(&self.stage)?.free;
            if free < min {
                return Err(PutError::NoSpace { free, min });
            }
        }
        let id = table.next_id;
        let file = self.make_stage_file(id)?;
        let ms = self.clock.now();
        let record = Record::Create {
            id,
            dir: dir.as_str().to_owned(),
            name: name.to_owned(),
            ms,
        };
        if let Err(error) = table.journal.append(&record) {
            drop(file);
            self.remove_stage_file(id);
            return Err(error.into());
        }
        table.next_id += 1;
        let handle = table.next_handle;
        table.next_handle += 1;
        let _ = table.handles.insert(handle, id);
        let body = Body {
            file,
            hash: InlineHash::new(),
        };
        let _ = table.bodies.insert(id, Arc::new(Mutex::new(body)));
        table.insert(
            id,
            Entry {
                dir: dir.as_str().to_owned(),
                name: name.to_owned(),
                ms,
                size: 0,
                committed: None,
            },
        );
        Ok(Handle(handle))
    }

    /// Writes `bytes` at `offset` of the file of `handle`, and gives the number written (all).
    ///
    /// # Errors
    /// - [`PutError::BadHandle`]: the handle is not open, or its file was removed.
    /// - [`PutError::TooLarge`]: the file would be over `max_object_bytes`.
    /// - [`PutError::Quota`]: its directory would be over `max_dir_bytes`.
    /// - [`PutError::Busy`]: a commit of the file is running.
    /// - [`PutError::Io`]: the write failed; the digest is then computed at commit.
    pub(in crate::server) fn write(
        &self,
        handle: Handle,
        offset: u64,
        bytes: &[u8],
    ) -> Result<usize, PutError> {
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or(PutError::TooLarge)?;
        let (id, body) = {
            let table = locked(&self.table);
            let id = *table.handles.get(&handle.0).ok_or(PutError::BadHandle)?;
            let body = table.bodies.get(&id).ok_or(PutError::BadHandle)?;
            (id, Arc::clone(body))
        };
        let mut body = locked(&body);
        let growth = self.reserve(id, end)?;
        if let Err(error) = body.file.write_all_at(bytes, offset) {
            body.hash = InlineHash::unknown();
            self.unreserve(id, growth);
            return Err(error.into());
        }
        body.hash.wrote(offset, bytes);
        Ok(bytes.len())
    }

    /// Makes the staged file numbered `id` at least `end` bytes long in the table, under the
    /// limits, and gives the bytes it grew by. The caller holds its body.
    pub(super) fn reserve(&self, id: u64, end: u64) -> Result<u64, PutError> {
        let mut table = locked(&self.table);
        if table.uploading.contains_key(&id) {
            return Err(PutError::Busy("a commit of the file is running"));
        }
        let Table { entries, dirs, .. } = &mut *table;
        let entry = entries.get_mut(&id).ok_or(PutError::BadHandle)?;
        let growth = end.saturating_sub(entry.size);
        if growth == 0 {
            return Ok(0);
        }
        if self.settings.max_object_bytes.is_some_and(|max| end > max) {
            return Err(PutError::TooLarge);
        }
        let use_ = dirs.entry(entry.dir.clone()).or_default();
        let over = self
            .settings
            .max_dir_bytes
            .is_some_and(|max| use_.bytes.saturating_add(growth) > max);
        if over {
            return Err(PutError::Quota("bytes"));
        }
        use_.bytes += growth;
        entry.size = end;
        Ok(growth)
    }

    /// Gives back `growth` bytes [`reserve`](Self::reserve) took for the staged file numbered `id`
    /// for a write that failed. The caller holds its body.
    pub(super) fn unreserve(&self, id: u64, growth: u64) {
        let mut table = locked(&self.table);
        let Table { entries, dirs, .. } = &mut *table;
        if let Some(entry) = entries.get_mut(&id) {
            entry.size -= growth;
            dirs.entry(entry.dir.clone()).or_default().bytes -= growth;
        }
    }

    /// Ends `handle`. Ending one that is not open does nothing.
    pub(in crate::server) fn release(&self, handle: Handle) {
        let _ = locked(&self.table).handles.remove(&handle.0);
    }

    /// Renames the `.part` file `src` of `dir` to `dst` in the same directory: to another `.part`
    /// name, a change of the name alone; to any other name, the commit, which the caller finishes
    /// with the answer of the upload ([`Commit::finish`]).
    ///
    /// # Errors
    /// - [`PutError::InvalidName`], [`PutError::NameTooLong`]: not a name, or a key too long.
    /// - [`PutError::NotPermitted`]: `src` is not a `.part` name.
    /// - [`PutError::NotFound`]: no `src` is staged in `dir`.
    /// - [`PutError::Exists`]: the `.part` name `dst` is staged.
    /// - [`PutError::Busy`]: a commit of `src`, or of the key of `dst`, is running, or (for a
    ///   commit) a handle of `src` is open.
    /// - [`PutError::DigestMismatch`]: the bytes do not hash to the digest `src` names.
    /// - [`PutError::TooLarge`]: the file is over `max_object_bytes`.
    /// - [`PutError::Io`]: the journal could not be written, or the file read.
    pub(in crate::server) fn rename(
        &self,
        dir: &PutDir,
        src: &str,
        dst: &str,
    ) -> Result<Rename<'_>, PutError> {
        check_component(src)?;
        check_component(dst)?;
        let key = dir.key(dst)?;
        if !is_part(src) {
            return Err(PutError::NotPermitted("only a .part name is renamed"));
        }
        if is_part(dst) {
            self.rename_part(dir, src, dst)?;
            return Ok(Rename::Renamed);
        }
        self.begin_commit(dir, src, dst, key).map(Rename::Commit)
    }

    /// Gives the staged file `src` of `dir` the `.part` name `dst`.
    fn rename_part(&self, dir: &PutDir, src: &str, dst: &str) -> Result<(), PutError> {
        let mut table = locked(&self.table);
        let id = table.id_of(dir, src).ok_or(PutError::NotFound)?;
        if src == dst {
            return Ok(());
        }
        if table.id_of(dir, dst).is_some() {
            return Err(PutError::Exists);
        }
        if table.uploading.contains_key(&id) {
            return Err(PutError::Busy("a commit of the file is running"));
        }
        let record = Record::Rename {
            id,
            name: dst.to_owned(),
        };
        table.journal.append(&record)?;
        let Table { entries, names, .. } = &mut *table;
        let entry = entries.get_mut(&id).ok_or(PutError::NotFound)?;
        let src = std::mem::replace(&mut entry.name, dst.to_owned());
        let _ = names.remove(&(entry.dir.clone(), src));
        let _ = names.insert((entry.dir.clone(), dst.to_owned()), id);
        Ok(())
    }

    /// Removes the `.part` file `name` of `dir` (the abort).
    ///
    /// # Errors
    /// - [`PutError::InvalidName`], [`PutError::NameTooLong`]: not a name.
    /// - [`PutError::NotPermitted`]: `name` is not a `.part` name.
    /// - [`PutError::NotFound`]: no `name` is staged in `dir`.
    /// - [`PutError::Busy`]: a commit of it is running.
    pub(in crate::server) fn unlink(&self, dir: &PutDir, name: &str) -> Result<(), PutError> {
        check_component(name)?;
        if !is_part(name) {
            return Err(PutError::NotPermitted("only a .part name is removed"));
        }
        let mut table = locked(&self.table);
        let id = table.id_of(dir, name).ok_or(PutError::NotFound)?;
        if table.uploading.contains_key(&id) {
            return Err(PutError::Busy("a commit of the file is running"));
        }
        let _ = table.take(id);
        table.note(&Record::Remove { id });
        drop(table);
        self.remove_stage_file(id);
        Ok(())
    }

    /// The attributes of the name `name` of `dir`, if it has one.
    pub(in crate::server) fn lookup(&self, dir: &PutDir, name: &str) -> Option<Attr> {
        let table = locked(&self.table);
        let id = table.id_of(dir, name)?;
        table.entries.get(&id).map(attr)
    }

    /// The names of `dir` and their attributes, in the order of their bytes.
    pub(in crate::server) fn list(&self, dir: &PutDir) -> Vec<(String, Attr)> {
        let table = locked(&self.table);
        let from = (dir.as_str().to_owned(), String::new());
        table
            .names
            .range(from..)
            .take_while(|((of, _), _)| of == dir.as_str())
            .filter_map(|((_, name), id)| Some((name.clone(), attr(table.entries.get(id)?))))
            .collect()
    }
}

/// The attributes of `entry`.
fn attr(entry: &Entry) -> Attr {
    Attr {
        size: entry.size,
        ms: entry.ms,
        kind: entry.committed.map_or(Kind::Staged, Kind::Committed),
    }
}
