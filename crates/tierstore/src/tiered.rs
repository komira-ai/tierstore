//! The proxy: a store over a hot and a cold backend.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::{Mutex as AsyncMutex, Notify, OnceCell};
use tokio::time::Instant;

use crate::journal::{
    USAGE_KEY, Usage, encode_journal, is_reserved, journal_key, journal_prefix, load_checkpoint,
    proxy_key, seq_of,
};
use crate::metrics::{Counters, Read, Skipped, Tier};
use crate::{
    Backend, ByteRange, Clock, Error, JournalEntry, JournalOp, Listed, Meta, OpenUpload, PartBody,
    UploadsAfter,
};

/// The most calls to the cold tier that one listing of uploads makes to fill a page that the
/// filter of reserved keys cut short.
const UPLOAD_LISTING_CALLS: u32 = 100;

/// How many times a delete looks at the cold tier's object again after it changed under the
/// delete.
const DELETE_ROUNDS: u32 = 3;

mod copy_back;
mod put;

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
    /// The age of the usage object past which it is ignored, as if it were missing.
    pub usage_stale_after: Duration,
    /// The time, for the entries and for the age of the usage object.
    pub clock: Arc<dyn Clock>,
}

impl ProxyConfig {
    /// The settings for a proxy `proxy_id` and a hot tier of `hot_budget` bytes, with a
    /// critical share of 0.95, batches of at most 64 entries that
    /// wait 50 ms, a usage object that is read every second and ignored after 30 seconds.
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
            clock,
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
    /// When the usage object read last was published, by the clock of the reconciler: none if
    /// it was missing or unreadable. Unlike the bytes of `read`, it is kept whatever the age.
    published: Option<u64>,
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
///   copied back into the hot tier, create-only (it is fine if another read got there
///   first), unless the hot tier is above the critical mark; the copy is journaled as a mirror
///   (`PutMirror`, with the content tag the cold object has and the tag the hot tier gives the
///   copy), in the background, so that the reconciler can evict it again, and can drop it if the
///   cold tier no longer holds that object. The cold tag is the one the cold tier's read gives
///   with the bytes (`get_tagged`: the bytes and the tag of one object), so it is the
///   tag of the bytes copied whatever the key held before or after; no HEAD of the cold tier is
///   made for it. The hot tag is the one the create-only write itself returns
///   (`put_tagged`), so no HEAD of the hot tier follows; a failed journal entry removes the copy
///   with `delete_if_match` on that tag, and if no tag was obtained nothing is deleted. The two tags are never compared with each
///   other: the tiers may tag the same bytes differently.
///   A failed copy-back does not fail the read. A ranged read that misses the hot tier asks the
///   cold tier for that range alone and copies nothing back: the memory of a read is its range,
///   whatever the size of the object, and a cache that reads a long object in chunks does not
///   make the proxy read it whole once for each chunk.
/// - **Head** reads the hot tier, else the cold tier, and copies nothing back.
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
///   it changed, three times at most), then from the hot tier, then appends a delete entry.
/// - **List** is the union of both tiers without duplicates (the hot tier's entry where both
///   hold the key), in key order, without the keys under the reserved prefixes `_journal/` and
///   `_tiering/`. [`Backend::list_page`] pages through the merged listing: the last key of a page is
///   the continuation token, and `max_keys` is kept to (a page of the hot tier that the filter of
///   the reserved prefixes cut short is made up from the next). A put or a delete of a key under a
///   reserved prefix is
///   refused with [`Error::Reserved`].
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
/// **Capacity.** The proxy reads the usage that the reconciler publishes (`_tiering/usage`,
/// at most every [`ProxyConfig::usage_refresh`]); a usage that is missing, unreadable or
/// older than [`ProxyConfig::usage_stale_after`] counts as not full (a dead reconciler thus
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
/// - A get that copies an object back into the hot tier while it is deleted may put it back
///   there, with a put entry after the delete entry; reads are served that copy until the
///   reconciler's next pass, which finds the cold tier without the object and drops the mirror.
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
    counters: Counters,
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
        match hot.put(&proxy_key(id), Bytes::new()).await {
            Ok(()) | Err(Error::AlreadyExists(_)) => {}
            Err(error) => return Err(error),
        }
        let next = newest.unwrap_or(0).max(checkpoint.seq) + 1;
        let inner = Inner {
            hot,
            cold,
            config,
            open: Mutex::new(None),
            next_seq: AsyncMutex::new(next),
            capacity: Mutex::new(Capacity::default()),
            counters: Counters::default(),
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// How long ago the reconciler published the usage object (`_tiering/usage`), by the clock
    /// of the proxy, as of a read that is at most [`ProxyConfig::usage_refresh`] old. None if the
    /// object is missing or cannot be read.
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

    fn is_fresh(&self, usage: &Usage) -> bool {
        let age = self.config.clock.now().saturating_sub(usage.time);
        u128::from(age) <= self.config.usage_stale_after.as_millis()
    }

    async fn refresh_usage(&self, now: Instant) -> Option<u64> {
        let fetched = self.hot.get(USAGE_KEY, None).await.ok();
        let usage = fetched.and_then(|body| serde_json::from_slice::<Usage>(&body).ok());
        let published = usage.as_ref().map(|u| u.time);
        let hot_bytes = usage.filter(|u| self.is_fresh(u)).map(|u| u.hot_bytes);
        let mut capacity = locked(&self.capacity);
        capacity.read = Some((now, hot_bytes));
        capacity.published = published;
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
        let published = locked(&self.capacity).published?;
        let age = self.config.clock.now().saturating_sub(published);
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

    /// A get of the cold tier, counted as a hit, a miss or an error of it. The read gives the
    /// content tag with the bytes, both of one object.
    async fn cold_get(
        &self,
        key: &str,
        range: Option<ByteRange>,
    ) -> Result<(Bytes, String), Error> {
        match self.cold.get_tagged(key, range).await {
            Ok((body, tag)) => {
                self.counters.hit(Tier::Cold, Read::Get);
                self.counters.bytes_read(Tier::Cold, body.len());
                Ok((body, tag))
            }
            Err(error @ Error::NotFound(_)) => {
                self.counters.miss(Tier::Cold, Read::Get);
                Err(error)
            }
            Err(error) => {
                self.counters.error(Tier::Cold, Read::Get);
                Err(error)
            }
        }
    }

    async fn get_object(
        self: &Arc<Self>,
        key: &str,
        range: Option<ByteRange>,
    ) -> Result<Bytes, Error> {
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
        } else {
            self.copy_back(key, &body, tag).await;
        }
        self.counters.served(Tier::Cold, body.len());
        Ok(body)
    }

    async fn head_object(&self, key: &str) -> Result<Meta, Error> {
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
    /// then a delete entry is appended (the books forget the key).
    async fn delete_object(&self, key: &str) -> Result<(), Error> {
        refuse_reserved(key)?;
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
            match self.cold.delete_if_match(key, &found.tag).await {
                Ok(()) | Err(Error::NotFound(_)) => return Ok(()),
                Err(Error::PreconditionFailed(_)) => {}
                Err(error) => return Err(error),
            }
        }
        let text = format!("the cold copy of {key} kept changing under its delete");
        Err(Error::Unavailable(text))
    }

    async fn list_objects(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        let mut in_hot = self.hot.list(prefix).await?;
        in_hot.retain(is_data);
        let in_cold = self.cold.list(prefix).await?;
        Ok(merge(in_hot, in_cold, usize::MAX))
    }

    async fn list_objects_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        let in_hot = data_page(self.hot.as_ref(), prefix, start_after, max_keys).await?;
        let in_cold = self.cold.list_page(prefix, start_after, max_keys).await?;
        Ok(merge(in_hot, in_cold, max_keys))
    }
}

