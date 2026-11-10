//! The local cache of a build node: a read-through disk cache in front of one upstream.
//!
//! A [`LocalCache`] is a [`Backend`] over an upstream [`Backend`] (an S3 endpoint, in the
//! binary: any S3 store, such as a bucket of MinIO or a storage node's tierstore). By default the S3 front serves it as a read cache and
//! refuses every write before it gets here (see `ServerConfig::cache`), so a cache has no
//! journal, no lease and no reconciler, and nothing to recover but a directory of files it can
//! delete. The only things that run on their own are the fills a miss starts, the eviction
//! pass ([`LocalCache::maintain`]) and the scrub ([`LocalCache::scrub`]).
//!
//! # Writes
//!
//! With [`CacheConfig::write_through`] (`write_through = true` in `[local_cache]`) the cache
//! accepts writes and sends every one of them to the upstream: nothing is acknowledged before
//! the upstream has it, and nothing is only local. The module `write` has what each call does.
//! A put under a `cache = true` policy, create-only or plain, is also kept on the node (module `keep`):
//! the body is copied to `.stage/` as it streams to the upstream, and once the upstream has
//! acknowledged it and said what tag it gave the object, the copy is put in the cache before the
//! answer. That copy is best effort and never fails the put.
//! Without the setting every write call of the [`Backend`] is `Unsupported`, as the front
//! refuses it before.
//!
//! # What is kept
//!
//! A key is kept if the longest prefix of the policies that holds it says `cache = true` and it
//! is not under `_journal/`, `_tiering/` or `_entries/`. A get, ranged get or head of a key
//! under those prefixes is refused with [`Error::Reserved`] without a look at the disk or the
//! upstream, and a listing leaves them out. Every other key that is not kept, and every other
//! listing, goes to the upstream as it is.
//!
//! - A hit (HEAD, GET, ranged GET of an object that is in the cache) is answered from the disk
//!   and makes no call to the upstream, unless [`CacheConfig::revalidate_after`] is set and the
//!   entry is older than it: then one HEAD checks the entry first (module `revalidate`).
//! - A miss on a whole GET fills the object. A miss on a ranged GET fills the whole object if it
//!   is at most 64 MiB, and otherwise passes the range through and keeps nothing. A HEAD of an
//!   object that is not kept goes to the upstream and fills nothing.
//! - The misses of one key fill it once: the first starts the fill on a task of its own, the
//!   others wait for it. A fill that no reader waits for any more still ends.
//!
//! # A fill
//!
//! The object is read from the upstream in chunks (ranged reads of `fill_chunk` bytes, so that a
//! fill holds one chunk in memory whatever the object's size), each chunk written to a file in
//! `<path>/.stage/` and hashed as it passes. The file is kept only if it is whole: the upstream
//! gave every byte of the size it said, and a key named by its content (`cas/l/<sha256>`) has
//! bytes that hash to the name. Then the sidecar is renamed into place and the file is linked
//! into `obj/` with a create-only link, so a name appears whole or not at all (layout:
//! [`layout`]). A fill that fails is counted by reason and removed.
//!
//! # Fills for the fill socket
//!
//! The filesystem process of the node asks for blobs over the fill socket (see
//! [`fill_socket`](crate::server::fill_socket)): `source` fills a blob that lies inside a larger
//! upstream object, ranged, under the reserved key `_tiering/fill/<sha256>`, and a whole
//! `cas/l/<sha256>`, with the same fetch as every fill. Those fills have their own permits
//! (`fill_socket_concurrency`) and wait for one up to `fill_socket_wait`; they never take one of
//! `max_concurrent_fills`, and the S3 path never takes theirs. Concurrent requests for one blob share
//! one fetch. A cache that does not hold the lock fills nothing for the socket.
//!
//! # A read
//!
//! A reader of the fill is given the bytes as the fill writes them (see `stream.rs`): every read
//! of the cache is a stream of chunks of at most `fill_chunk` bytes, so the memory of a read is a
//! chunk, not the size of the object. The readers of a key read the file the fill stages, all
//! through the one descriptor the fill writes it with (a read and a write at an offset share
//! nothing else), never a copy in memory. The last `fill_chunk` bytes of the window a reader asked
//! for are given only after the fill has said the object is good, so a ranged read of an object of
//! at most 64 MiB waits for the whole fill before its last chunk.
//!
//! - A fill that fails because the data is bad (the bytes do not hash to the name, the object was
//!   written over: a version the upstream says is another (by default a changed tag or size; see
//!   [`Backend::same_version`]), or a short or long body) ends the readers' bodies with an
//!   error, never cleanly.
//! - A fill that a write or a delete through the cache marks (`forget`), or that fails for a
//!   reason of the cache (no room, the directory), is finished for each reader from the
//!   upstream, from the byte after the last one it was given. A reader given a byte goes on only
//!   if the upstream still has the version the byte came from (a HEAD, and another before the
//!   last chunk): it gets the error of the upstream (`NotFound` after a delete) or `Unavailable`
//!   after a write, never a clean end. A reader given no byte is answered as if there had been no
//!   fill. The bytes read this way are not hashed, as a pass-through never was.
//! - A read of more than one chunk from the upstream (a key not kept, a range over 64 MiB, the
//!   rest of a read) is held to the version it began with in the same way.
//! - A client that stops reading keeps the unlinked file of an evicted object, and its blocks, on
//!   the disk until the connection's idle time ends it: those blocks are outside the budget.
//!
//! A fill is skipped, and counted, when the object is larger than the budget or the cache, with the
//! bytes of the fills that are running, would pass its critical mark; the reader is served from
//! the upstream. A key that is not named by its content is read twice by HEAD, before the first
//! chunk and after the last: if the upstream says the version changed (see [`Backend::same_version`]), the object was
//! written over during the fill and is not kept, and the readers get an error. A key named by its content whose bytes
//! do not hash to the name is the reader's error, not passed through. A hit is not hashed again:
//! the scrub does that.
//!
//! At most `max_concurrent_fills` fills run at once, each holding one chunk (`fill_chunk`, 8 MiB)
//! in memory at a time, so the fills hold at most that many chunks. A miss of a key that has no
//! fill running, when every permit is taken, does not wait for one: it is served from the upstream
//! as a pass-through, keeps nothing and is counted in `fills_skipped` and `fills_skipped_busy`.
//! The readers of a key that has a fill running share it and need no permit. A fill that panics
//! or is cancelled frees its key and its permit, and the readers that waited for it get an error
//! (`Unavailable`, counted as a failed fill with the reason `aborted`).
//!
//! # Eviction
//!
//! The cache keeps its objects under `budget` bytes. Above `high_water` percent of it, and
//! whenever the file system's free space is under `free_floor` percent, a pass
//! ([`LocalCache::evict`]) removes the least recently used objects until the cache is down to
//! `low_water` percent and the free space is back at the floor. The order of use is in memory,
//! seeded at start from the files' times and written back to them (`utimensat`, once per object
//! per pass however often it was used) so that a restart keeps it. An object with more than one
//! link is in use by another tree: it is skipped, counted, and stays charged to the budget.
//! So is an object that a tree pins (module `pins`), counted separately. Files in `.stage/` are
//! never evicted and are removed at start.
//!
//! # The lock and read-only mode
//!
//! A cache opened with [`LocalCache::open_locked`] takes an exclusive lock on `<path>/.lock`.
//! A second process over the directory (the next release, started beside the one that runs) is
//! read-only until the first exits: it serves the hits its scan found, sends every miss and every
//! write to the upstream, and creates, changes and removes nothing in the directory (no clean-up
//! at open, no probe, no fill, no eviction, no file time, no deletion on a read error). When the
//! lock is free, [`LocalCache::tick`] takes it, cleans up, scans again and makes the cache
//! read-write. The module `hold` has the rules.
//!
//! # Scrub
//!
//! A background walk hashes each object again at a bounded rate and drops the ones whose bytes
//! changed on the disk; see [`scrub`].

