//! A fill: the object read from the upstream into the stage directory, checked, and linked
//! into `obj/`; and the single flight that lets the readers of one key share it.

use std::collections::hash_map::Entry;
use std::fs::File;
use std::future::Future;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};

use futures::FutureExt as _;
use futures::future::{BoxFuture, OptionFuture, Shared};
use sha2::{Digest, Sha256};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{OwnedSemaphorePermit, oneshot, watch};
use tokio::task::JoinHandle;

use crate::{ByteRange, Error, Meta};

use super::holds::State;
use super::layout::{
    Name, Origin, Sidecar, data_path, make_dirs, meta_path, object_dir, stage_dir,
};
use super::metrics::{FailedFill, bump};
use super::{Disk, Inner, WHOLE_FILL_LIMIT, locked, percent_of};

/// How a fill ended, for the readers that waited for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Filled {
    /// The object is in the cache.
    Kept,
    /// It is not: the readers are answered from the upstream.
    Skipped,
    /// The object was written over while it was read (the tag changed between the two HEADs):
    /// a reader that was given no byte is answered from the upstream, which has the new object;
    /// one that was given bytes of the old one gets an error.
    Changed,
}

/// The error of a read whose object was written over while it was being read: bytes of two
/// versions must never be given as one.
pub(super) fn written_over() -> Error {
    let text = "the object was written over while it was being read";
    Error::Unavailable(text.to_owned())
}

/// Refuses an upstream that gives no content tag for `key`: the tag is what tells one version of an
/// object from another of the same size, so a read of it could not be held to one version.
pub(super) fn tagged(key: &str, meta: &Meta) -> Result<(), Error> {
    if meta.tag.is_empty() {
        let text = format!("the upstream gives no content tag for {key}");
        return Err(Error::Unsupported(text));
    }
    Ok(())
}

/// How far a fill has come, as its readers see it. The bytes are in a file, never in a copy in
/// memory that the readers share.
#[derive(Clone, Debug)]
pub(super) enum Live {
    /// The fill has not made its file yet.
    Starting,
    /// The bytes of the object up to `written` are in the file `file` reads, flushed. The file
    /// stays readable through this handle after it is linked into `obj/` or removed.
    Staging {
        /// What the upstream said of the object when the fill began: its size, and the tag a
        /// reader that goes on from the upstream must find still there.
        meta: Meta,
        file: Arc<File>,
        written: u64,
    },
}

/// How a fill ended, as every path that waits for it can read it: the fill's own verdict, from
/// which the S3 front and the fill socket each take their answer.
pub(super) type Ended = Result<Filled, Stop>;

/// A fill that is running: its end, which every reader of the name waits on, and its progress,
/// which a reader that streams from the staged file follows.
#[derive(Clone)]
pub(super) struct Pending {
    done: Shared<BoxFuture<'static, Ended>>,
    pub(super) live: watch::Receiver<Live>,
    /// Whether the fill cuts its bytes out of a larger object (see [`Origin::Span`]): the S3
    /// front does not wait for it, since the name is not an object of the upstream.
    span: bool,
    /// Whether this is the place a keep of the key holds (see [`Pending::keeping`]).
    pub(super) keeping: bool,
}

impl Pending {
    /// A fill that never ends, for the tests that need a key to be in the table of the fills that
    /// run.
    #[cfg(test)]
    pub(super) fn never() -> Self {
        let done = futures::future::pending().boxed().shared();
        let (_, live) = watch::channel(Live::Starting);
        Self {
            done,
            live,
            span: false,
            keeping: false,
        }
    }

    /// A fill that is over already, with no object and no progress, for the tests that need one
    /// (a keep's place is [`Pending::keeping`]).
    #[cfg(test)]
    pub(super) fn finished(filled: Filled) -> Self {
        let done = futures::future::ready(Ok(filled)).boxed().shared();
        // The sender is dropped at once: nothing is ever written.
        let (_, live) = watch::channel(Live::Starting);
        Self {
            done,
            live,
            span: false,
            keeping: false,
        }
    }

