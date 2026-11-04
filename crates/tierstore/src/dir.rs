//! A [`Backend`] over a directory of the local file system, for a tier that is a disk.
//!
//! A key is the name of one file, percent-encoded: every byte but an ASCII letter or digit
//! (and a few marks) costs three bytes, so a key whose encoded name is empty or over 255 bytes
//! (about 85 bytes of non-ASCII text) cannot be stored. A write that validates the key itself
//! (put, create of an upload, streamed puts) is [`Error::InvalidKey`] (S3 answer 400
//! `KeyTooLongError`); a read, a head, a delete and the held-body conditional write
//! ([`Backend::put_if_match`], which first looks for the stored object) are [`Error::NotFound`],
//! since no object can have it.

use std::io::{ErrorKind, SeekFrom};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::{Buf, Bytes};
use futures::StreamExt;
use futures::stream;
use sha2::{Digest, Sha256};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::backend::upload_page;
use crate::{
    Backend, ByteRange, Error, Listed, Meta, OpenUpload, Opened, PartBody, ReadBody, Span,
    UploadsAfter, Window,
};

#[path = "dir_entries.rs"]
mod entries;
#[path = "dir_list.rs"]
mod listing;
#[path = "dir_names.rs"]
mod names;
#[path = "dir_sync.rs"]
mod sync;
#[path = "dir_tags.rs"]
mod tags;

use names::encode;
use sync::{FsyncDir, SyncDir};
use tags::{finish, modified_of, sha256};

/// The length of the content tag that starts the file of an object: the SHA-256 of its body.
const TAG_LEN: usize = 32;

/// The mode of the file of an object: read and write for its owner alone, so that no other local
/// account reads what a scoped or read-only key is meant to reach.
const PLAIN_MODE: u32 = 0o600;

/// The mode of every directory this backend makes (the tier root, the uploads directory and the
/// directory of an upload): open to its owner alone.
const DIR_MODE: u32 = 0o700;

/// The longest file name, in bytes, that a key may be encoded to.
const NAME_LIMIT: usize = 255;

/// The size of the pieces in which the parts of an upload are joined.
const COPY_CHUNK: usize = 1 << 20;

/// The most bytes of an object that a streamed read ([`Backend::get_stream`]) holds at a time:
/// each chunk it gives is this long, but for the last, which is what is left.
const READ_CHUNK: u64 = 1 << 16;

/// The directory, inside the one of the backend, that holds the uploads in progress.
const UPLOADS: &str = ".uploads";

/// The file of an upload that holds the key it is for.
const KEY_FILE: &str = "key";

/// The number of the backends opened in this process, which tells their temporary files apart.
static OPENED: AtomicU64 = AtomicU64::new(0);