mod calls;
mod clean;
mod disk;
mod evict;
mod fill;
mod hold;
mod index;
mod keep;
pub(super) mod layout;
mod listing;
mod metrics;
mod pin_set;
mod pins;
mod revalidate;
mod scrub;
mod selfcheck;
mod source;
mod stream;
mod write;

#[cfg(test)]
mod clean_tests;
#[cfg(test)]
mod evict_tests;
#[cfg(test)]
mod fill_tests;
#[cfg(test)]
mod forget_tests;
#[cfg(test)]
mod front_tests;
#[cfg(test)]
mod hit_tests;
#[cfg(test)]
mod hold_tests;
#[cfg(test)]
mod keep_head_tests;
#[cfg(test)]
mod keep_lock_tests;
#[cfg(test)]
mod keep_replace_tests;
#[cfg(test)]
mod keep_s3_tests;
#[cfg(test)]
mod keep_tests;
#[cfg(test)]
mod limits_tests;
#[cfg(test)]
mod listing_tests;
#[cfg(test)]
mod lost_tests;
#[cfg(test)]
mod metrics_text_tests;
#[cfg(test)]
pub(in crate::server) mod pack_testing;
#[cfg(test)]
mod pin_set_tests;
#[cfg(test)]
mod pins_tests;
#[cfg(test)]
mod read_only_tasks_tests;
#[cfg(test)]
mod read_only_tests;
#[cfg(test)]
mod revalidate_tests;
#[cfg(test)]
mod scrub_tests;
#[cfg(test)]
mod selfcheck_tests;
#[cfg(test)]
mod source_tests;
#[cfg(test)]
mod stale_index_tests;
#[cfg(test)]
mod start_tests;
#[cfg(test)]
mod stream_tests;
#[cfg(test)]
pub(in crate::server) mod testing;
#[cfg(test)]
mod tiered_tests;
#[cfg(test)]
mod upstream_counts_tests;
#[cfg(test)]
mod write_front_tests;
#[cfg(test)]
mod write_race_tests;
#[cfg(test)]
mod write_tests;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::StreamExt as _;
use tokio::sync::Semaphore;

