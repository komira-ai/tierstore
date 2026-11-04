//! The proxy: a store over a hot and a cold backend.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use tokio::sync::{Mutex as AsyncMutex, Notify, OnceCell};
use tokio::time::Instant;

use crate::journal::{
    USAGE_KEY, Usage, encode_journal, is_reserved, journal_key, journal_prefix, load_checkpoint,
    proxy_key, seq_of,
};
use crate::metrics::{Counters, Read, Skipped, Tier};
use crate::{
    Backend, ByteRange, Clock, Entries, Error, JournalEntry, JournalOp, Listed, Meta,
    MonotonicClock, OpenUpload, Opened, PartBody, ReadBody, Span, UploadsAfter, Window,
};

/// The most calls to the cold tier that one listing of uploads makes to fill a page that the
/// filter of reserved keys cut short.
const UPLOAD_LISTING_CALLS: u32 = 100;

/// How many cold objects of a listing that may be reservations are asked about at once.
const LISTING_HEADS: usize = 16;

/// How many times a delete looks at the cold tier's object again after it changed under the
/// delete.
const DELETE_ROUNDS: u32 = 3;

mod copy_back;
#[cfg(test)]
mod copy_back_tests;
mod entry_delete;
mod entry_put;
#[cfg(test)]
mod entry_put_race_tests;
#[cfg(test)]
mod entry_put_tests;
mod heartbeat;
#[cfg(test)]
mod heartbeat_tests;
mod listing;
mod pause;
mod put;
mod read;
mod relay;

pub use entry_delete::DeleteWait;
use heartbeat::Heartbeat;
use pause::WriteMode;

/// The settings of a [`Proxy`].
#[derive(Clone)]
pub struct ProxyConfig {
    /// Names the proxy's journal and its checkpoint. It is not empty and has no `/`. Every
    /// running proxy has an id of its own, and a proxy that restarts keeps its id.
    pub proxy_id: String,
    /// The bytes the hot tier is meant to hold, which the usage the reconciler publishes is
    /// compared with.
    pub hot_budget: u64,
    /// The share of the budget (0.95, say) above which a read does not copy the object it
    /// found in the cold tier into the hot tier. A put is not changed by it: it still goes to
    /// the hot tier, at once, until the usage reaches the budget.
    pub critical: f64,
    /// The most entries one journal object holds. A batch that reaches it is written at once.
    pub batch_entries: usize,
    /// How long a batch waits for more entries before it is written.
    pub linger: Duration,
    /// How long the usage object that was read is used before it is read again, and how long
    /// the hot tier is taken to be full after it said so.
    pub usage_refresh: Duration,
    /// How long the usage object may stand unchanged, by the monotonic clock of this proxy
    /// (`mono`), before it is ignored, as if it were missing. The leader rewrites it every
    /// publication, so an object that no longer changes means the leader has stopped; no time that
    /// another machine wrote is read.
    pub usage_stale_after: Duration,
    /// The refresh interval: how long the format object (`_tiering/format`) that was read is used
    /// before it is read again. Under successful reads a proxy reads a pause at the next write
    /// after this interval; it is not a bound on how long a pause takes to reach a proxy, nor an
    /// acknowledgement of it, because a read that fails keeps the last answer and writes go on.
    pub format_refresh: Duration,
    /// The time, for the entries.
    pub clock: Arc<dyn Clock>,
    /// The monotonic clock of this process, for `live_until` (see [`Proxy::is_live`]) and for how
    /// long the usage object has stood unchanged (`usage_stale_after`). Nothing
    /// that another machine wrote is compared with it.
    pub mono: Arc<dyn Clock>,
    /// How often the proxy rewrites its registration once [`Proxy::start_heartbeat`] has been
    /// called, and the longest a call of the heartbeat may take.
    pub heartbeat: Duration,
    /// How long a proxy that is not heard of stays live, less `deadline_margin`: `live_until`
    /// is the send time of the last acknowledged beat plus this less the margin. Only the
    /// puts and deletes made on entries read it ([`Proxy::is_live`]); the reconciler's removal of
    /// registrations uses its own `registration_retention`.
    pub departure: Duration,
    /// The time that must be left before `live_until` for a call that destroys to start; the
    /// longest such a call may take.
    pub deadline_margin: Duration,
    /// How many times a put on entries looks again at the entry of its key when it was gone
    /// between the put's create and its read: past it the put is answered `Unavailable`.
    pub entry_rounds: u32,
    /// How a delete that met a copy in flight waits for the reconciler to release the entry.
    pub delete_wait: DeleteWait,
}