/// A bucket kept in a directory.
///
/// An object is one file of the directory. Its name is the key with `/` written as `~`; letters,
/// digits, `_`, `-` and a `.` (but not as the first byte) stay as they are, and every other
/// byte is written as `%` and two hex digits. So no key can name a file outside the directory
/// or a file the backend keeps for itself (their names start with `.`). A key whose name is
/// empty or longer than 255 bytes is refused with [`Error::InvalidKey`] by the writes that
/// validate the key, and is [`Error::NotFound`] for reads, deletes and the conditional writes
/// that look for the stored object first (see the module doc). The file holds the SHA-256
/// of the body, which is the content tag, and then the body, so that a head reads 32 bytes and
/// not the object.
///
/// - A put writes a temporary file in the directory, syncs it, and hard-links it to the name,
///   which fails where the name is taken: the object appears whole or not at all, and a put of a
///   key that exists leaves it as it was. The directory needs a file system with hard links. A
///   crash leaves a `.tmp-` file behind, which is never listed and can be removed when the
///   process is stopped. The file is synced before it is linked, and the directory is synced
///   after the link, so that the name of an object whose put was acknowledged survives a power
///   loss. A replacement ([`Backend::put_if_match`]), a delete, the placing of a part and the
///   completion of an upload sync the directory they changed the same way, before they return.
///   If the sync after a link fails the name is removed again, so that an object that was not
///   acknowledged is not left visible and a retry can succeed (if the name cannot be removed the
///   error says it may exist without being durable). A replacement or a delete whose sync fails
///   cannot be undone, the old body being gone: the call fails and the change may be visible; a
///   retry of a delete is fine, and the caller of a replacement reads the object to see which
///   body it holds.
/// - A conditional delete ([`Backend::delete_if_match`]) checks the content tag and removes the
///   file with the lock a replacement holds, and a plain delete takes that lock for the removal
///   too, so that a delete and a put of new content that come between the check and the removal
///   of a conditional delete cannot be undone by it. The directory is synced after the lock is
///   released.
/// - A streamed put ([`Backend::put_stream`]) writes the chunks to the temporary file as they
///   arrive, so that the body is never held in memory; the content tag, which starts the file,
///   is written over reserved room after the last chunk, the file is synced, and only then is it
///   linked to the name, with the directory synced after the link as for any put. A body that
///   fails leaves no object and removes the temporary file.
/// - A streamed read ([`Backend::get_stream`]) opens the file once and reads the range in chunks of
///   at most 64 KiB as they are asked for, so that a reader holds one chunk and not the object;
///   a replacement or a delete of the name meanwhile leaves it on the file it opened. A read with
///   the tag ([`Backend::get_tagged`]) takes the bytes and the tag from the same open file.
/// - A streamed replacement ([`Backend::put_stream_if_match`]) writes the chunks to a temporary
///   file as they arrive, outside the lock, and then, under the lock and the tag check of
///   [`Backend::put_if_match`], moves the file over the object and syncs the directory. A body
///   that fails or a tag that does not match leaves the object as it was and removes the file.
/// - A multipart upload is a directory `.uploads/<id>` that holds a file with the key and a file
///   for each part, written as the chunks of the part arrive, so that a part is never held in
///   memory; the completion joins the parts into the new object by streaming them, and removes
///   the upload. The uploads are files, so an upload outlives a restart of the process, and an
///   upload that is never completed or aborted stays until it is aborted.
///
/// - The keys that start with `_entries/` are files of the subdirectory `.entries/` of the root
///   (a name no key encodes to), which the first write of one makes, unless the previous release
///   made the key a file of the root, named like any key: that file is the key for as long as it
///   exists, and is read, listed, replaced and deleted in place, so that the previous release
///   reads the current body (see the module `dir_entries`). Their create-only puts, replacements
///   by the tag and deletes take an exclusive `flock` on `.entries/.lock`, so they are atomic
///   between processes too.
///
/// The directory must be on a case-sensitive file system: a key is a file name with `A-Z` as they
/// are, so on a case-insensitive one (the default APFS volume on macOS) the keys `Foo` and `foo`
/// name one file, a get of one returns the other's bytes and a create-only put of one fails for
/// a key that does not exist. The backend does not probe for this; a file name is not changed
/// because a release already wrote its objects under it.
///
/// The backend counts on being the only writer of its directory but for other backends over the
/// same directory (several proxies over one cold directory are fine: every change is atomic and
/// create-only, and the temporary files of two backends never have one name, since a name holds
/// the process id, the time the backend was opened and its number in the process);
/// [`Backend::put_if_match`] and [`Backend::delete_if_match`] are atomic among the calls of one
/// backend only, but for the keys under `_entries/`, and so is the replacement of a part by
/// another of its number: the parts of one upload should be taken by one backend.
pub struct DirBackend {
    root: PathBuf,
    /// Tells the temporary files of this backend from those of any other backend over the same
    /// directory, in this process or in another.
    nonce: String,
    /// Numbers the temporary files and the uploads.
    counter: AtomicU64,
    /// Held while an object is replaced, or removed, so that two replacements of one backend do
    /// not interleave their check and their write, and a conditional delete's check and removal
    /// are not split by another delete or replacement. A plain delete therefore waits for a
    /// replacement in progress, through its temporary write and its sync.
    replacing: Mutex<()>,
    /// Held while a part is moved to its name and the other files of its number are removed, so
    /// that two overlapping uploads of one part number do not remove each other's file.
    placing: Mutex<()>,
    /// Syncs a directory after a call changed its entries.
    syncer: Arc<dyn SyncDir>,
    /// Set once a sync of the root has succeeded after the subdirectory of `_entries/` was known
    /// to exist, so that the later entry writes do not sync the root again.
    root_synced: AtomicBool,
}

/// The error of a failed call on the file `path`.
fn failed(path: &Path, why: &std::io::Error) -> Error {
    let text = format!("{}: {why}", path.display());
    match why.kind() {
        ErrorKind::StorageFull => Error::Full(text),
        _ => Error::Other(text),
    }
}

/// Makes the error of a failed call on the file `path` from an I/O error.
fn io_error(path: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |why| failed(path, &why)
}

/// The error of a call on `path` that failed with `why`: `found` for the key if `why` is of the
/// kind `expected`, else as [`failed`].
fn classify(
    key: &str,
    path: &Path,
    why: &std::io::Error,
    expected: ErrorKind,
    found: fn(String) -> Error,
) -> Error {
    if why.kind() == expected {
        found(key.to_owned())
    } else {
        failed(path, why)
    }
}

