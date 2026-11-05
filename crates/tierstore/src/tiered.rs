//! The proxy: a store over a hot and a cold backend.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;

use crate::journal::{USAGE_KEY, Usage, is_reserved, proxy_key};
use crate::metrics::{Counters, Read, Skipped, Tier};
use crate::{
    Backend, ByteRange, Clock, Entries, Error, Listed, Meta, MonotonicClock, OpenUpload, Opened,
    PartBody, ReadBody, Span, UploadedPart, UploadsAfter, Window,
};

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
mod uploads;
#[cfg(test)]
mod uploads_tests;

pub use entry_delete::DeleteWait;
use heartbeat::Heartbeat;

/// The settings of a [`Proxy`].
#[derive(Clone)]
pub struct ProxyConfig {
    /// Names the proxy's registration and the owner of the entries it holds. It is not empty and
    /// has no `/`. Every running proxy has an id of its own.
    pub proxy_id: String,
    /// The bytes the hot tier is meant to hold, which the usage the reconciler publishes is
    /// compared with.
    pub hot_budget: u64,
    /// The share of the budget (0.95, say) above which a read does not copy the object it
    /// found in the cold tier into the hot tier. A put is not changed by it: it still goes to
    /// the hot tier, at once, until the usage reaches the budget.
    pub critical: f64,
    /// How long the usage object that was read is used before it is read again, and how long
    /// the hot tier is taken to be full after it said so.
    pub usage_refresh: Duration,
    /// How long the usage object may stand unchanged, by the monotonic clock of this proxy
    /// (`mono`), before it is ignored, as if it were missing. The leader rewrites it every
    /// publication, so an object that no longer changes means the leader has stopped; no time that
    /// another machine wrote is read.
    pub usage_stale_after: Duration,
    /// The most calls to the cold tier that one listing of uploads makes to fill a page that the
    /// filter of reserved keys cut short.
    pub upload_listing_calls: u32,
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
    /// puts and deletes made on entries read it ([`Proxy::is_live`]); the reconciler counts the
    /// owner of an entry as departed by its own `EntryReconcilerConfig::departure`.
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
    /// critical share of 0.95, a usage object that is read every second and ignored after 30 seconds, at most 100 calls to the cold tier for one listing
    /// of uploads, a format object that is read every 5 seconds, a heartbeat every 10 seconds, a departure time
    /// of 60 seconds and a margin of 10 seconds, on a [`MonotonicClock`] that reads zero now, and
    /// 3 looks at an entry, and the [`DeleteWait`] of [`DeleteWait::new`].
    #[must_use]
    pub fn new(proxy_id: &str, hot_budget: u64, clock: Arc<dyn Clock>) -> Self {
        Self {
            proxy_id: proxy_id.to_owned(),
            hot_budget,
            critical: 0.95,
            usage_refresh: Duration::from_secs(1),
            usage_stale_after: Duration::from_secs(30),
            upload_listing_calls: 100,
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
/// Objects are immutable. "Flushed" means that the cold tier holds the object, and the
/// [`EntryReconciler`](crate::EntryReconciler) does the flushing from the entries the proxies
/// make.
///
/// **Entries.** Every write is made on the journal entry of its key, `_entries/<sha256 of the
/// key>` in the hot tier (the module `entry`), and only while the format object
/// (`_tiering/format`) says `{"epoch": 2, "phase": "active"}`. Until it does, and while it says
/// `drain` or `drained`, a put, a delete, and the create, part upload and completion of a
/// multipart upload are answered with [`Error::Unavailable`] (a 503 at the S3 front), and reads
/// are served. The format is read at most every [`ProxyConfig::format_refresh`], and at each
/// heartbeat.
///
/// - **Get** reads the hot tier, else the cold tier, and reads no entry. An object it read from
///   the cold tier is not copied by the read: when the hot tier is below the critical mark the
///   read asks for the copy with a create-only `(CB, PENDING)` entry of the key, in a task of its
///   own (nothing happens if an entry of the key exists), and the entry reconciler copies the
///   cold object into the hot tier under the entry. A read through [`Backend::open_read`] or
///   [`Backend::get_stream`] reads the cold object once and gives its body to the caller as it
///   arrives, never held whole. A ranged read that misses the hot tier asks the cold tier for that
///   range alone and asks for no copy-back: the memory of a read is its range, whatever the size of
///   the object. A failed ask does not fail the read.
/// - **Head** reads the hot tier, else the cold tier, and asks for no copy-back.
/// - **Put** is refused if the key exists in either tier, is written to the hot tier create-only,
///   and is acknowledged when its entry says so (`entry_put.rs`): the entry is created
///   `(PUT, HELD)` before the object is written, so that no other put, delete or copy of the key
///   goes on beside it, and becomes `(PUT, PENDING)` after, which hands the key to the reconciler.
///   A put whose entry cannot be made or kept is refused with [`Error::Unavailable`] and a client
///   retries it; a key that exists is [`Error::AlreadyExists`]. A cold tier that cannot be asked
///   whether it holds the key is [`Error::Unavailable`]: a new key is not accepted while it is down.
/// - **Streamed put** ([`Backend::put_stream`]) is a put whose body arrives as chunks and is
///   never held whole: the chunks go on to the hot tier's own streamed put (or the cold tier's,
///   where the hot tier is full), which makes the object visible, create-only, only after the
///   last chunk, and the entry becomes pending after that. A body that ends with an error (a
///   checksum that does not match, a connection that was cut) leaves no object in either tier and
///   no entry. If the hot tier answers [`Error::Full`] after it took some of the body, the bytes
///   cannot be offered to the cold tier, so the put fails with [`Error::Full`] (the tier is latched
///   full, and the client's retry goes to the cold tier); if it took none, the cold tier gets the
///   whole body as for a held put.
/// - **Delete** (`entry_delete.rs`) deletes the object from the cold tier at once and amends,
///   claims or creates the entry of the key; a delete that meets a copy in flight (an entry the
///   reconciler holds or has sealed) waits for the reconciler's release ([`DeleteWait`]) and is
///   answered 503 at the request deadline.
/// - **List** is the union of both tiers without duplicates (the hot tier's entry where both
///   hold the key), in key order, without the keys under the reserved prefixes `_journal/`,
///   `_tiering/` and `_entries/` (whichever tier holds them). [`Backend::list_page`] pages through
///   the merged listing: the last key of a page is the continuation token, and `max_keys` is kept
///   to (a page of a tier that the filter of the reserved prefixes cut short is made up from the
///   next). A get, a head, a put, a delete and every
///   multipart call of a key under a reserved prefix is refused with [`Error::Reserved`]. The S3
///   front and the local cache of a build node refuse those keys as well, each for itself.
/// - **Multipart uploads** (`create_upload`, `upload_part`, `complete_upload`, `abort_upload`)
///   are made in the hot tier whenever it has room and in the cold tier only when it is full
///   (`uploads.rs`): the tier is chosen at `create_upload`, as a put's is, and is named in the id
///   the proxy gives, which is the hot tier's own id with a prefix, or the cold tier's own id
///   unchanged, so that an id of a release before the prefix names a cold upload. The parts are
///   passed on to that tier as they arrive, so the proxy holds no part, in memory or on disk, and
///   the uploads are the tiers' own (they outlive the proxy where the tier keeps them). A hot tier
///   with no room for a part answers [`Error::Full`] and keeps the upload; the part is never moved
///   to the cold tier. The completion is a put on the entry of the key whose object is made, in the
///   upload's tier, by that tier's own create-only completion, and so is refused with
///   [`Error::AlreadyExists`] where either tier holds the key; one made in the hot tier leaves a
///   `(PUT, PENDING)` entry, and the reconciler copies the object to the cold tier. Both tiers must
///   do multipart uploads (the provided methods of [`Backend`] do not). [`Backend::list_uploads`]
///   lists the open uploads of both tiers (the cold tier's uploads of a key first): the proxy keeps
///   no state of an upload. The reconciler aborts an upload of the hot tier that has stood open
///   for `abandoned_upload_after`.
///
/// **Capacity.** The proxy reads the usage that the reconciler publishes (`_tiering/usage`,
/// at most every [`ProxyConfig::usage_refresh`]); a usage that is missing, unreadable or
/// unchanged for [`ProxyConfig::usage_stale_after`] counts as not full (a dead reconciler thus
/// leaves puts unrestrained: [`Proxy::usage_age`] is how a caller sees it, and the server's
/// `/usagez` reports it). Above [`ProxyConfig::critical`] of the budget a put is written to the
/// hot tier as it is under it (it does not wait), and a read of a cold object asks for no
/// copy-back. At the budget, or when the hot tier answers a put with [`Error::Full`], puts go
/// straight to the cold tier, create-only, and the object's data never fails a put for lack of
/// room in the hot tier. The entry of the key is made in the hot tier first, though: a hot tier
/// too full to create or update that entry ([`Error::Full`] from it) fails the put before the
/// cold tier is tried. Reads still use both tiers, and the hot tier is used again once the usage falls (or a
/// [`ProxyConfig::usage_refresh`] after it said it was full).
///
/// **Known limits.**
///
/// - The checks and the write of a put are serialised by the entry of the key, but an actor that
///   stalls between a check and its write (a frozen process, a store call that answers after its
///   deadline) can still act on a key that another actor has taken since. The design document
///   states these residuals (R1 to R4) and the tests pin them.
/// - Two puts of the same key at once, one to the hot tier and one (the hot tier being full for
///   it, or having just said so) to the cold tier, are serialised by the entry: the second finds
///   the entry held and is refused with [`Error::Unavailable`], or finds the key and is refused
///   with [`Error::AlreadyExists`].
pub struct Proxy {
    inner: Arc<Inner>,
}

struct Inner {
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
    config: ProxyConfig,
    capacity: Mutex<Capacity>,
    /// What was last read of the format object.
    pause: Arc<Mutex<pause::Pause>>,
    /// The calls on the entries of keys, as this proxy.
    entries: Entries,
    /// One caller reads the format object at a time, so that a caller that waited uses the read of
    /// the caller before it. The heartbeat's look at the format goes through it too.
    format_gate: Arc<AsyncMutex<()>>,
    counters: Counters,
    heartbeat: Heartbeat,
}

impl Proxy {
    /// Starts a proxy over `hot` and `cold`.
    ///
    /// It writes its registration, which tells the reconciler that it exists and which it
    /// rewrites at each heartbeat.
    ///
    /// # Errors
    /// [`Error::Other`] if the proxy id is empty or has a `/`; the error of the hot tier if it
    /// fails the write of the registration.
    pub async fn open(
        hot: Arc<dyn Backend>,
        cold: Arc<dyn Backend>,
        config: ProxyConfig,
    ) -> Result<Self, Error> {
        let id = config.proxy_id.as_str();
        if id.is_empty() || id.contains('/') {
            return Err(Error::Other(format!("invalid proxy id {id:?}")));
        }
        let heartbeat = Heartbeat::new(&config);
        heartbeat.register(hot.as_ref(), &proxy_key(id)).await?;
        #[cfg(feature = "server")]
        tracing::debug!(proxy = %id, "tierstore: proxy opened");
        let entries = Entries::new(Arc::clone(&hot), id, Arc::clone(&config.clock));
        let inner = Inner {
            hot,
            cold,
            config,
            capacity: Mutex::new(Capacity::default()),
            pause: Arc::default(),
            entries,
            format_gate: Arc::default(),
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
            // A read that failed says nothing of the object: the body last read keeps its age.
            (_, None) => before,
            // A body that is not a usage object is not one to age.
            (None, Some(_)) => None,
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
        let critical = self.level().await >= Level::Critical;
        let (body, _) = self.cold_get(key, None).await?;
        if critical {
            self.counters.skipped(Skipped::Critical);
        } else if self.entries_active().await {
            self.ask_copy_back(key);
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

    /// Deletes the object on the entry of its key (see `entry_delete.rs`): refused with
    /// [`Error::Unavailable`] while writes are paused or the format is not active.
    async fn delete_object(&self, key: &str) -> Result<(), Error> {
        self.admit_write(key).await?;
        self.delete_on_entry(key).await
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

    /// The tag of an object is the tag of the tier that answers, and moves when the proxy copies
    /// the object from one tier to the other, so versions are told apart by size alone: the proxy
    /// never replaces an object. Limit: a key that is deleted and put again with a body of the
    /// same size between two HEADs is not seen. A key named by its content (`cas/l/<digest>`) is
    /// not affected, as its bytes are checked against the digest.
    fn same_version(&self, a: &Meta, b: &Meta) -> bool {
        a.size == b.size
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
        self.inner.start_upload(key).await
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
        self.inner.send_part(key, id, number, len, body).await
    }

    async fn complete_upload(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        self.inner.complete_on_entry(key, id, parts).await
    }

    async fn abort_upload(&self, key: &str, id: &str) -> Result<(), Error> {
        self.inner.drop_upload(key, id).await
    }

    /// The parts of an open upload are in the tier its id names (see the type), so this asks that
    /// tier.
    async fn list_parts(
        &self,
        key: &str,
        id: &str,
        after: Option<u32>,
        max_parts: usize,
    ) -> Result<Vec<UploadedPart>, Error> {
        self.inner
            .list_upload_parts(key, id, after, max_parts)
            .await
    }

    /// The open uploads of both tiers (see the type): the proxy keeps no state of an upload, so
    /// this asks the tiers, and leaves out an upload of a reserved key: none is made through the
    /// proxy, but the tiers are shared with other writers. A page that the filter cut short is made
    /// up from the next page of the tier, so that a short page is the last, as the trait says.
    async fn list_uploads(
        &self,
        prefix: &str,
        after: Option<UploadsAfter<'_>>,
        max_uploads: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        self.inner
            .list_open_uploads(prefix, after, max_uploads)
            .await
    }
}