impl ProxyConfig {
    /// The settings for a proxy `proxy_id` and a hot tier of `hot_budget` bytes, with a
    /// critical share of 0.95, batches of at most 64 entries that
    /// wait 50 ms, a usage object that is read every second and ignored after 30 seconds, a
    /// format object that is read every 5 seconds, a heartbeat every 10 seconds, a departure time
    /// of 60 seconds and a margin of 10 seconds, on a [`MonotonicClock`] that reads zero now, and
    /// 3 looks at an entry, and the [`DeleteWait`] of [`DeleteWait::new`].
    #[must_use]
    pub fn new(proxy_id: &str, hot_budget: u64, clock: Arc<dyn Clock>) -> Self {
        Self {
            proxy_id: proxy_id.to_owned(),
            hot_budget,
            critical: 0.95,
            batch_entries: 64,
            linger: Duration::from_millis(50),
            usage_refresh: Duration::from_secs(1),
            usage_stale_after: Duration::from_secs(30),
            format_refresh: Duration::from_secs(5),
            clock,
            mono: Arc::new(MonotonicClock::new()),
            heartbeat: Duration::from_secs(10),
            departure: Duration::from_secs(60),
            deadline_margin: Duration::from_secs(10),
            entry_rounds: 3,
            delete_wait: DeleteWait::new(),
        }
    }
}

/// How full the hot tier is taken to be, least full first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Open,
    Critical,
    Full,
}

/// What the proxy believes of the hot tier's capacity.
#[derive(Default)]
struct Capacity {
    /// When the usage object was last read, and the bytes it gave if it gave any.
    read: Option<(Instant, Option<u64>)>,
    /// Until when the hot tier is taken to be full, after it said so.
    full_until: Option<Instant>,
    /// The usage object read last and when this proxy first read exactly that object, by its own
    /// monotonic clock: none if it was missing or unreadable. Unlike the bytes of `read`, it is
    /// kept whatever the age.
    seen: Option<UsageSeen>,
}

/// A usage object as it was read, and when this proxy first saw it.
struct UsageSeen {
    /// The body. The content tag of the object is a function of it, so a body that is the same as
    /// the one before is an object that was not rewritten.
    body: Bytes,
    /// The time of the monotonic clock of the first read that gave this body, in milliseconds.
    since: u64,
}

/// One journal object being filled, and the outcome of writing it.
#[derive(Default)]
struct Batch {
    entries: Mutex<Vec<JournalEntry>>,
    /// Notified when the batch reaches its size, to cut the linger short.
    full: Notify,
    /// Set once, by whichever caller waiting on the batch writes it.
    outcome: OnceCell<Result<(), Error>>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // No method panics while holding the lock, so a poisoned state is still whole.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn is_data(object: &Listed) -> bool {
    !is_reserved(&object.key)
}

fn refuse_reserved(key: &str) -> Result<(), Error> {
    if is_reserved(key) {
        Err(Error::Reserved(key.to_owned()))
    } else {
        Ok(())
    }
}