/// Whether the byte can be in the id of an upload, which names a directory.
fn is_id_byte(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'a'..=b'z' | b'-')
}

/// The name of the file of a part of an upload: its number and its content tag.
fn part_name(number: u32, tag: &str) -> String {
    format!("{number:05}-{tag}")
}

/// The number and the content tag a file name of a part has, if it is one.
fn parse_part(name: &str) -> Option<(u32, &str)> {
    let (number, tag) = name.split_once('-')?;
    Some((number.parse().ok()?, tag))
}

/// A temporary file that is removed when the guard is dropped: when the call that made it ends or
/// fails, and when the call is dropped half way, as the server drops the call of a client that
/// went away. The removal is not an error where the file was moved to its name.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Makes the new file at `path`, for writing, with the mode [`PLAIN_MODE`] (less the umask). It is
/// an error if the name is taken, so that two writers never write to one file.
async fn create_new(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PLAIN_MODE)
        .open(path)
        .await
}

/// Writes `data` to a new file at `path`, and syncs it to the disk.
async fn create_file(path: &Path, mut data: impl Buf) -> std::io::Result<()> {
    let mut file = create_new(path).await?;
    let written = file.write_all_buf(&mut data).await;
    let flushed = written.and(file.flush().await);
    flushed.and(file.sync_all().await)
}

/// Writes the chunks of `body` to a new file at `path`, and gives their SHA-256.
async fn write_stream(path: &Path, body: &mut PartBody<'_>) -> Result<[u8; TAG_LEN], Error> {
    let fail = io_error(path);
    let mut file = create_new(path).await.map_err(&fail)?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(&fail)?;
    }
    let flushed = file.flush().await;
    flushed.and(file.sync_all().await).map_err(&fail)?;
    Ok(finish(hasher))
}

/// Writes the contents of `files`, one after the other, to a new file at `path` behind the
/// content tag of the whole, which is written last, and gives the tag.
async fn join_files(path: &Path, files: &[PathBuf]) -> Result<[u8; TAG_LEN], Error> {
    let fail = io_error(path);
    let mut out = create_new(path).await.map_err(&fail)?;
    out.write_all(&[0_u8; TAG_LEN]).await.map_err(&fail)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; COPY_CHUNK];
    for file in files {
        let mut input = File::open(file).await.map_err(io_error(file))?;
        loop {
            let count = input.read(&mut buffer).await.map_err(io_error(file))?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            out.write_all(&buffer[..count]).await.map_err(&fail)?;
        }
    }
    seal(&mut out, hasher).await.map_err(&fail)
}

/// Writes the chunks of `body` to a new file at `path` behind room for the content tag, then
/// the tag of the whole, which is known only after the last chunk, and syncs the file. A body
/// that ends with an error leaves a file that is not an object (the caller removes it). Gives
/// the tag.
async fn write_object_stream(path: &Path, body: &mut PartBody<'_>) -> Result<[u8; TAG_LEN], Error> {
    let fail = io_error(path);
    let mut out = create_new(path).await.map_err(&fail)?;
    out.write_all(&[0_u8; TAG_LEN]).await.map_err(&fail)?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        out.write_all(&chunk).await.map_err(&fail)?;
    }
    seal(&mut out, hasher).await.map_err(&fail)
}

/// Writes the tag of the hash `hasher` has come to over the room at the start of `out`, and
/// syncs the file. Gives the tag.
async fn seal(out: &mut File, hasher: Sha256) -> std::io::Result<[u8; TAG_LEN]> {
    let tag = finish(hasher);
    out.seek(SeekFrom::Start(0)).await?;
    out.write_all(&tag).await?;
    out.flush().await?;
    out.sync_all().await?;
    Ok(tag)
}

/// An object's file, opened, with its content tag read.
struct Stored {
    file: File,
    path: PathBuf,
    tag: [u8; TAG_LEN],
    size: u64,
    modified: u64,
}

impl Stored {
    /// Positions the file at `offset` bytes into the body, behind the content tag.
    async fn seek_body(&mut self, offset: u64) -> Result<(), Error> {
        let start = SeekFrom::Start(TAG_LEN as u64 + offset);
        let sought = self.file.seek(start).await;
        sought.map(drop).map_err(io_error(&self.path))
    }
}

/// The offset in the body, and the number of bytes from there, that `range` of a body of `size`
/// bytes asks for.
fn window(size: u64, range: Option<ByteRange>) -> Result<(u64, u64), Error> {
    match range {
        None => Ok((0, size)),
        Some(range) if range.offset() > size => {
            let text = format!(
                "range starting at {} is past the end of the {size}-byte object",
                range.offset()
            );
            Err(Error::Other(text))
        }
        Some(range) => Ok((range.offset(), range.size().min(size - range.offset()))),
    }
}