use crate::journal::is_reserved;
use crate::server::lock;
use crate::{
    Backend, ByteRange, Clock, Error, Listed, Meta, OpenUpload, PartBody, PutAnswer, PutClaim,
    ReadBody, UploadedPart, UploadsAfter,
};

pub use disk::{Disk, Space, SystemDisk};
pub use hold::{Mode, Tick};
pub use metrics::Counters;
pub use pins::{PinAnswer, PinError};
pub use scrub::{Every, SCRUB_MIN_ROUND, SCRUB_RATE, SCRUB_TICK, ScrubPass, Scrubber, Ticker};
pub use selfcheck::{LocalTierFailure, check_local};

use fill::Pending;
use hold::{Hold, Standing};
use index::Index;
use keep::Sent;
use layout::{Name, data_path, name_of};
use metrics::{add, bump};
use revalidate::Verdict;
pub(super) use source::{Blob, FillError};
pub(super) use source::{blob_path, manifest_path};
use stream::eager;

/// The most an uncached object may be, for a ranged read of it to fill the whole of it.
pub const WHOLE_FILL_LIMIT: u64 = 64 << 20;

/// The bytes a fill reads from the upstream at a time.
pub const FILL_CHUNK: u64 = 8 << 20;

/// The fills that may run at once, unless the settings say otherwise.
pub const MAX_CONCURRENT_FILLS: usize = 4;

/// The keys a read-only cache remembers it deleted or replaced, unless the settings say
/// otherwise: 65536 names of 64 bytes, about 4 MiB.
pub const FORGET_LIMIT: usize = 65_536;

/// How long the eviction loop of the binary waits between two passes, unless the settings say
/// otherwise.
pub const PASS_INTERVAL: Duration = Duration::from_secs(10);

/// How long a cache waits between two tries of the lock of its directory, unless the settings say
/// otherwise.
pub const LOCK_INTERVAL: Duration = Duration::from_secs(1);

/// The fills of the fill socket that may run at once, unless the settings say otherwise.
pub const FILL_SOCKET_CONCURRENCY: usize = 16;

/// How long a request of the fill socket waits for a permit of its pool, unless the settings say
/// otherwise.
pub const FILL_SOCKET_WAIT: Duration = Duration::from_secs(60);

/// The keeps of a put that may run at once, unless the settings say otherwise.
pub const MAX_CONCURRENT_KEEPS: usize = 8;

/// How long a put waits for its keep after the upstream has the object, unless the settings say
/// otherwise.
pub const KEEP_WAIT: Duration = Duration::from_secs(5);

/// How long a keep may run in all, from the upstream's acknowledgement, before it is abandoned,
/// unless the settings say otherwise.
pub const KEEP_DEADLINE: Duration = Duration::from_secs(60);

/// The text of the refusal of a write at the Backend level (the front refuses it earlier).
const READ_ONLY: &str = "this tierstore is a read cache";

/// What a cache calls each time a put's copy has been put in the index (see
/// [`LocalCache::open_observed`]): it is called on the task of the keep, so it must not block.
pub type KeepObserver = Arc<dyn Fn() + Send + Sync>;