    /// The place a keep of a key holds in the table of the fills that run: [`Pending::finished`]
    /// with no object, so that a reader of the S3 front is answered from the upstream, and a
    /// sender that the keep's [`Flight`] holds until it leaves the table, which ends the wait of
    /// [`Pending::released`]: a request of the fill socket waits for the keep to end (it is
    /// bounded by the keep's deadline) and then fills, since the keep is no reason for it to be
    /// answered `no space`.
    pub(super) fn keeping() -> (Self, watch::Sender<Live>) {
        let done = futures::future::ready(Ok(Filled::Skipped)).boxed().shared();
        let (gate, live) = watch::channel(Live::Starting);
        let place = Self {
            done,
            live,
            span: false,
            keeping: true,
        };
        (place, gate)
    }

    /// Ends when the keep that holds this place is over and has left the table.
    pub(super) async fn released(mut self) {
        // Nothing is ever sent: the call ends in the error of the sender's drop.
        let _ = self.live.changed().await;
    }

    /// The end of the fill, as the fill socket reads it.
    pub(super) async fn ended(self) -> Ended {
        self.done.await
    }
}

/// What a fill that ended as `ended` says to a reader of the S3 front.
pub(super) fn s3_answer(ended: Ended) -> Result<Filled, Error> {
    match ended {
        Ok(filled) => Ok(filled),
        Err(Stop::Upstream(error)) => Err(error),
        // The readers of a fill that could not finish it go to the upstream.
        Err(Stop::Short | Stop::Local | Stop::Invalidated | Stop::Lost) => Ok(Filled::Skipped),
        // Readers may have been given bytes of the first version already: each reader decides by
        // what it was given.
        Err(Stop::Changed) => Ok(Filled::Changed),
        Err(Stop::Mismatch) => {
            // The bytes are known to be wrong: they go to nobody.
            let text = "the upstream's bytes do not hash to the name of the object";
            Err(Error::Unavailable(text.to_owned()))
        }
        Err(Stop::TooLarge) => {
            let text = "the object is larger than a whole-object fill may be";
            Err(Error::Unavailable(text.to_owned()))
        }
        Err(Stop::Aborted) => {
            let text = "the fill of the object did not finish";
            Err(Error::Unavailable(text.to_owned()))
        }
    }
}

impl Future for Pending {
    type Output = Result<Filled, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().done).poll(cx).map(s3_answer)
    }
}

/// The task of a fill.
pub(super) type Task = JoinHandle<Ended>;

/// What a fill reads from the upstream.
pub(super) enum Read<'a> {
    /// The whole object of the key, whose size the fill asks the upstream for.
    Object,
    /// The whole object of the key, whose HEAD the caller has made.
    Headed(Meta),
    /// `size` bytes from `offset` of the upstream object `from`, which must hash to `sha256`: a
    /// blob inside a larger object, kept under the key of the fill (see `source`).
    Range {
        from: &'a str,
        offset: u64,
        size: u64,
        sha256: &'a str,
    },
}

/// What a fill reads, owned, so that the task of the fill holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Plan {
    /// The object of the key, whole, whose size the fill asks the upstream for.
    Object,
    /// The object `cas/l/<sha256>`, whole, up to [`WHOLE_FILL_LIMIT`](super::WHOLE_FILL_LIMIT):
    /// the file a tree's manifest is, which the caller knows by its hash and not its size.
    Manifest,
    /// A blob inside another object: `size` bytes from `offset` of the upstream object `from`.
    Blob {
        from: String,
        offset: u64,
        size: u64,
    },
}

impl Plan {
    /// Where the bytes of the fill of `key` come from: a blob read from the start of the very
    /// object `key` is that object, any other is cut out of a larger one.
    fn origin(&self, key: &str) -> Origin {
        match self {
            Plan::Blob { from, offset, .. } if from != key || *offset != 0 => Origin::Span,
            Plan::Blob { .. } | Plan::Object | Plan::Manifest => Origin::Object,
        }
    }
}

/// The pool of permits a fill starts under. A fill that joins one that runs takes none.
pub(super) enum Class {
    /// A miss of the S3 front: a permit of `max_concurrent_fills`, which is not waited for.
    S3,
    /// A fill asked for over the fill socket, which has waited for a permit of
    /// `fill_socket_concurrency`.
    Mount(OwnedSemaphorePermit),
}