/// The bytes of `range` of the object `stored` was opened from and its content tag. Both
/// are those of the one open file (the tag is the header of that file), so a replacement of
/// the object that comes meanwhile (a new file under the name, whatever its tag) is not
/// seen by this read.
async fn read_tagged(
    mut stored: Stored,
    range: Option<ByteRange>,
) -> Result<(Bytes, String), Error> {
    let (offset, len) = window(stored.size, range)?;
    stored.seek_body(offset).await?;
    let mut body = Vec::new();
    let read = (&mut stored.file).take(len).read_to_end(&mut body).await;
    read.map_err(io_error(&stored.path))?;
    Ok((Bytes::from(body), hex::encode(stored.tag)))
}

/// The `left` bytes of the body that `stored` is positioned at, as chunks of at most
/// [`READ_CHUNK`] bytes, each read when the chunk is asked for. A file that ends before the
/// bytes do, or a read that fails, is the last chunk, as an error.
fn chunks(stored: Stored, left: u64) -> ReadBody {
    let next = |(mut stored, left): (Stored, u64)| async move {
        if left == 0 {
            return None;
        }
        let want = left.min(READ_CHUNK);
        let mut chunk = Vec::new();
        let read = (&mut stored.file).take(want).read_to_end(&mut chunk).await;
        let read = read.map_err(io_error(&stored.path));
        let got = read.and_then(|got| {
            if got as u64 == want {
                return Ok(Bytes::from(chunk));
            }
            let text = format!(
                "{}: the file ended inside the object",
                stored.path.display()
            );
            Err(Error::Other(text))
        });
        match got {
            Ok(chunk) => Some((Ok(chunk), (stored, left - want))),
            Err(error) => Some((Err(error), (stored, 0))),
        }
    };
    stream::unfold((stored, left), next).boxed()
}

/// Opens the file of an object and reads its content tag.
async fn open_stored(path: &Path) -> std::io::Result<Stored> {
    let mut file = File::open(path).await?;
    let meta = file.metadata().await?;
    let mut tag = [0_u8; TAG_LEN];
    file.read_exact(&mut tag).await?;
    Ok(Stored {
        file,
        path: path.to_owned(),
        tag,
        size: meta.len().saturating_sub(TAG_LEN as u64),
        modified: modified_of(&meta),
    })
}

/// A part that is stored in an upload.
struct StoredPart {
    number: u32,
    tag: String,
    name: String,
}

impl StoredPart {
    /// Whether this is the part `number` with the content tag `tag`.
    fn is(&self, number: u32, tag: &str) -> bool {
        (self.number, self.tag.as_str()) == (number, tag)
    }
}

/// The parts that are stored in the upload directory `dir`.
async fn stored_parts(dir: &Path) -> Result<Vec<StoredPart>, Error> {
    let fail = io_error(dir);
    let mut entries = fs::read_dir(dir).await.map_err(&fail)?;
    let mut parts = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(&fail)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some((number, tag)) = parse_part(&name) {
            let tag = tag.to_owned();
            parts.push(StoredPart { number, tag, name });
        }
    }
    Ok(parts)
}

/// Removes each of `files`, one that is not there being no error, and gives those removed.
async fn remove_files(files: &[PathBuf]) -> Result<Vec<PathBuf>, Error> {
    let mut gone = Vec::new();
    for file in files {
        match fs::remove_file(file).await {
            Ok(()) => gone.push(file.clone()),
            Err(why) if why.kind() == ErrorKind::NotFound => {}
            Err(why) => return Err(failed(file, &why)),
        }
    }
    Ok(gone)
}