/// The settings of a [`LocalCache`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct CacheConfig {
    /// The directory of the cache: made if it is not there.
    pub path: PathBuf,
    /// The bytes the cache is meant to hold.
    pub budget: u64,
    /// The percent of the budget above which a pass evicts.
    pub high_water: u8,
    /// The percent of the budget a pass evicts down to.
    pub low_water: u8,
    /// The percent of the budget above which a fill is skipped.
    pub critical: u8,
    /// The most bytes of objects that trees may pin (see [`LocalCache::pin`]); a pin that would
    /// take the pinned bytes over it is refused. Half the budget unless the settings say otherwise.
    pub pin_limit: u64,
    /// The percent of the file system that is kept free: below it a pass evicts.
    pub free_floor: u8,
    /// The prefixes and whether the keys under each are kept; the longest prefix that holds a
    /// key decides, and a key under none is not kept.
    pub policies: Vec<(String, bool)>,
    /// The bytes a fill reads at a time.
    pub fill_chunk: u64,
    /// The most an object may be for a ranged read of it to fill it.
    pub whole_limit: u64,
    /// The most fills that run at once. A miss that finds all of them running is served from
    /// the upstream and keeps nothing. At least one (0 is read as 1).
    pub max_concurrent_fills: usize,
    /// The most fills that the fill socket runs at once: a pool of its own, which the S3 path's
    /// `max_concurrent_fills` never draws on. A request that finds them all running waits for one
    /// (see `fill_socket_wait`). At least one (0 is read as 1).
    pub fill_socket_concurrency: usize,
    /// The longest a request of the fill socket waits for a permit of its pool before it is
    /// answered `busy`.
    pub fill_socket_wait: Duration,
    /// The most create-only puts that are copied to the stage directory at once, apart from the
    /// fills. A put that finds them all running is not kept. At least one (0 is read as 1).
    pub max_concurrent_keeps: usize,
    /// The longest a put waits, after the upstream has the object, for its copy to be put in the
    /// cache. Past it the put is answered and the keep goes on in the background; the object is
    /// at the upstream either way, so a read that misses costs only the trip.
    pub keep_wait: Duration,
    /// The longest a keep runs, from the upstream's acknowledgement, before it is abandoned (its
    /// stage file removed, its permit and room given back, its place in the table of the fills
    /// freed) and counted as `deadline`. Not below `keep_wait`.
    pub keep_deadline: Duration,
    /// The most bytes of a put's body that may wait for the disk before its copy is dropped
    /// (counted as `slow`), so that the upstream never waits for the disk. The first chunk is always
    /// taken. It bounds the memory of the copy of one put: the keeps that run at once hold at most
    /// `max_concurrent_keeps` times this.
    pub keep_queue_bytes: u64,
    /// Whether the cache takes writes and sends them to the upstream. Off, every write call is
    /// `Unsupported`. The binary sets the front's `cache_writes` from the same setting, so that
    /// the front does not refuse the writes either.
    pub write_through: bool,
    /// The most keys a read-only cache remembers it deleted or replaced through the upstream
    /// (see the module `hold`). Past it the cache is emptied when it takes the lock.
    pub forget_limit: usize,
    /// How long the eviction loop waits between two passes (`maintain`).
    pub pass_interval: Duration,
    /// How long the loop that holds the lock of the directory waits between two tries (`hold`).
    pub lock_interval: Duration,
    /// The time between two ticks of the scrub. The scrub reads `scrub_rate` bytes a second, so
    /// `scrub_rate` times this to the second at each tick.
    pub scrub_tick: Duration,
    /// The most pages of the upstream one listing reads to make up for the names it leaves out.
    /// At least one (0 is read as 1).
    pub listing_calls: u32,
    /// How old a hit may be, since its entry was filled or last checked, before it is checked
    /// against the upstream with one HEAD (see the module `revalidate`). `None`, the default,
    /// never checks: a hit is served from the disk until the entry is evicted.
    pub revalidate_after: Option<Duration>,
    /// Whether a create-only put asks the upstream for the key (one HEAD) before it reads the
    /// body (default `true`; a plain put never does). The HEAD spares an upstream that refuses a key it holds only after
    /// the whole body has arrived the transfer of a large blob it has already; an upstream that
    /// answers before it reads the body (a proxy whose format is `active-marks`) makes it three
    /// requests more of the proxy for every put of a new key (a hot HEAD, a marker GET and a cold
    /// HEAD), and with `false` the put is only the upstream's own (see `put_through`, in `keep`).
    pub head_before_put: bool,
}

impl CacheConfig {
    /// A cache in `path` of `budget` bytes, with the water marks at 90, 75 and 95 percent, a
    /// floor of 10 percent free, a pin limit of half the budget, the fill sizes of the binary, and no writes.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, budget: u64, policies: Vec<(String, bool)>) -> Self {
        Self {
            path: path.into(),
            budget,
            high_water: 90,
            low_water: 75,
            critical: 95,
            pin_limit: budget / 2,
            free_floor: 10,
            policies,
            fill_chunk: FILL_CHUNK,
            whole_limit: WHOLE_FILL_LIMIT,
            max_concurrent_fills: MAX_CONCURRENT_FILLS,
            fill_socket_concurrency: FILL_SOCKET_CONCURRENCY,
            fill_socket_wait: FILL_SOCKET_WAIT,
            max_concurrent_keeps: MAX_CONCURRENT_KEEPS,
            keep_wait: KEEP_WAIT,
            keep_deadline: KEEP_DEADLINE,
            keep_queue_bytes: keep::KEEP_QUEUE_BYTES,
            write_through: false,
            forget_limit: FORGET_LIMIT,
            pass_interval: PASS_INTERVAL,
            lock_interval: LOCK_INTERVAL,
            scrub_tick: SCRUB_TICK,
            listing_calls: listing::LISTING_CALLS,
            revalidate_after: None,
            head_before_put: true,
        }
    }

    /// The most the budget may be on a file system of `total` bytes: its size less the
    /// `free_floor` percent that is kept free.
    #[must_use]
    pub fn room(&self, total: u64) -> u64 {
        total.saturating_sub(percent_of(total, self.free_floor))
    }
}