/// The permit a fill that is running holds, which says which pool it came from.
pub(super) struct Held {
    permit: OwnedSemaphorePermit,
    /// Whether the permit is one of `prefetch_concurrency`: when the fill ends, the next queued
    /// blob starts on it.
    prefetch: bool,
}

impl Held {
    /// A permit of a pool of the reads: `max_concurrent_fills` or `fill_socket_concurrency`.
    pub(super) fn foreground(permit: OwnedSemaphorePermit) -> Self {
        Self {
            permit,
            prefetch: false,
        }
    }

    /// A permit of the prefetch workers.
    pub(super) fn worker(permit: OwnedSemaphorePermit) -> Self {
        Self {
            permit,
            prefetch: true,
        }
    }
}

/// What starts a fill that has its place in the table: the object it reads, the sender that
/// gives its task to the readers that wait for it, and the sender of its progress.
pub(super) struct Launch {
    pub(super) key: String,
    pub(super) name: Name,
    pub(super) plan: Plan,
    live: watch::Sender<Live>,
    sender: oneshot::Sender<Task>,
}

/// Why a fill ended without an object.
#[derive(Clone, Debug)]
pub(super) enum Stop {
    /// The upstream failed, or has no such object.
    Upstream(Error),
    /// The upstream gave fewer bytes than it had said the object has.
    Short,
    /// The cache directory could not be written.
    Local,
    /// A write or a delete through the cache came while the object was being read (see `forget`):
    /// what was read is not kept, and the readers go on from the upstream.
    Invalidated,
    /// The bytes of a key named by its content hash to another name.
    Mismatch,
    /// The object changed while it was being read: the bytes may be of two versions.
    Changed,
    /// The cache lost the lock of its directory while the fill ran: nothing more is written.
    Lost,
    /// A whole object is larger than a manifest may be.
    TooLarge,
    /// The task of the fill panicked or was cancelled.
    Aborted,
}

impl From<io::Error> for Stop {
    fn from(_: io::Error) -> Self {
        Stop::Local
    }
}

impl From<Error> for Stop {
    fn from(error: Error) -> Self {
        Stop::Upstream(error)
    }
}

/// A file of the stage directory, removed when it is dropped, and counted while it exists.
pub(super) struct Staged {
    inner: Arc<Inner>,
    pub(super) file: tokio::fs::File,
    path: PathBuf,
    /// Where the sidecar is written before it is renamed into place.
    sidecar: PathBuf,
}

impl Staged {
    /// A new file, read-only for everyone once it is made (mode 0444: the descriptor that made
    /// it can still write).
    pub(super) async fn create(inner: &Arc<Inner>) -> io::Result<Self> {
        let number = inner.staged.fetch_add(1, Ordering::Relaxed);
        let name = format!("{}-{number:016x}", inner.stage_prefix);
        let path = stage_dir(&inner.config.path).join(&name);
        let sidecar = stage_dir(&inner.config.path).join(format!("{name}.meta"));
        let mut options = OpenOptions::new();
        let file = options
            .write(true)
            .create_new(true)
            .mode(0o444)
            .open(&path)
            .await?;
        // The mode given to `open` is cut by the umask: set it, so that it is 0444 for any.
        file.set_permissions(std::fs::Permissions::from_mode(0o444))
            .await?;
        let _ = inner.counters.staging_files.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            inner: Arc::clone(inner),
            file,
            path,
            sidecar,
        })
    }

    /// Writes the end of the file to the operating system, so that a name linked to it is whole
    /// for every reader. It does not make the bytes durable: the cache is a copy, and a file a
    /// power cut left short is found by the size check of every open (see `open_whole`).
    pub(super) async fn finish(&mut self) -> io::Result<()> {
        let disk = Arc::clone(&self.inner.disk);
        disk.finish_stage(&mut self.file).await
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(&self.sidecar);
        let counter = &self.inner.counters.staging_files;
        let _ = counter.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Writes the sidecar at `path`, readable by everyone and writable by the owner whatever the
/// umask.
fn write_sidecar(path: &Path, text: &[u8]) -> io::Result<()> {
    std::fs::write(path, text)
        .and_then(|()| std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)))
}