impl DirBackend {
    /// A bucket in the directory `root`, which is made if it does not exist.
    ///
    /// # Errors
    /// [`Error::Other`] if `root` is empty or the directory cannot be made.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, Error> {
        Self::open_with(root.into(), Arc::new(FsyncDir))
    }

    /// A bucket in the directory `root` that syncs directories with `syncer`.
    fn open_with(root: PathBuf, syncer: Arc<dyn SyncDir>) -> Result<Self, Error> {
        if root.as_os_str().is_empty() {
            let text = "the directory of a tier has no name";
            return Err(Error::Other(text.to_owned()));
        }
        let uploads = root.join(UPLOADS);
        let made = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(&uploads);
        made.map_err(io_error(&uploads))?;
        let since = SystemTime::now().duration_since(UNIX_EPOCH);
        let nanos = since.map_or(0, |elapsed| elapsed.as_nanos());
        let opened = OPENED.fetch_add(1, Ordering::SeqCst);
        Ok(Self {
            root,
            nonce: format!("{}-{nanos:x}-{opened:x}", std::process::id()),
            counter: AtomicU64::new(0),
            replacing: Mutex::new(()),
            placing: Mutex::new(()),
            syncer,
            root_synced: AtomicBool::new(false),
        })
    }

    /// Syncs `dir`, whose entries a call has just changed, so that the call does not return
    /// before the change survives a power loss.
    async fn sync(&self, dir: &Path) -> Result<(), Error> {
        self.syncer.sync_dir(dir).await.map_err(io_error(dir))
    }

    /// The path of the file of the object `key`.
    fn path(&self, key: &str) -> Result<PathBuf, Error> {
        let (dir, rest) = match entries::rest(key) {
            Some(rest) => (self.root.join(entries::DIR), rest),
            None => (self.root.clone(), key),
        };
        let name = encode(rest);
        if !(1..=NAME_LIMIT).contains(&name.len()) {
            return Err(Error::InvalidKey(key.to_owned()));
        }
        Ok(dir.join(name))
    }

    /// The files the object `key` may be in: for a key under `_entries/`, the file the previous
    /// release made in the root comes second (see the module `dir_entries`). A key that makes no
    /// file name has none; an error unless the previous release could have a file for it.
    fn files(&self, key: &str) -> Result<Vec<PathBuf>, Error> {
        let legacy = entries::legacy(&self.root, key);
        match self.path(key) {
            Ok(path) => Ok(std::iter::once(path).chain(legacy).collect()),
            Err(error) => legacy.map(|old| vec![old]).ok_or(error),
        }
    }

    /// The file the object `key` is in or, if it has none, would be made in: the file the previous
    /// release made in the root where there is one, which a key keeps for its whole life, else
    /// the one a write makes now.
    async fn locate(&self, key: &str) -> Result<PathBuf, Error> {
        let legacy = entries::legacy(&self.root, key);
        if let Some(old) = legacy
            && fs::try_exists(&old).await.unwrap_or(false)
        {
            return Ok(old);
        }
        self.path(key)
    }

    /// Syncs the directory of each of the files `changed` calls removed or made.
    async fn sync_parents(&self, changed: &[PathBuf]) -> Result<(), Error> {
        for file in changed {
            self.sync(file.parent().unwrap_or(&self.root)).await?;
        }
        Ok(())
    }

    /// Takes the lock between processes that the conditional writes of the keys under `_entries/`
    /// hold (none for any other key), making the subdirectory if it is not there. It is taken
    /// before the `replacing` lock, always, so that no two calls wait for each other. The root is
    /// synced, with the lock held, until one sync of it has succeeded: whichever call made the
    /// subdirectory, and whether an earlier sync failed, no entry write returns before the
    /// subdirectory is durable.
    async fn entry_lock(&self, key: &str) -> Result<Option<entries::EntryLock>, Error> {
        if entries::rest(key).is_none() {
            return Ok(None);
        }
        let syncer = Arc::clone(&self.syncer);
        let reached = move || syncer.lock_reached();
        let lock = entries::take(self.root.join(entries::DIR), reached).await?;
        if !self.root_synced.load(Ordering::SeqCst) {
            self.sync(&self.root).await?;
            self.root_synced.store(true, Ordering::SeqCst);
        }
        Ok(Some(lock))
    }

    /// A name for a temporary file in `dir`, which is not listed.
    fn temp(&self, dir: &Path) -> PathBuf {
        let number = self.counter.fetch_add(1, Ordering::SeqCst);
        dir.join(format!(".tmp-{}-{number}", self.nonce))
    }

    /// Opens the object `key`.
    async fn stored(&self, key: &str) -> Result<Stored, Error> {
        let path = self
            .locate(key)
            .await
            .map_err(|_| Error::NotFound(key.to_owned()))?;
        let opened = open_stored(&path).await;
        opened.map_err(|why| classify(key, &path, &why, ErrorKind::NotFound, Error::NotFound))
    }

    /// Opens the object `key` if its content tag is `etag`. The caller holds the `replacing` lock,
    /// so that the check and what follows it are not split by another call of this backend.
    async fn matching(&self, key: &str, etag: &str) -> Result<Stored, Error> {
        let current = self.stored(key).await?;
        if hex::encode(current.tag) == etag {
            Ok(current)
        } else {
            Err(Error::PreconditionFailed(key.to_owned()))
        }
    }

    /// Moves the temporary file `temp` over `path`, the file of the object (the name holds the
    /// new body whole or the old one), and syncs the directory. The temporary file is gone either
    /// way. The caller holds the `replacing` lock.
    async fn install(&self, temp: &Path, path: &Path) -> Result<(), Error> {
        let renamed = fs::rename(temp, path).await;
        let _ = fs::remove_file(temp).await;
        renamed.map_err(io_error(path))?;
        self.sync(path.parent().unwrap_or(&self.root)).await
    }

    /// Removes the files of the object `key` if its content tag is `etag`, with the lock that
    /// replacements and deletes take held from the check to the removal, so that no other call
    /// of this backend replaces or deletes the object in between. Gives the files removed; the
    /// directories are not synced.
    async fn unlink_if_match(&self, key: &str, etag: &str) -> Result<Vec<PathBuf>, Error> {
        let files = self
            .files(key)
            .map_err(|_| Error::NotFound(key.to_owned()))?;
        let _entry = self.entry_lock(key).await?;
        let _replacing = self.replacing.lock().await;
        let current = self.matching(key, etag).await?;
        let path = current.path.clone();
        drop(current);
        // The lock keeps out every other call of this backend, so a file that is gone now was
        // taken by another process: the key has no object, as the trait says.
        // The other file goes first: a failure leaves the object, not a stale file in its place.
        let others: Vec<PathBuf> = files.into_iter().filter(|file| *file != path).collect();
        let mut gone = remove_files(&others).await?;
        let removed = fs::remove_file(&path).await;
        let absent = ErrorKind::NotFound;
        removed.map_err(|why| classify(key, &path, &why, absent, Error::NotFound))?;
        gone.push(path);
        Ok(gone)
    }

    /// Writes the file of an object with this body to a new temporary file.
    async fn write_temp(&self, body: Bytes) -> Result<(PathBuf, [u8; TAG_LEN]), Error> {
        let tag = sha256(&body);
        let data = Bytes::copy_from_slice(&tag).chain(body);
        let temp = self.temp(&self.root);
        match create_file(&temp, data).await {
            Ok(()) => Ok((temp, tag)),
            Err(why) => {
                let _ = fs::remove_file(&temp).await;
                Err(failed(&temp, &why))
            }
        }
    }

    /// Moves the temporary file `temp` to `path` if nothing is there: a hard link fails where
    /// the name is taken. The temporary file is gone either way. Where the link was made, the
    /// directory is synced before this returns. If that sync fails the name is removed again
    /// (and the directory synced, best effort), so that an object that was not acknowledged is
    /// not left visible and a retry of the create-only put can succeed; if the name cannot be
    /// removed the error says that it may exist without being durable.
    async fn publish(&self, temp: &Path, path: &Path, key: &str) -> Result<(), Error> {
        // Gone on every way out, the failure to take the lock and a call dropped while it waits
        // included.
        let _scratch = Scratch(temp.to_owned());
        let _entry = self.entry_lock(key).await?;
        // The key is taken where the previous release made it, though not under the name a write
        // makes now.
        if let Some(old) = entries::legacy(&self.root, key)
            && fs::try_exists(&old).await.unwrap_or(false)
        {
            return Err(Error::AlreadyExists(key.to_owned()));
        }
        let dir = path.parent().unwrap_or(&self.root);
        let linked = fs::hard_link(temp, path).await;
        let _ = fs::remove_file(temp).await;
        let taken = ErrorKind::AlreadyExists;
        linked.map_err(|e| classify(key, path, &e, taken, Error::AlreadyExists))?;
        let Err(synced) = self.sync(dir).await else {
            return Ok(());
        };
        // Under the lock of the replacements, so that the rollback is no exception to a check
        // and a removal that no delete or replacement of this backend can split.
        let rolled_back = {
            let _replacing = self.replacing.lock().await;
            fs::remove_file(path).await
        };
        match rolled_back {
            Ok(()) => {
                let _ = self.sync(dir).await;
                Err(synced)
            }
            Err(why) => {
                let text = format!(
                    "{}: the name may exist without being durable: the directory sync failed ({synced}) and the name could not be removed ({why})",
                    path.display()
                );
                Err(Error::Other(text))
            }
        }
    }

    /// Replaces the object `key` if it has the content tag `etag`, and gives the new tag.
    async fn replace(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        let located = self.locate(key).await;
        located.map_err(|_| Error::NotFound(key.to_owned()))?;
        let _entry = self.entry_lock(key).await?;
        let _replacing = self.replacing.lock().await;
        let path = self.matching(key, etag).await?.path;
        let (temp, tag) = self.write_temp(body).await?;
        self.install(&temp, &path).await?;
        Ok(hex::encode(tag))
    }

    /// The directory of the upload `id` of `key`.
    async fn upload_dir(&self, key: &str, id: &str) -> Result<PathBuf, Error> {
        let unknown = || Error::NotFound(id.to_owned());
        if !id.bytes().all(is_id_byte) {
            return Err(unknown());
        }
        let dir = self.root.join(UPLOADS).join(id);
        let named = fs::read_to_string(dir.join(KEY_FILE)).await;
        let named = named.map_err(|_| unknown())?;
        if named == key {
            Ok(dir)
        } else {
            Err(unknown())
        }
    }
}