/// Refuses a read of a key that the proxy keeps for itself: the upstream may be a plain bucket
/// that holds it, and nothing else on the way refuses it.
fn refuse_reserved(key: &str) -> Result<(), Error> {
    if is_reserved(key) {
        return Err(Error::Reserved(key.to_owned()));
    }
    Ok(())
}

/// `percent` percent of `total`.
fn percent_of(total: u64, percent: u8) -> u64 {
    let share = u128::from(total) * u128::from(percent) / 100;
    u64::try_from(share).unwrap_or(u64::MAX)
}

/// The mutex's contents, whether or not a thread panicked holding it: the state it guards is
/// kept consistent by each step.
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The offset and length that `range` asks of an object of `size` bytes.
fn window(range: Option<ByteRange>, size: u64) -> Result<(u64, u64), Error> {
    let Some(range) = range else {
        return Ok((0, size));
    };
    if range.offset() > size {
        let text = format!(
            "range starting at {} is past the end of the {size}-byte object",
            range.offset()
        );
        return Err(Error::Other(text));
    }
    Ok((range.offset(), range.size().min(size - range.offset())))
}

/// Whether `error` says the file is not there, or not as long as the cache believes.
fn gone(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::UnexpectedEof
    )
}

/// What the cache is made of; a [`LocalCache`] is a handle to one, so a fill that runs on its
/// own task keeps it alive.
pub(super) struct Inner {
    upstream: Arc<dyn Backend>,
    disk: Arc<dyn Disk>,
    clock: Arc<dyn Clock>,
    config: CacheConfig,
    index: Mutex<Index>,
    counters: Counters,
    /// The fills that are running, by the name of the object.
    flights: Mutex<HashMap<String, Pending>>,
    /// The fills that are running for an object that was deleted or replaced since they began:
    /// they must not keep what they read. A name is put here only while it has a fill in
    /// `flights`, and leaves with it; the lock is taken with `flights` held.
    invalidated: Mutex<HashSet<String>>,
    /// The puts that are running, by the name of the object (see `Writing`).
    writers: Mutex<HashMap<String, write::Writers>>,
    /// The number of the next file in `.stage/`.
    staged: AtomicU64,
    /// What every name in `.stage/` that this cache makes starts with: it is not the prefix of
    /// the name of any other cache, of this process or of another, that has the directory open,
    /// so that none of them removes the staging file of another.
    stage_prefix: String,
    /// The bytes of the fills that are running and have no object in the index yet.
    reserved: AtomicU64,
    /// One permit for each fill that may run; a fill holds one until it ends.
    permits: Arc<Semaphore>,
    /// One permit for each fill of the fill socket that may run; a request waits for one.
    source_permits: Arc<Semaphore>,
    /// The fills of the fill socket that are running, by the name of the object: concurrent
    /// requests for one blob share the fetch. A delete or put of the name marks these fills in
    /// `invalidated` as it does those of `flights` (lock order: `flights`, `source_flights`,
    /// `invalidated`).
    source_flights: Mutex<HashMap<String, source::Flight>>,
    /// One permit for each put that may be copied to the stage directory; held until it ends.
    keeps: Arc<Semaphore>,
    /// Called after a keep has put its object in the index.
    observer: Option<KeepObserver>,
    /// Run by a test just before a kept object is put in the index (see `keep`).
    #[cfg(test)]
    before_insert: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// Whether a pass of the eviction is running.
    evicting: AtomicBool,
    /// Whether this cache may write to its directory, and the lock that says so.
    hold: Hold,
}

/// A prefix for the staging files of one cache that no other cache has: the process id, the time
/// of the open in nanoseconds and the number of caches opened in this process before it.
fn stage_prefix() -> String {
    static OPENED: AtomicU64 = AtomicU64::new(0);
    #[expect(
        clippy::disallowed_methods,
        reason = "a name no other cache shares, not a time anyone judges"
    )]
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    let nanos = since.map_or(0, |elapsed| elapsed.as_nanos());
    let number = OPENED.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{nanos:x}-{number:x}", std::process::id())
}

/// A read-through cache of an upstream, on a directory of the local file system.
#[derive(Clone)]
pub struct LocalCache {
    inner: Arc<Inner>,
}