/// What [`publish`] did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Published {
    /// The data file is new.
    New,
    /// The data file was there already.
    Existed,
    /// The cache does not own the directory any more, and nothing of the object is in it.
    Lost,
}

/// Puts the staged file into the cache: the sidecar first, by rename, then the data file by a
/// create-only link. `owns` is asked before the first change to the directory and again before
/// the rename, the first step that puts the object in it: a cache that lost the lock of the
/// directory since the fill started publishes nothing ([`Published::Lost`]).
pub(super) fn publish(
    disk: &dyn Disk,
    root: &Path,
    hex: &str,
    staged: (&Path, &Path),
    sidecar: &[u8],
    owns: &dyn Fn() -> bool,
) -> io::Result<Published> {
    let data = data_path(root, hex);
    let (from, temporary) = staged;
    let meta = meta_path(root, hex);
    if !owns() {
        return Ok(Published::Lost);
    }
    make_dirs(root, &object_dir(root, hex)).and_then(|()| write_sidecar(temporary, sidecar))?;
    if !owns() {
        return Ok(Published::Lost);
    }
    std::fs::rename(temporary, &meta)?;
    match disk.link(from, &data) {
        Ok(()) => Ok(Published::New),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(Published::Existed),
        Err(error) => {
            // Nothing owns the sidecar: leave no file that says an object is there. A cache that
            // has lost the directory since the rename leaves it: the name may be the new
            // owner's by now, and a sidecar with no data file is what that owner's scan deletes.
            if owns() {
                let _ = std::fs::remove_file(meta_path(root, hex));
            }
            Err(error)
        }
    }
}

/// The bytes of a fill that is running, counted against the critical mark from the moment the
/// fill starts until its object is in the index (or it ends without one).
pub(super) struct Reserved {
    inner: Arc<Inner>,
    size: u64,
}

impl Drop for Reserved {
    fn drop(&mut self) {
        let _ = self.inner.reserved.fetch_sub(self.size, Ordering::SeqCst);
    }
}

/// A fill's place in the table of the fills that run: taken out when it is dropped, whether the
/// fill ended, panicked or was cancelled, so that no key stays marked as being filled.
pub(super) struct Flight {
    pub(super) inner: Arc<Inner>,
    pub(super) hex: String,
    /// Held by a keep, and dropped after the place is out of the table: see [`Pending::keeping`].
    pub(super) _gate: Option<watch::Sender<Live>>,
}

impl Drop for Flight {
    fn drop(&mut self) {
        let gone = {
            let mut flights = locked(&self.inner.flights);
            let gone = flights.remove(&self.hex);
            // The mark of a delete is of the fill of the name: it goes with it.
            locked(&self.inner.invalidated).remove(&self.hex);
            gone
        };
        drop(gone);
        // Whatever ended the flight (a fill, a keep, a task cancelled or panicked), the holds that
        // wait for the blob are told, after the place is out of the table: a hold that asks later
        // finds no flight. The index says whether the blob is in the cache.
        let state = if locked(&self.inner.index).get(&self.hex).is_some() {
            State::Filled
        } else {
            State::Failed
        };
        self.inner.hold_settle(&self.hex, state);
    }
}

impl Inner {
    /// Reserves room for an object of `size` bytes if it fits in the budget and the cache, with
    /// the fills that are running, would not pass its critical mark.
    pub(super) fn reserve(self: &Arc<Self>, size: u64) -> Option<Reserved> {
        self.reserve_between(size, || {})
    }

    /// [`reserve`](Self::reserve), running `between` after the bytes are reserved and before the
    /// index is read. The order matters: a fill that commits puts its object in the index before
    /// it drops its reservation, so an object is in the reserved bytes or in the index (possibly
    /// both, which only makes the count high), never in neither. A refused reservation
    /// is counted while it is held, so a reservation made at the same time can be refused for it:
    /// that costs a skipped fill, never room.
    pub(super) fn reserve_between(
        self: &Arc<Self>,
        size: u64,
        between: impl FnOnce(),
    ) -> Option<Reserved> {
        let before = self.reserved.fetch_add(size, Ordering::SeqCst);
        // A refused reservation is dropped, which gives the bytes back.
        let reservation = Reserved {
            inner: Arc::clone(self),
            size,
        };
        between();
        let held = locked(&self.index).bytes();
        let critical = percent_of(self.config.budget, self.config.critical);
        let total = held.saturating_add(before).saturating_add(size);
        (size <= self.config.budget && total <= critical).then_some(reservation)
    }