/// A store over a HOT and a COLD [`Backend`] that is itself a [`Backend`].
///
/// The proxy needs no state for correctness: any number of proxies, each with an id of its
/// own, work over the same two backends, and one that restarts finds what it needs in them.
/// Objects are immutable. "Flushed" means that the cold tier holds the object, and a
/// [`Reconciler`](crate::Reconciler) does the flushing from the journals the proxies write.
///
/// - **Get** reads the hot tier, else the cold tier. An object it read from the cold tier is
///   copied back into the hot tier, create-only, unless the hot tier is above the critical mark,
///   and a failed copy-back does not fail the read. Two ways, by the format. Before it is active
///   the read copies the object itself (the bytes the read gives, queued for a streamed
///   create-only write into the hot tier in a task of its own, abandoned if it falls more than
///   16 MiB behind or the caller drops the body) and journals the copy as a mirror (`PutMirror`)
///   with the cold tier's tag of the bytes it read and the hot tier's tag of the copy, which are
///   never compared with each other. Once it is active the read copies nothing and reads no entry:
///   it asks for the copy-back with a create-only `(CB, PENDING)` entry of the key in a task of its
///   own (nothing happens if an entry exists), and the entry reconciler copies the cold object under
///   the entry. A read of a range copies nothing back, and neither does a read that is not given
///   its body (a 304, a 412). A reservation in the cold tier is not an object: not found.
/// - **Head** reads the hot tier, else the cold tier, and copies nothing back; a reservation is
///   not found.
/// - **Put** is refused with [`Error::AlreadyExists`] if the key exists in the cold tier (it
///   may have been evicted from the hot tier) or in the hot tier. The cold tier is asked
///   first (a HEAD); then the object is written to the hot tier, create-only; then the cold
///   tier is asked again, and if it now holds the key (a copy to the cold tier and a delete from
///   the hot tier came between the first ask and the create) the object just created is removed
///   from the hot tier and the put is refused, since the reconciler copies an object to the cold
///   tier before it deletes it from the hot tier, so a create that wins the hot tier after that
///   delete always finds the cold copy; then an entry
///   is appended to the journal; then the put is acknowledged. **The acknowledgement waits
///   for the journal object that holds the entry to be written to the hot tier**: an object
///   that is acknowledged and not journaled would be in no one's list of objects to copy to
///   the cold tier, and so could be lost with the hot tier. A put whose journal append fails
///   is not acknowledged, and the object it wrote to the hot tier is removed, best effort:
///   left there it would never be copied, and would make a retry of the put fail with
///   [`Error::AlreadyExists`].
/// - **Streamed put** ([`Backend::put_stream`]) is a put whose body arrives as chunks and is
///   never held whole: the chunks go on to the hot tier's own streamed put (or the cold tier's,
///   where the hot tier is full), which makes the object visible, create-only, only after the
///   last chunk, and the journal entry is appended after that. A body that ends with an error
///   (a checksum that does not match, a connection that was cut) leaves no object in either
///   tier and no journal entry. The checks of a put (the key exists in either tier, reserved
///   keys) are made before the first chunk is taken. If the hot tier answers [`Error::Full`]
///   after it took some of the body, the bytes cannot be offered to the cold tier, so the put
///   fails with [`Error::Full`] (the tier is latched full, and the client's retry goes to the
///   cold tier); if it took none, the cold tier gets the whole body as for a held put.
/// - **Delete** removes the object from the cold tier first, by its content tag (looking again if
///   it changed, three times at most), then from the hot tier, then appends a delete entry. A
///   reservation in the cold tier is removed only if the hot tier holds the key; a bare one is a
///   put that has not written its object yet, and is left (the delete finds no object, so it
///   comes before that put).
/// - **List** is the union of both tiers without duplicates (the hot tier's entry where both
///   hold the key), in key order, without the keys under the reserved prefixes
///   `_journal/` and `_tiering/` (whichever tier holds them), and without reservations: a cold object of the size of one that the hot
///   listing does not show is asked about with a HEAD (sixteen at a time; an object a HEAD finds
///   real is shown, and no answer is kept from one listing to the next, since a listing gives no
///   version of an object to know it by). A HEAD that fails fails the listing.
///   [`Backend::list_page`] pages through the merged listing: the last key of a page is the
///   continuation token, and `max_keys` is kept to (a page of the cold tier that the filter cut
///   short is made up from the next). A get, a head, a put, a delete and every multipart call of
///   a key under a reserved prefix is refused with [`Error::Reserved`]. The S3 front and the
///   local cache of a build node refuse those keys as well, each for itself.
/// - **Multipart uploads** (`create_upload`, `upload_part`, `complete_upload`, `abort_upload`)
///   go straight to the cold tier: the parts are passed on as they arrive, so the proxy holds no
///   part, in memory or on disk, and the uploads are the cold tier's own (they outlive the
///   proxy where the cold tier keeps them). The completion is a create-only put to the cold
///   tier, and so is refused with [`Error::AlreadyExists`] where either tier holds the key. It
///   journals nothing, as a put that goes to the cold tier of a full hot tier journals nothing:
///   the object is in the cold tier, so a get, a head, a listing and a delete find it, and the
///   reconciler has nothing to copy. It is in the hot tier only after a get copies it back.
///   The cold tier must do multipart uploads (the provided methods of [`Backend`] do not).
///   [`Backend::list_uploads`] lists the cold tier's open uploads, which are the only ones: the
///   proxy keeps no state of an upload.
///
/// The journal is written in batches of at most [`ProxyConfig::batch_entries`] entries, each
/// batch after [`ProxyConfig::linger`] at most, as `_journal/<proxy_id>/<seq>`, create-only,
/// one object at a time. The numbers of a proxy go up by one from the first; a proxy that
/// starts lists its own journal prefix and its checkpoint, and goes on after the highest
/// number it finds.
///
/// **Entries.** While the format object says `{"epoch": 2, "phase": "active"}` a put (held,
/// streamed, presigned or the completion of a multipart upload) is made on the journal entry of its
/// key, not in the journal above (`entry_put.rs`), and a read of a cold object asks for a copy-back
/// on an entry (`copy_back.rs`). So is a delete (see `entry_delete.rs`): the cold tier's object is
/// deleted at once, and a delete that meets a copy in flight deletes both tiers, hands the entry
/// over and waits for the reconciler to release it before it is acknowledged ([`DeleteWait`]).
///
/// **Capacity.** The proxy reads the usage that the reconciler publishes (`_tiering/usage`,
/// at most every [`ProxyConfig::usage_refresh`]); a usage that is missing, unreadable or
/// unchanged for [`ProxyConfig::usage_stale_after`] counts as not full (a dead reconciler thus
/// leaves puts unrestrained: [`Proxy::usage_age`] is how a caller sees it, and the server's
/// `/usagez` reports it). Above
/// [`ProxyConfig::critical`] of the budget a put is written to the hot tier as it is under it
/// (it does not wait), and a read of a cold object is not copied back.
/// At the budget, or when the hot tier answers a put (of the object, or of the journal object
/// that holds its entry) with [`Error::Full`], puts go straight to the cold tier, create-only
/// and unjournaled, and never fail for lack of room in the hot tier; reads still use both
/// tiers, and the hot tier is used again once the usage falls (or a
/// [`ProxyConfig::usage_refresh`] after it said it was full).
///
/// **Known limits.**
///
/// - A put that is dropped (its caller gave up) after the object reached the hot tier and
///   before its journal object was written leaves the object in the hot tier with no entry:
///   it is never copied or evicted, and a retry of the put is refused as existing. Bound the
///   calls inside the backends rather than dropping a put.
/// - The checks and the write of a put are not atomic. Two puts of the same key at once, one to
///   the hot tier and one (the hot tier being full for it, or having just said so) to the cold
///   tier, may both succeed, whether through one proxy or two; a presigned PUT of the S3 front
///   never takes that path ([`Backend::put_stream_hot_only`] answers [`Error::Full`] instead), so the
///   hot tier's create-only write alone decides it. For the other keys the race remains: the
///   reconciler then counts the cold tier's object as the copy of the hot tier's, and the object
///   of the cold tier is the one that stays.
/// - Before the format is active, a get that copies an object back into the hot tier while it is
///   deleted may put it back there, with a put entry after the delete entry; reads are served that
///   copy until the reconciler's next pass, which finds the cold tier without the object and drops
///   the mirror. On entries a copy-back lands only under a seal that a delete cannot amend.
pub struct Proxy {
    inner: Arc<Inner>,
}