impl LocalCache {
    /// The settings the cache was opened with.
    #[must_use]
    pub fn config(&self) -> &CacheConfig {
        &self.inner.config
    }

    /// Opens the cache in `config.path`: makes the directory, removes what a fill that was
    /// running left in `.stage/`, and reads the objects back (an object whose sidecar does not
    /// say what the file holds is deleted), the least recently used first by the time of the
    /// file.
    ///
    /// This cache takes no lock: it is read-write whatever else has the directory open. The
    /// binary uses [`open_locked`](Self::open_locked); a second cache over a directory that has
    /// one must not use this.
    ///
    /// # Errors
    /// What the system said about the directory.
    pub fn open(
        config: CacheConfig,
        upstream: Arc<dyn Backend>,
        disk: Arc<dyn Disk>,
        clock: Arc<dyn Clock>,
    ) -> io::Result<Self> {
        Self::start(config, upstream, disk, clock, Standing::Lockless, None)
    }

    /// Opens the cache as [`open`](Self::open) does, and calls `observer` each time the copy of a
    /// put has been put in the index, which is the end of a keep that kept the object (a keep
    /// that was skipped, abandoned or marked deleted does not call it). A caller that waits for
    /// its own put's copy waits for this, not for the put, which may be answered first.
    ///
    /// # Errors
    /// What the system said about the directory.
    pub fn open_observed(
        config: CacheConfig,
        upstream: Arc<dyn Backend>,
        disk: Arc<dyn Disk>,
        clock: Arc<dyn Clock>,
        observer: Option<KeepObserver>,
    ) -> io::Result<Self> {
        Self::start(config, upstream, disk, clock, Standing::Lockless, observer)
    }

    /// Opens the cache as [`open`](Self::open) does, if the lock of the directory (`.lock`) is
    /// free: this cache holds it and is read-write. If another process holds it, this one is
    /// read-only: it reads the directory as it finds it and changes nothing in it (see
    /// [`Mode`]), and [`tick`](Self::tick), which [`hold`](Self::hold) calls, takes the lock
    /// when it is free and makes the cache read-write.
    ///
    /// # Errors
    /// What the system said about the directory or the lock file.
    pub fn open_locked(
        config: CacheConfig,
        upstream: Arc<dyn Backend>,
        disk: Arc<dyn Disk>,
        clock: Arc<dyn Clock>,
    ) -> io::Result<Self> {
        std::fs::create_dir_all(&config.path)?;
        let standing = match lock::try_take(&config.path.join(hold::LOCK))? {
            Some(file) => Standing::Holding(file),
            None => Standing::Waiting,
        };
        Self::start(config, upstream, disk, clock, standing, None)
    }

    /// The cache over `config.path` as `standing` says it may treat it.
    fn start(
        config: CacheConfig,
        upstream: Arc<dyn Backend>,
        disk: Arc<dyn Disk>,
        clock: Arc<dyn Clock>,
        standing: Standing,
        observer: Option<KeepObserver>,
    ) -> io::Result<Self> {
        let counters = Counters::default();
        let scanned = if matches!(standing, Standing::Waiting) {
            layout::read_only_scan(&config.path)
        } else {
            let removed = layout::clear_stage(&config.path)?;
            let scanned = layout::scan(&config.path)?;
            add(&counters.discarded_at_start, scanned.discarded + removed);
            scanned
        };
        let mut index = Index::of(scanned.found, scanned.unclean);
        if !matches!(standing, Standing::Waiting) {
            pins::restore(&config, &mut index);
        }
        let permits = Arc::new(Semaphore::new(config.max_concurrent_fills.max(1)));
        let keeps = Arc::new(Semaphore::new(config.max_concurrent_keeps.max(1)));
        let source_permits = Arc::new(Semaphore::new(config.fill_socket_concurrency.max(1)));
        let inner = Inner {
            upstream,
            disk,
            clock,
            config,
            index: Mutex::new(index),
            counters,
            flights: Mutex::new(HashMap::new()),
            invalidated: Mutex::new(HashSet::new()),
            writers: Mutex::new(HashMap::new()),
            staged: AtomicU64::new(0),
            stage_prefix: stage_prefix(),
            reserved: AtomicU64::new(0),
            permits,
            source_permits,
            source_flights: Mutex::new(HashMap::new()),
            keeps,
            observer,
            #[cfg(test)]
            before_insert: Mutex::new(None),
            evicting: AtomicBool::new(false),
            hold: Hold::new(standing),
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// The directory of the cache.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.inner.config.path
    }

    /// The bytes and the number of the objects in the cache.
    #[must_use]
    pub fn held(&self) -> (u64, usize) {
        let index = locked(&self.inner.index);
        (index.bytes(), index.len())
    }

    /// The counters.
    #[must_use]
    pub fn counters(&self) -> &Counters {
        &self.inner.counters
    }

    /// The text of `/metricz`: the counters, the bytes in the cache and the free space of the
    /// file system now.
    #[must_use]
    pub fn metrics(&self) -> String {
        let inner = &self.inner;
        let (bytes, objects) = self.held();
        let free = inner
            .disk
            .space(&inner.config.path)
            .map_or(0, |space| space.free);
        let read_write = inner.read_write();
        inner
            .counters
            .render(bytes, objects as u64, free, read_write)
    }
}

impl Inner {
    /// Whether `a` and `b` are one version of an object: the upstream says (see
    /// [`Backend::same_version`]), as only it knows what its tags stand for. The fill asks it, of
    /// two HEADs a read apart; the check of an old hit does not (see `revalidate`).
    pub(super) fn same_version(&self, a: &Meta, b: &Meta) -> bool {
        self.upstream.same_version(a, b)
    }