    /// The fill of `key` to wait for (`Ok`), started if none is running; or (`Err`) the answer
    /// that needs no wait. It does not wait, and it holds no lock when it returns. This is the
    /// one way into the table of the fills that run, for every path: the name has one fill
    /// whoever asks, and `class` says which pool a fill that has to be started takes its permit
    /// from.
    ///
    /// The S3 front does not take an entry of origin `span` for an object of the upstream: it
    /// is answered [`Filled::Skipped`], so its read goes to the upstream.
    ///
    /// A fill that is queued (see the module `prefetch`) has its place in the table and has not
    /// started: the read that finds it starts it, on the permit of `class`, and the queue forgets
    /// it. The S3 front, which waits for no permit, is answered [`Filled::Skipped`] if all are
    /// taken, and the blob stays queued.
    ///
    /// The fill is started after the table of the fills that run is unlocked. The task is
    /// spawned with its [`Flight`], and a spawn that drops the task at once (the runtime is
    /// shutting down) drops the flight on this thread, which locks the table: with the table
    /// locked by this thread that is a deadlock. The entry is therefore put in the table first,
    /// with a fill that waits for the task, and the task is handed to it after the unlock.
    pub(super) fn join_or_start(
        self: &Arc<Self>,
        key: &str,
        name: &Name,
        plan: Plan,
        class: Class,
    ) -> Result<Pending, Filled> {
        let s3 = matches!(class, Class::S3);
        let (pending, launch) = {
            let mut flights = locked(&self.flights);
            // A fill that ended after the reader missed, and before it got here, left the object
            // in the index (it is put there before the flight is removed): that is not a miss
            // to fill again.
            if let Some(entry) = locked(&self.index).get(&name.hex) {
                let served = !(s3 && entry.origin == Origin::Span);
                return Err(if served {
                    Filled::Kept
                } else {
                    Filled::Skipped
                });
            }
            match flights.entry(name.hex.clone()) {
                Entry::Occupied(running) if s3 && running.get().span => {
                    return Err(Filled::Skipped);
                }
                Entry::Occupied(running) => {
                    let pending = running.get().clone();
                    // A fill that is still queued is started now, on the permit of this read.
                    let launch = self.take_queued(&name.hex, class)?;
                    if launch.is_none() && !pending.keeping {
                        self.counters.joined(s3);
                    }
                    (pending, launch)
                }
                Entry::Vacant(slot) => {
                    let permit = self.foreground_permit(class)?;
                    let (pending, launch) = self.place(key, name, plan);
                    slot.insert(pending.clone());
                    (pending, Some((launch, Held::foreground(permit))))
                }
            }
        };
        if let Some((launch, held)) = launch {
            self.start(launch, held);
        }
        Ok(pending)
    }

    /// The permit of the pool of `class` for a read that starts a fill: the one it holds, or, for
    /// the S3 front, one it does not wait for (the read is answered from the upstream if all are
    /// taken).
    pub(super) fn foreground_permit(&self, class: Class) -> Result<OwnedSemaphorePermit, Filled> {
        match class {
            Class::Mount(permit) => Ok(permit),
            Class::S3 => Arc::clone(&self.permits).try_acquire_owned().map_err(|_| {
                bump(&self.counters.fills_skipped);
                bump(&self.counters.fills_skipped_busy);
                Filled::Skipped
            }),
        }
    }