struct Inner {
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
    config: ProxyConfig,
    /// The batch that takes the next entry, if there is one.
    open: Mutex<Option<Arc<Batch>>>,
    /// The number of the next journal object. It is held while an object is written, so that
    /// the objects are written one at a time, and a number is used again if its write failed.
    next_seq: AsyncMutex<u64>,
    capacity: Mutex<Capacity>,
    /// What was last read of the format object.
    pause: Arc<Mutex<pause::Pause>>,
    /// The calls on the entries of keys, as this proxy.
    entries: Entries,
    /// One caller reads the format object at a time, so that a caller that waited uses the read of
    /// the caller before it.
    format_gate: AsyncMutex<()>,
    counters: Counters,
    heartbeat: Heartbeat,
}

impl Proxy {
    /// Starts a proxy over `hot` and `cold`.
    ///
    /// It lists the journal objects of its own id in the hot tier, reads its checkpoint, and
    /// tells the reconciler that it has a journal.
    ///
    /// # Errors
    /// [`Error::Other`] if the proxy id is empty or has a `/`, or the checkpoint is not
    /// readable; the error of the hot tier if it fails one of those calls.
    pub async fn open(
        hot: Arc<dyn Backend>,
        cold: Arc<dyn Backend>,
        config: ProxyConfig,
    ) -> Result<Self, Error> {
        let id = config.proxy_id.as_str();
        if id.is_empty() || id.contains('/') {
            return Err(Error::Other(format!("invalid proxy id {id:?}")));
        }
        let prefix = journal_prefix(id);
        let listed = hot.list(&prefix).await?;
        let newest = listed
            .iter()
            .filter_map(|object| seq_of(&prefix, &object.key))
            .max();
        let checkpoint = load_checkpoint(hot.as_ref(), id).await?;
        let heartbeat = Heartbeat::new(&config);
        heartbeat.register(hot.as_ref(), &proxy_key(id)).await?;
        let next = newest.unwrap_or(0).max(checkpoint.seq) + 1;
        let entries = Entries::new(Arc::clone(&hot), id, Arc::clone(&config.clock));
        let inner = Inner {
            hot,
            cold,
            config,
            open: Mutex::new(None),
            next_seq: AsyncMutex::new(next),
            capacity: Mutex::new(Capacity::default()),
            pause: Arc::default(),
            entries,
            format_gate: AsyncMutex::default(),
            counters: Counters::default(),
            heartbeat,
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// How long the usage object (`_tiering/usage`) has stood unchanged, by the monotonic clock
    /// of the proxy counted from the first time it read exactly that object, as of a read that is
    /// at most [`ProxyConfig::usage_refresh`] old. None if the object is missing or cannot be
    /// read.
    ///
    /// A usage older than [`ProxyConfig::usage_stale_after`] is ignored by the puts, so a
    /// reconciler that has stopped means a hot tier that is never taken to be full; this is the
    /// age that tells it, whatever it is.
    pub async fn usage_age(&self) -> Option<Duration> {
        self.inner.usage_age().await
    }

    /// What this proxy has counted of its reads since it started, in the Prometheus text
    /// format: hits, misses and errors of each tier, the bytes each served and read, and the
    /// copies into the hot tier that cold reads made. Every sample is a counter
    /// whose labels have a few fixed values; the names and their meaning are in the README of
    /// the crate.
    #[must_use]
    pub fn metrics(&self) -> String {
        self.inner.counters.render()
    }
}

impl Inner {
    fn entry(&self, op: JournalOp, key: &str, size: u64, cold_tag: Option<String>) -> JournalEntry {
        JournalEntry {
            op,
            key: key.to_owned(),
            size,
            time: self.config.clock.now(),
            cold_tag,
            hot_tag: None,
        }
    }

    /// Adds an entry to the open batch, which becomes the batch that is written when it is
    /// full.
    fn join(&self, entry: JournalEntry) -> Arc<Batch> {
        let mut open = locked(&self.open);
        let batch = Arc::clone(open.get_or_insert_with(Arc::default));
        let count = {
            let mut entries = locked(&batch.entries);
            entries.push(entry);
            entries.len()
        };
        if count >= self.config.batch_entries {
            *open = None;
            batch.full.notify_one();
        }
        batch
    }

    /// Writes the batch as the next journal object: after its linger, or at once if it is
    /// full. A number that is taken (by a write that failed after it reached the hot tier)
    /// is skipped.
    async fn write_batch(&self, batch: &Arc<Batch>) -> Result<(), Error> {
        let wait = tokio::time::timeout(self.config.linger, batch.full.notified());
        let _ = wait.await;
        {
            let mut open = locked(&self.open);
            let _ = open.take_if(|current| Arc::ptr_eq(current, batch));
        }
        let entries = locked(&batch.entries).clone();
        let body = encode_journal(&entries);
        let mut next = self.next_seq.lock().await;
        loop {
            let key = journal_key(&self.config.proxy_id, *next);
            match self.hot.put(&key, body.clone()).await {
                Ok(()) => {
                    *next += 1;
                    return Ok(());
                }
                Err(Error::AlreadyExists(_)) => *next += 1,
                Err(error) => return Err(error),
            }
        }
    }

    /// Appends an entry to the journal: waits until the journal object that holds it is
    /// written to the hot tier. Callers that wait on one batch share its outcome, and if the
    /// one that writes it is dropped, another takes over.
    async fn append(&self, entry: JournalEntry) -> Result<(), Error> {
        let batch = self.join(entry);
        let outcome = batch.outcome.get_or_init(|| self.write_batch(&batch));
        outcome.await.clone()
    }

    /// Journals a put of an object that the hot tier holds (`op` says of what kind, and
    /// `cold_tag` names the cold object the entry is about, if one is), and takes the object out
    /// of the hot tier if that fails.
    async fn journal_put(
        &self,
        op: JournalOp,
        key: &str,
        size: u64,
        cold_tag: Option<String>,
    ) -> Result<(), Error> {
        self.journal_entry(self.entry(op, key, size, cold_tag))
            .await
    }

    /// Appends the entry of a put of an object that the hot tier holds, and takes the object out
    /// of the hot tier if that fails.
    async fn journal_entry(&self, entry: JournalEntry) -> Result<(), Error> {
        let key = entry.key.clone();
        let appended = self.append(entry).await;
        if appended.is_err() {
            let _ = self.hot.delete(&key).await;
        }
        appended
    }

    /// Journals a copy-back as a mirror: `cold_tag` is the tag of the cold object read, `hot_tag`
    /// the tag the hot tier's create-only write gave the copy. If the append fails, takes the copy
    /// out of the hot tier by `hot_tag` (`If-Match`): an object that another writer put under the
    /// key since has another tag and is left.
    async fn journal_mirror(
        &self,
        key: &str,
        size: u64,
        cold_tag: String,
        hot_tag: String,
    ) -> Result<(), Error> {
        let mut entry = self.entry(JournalOp::PutMirror, key, size, Some(cold_tag));
        entry.hot_tag = Some(hot_tag.clone());
        let appended = self.append(entry).await;
        if appended.is_err() {
            let _ = self.hot.delete_if_match(key, &hot_tag).await;
        }
        appended
    }

    fn latch_full(&self) {
        let until = Instant::now() + self.config.usage_refresh;
        locked(&self.capacity).full_until = Some(until);
    }

    /// Reads the usage object, and notes whether it is the one that was read before: it counts as
    /// fresh until it has stood unchanged for `usage_stale_after` on the monotonic clock.
    async fn refresh_usage(&self, now: Instant) -> Option<u64> {
        let fetched = self.hot.get(USAGE_KEY, None).await.ok();
        let mono = self.config.mono.now();
        let usage = fetched
            .as_ref()
            .and_then(|body| serde_json::from_slice::<Usage>(body).ok());
        let mut capacity = locked(&self.capacity);
        let before = capacity.seen.take();
        capacity.seen = match (&usage, fetched) {
            (Some(_), Some(body)) => match before {
                Some(seen) if seen.body == body => Some(seen),
                _ => Some(UsageSeen { body, since: mono }),
            },
            _ => None,
        };
        let limit = self.config.usage_stale_after;
        let fresh = capacity
            .seen
            .as_ref()
            .is_some_and(|seen| Duration::from_millis(mono.saturating_sub(seen.since)) <= limit);
        let hot_bytes = usage.filter(|_| fresh).map(|u| u.hot_bytes);
        capacity.read = Some((now, hot_bytes));
        hot_bytes
    }

    /// The bytes of the usage object, as of a read that is at most `usage_refresh` old.
    async fn cached_hot_bytes(&self, now: Instant) -> Option<u64> {
        let read = locked(&self.capacity).read;
        match read {
            Some((at, bytes)) if now.duration_since(at) < self.config.usage_refresh => bytes,
            _ => self.refresh_usage(now).await,
        }
    }

    async fn usage_age(&self) -> Option<Duration> {
        self.cached_hot_bytes(Instant::now()).await;
        let since = locked(&self.capacity).seen.as_ref()?.since;
        let age = self.config.mono.now().saturating_sub(since);
        Some(Duration::from_millis(age))
    }

    fn classify(&self, hot_bytes: Option<u64>) -> Level {
        let budget = self.config.hot_budget;
        let critical = (self.config.critical * budget as f64) as u64;
        match hot_bytes {
            Some(bytes) if bytes >= budget => Level::Full,
            Some(bytes) if bytes >= critical => Level::Critical,
            _ => Level::Open,
        }
    }

    async fn level(&self) -> Level {
        let now = Instant::now();
        let full_until = locked(&self.capacity).full_until;
        if full_until.is_some_and(|until| now < until) {
            return Level::Full;
        }
        let hot_bytes = self.cached_hot_bytes(now).await;
        self.classify(hot_bytes)
    }

    /// A get of the cold tier, counted as a hit, a miss or an error of it. A reservation is not an
    /// object: the read of one is a miss. The read gives the mark and the content tag with the
    /// bytes, all of one object; an error that is neither "not found" nor "unavailable" (a range
    /// past the end of the 56 bytes of a reservation is one) is told from a failure by one HEAD.
    async fn cold_get(
        &self,
        key: &str,
        range: Option<ByteRange>,
    ) -> Result<(Bytes, String), Error> {
        match self.cold.get_tagged(key, range).await {
            Ok((_, true, _)) => {
                self.counters.miss(Tier::Cold, Read::Get);
                Err(Error::NotFound(key.to_owned()))
            }
            Ok((body, false, tag)) => {
                self.counters.hit(Tier::Cold, Read::Get);
                self.counters.bytes_read(Tier::Cold, body.len());
                Ok((body, tag))
            }
            Err(error @ Error::NotFound(_)) => {
                self.counters.miss(Tier::Cold, Read::Get);
                Err(error)
            }
            Err(error @ Error::Unavailable(_)) => {
                self.counters.error(Tier::Cold, Read::Get);
                Err(error)
            }
            Err(error) => match self.cold.head(key).await {
                Ok(meta) if meta.reservation => {
                    self.counters.miss(Tier::Cold, Read::Get);
                    Err(Error::NotFound(key.to_owned()))
                }
                _ => {
                    self.counters.error(Tier::Cold, Read::Get);
                    Err(error)
                }
            },
        }
    }

    async fn get_object(
        self: &Arc<Self>,
        key: &str,
        range: Option<ByteRange>,
    ) -> Result<Bytes, Error> {
        refuse_reserved(key)?;
        match self.hot.get(key, range).await {
            Ok(body) => {
                self.counters.hit(Tier::Hot, Read::Get);
                self.counters.bytes_read(Tier::Hot, body.len());
                self.counters.served(Tier::Hot, body.len());
                return Ok(body);
            }
            Err(Error::NotFound(_)) => self.counters.miss(Tier::Hot, Read::Get),
            Err(error) => {
                self.counters.error(Tier::Hot, Read::Get);
                return Err(error);
            }
        }
        if let Some(part) = range {
            // A read of a part asks the cold tier for that part alone and copies nothing back: a
            // whole read per chunk of a long blob would be quadratic, and would hold the whole of
            // the object in memory for a read of a part of it.
            if self.level().await >= Level::Critical {
                self.counters.skipped(Skipped::Critical);
            }
            let (body, _) = self.cold_get(key, Some(part)).await?;
            self.counters.served(Tier::Cold, body.len());
            return Ok(body);
        }
        // The tag the cold tier's read gives is the tag of the very bytes read, so the copy is
        // journaled under it whatever the key holds by then.
        let critical = self.level().await >= Level::Critical;
        let (body, tag) = self.cold_get(key, None).await?;
        if critical {
            self.counters.skipped(Skipped::Critical);
        } else if self.entries_active().await {
            self.ask_copy_back(key);
        } else {
            self.copy_back(key, &body, tag).await;
        }
        self.counters.served(Tier::Cold, body.len());
        Ok(body)
    }

    async fn head_object(&self, key: &str) -> Result<Meta, Error> {
        refuse_reserved(key)?;
        match self.hot.head(key).await {
            Ok(meta) => {
                self.counters.hit(Tier::Hot, Read::Head);
                return Ok(meta);
            }
            Err(Error::NotFound(_)) => self.counters.miss(Tier::Hot, Read::Head),
            Err(error) => {
                self.counters.error(Tier::Hot, Read::Head);
                return Err(error);
            }
        }
        match self.cold.head(key).await {
            Ok(meta) if meta.reservation => {
                self.counters.miss(Tier::Cold, Read::Head);
                Err(Error::NotFound(key.to_owned()))
            }
            Ok(meta) => {
                self.counters.hit(Tier::Cold, Read::Head);
                Ok(meta)
            }
            Err(error @ Error::NotFound(_)) => {
                self.counters.miss(Tier::Cold, Read::Head);
                Err(error)
            }
            Err(error) => {
                self.counters.error(Tier::Cold, Read::Head);
                Err(error)
            }
        }
    }

    /// Deletes the object: the cold tier's object first, by its content tag, then the hot tier's,
    /// then a delete entry is appended (the books forget the key). A reservation is removed only
    /// if the hot tier holds the key, which is an acknowledged put or about to be; a bare one is
    /// a put in flight, and the delete, which finds no object, is one that comes before it.
    async fn delete_object(&self, key: &str) -> Result<(), Error> {
        if self.admit_write_mode(key).await? == WriteMode::Entries {
            return self.delete_on_entry(key).await;
        }
        self.remove_from_cold(key).await?;
        self.hot.delete(key).await?;
        self.append(self.entry(JournalOp::Delete, key, 0, None))
            .await
    }

    /// Removes the cold tier's object under `key` as `delete_object` says, looking again if the
    /// object changes between the look and the delete, [`DELETE_ROUNDS`] looks at most.
    async fn remove_from_cold(&self, key: &str) -> Result<(), Error> {
        for _ in 0..DELETE_ROUNDS {
            let found = match self.cold.head(key).await {
                Ok(found) => found,
                Err(Error::NotFound(_)) => return Ok(()),
                Err(error) => return Err(error),
            };
            if found.reservation && !self.hot_holds(key).await? {
                return Ok(());
            }
            match self.cold.delete_if_match(key, &found.tag).await {
                Ok(()) | Err(Error::NotFound(_)) => return Ok(()),
                Err(Error::PreconditionFailed(_)) => {}
                Err(error) => return Err(error),
            }
        }
        let text = format!("the cold copy of {key} kept changing under its delete");
        Err(Error::Unavailable(text))
    }

    /// Whether the hot tier holds an object under `key`.
    async fn hot_holds(&self, key: &str) -> Result<bool, Error> {
        match self.hot.head(key).await {
            Ok(_) => Ok(true),
            Err(Error::NotFound(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }
}

#[async_trait]
impl Backend for Proxy {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put_object(key, body).await
    }

    /// Whether either tier needs the length of a streamed body (the put may go to either).
    fn needs_length(&self) -> bool {
        self.inner.hot.needs_length() || self.inner.cold.needs_length()
    }

    /// A put whose body is not held: the chunks go to the hot tier (or the cold tier of a full
    /// hot tier) as they arrive, and the object is created, create-only, only after the body has
    /// ended whole; a body that fails leaves no object and no journal entry. See [`Proxy`].
    async fn put_stream(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.inner.put_object_stream(key, len, body, false).await
    }

    async fn put_stream_hot_only(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.inner.put_object_stream(key, len, body, true).await
    }

    /// The proxy keeps objects immutable: it never replaces one.
    async fn put_if_match(&self, key: &str, _body: Bytes, _etag: &str) -> Result<String, Error> {
        Err(Error::Other(format!("the proxy does not replace {key}")))
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.inner.get_object(key, range).await
    }

    /// One call to the hot tier on a hit, with the object's meta, and a body that is the hot
    /// tier's own stream (see `open_object`).
    async fn open_read(&self, key: &str, span: Option<Span>) -> Result<Opened, Error> {
        self.inner.open_object(key, span).await
    }

    /// A streamed read: the hot tier's own stream on a hit, and not the object collected as
    /// [`Backend::get`] gives it. A range of no bytes, or one that starts at or past the end, is
    /// answered as `get` answers it (an empty body, or an error if it starts past the end), from
    /// the size the open read gave and with no read of the object collected.
    async fn get_stream(&self, key: &str, range: Option<ByteRange>) -> Result<ReadBody, Error> {
        // A range of no bytes is opened as one of the byte at its start: no body of it is read.
        let span = range.map(|range| Span::From {
            first: range.offset(),
            last: Some(
                range
                    .offset()
                    .saturating_add(range.size().saturating_sub(1)),
            ),
        });
        let opened = self.inner.open_object(key, span).await?;
        match range {
            Some(range) if range.size() == 0 || opened.window == Window::Unsatisfiable => {
                range.within(opened.meta.size)?;
                Ok(futures::stream::empty().boxed())
            }
            _ => Ok(opened.body),
        }
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.inner.head_object(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete_object(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list_objects(prefix).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        self.inner
            .list_objects_page(prefix, start_after, max_keys)
            .await
    }

    async fn create_upload(&self, key: &str) -> Result<String, Error> {
        self.inner.admit_write(key).await?;
        self.inner.cold.create_upload(key).await
    }

    async fn upload_part(
        &self,
        key: &str,
        id: &str,
        number: u32,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        let sized = self.upload_part_with_len(key, id, number, None, body);
        sized.await
    }

    async fn upload_part_with_len(
        &self,
        key: &str,
        id: &str,
        number: u32,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        self.inner.admit_write(key).await?;
        let cold = &self.inner.cold;
        let upload = cold.upload_part_with_len(key, id, number, len, body);
        upload.await
    }

    async fn complete_upload(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        self.inner.complete_cold(key, id, parts).await
    }

    async fn abort_upload(&self, key: &str, id: &str) -> Result<(), Error> {
        refuse_reserved(key)?;
        self.inner.cold.abort_upload(key, id).await
    }

    /// The open uploads are the cold tier's own (see the type), so this asks the cold tier, and
    /// leaves out an upload of a reserved key: none is made through the proxy, but the tier is
    /// shared with other writers. A page that the filter cut short is made up from the next
    /// page of the tier, so that a short page is the last, as the trait says.
    async fn list_uploads(
        &self,
        prefix: &str,
        after: Option<UploadsAfter<'_>>,
        max_uploads: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        let cold = &self.inner.cold;
        let mut after = after.map(|(key, id)| (key.to_owned(), id.map(str::to_owned)));
        let mut found: Vec<OpenUpload> = Vec::new();
        let mut calls = 0;
        while found.len() < max_uploads {
            calls += 1;
            if calls > UPLOAD_LISTING_CALLS {
                let text = format!(
                    "the cold tier's listing of uploads took over {UPLOAD_LISTING_CALLS} calls"
                );
                return Err(Error::Unavailable(text));
            }
            let want = max_uploads - found.len();
            let marker = after
                .as_ref()
                .map(|(key, id)| (key.as_str(), id.as_deref()));
            let page = cold.list_uploads(prefix, marker, want).await?;
            let last = page.last().map(|up| (up.key.clone(), Some(up.id.clone())));
            let full = page.len() == want;
            found.extend(page.into_iter().filter(|up| !is_reserved(&up.key)));
            match last {
                Some(last) if full => {
                    // A full page that ends where the last one did would be asked for for ever.
                    if after.as_ref().is_some_and(|before| last <= *before) {
                        let text = "the cold tier's listing of uploads did not advance";
                        return Err(Error::Unavailable(text.to_owned()));
                    }
                    after = Some(last);
                }
                _ => break,
            }
        }
        Ok(found)
    }
}