    /// The on-disk name of `key` if the cache keeps it. A reserved key never gets here: every
    /// read and write refuses it first.
    fn cacheable(&self, key: &str) -> Option<Name> {
        let policies = &self.config.policies;
        let holding = policies
            .iter()
            .filter(|(prefix, _)| key.starts_with(prefix.as_str()));
        let longest = holding.max_by_key(|(prefix, _)| prefix.len());
        longest.is_some_and(|(_, keep)| *keep).then(|| name_of(key))
    }

    /// What the cache says of the object, if it holds it.
    fn kept(&self, name: &Name) -> Option<Meta> {
        locked(&self.index).get(&name.hex).map(index::Entry::meta)
    }

    /// Whether the object the index holds is the one on the disk. A read-only cache's index is
    /// its scan, and the holder may have put the object again since, so the sidecar is read; a
    /// read-write cache, and a key named by its content, need no check.
    async fn unchanged(&self, name: &Name, meta: &Meta) -> bool {
        if self.read_write() || name.content.is_some() {
            return true;
        }
        let believed = stream::Believed {
            size: meta.size,
            tag: meta.tag.clone(),
        };
        let (root, hex) = (self.config.path.clone(), name.hex.clone());
        let reading = tokio::task::spawn_blocking(move || {
            stream::same_version_on_disk(&root, &hex, &believed)
        });
        reading.await.unwrap_or(false)
    }

    /// Removes the object from the index and, if this cache may write to its directory, from the
    /// disk. A read-only cache forgets the object and leaves its files: they are the holder's.
    /// It notes the name, and removes the files when it takes the lock (see `hold`), so that the
    /// object is not served from them then: it was deleted or replaced at the upstream.
    fn drop_entry(&self, hex: &str) {
        let _ = self.remove_entry(hex, false);
    }

    /// [`drop_entry`](Self::drop_entry) for the eviction: an object that is pinned stays. Says
    /// whether the object was removed.
    fn evict_entry(&self, hex: &str) -> bool {
        self.remove_entry(hex, true)
    }

    /// The work of both; `keep_pinned` leaves a pinned object where it is, looking at the pins
    /// under the lock of the index, so that a pin made since the eviction chose the object holds.
    fn remove_entry(&self, hex: &str, keep_pinned: bool) -> bool {
        // Asked first: a take that is running holds the answer until it has looked at the notes,
        // and an entry removed before that would be put back by the index it makes.
        let may_delete = self.may_delete(hex);
        #[cfg(test)]
        self.trace_between_delete_steps();
        {
            let mut index = locked(&self.index);
            if keep_pinned && index.pins.is_pinned(hex) {
                return false;
            }
            drop(index.remove(hex));
        }
        if may_delete {
            let root = &self.config.path;
            let _ = std::fs::remove_file(data_path(root, hex));
            let _ = std::fs::remove_file(layout::meta_path(root, hex));
        }
        true
    }
}

#[async_trait]
impl Backend for LocalCache {
    /// Whether a streamed body must come with its length: the upstream's answer once the cache
    /// takes writes, and `false` for a read cache, which takes no body.
    fn needs_length(&self) -> bool {
        let inner = &self.inner;
        inner.config.write_through && inner.upstream.needs_length()
    }

    fn same_version(&self, a: &Meta, b: &Meta) -> bool {
        self.inner.same_version(a, b)
    }

    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put_whole(key, body).await
    }

    async fn put_stream(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.inner.put_through(key, len, body, Sent::Creates).await
    }

    async fn put_stream_hot_only(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.inner
            .put_through(key, len, body, Sent::CreatesHotOnly)
            .await
    }

    /// A plain `PUT`: sent to the upstream as its plain put, and the cache keeps the new copy
    /// and forgets the old one (see `put_through`, in `keep`).
    async fn put_stream_replacing(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        claim: &PutClaim,
    ) -> Result<Option<PutAnswer>, Error> {
        let sent = Sent::Replaces(claim);
        self.inner.put_through(key, len, body, sent).await?;
        Ok(None)
    }