/// The first `max_keys` objects of the listing of `hot` after `start_after`, without the
/// objects under the reserved prefixes: pages of the hot tier are read until enough objects
/// are left or the listing ends.
async fn data_page(
    hot: &dyn Backend,
    prefix: &str,
    start_after: Option<&str>,
    max_keys: usize,
) -> Result<Vec<Listed>, Error> {
    let mut found: Vec<Listed> = Vec::new();
    let mut after = start_after.map(str::to_owned);
    loop {
        let page = hot.list_page(prefix, after.as_deref(), max_keys).await?;
        let full_page = page.len() >= max_keys;
        after = page.last().map(|object| object.key.clone());
        found.extend(page.into_iter().filter(is_data));
        if !full_page || found.len() >= max_keys {
            break;
        }
    }
    found.truncate(max_keys);
    Ok(found)
}

/// The objects of both listings, each key once (the hot tier's entry where both have it), in
/// key order, at most `max_keys` of them.
fn merge(in_hot: Vec<Listed>, in_cold: Vec<Listed>, max_keys: usize) -> Vec<Listed> {
    let mut merged: BTreeMap<String, Listed> = BTreeMap::new();
    for object in in_hot.into_iter().chain(in_cold) {
        merged.entry(object.key.clone()).or_insert(object);
    }
    merged.into_values().take(max_keys).collect()
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
        refuse_reserved(key)?;
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
        refuse_reserved(key)?;
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