#[async_trait]
impl Backend for DirBackend {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.put_tagged(key, body).await.map(drop)
    }

    /// The tag is that of the file this call published: the SHA-256 of the bytes it wrote.
    async fn put_tagged(&self, key: &str, body: Bytes) -> Result<String, Error> {
        let path = self.path(key)?;
        let (temp, tag) = self.write_temp(body).await?;
        self.publish(&temp, &path, key).await?;
        Ok(hex::encode(tag))
    }

    /// Writes the chunks to a temporary file as they arrive, and links it to the name only
    /// after the last chunk, when the content tag is known and the file is synced. A body that
    /// fails, or a call that is dropped half way, removes the temporary file and leaves the
    /// name as it was.
    async fn put_stream(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.put_stream_tagged(key, len, body).await.map(drop)
    }

    /// As [`Backend::put_stream`], and gives the tag of the file it published: the SHA-256 of the
    /// bytes written.
    async fn put_stream_tagged(
        &self,
        key: &str,
        _len: Option<u64>,
        mut body: PartBody<'_>,
    ) -> Result<String, Error> {
        let path = self.path(key)?;
        let temp = self.temp(&self.root);
        let _scratch = Scratch(temp.clone());
        let tag = write_object_stream(&temp, &mut body).await?;
        self.publish(&temp, &path, key).await?;
        Ok(hex::encode(tag))
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.replace(key, body, etag).await
    }

    /// Writes the chunks to a temporary file as they arrive, outside the lock, so that a slow
    /// body holds up no delete or replacement; then, under the lock and the tag check of
    /// [`Backend::put_if_match`], moves the file over the object. A body that fails, a tag that
    /// does not match and a call that is dropped half way remove the temporary file and leave the
    /// object as it was.
    async fn put_stream_if_match(
        &self,
        key: &str,
        _len: Option<u64>,
        mut body: PartBody<'_>,
        etag: &str,
    ) -> Result<String, Error> {
        self.locate(key).await?;
        let temp = self.temp(&self.root);
        let _scratch = Scratch(temp.clone());
        let tag = write_object_stream(&temp, &mut body).await?;
        let _entry = self.entry_lock(key).await?;
        let _replacing = self.replacing.lock().await;
        let path = self.matching(key, etag).await?.path;
        self.install(&temp, &path).await?;
        Ok(hex::encode(tag))
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        Ok(self.get_tagged(key, range).await?.0)
    }
    async fn get_tagged(
        &self,
        key: &str,
        range: Option<ByteRange>,
    ) -> Result<(Bytes, String), Error> {
        read_tagged(self.stored(key).await?, range).await
    }

    /// Opens the file and gives its bytes in chunks of at most 64 KiB, read as they are asked
    /// for: the object is never held, and a replacement or a delete of the name while the chunks
    /// are read leaves this read on the file it opened.
    async fn get_stream(&self, key: &str, range: Option<ByteRange>) -> Result<ReadBody, Error> {
        let mut stored = self.stored(key).await?;
        let (offset, len) = window(stored.size, range)?;
        stored.seek_body(offset).await?;
        Ok(chunks(stored, len))
    }

    /// Opens the file once: the meta and the chunks are those of that file, as for
    /// [`Backend::get_stream`], whose bounded chunks the body has.
    async fn open_read(&self, key: &str, span: Option<Span>) -> Result<Opened, Error> {
        let mut stored = self.stored(key).await?;
        let meta = Meta {
            size: stored.size,
            tag: hex::encode(stored.tag),
            modified: stored.modified,
        };
        let window = Window::of(span, meta.size);
        let (offset, len) = match window {
            Window::Whole => (0, meta.size),
            Window::Part(range) => (range.offset(), range.size()),
            Window::Unsatisfiable => (0, 0),
        };
        stored.seek_body(offset).await?;
        Ok(Opened {
            meta,
            window,
            body: chunks(stored, len),
        })
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        let stored = self.stored(key).await?;
        Ok(Meta {
            size: stored.size,
            tag: hex::encode(stored.tag),
            modified: stored.modified,
        })
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        // The previous release's file goes first: a failure leaves the object, not a stale file.
        let mut files = self
            .files(key)
            .map_err(|_| Error::NotFound(key.to_owned()))?;
        files.reverse();
        // Not held across the sync: a replacement or delete that follows needs nothing it protects.
        let gone = {
            let _entry = self.entry_lock(key).await?;
            let _replacing = self.replacing.lock().await;
            remove_files(&files).await?
        };
        self.sync_parents(&gone).await
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> Result<(), Error> {
        let gone = self.unlink_if_match(key, etag).await?;
        self.sync_parents(&gone).await
    }

    /// The root holds the keys that do not start with `_entries/`, and the files of the previous
    /// release for those that do; the subdirectory holds the others, and is read only if
    /// `prefix` can match one of them. A key in both is listed once, from the root.
    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        listing::list(&self.root, prefix).await
    }

    async fn create_upload(&self, key: &str) -> Result<String, Error> {
        self.path(key)?;
        let number = self.counter.fetch_add(1, Ordering::SeqCst);
        let since = SystemTime::now().duration_since(UNIX_EPOCH);
        let nanos = since.map_or(0, |elapsed| elapsed.as_nanos());
        let id = format!("u-{nanos:x}-{:x}-{number:x}", std::process::id());
        let dir = self.root.join(UPLOADS).join(&id);
        let made = fs::DirBuilder::new().mode(DIR_MODE).create(&dir).await;
        let named = create_file(&dir.join(KEY_FILE), key.as_bytes()).await;
        made.and(named).map_err(io_error(&dir))?;
        Ok(id)
    }

    async fn upload_part(
        &self,
        key: &str,
        id: &str,
        number: u32,
        mut body: PartBody<'_>,
    ) -> Result<String, Error> {
        let dir = self.upload_dir(key, id).await?;
        let temp = Scratch(self.temp(&dir));
        let tag = hex::encode(write_stream(&temp.0, &mut body).await?);
        let name = part_name(number, &tag);
        let _placing = self.placing.lock().await;
        let renamed = fs::rename(&temp.0, dir.join(&name)).await;
        renamed.map_err(io_error(&dir))?;
        self.sync(&dir).await?;
        let stored = stored_parts(&dir).await?;
        let others = stored.iter().filter(|part| part.number == number);
        for part in others.filter(|part| part.name != name) {
            let _ = fs::remove_file(dir.join(&part.name)).await;
        }
        Ok(tag)
    }

    async fn complete_upload(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        let dir = self.upload_dir(key, id).await?;
        let path = self.path(key)?;
        let stored = stored_parts(&dir).await?;
        let mut files = Vec::new();
        for (number, tag) in parts {
            let found = stored.iter().find(|part| part.is(*number, tag));
            let Some(found) = found else {
                return Err(Error::InvalidPart(format!("part {number}")));
            };
            files.push(dir.join(&found.name));
        }
        if fs::try_exists(&path).await.unwrap_or(false) {
            return Err(Error::AlreadyExists(key.to_owned()));
        }
        let temp = self.temp(&self.root);
        let tag = match join_files(&temp, &files).await {
            Ok(tag) => tag,
            Err(error) => {
                let _ = fs::remove_file(&temp).await;
                return Err(error);
            }
        };
        self.publish(&temp, &path, key).await?;
        let _ = fs::remove_dir_all(&dir).await;
        Ok(hex::encode(tag))
    }

    async fn abort_upload(&self, key: &str, id: &str) -> Result<(), Error> {
        let dir = self.upload_dir(key, id).await?;
        fs::remove_dir_all(&dir).await.map_err(io_error(&dir))
    }

    async fn list_uploads(
        &self,
        prefix: &str,
        after: Option<UploadsAfter<'_>>,
        max_uploads: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        let uploads = self.root.join(UPLOADS);
        let mut entries = fs::read_dir(&uploads).await.map_err(io_error(&uploads))?;
        let mut open = Vec::new();
        while let Some(entry) = entries.next_entry().await.map_err(io_error(&uploads))? {
            let id = entry.file_name().to_string_lossy().into_owned();
            // An upload that is made, completed or aborted while this is read has no key file
            // (yet, or any more): it is not open, so it is left out.
            let named = entry.path().join(KEY_FILE);
            let Ok(key) = fs::read_to_string(&named).await else {
                continue;
            };
            let started = fs::metadata(&named).await.and_then(|meta| meta.modified());
            let since = started.map(|time| time.duration_since(UNIX_EPOCH));
            let elapsed = since.ok().and_then(Result::ok);
            let initiated = elapsed.map_or(0, |elapsed| elapsed.as_millis() as u64);
            open.push(OpenUpload { key, id, initiated });
        }
        Ok(upload_page(open, prefix, after, max_uploads))
    }
}

#[cfg(test)]
#[path = "dir_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "dir_sync_tests.rs"]
mod sync_tests;

#[cfg(test)]
#[path = "dir_entries_tests.rs"]
mod entries_tests;