    async fn put_if_match(&self, _key: &str, _body: Bytes, _etag: &str) -> Result<String, Error> {
        Err(Error::Unsupported(READ_ONLY.to_owned()))
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete_through(key).await
    }

    async fn create_upload(&self, key: &str) -> Result<String, Error> {
        self.inner.create_through(key).await
    }

    async fn upload_part(
        &self,
        key: &str,
        id: &str,
        number: u32,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        let sent = self.inner.part_through(key, id, number, None, body);
        sent.await
    }

    async fn upload_part_with_len(
        &self,
        key: &str,
        id: &str,
        number: u32,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        let sent = self.inner.part_through(key, id, number, len, body);
        sent.await
    }

    async fn complete_upload(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        self.inner.complete_through(key, id, parts).await
    }

    async fn abort_upload(&self, key: &str, id: &str) -> Result<(), Error> {
        self.inner.abort_through(key, id).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        let mut body = self.get_stream(key, range).await?;
        let mut whole = BytesMut::new();
        while let Some(chunk) = body.next().await {
            whole.extend_from_slice(&chunk?);
        }
        Ok(whole.freeze())
    }

    async fn get_stream(&self, key: &str, range: Option<ByteRange>) -> Result<ReadBody, Error> {
        refuse_reserved(key)?;
        let inner = &self.inner;
        let passed = |range: Option<ByteRange>| {
            let offset = range.map_or(0, ByteRange::offset);
            eager(inner.chunked(key, offset, range.map(ByteRange::size), None))
        };
        let Some(name) = inner.cacheable(key) else {
            return passed(range).await;
        };
        // An old hit is checked first. An entry that is not the upstream's any more is dropped
        // by the check, and the read below is a miss.
        drop(inner.revalidate(key, &name).await);
        if let Some(body) = inner.open_kept(&name, range, true).await? {
            return Ok(body);
        }
        // A read of a kept key that the cache does not serve is one miss, counted here, before
        // any call to the upstream, whatever comes of the read.
        bump(&inner.counters.misses);
        if !inner.read_write() {
            // A read-only cache fills nothing: the miss goes straight to the upstream.
            return passed(range).await;
        }
        if let Some(wanted) = range {
            let meta = inner.upstream_head(key).await?;
            if meta.size > inner.config.whole_limit {
                return passed(Some(wanted)).await;
            }
        }
        match inner.join_or_start(key, &name) {
            Ok(pending) => eager(inner.follow(key, range, pending)).await,
            Err(fill::Filled::Kept) => match inner.open_kept(&name, range, false).await? {
                Some(body) => Ok(body),
                None => passed(range).await,
            },
            Err(fill::Filled::Skipped | fill::Filled::Changed) => passed(range).await,
        }
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        refuse_reserved(key)?;
        let inner = &self.inner;
        let name = inner.cacheable(key);
        let kept = name.as_ref().and_then(|name| inner.kept(name));
        // A HEAD the index answers is a head hit, and one the upstream answers a head miss.
        let (Some(meta), Some(name)) = (kept, name) else {
            bump(&inner.counters.head_misses);
            return inner.upstream_head(key).await;
        };
        // The check of an old hit is the HEAD that answers when the object is not the one kept.
        match inner.revalidate(key, &name).await {
            Verdict::Changed(found) => {
                bump(&inner.counters.head_misses);
                return Ok(found);
            }
            Verdict::Gone => {
                bump(&inner.counters.head_misses);
                return Err(Error::NotFound(key.to_owned()));
            }
            Verdict::Serve => {}
        }
        if inner.unchanged(&name, &meta).await {
            bump(&inner.counters.head_hits);
            Ok(meta)
        } else {
            bump(&inner.counters.head_misses);
            inner.upstream_head(key).await
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list_data(prefix).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        let page = self.inner.list_data_page(prefix, start_after, max_keys);
        page.await
    }

    async fn list_uploads(
        &self,
        prefix: &str,
        after: Option<UploadsAfter<'_>>,
        max_uploads: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        let found = self.inner.list_data_uploads(prefix, after, max_uploads);
        found.await
    }

    /// The parts of an upload are the upstream's, listed as its uploads are (also by a node
    /// that does not write: a listing changes nothing).
    async fn list_parts(
        &self,
        key: &str,
        id: &str,
        after: Option<u32>,
        max_parts: usize,
    ) -> Result<Vec<UploadedPart>, Error> {
        refuse_reserved(key)?;
        let upstream = &self.inner.upstream;
        upstream.list_parts(key, id, after, max_parts).await
    }
}