    /// The place of a fill of `plan` in the table of the fills (to be put there by the caller,
    /// which holds the table) and what starts it. A reader that waits on the place waits for the
    /// task that `start` gives it.
    pub(super) fn place(self: &Arc<Self>, key: &str, name: &Name, plan: Plan) -> (Pending, Launch) {
        let (sender, receiver) = oneshot::channel();
        let (live, progress) = watch::channel(Live::Starting);
        let pending = Pending {
            done: self.waiting(receiver),
            live: progress,
            span: plan.origin(key) == Origin::Span,
            keeping: false,
        };
        let (key, name) = (key.to_owned(), name.clone());
        let launch = Launch {
            key,
            name,
            plan,
            live,
            sender,
        };
        (pending, launch)
    }

    /// The fill that waits for the task that `receiver` is to be given, and for its end. A task
    /// that did not end by itself is an error of the readers that waited for it, and is counted.
    fn waiting(
        self: &Arc<Self>,
        receiver: oneshot::Receiver<Task>,
    ) -> Shared<BoxFuture<'static, Ended>> {
        let inner = Arc::clone(self);
        let waiting = async move {
            let task = OptionFuture::from(receiver.await.ok()).await;
            match task.and_then(Result::ok) {
                Some(ended) => ended,
                None => {
                    inner.counters.failed(FailedFill::Aborted);
                    Err(Stop::Aborted)
                }
            }
        };
        waiting.boxed().shared()
    }

    /// Runs the fill on a task of its own, so that it ends even if every reader goes away, and
    /// takes it out of the table of the fills that run when it ends, panics or is cancelled. The
    /// permit is held until then. The task is sent to the fill that waits for it. The table must
    /// not be locked by the caller (see [`join_or_start`](Self::join_or_start)).
    pub(super) fn start(self: &Arc<Self>, launch: Launch, held: Held) {
        let Launch {
            key,
            name,
            plan,
            live,
            sender,
        } = launch;
        let inner = Arc::clone(self);
        // Built before the spawn, so a task dropped before its first poll still takes its key out.
        let flight = Flight {
            inner: Arc::clone(&inner),
            hex: name.hex.clone(),
            _gate: None,
        };
        let task = tokio::spawn(async move {
            let _flight = flight;
            let ended = inner.fill(&key, &name, &plan, &live).await;
            let Held { permit, prefetch } = held;
            if prefetch {
                inner.prefetch_ended(&ended, permit);
            }
            ended
        });
        // The receiver is in the fill that is in the table, and in the caller: it is there.
        let _ = sender.send(task);
    }

    /// Reads what `plan` says into the stage directory and, if it is whole and is what its name
    /// says, puts it in the cache. A fill that stopped is counted here, once, whoever waits.
    async fn fill(
        self: &Arc<Self>,
        key: &str,
        name: &Name,
        plan: &Plan,
        live: &watch::Sender<Live>,
    ) -> Ended {
        let ended = self.carry_out(key, name, plan, live).await;
        if let Err(stop) = &ended {
            self.count(stop);
        }
        ended
    }

    /// Counts a fill that stopped for `stop` as a fill that failed, or skipped.
    pub(super) fn count(&self, stop: &Stop) {
        match stop {
            Stop::Upstream(error) => {
                if !matches!(error, Error::NotFound(_)) {
                    self.counters.failed(FailedFill::UpstreamError);
                }
            }
            Stop::Short => self.counters.failed(FailedFill::ShortBody),
            Stop::Local => self.counters.failed(FailedFill::LocalIo),
            Stop::Changed | Stop::Invalidated => self.counters.failed(FailedFill::Changed),
            // Not a failure of the fill: the directory is another cache's now.
            Stop::Lost => bump(&self.counters.fills_skipped),
            Stop::Mismatch => self.counters.failed(FailedFill::HashMismatch),
            // Not failures of the upstream; an abort is counted where it is noticed.
            Stop::TooLarge | Stop::Aborted => {}
        }
    }

    /// The fetch that `plan` says.
    async fn carry_out(
        self: &Arc<Self>,
        key: &str,
        name: &Name,
        plan: &Plan,
        live: &watch::Sender<Live>,
    ) -> Ended {
        match plan {
            Plan::Object => self.fetch_from(key, name, live, &Read::Object).await,
            Plan::Manifest => {
                let meta = self.upstream_head(key).await?;
                tagged(key, &meta)?;
                if meta.size > WHOLE_FILL_LIMIT {
                    return Err(Stop::TooLarge);
                }
                self.fetch_from(key, name, live, &Read::Headed(meta)).await
            }
            Plan::Blob { from, offset, size } => {
                let read = Read::Range {
                    from,
                    offset: *offset,
                    size: *size,
                    sha256: &name.hex,
                };
                self.fetch_from(key, name, live, &read).await
            }
        }
    }

    /// Reads the object into the stage directory and puts it in the cache. The fill remembers
    /// the generation of the lock it started under and asks [`owns`](Inner::owns) before it
    /// makes the staging file, before it writes each chunk, inside the publish and before it
    /// puts the object in the index: a cache that lost the lock of its directory meanwhile
    /// writes nothing more ([`Stop::Lost`]). The lock is noticed lost by a tick at most once a
    /// second, so the step that is running when the lock file is replaced can still finish.
    ///
    /// What `read` says is read: the object of `key`, or a range of another
    /// object kept as `key`. A range is checked against the hash it was asked under, and its
    /// sidecar takes that hash as its tag, since the bytes are named by their content.
    pub(super) async fn fetch_from(
        self: &Arc<Self>,
        key: &str,
        name: &Name,
        live: &watch::Sender<Live>,
        read: &Read<'_>,
    ) -> Result<Filled, Stop> {
        let generation = self.generation();
        let (from, start, meta, wanted) = match read {
            Read::Object => {
                let meta = self.upstream_head(key).await?;
                tagged(key, &meta)?;
                (key, 0, meta, name.content.as_deref())
            }
            Read::Headed(meta) => (key, 0, meta.clone(), name.content.as_deref()),
            Read::Range {
                from,
                offset,
                size,
                sha256,
            } => {
                let meta = Meta {
                    size: *size,
                    tag: (*sha256).to_owned(),
                    modified: self.clock.now(),
                };
                (*from, *offset, meta, Some(*sha256))
            }
        };
        let Some(reservation) = self.reserve(meta.size) else {
            bump(&self.counters.fills_skipped);
            return Ok(Filled::Skipped);
        };
        if !self.owns(generation) {
            return Err(Stop::Lost);
        }
        let mut staged = Staged::create(self).await?;
        let mut hasher = Sha256::new();
        let mut offset = 0;
        // The readers read the staged file through a descriptor of its own, opened for the fills only
        // (a keep has no readers).
        let reader = Arc::new(tokio::fs::File::open(&staged.path).await?.into_std().await);
        let (about, file) = (meta.clone(), reader);
        let progress = |written| Live::Staging {
            meta: about.clone(),
            file: Arc::clone(&file),
            written,
        };
        let _ = live.send_replace(progress(0));
        while offset < meta.size {
            let want = self.config.fill_chunk.max(1).min(meta.size - offset);
            let range = ByteRange::new(start + offset, want);
            let chunk = self.upstream_get(from, range).await?;
            if chunk.len() as u64 != want {
                return Err(Stop::Short);
            }
            if !self.owns(generation) {
                return Err(Stop::Lost);
            }
            hasher.update(&chunk);
            staged.file.write_all(&chunk).await?;
            // The bytes are in the file for a reader of it once they are flushed.
            staged.file.flush().await?;
            offset += want;
            let _ = live.send_replace(progress(offset));
        }
        staged.finish().await?;
        // A key named by its content is checked by the hash below. Any other key could have been
        // written over while it was read, and the file would hold bytes of two versions.
        let rechecked = matches!(read, Read::Object) && name.content.is_none();
        if rechecked && !self.same_version(&self.upstream_head(key).await?, &meta) {
            return Err(Stop::Changed);
        }
        let digest = hex::encode(hasher.finalize());
        if wanted.is_some_and(|wanted| wanted != digest) {
            return Err(Stop::Mismatch);
        }
        let sidecar = Sidecar {
            key: key.to_owned(),
            size: meta.size,
            sha256: digest,
            tag: meta.tag.clone(),
            modified: meta.modified,
            origin: match read {
                Read::Range { from, offset, .. } if *from != key || *offset != 0 => Origin::Span,
                _ => Origin::Object,
            },
        };
        let fresh = match self.commit(&staged, name, &sidecar, generation).await? {
            Published::New => true,
            Published::Existed => false,
            Published::Lost => return Err(Stop::Lost),
        };
        #[cfg(test)]
        self.trace_after_publish();
        if !self.owns(generation) {
            if fresh {
                self.remove_linked(&name.hex);
            }
            return Err(Stop::Lost);
        }
        let checked = Some(self.clock.now());
        let kept = (meta.size, meta.tag, meta.modified);
        locked(&self.index).insert_from(&name.hex, kept, checked, sidecar.origin);
        // A delete that came while the object was being read marked this fill: what it read is
        // gone from the upstream, so it is not kept (the mark is looked for after the entry is
        // in the index; see `forget`).
        if self.is_invalidated(&name.hex) {
            self.drop_entry(&name.hex);
            return Err(Stop::Invalidated);
        }
        drop(reservation);
        if fresh {
            bump(&self.counters.fills);
        }
        self.relieve().await;
        Ok(Filled::Kept)
    }

    /// Links the staged file into the cache, on a thread that may block.
    #[expect(
        clippy::expect_used,
        reason = "a blocking task's panic is a bug, passed on"
    )]
    async fn commit(
        self: &Arc<Self>,
        staged: &Staged,
        name: &Name,
        sidecar: &Sidecar,
        generation: u64,
    ) -> io::Result<Published> {
        let publishing = self.begin_commit(staged, name, sidecar, generation);
        publishing.await.expect("a publish does not panic")
    }

    /// Removes the data file and the sidecar that a fill or a keep of this cache linked just
    /// now, when it finds that the cache no longer owns the directory.
    ///
    /// This is the one delete a cache makes in a directory it does not own. The caller linked
    /// these names itself (the publish said `New`), but the new owner may publish or index the
    /// same name in that very instant, so the removal can take away an object that is the new
    /// owner's. That costs one refill and nothing else: whoever made it, the object is a whole
    /// object of the key, so neither a read before the removal nor one after it gets wrong
    /// bytes. Leaving the object is the worse choice: the new owner may not index it, and then
    /// nothing evicts it, and a later scan indexes it, stale if the key was deleted at the
    /// upstream in between. The call covers the whole span from the check before the rename,
    /// through the rename, the link and the return of the publish. What is left is a whole
    /// object that is on the disk for the moments until this removes it.
    pub(super) fn remove_linked(&self, hex: &str) {
        let root = &self.config.path;
        let _ = std::fs::remove_file(data_path(root, hex));
        let _ = std::fs::remove_file(meta_path(root, hex));
    }

    /// Starts the link of the staged file into the cache on a thread that may block, and gives
    /// the handle of the job, which goes on when the handle is dropped. The job publishes only
    /// while the cache still owns the directory as of `generation` (see `Inner::owns`): fills
    /// and keeps both run it, and both give up when it says [`Published::Lost`].
    pub(super) fn begin_commit(
        self: &Arc<Self>,
        staged: &Staged,
        name: &Name,
        sidecar: &Sidecar,
        generation: u64,
    ) -> JoinHandle<io::Result<Published>> {
        let disk = Arc::clone(&self.disk);
        let root = self.config.path.clone();
        let hex = name.hex.clone();
        let (from, temporary) = (staged.path.clone(), staged.sidecar.clone());
        let text = serde_json::to_vec(sidecar).unwrap_or_default();
        let inner = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let owns = || inner.owns(generation);
            publish(&*disk, &root, &hex, (&from, &temporary), &text, &owns)
        })
    }

    /// Runs a pass of the eviction if the cache is above its high water mark.
    #[expect(
        clippy::expect_used,
        reason = "a blocking task's panic is a bug, passed on"
    )]
    pub(super) async fn relieve(self: &Arc<Self>) {
        let held = locked(&self.index).bytes();
        if held > percent_of(self.config.budget, self.config.high_water) {
            let inner = Arc::clone(self);
            let pass = tokio::task::spawn_blocking(move || inner.evict_pass());
            pass.await.expect("a pass does not panic");
        }
    }
}
