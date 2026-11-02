//! The reconciler: copies what the proxies journal to the cold tier, and frees the hot tier.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::TryStreamExt;
use futures::stream::{self, StreamExt};

use crate::journal::{
    PROXIES_PREFIX, USAGE_KEY, Usage, checkpoint_key, decode_journal, journal_key, journal_prefix,
    load_checkpoint, seq_of, to_body, write_object,
};
use crate::lease::Lease;
use crate::state::{Books, Stream};
use crate::{Backend, Clock, Error, JournalEntry, JournalOp, Meta};

/// The settings of a [`Reconciler`].
#[derive(Clone)]
pub struct ReconcilerConfig {
    /// Names this reconciler in the lease. No two reconcilers that run at the same time have
    /// the same name.
    pub holder: String,
    /// The bytes the hot tier is meant to hold at most.
    pub hot_budget: u64,
    /// The share of the budget (0.8, say) above which the eviction starts.
    pub high_water: f64,
    /// The share of the budget (0.7, say) down to which the eviction goes on.
    pub low_water: f64,
    /// The most objects copied to the cold tier at once. Size it to what the cold tier
    /// takes in parallel.
    pub parallel: usize,
    /// How long after its last renewal a lease may be taken over. Passes must be run more
    /// often than this.
    pub lease_ttl: Duration,
    /// How long a journal object is kept, counted from the time it was written, once it is
    /// applied and evicted past.
    pub retention: Duration,
    /// The most journal objects asked for in one listing.
    pub page_size: usize,
    /// The time, for the lease, the retention and the usage object. It must agree with the
    /// times the hot tier gives its objects to within the lease ttl and the retention.
    pub clock: Arc<dyn Clock>,
}

impl ReconcilerConfig {
    /// The settings for a reconciler `holder` and a hot tier of `hot_budget` bytes, with
    /// high and low water of 0.8 and 0.7, 16 parallel copies, a lease ttl of 30 seconds, a
    /// retention of an hour, and listings of 100 journal objects.
    #[must_use]
    pub fn new(holder: &str, hot_budget: u64, clock: Arc<dyn Clock>) -> Self {
        Self {
            holder: holder.to_owned(),
            hot_budget,
            high_water: 0.8,
            low_water: 0.7,
            parallel: 16,
            lease_ttl: Duration::from_secs(30),
            retention: Duration::from_secs(3600),
            page_size: 100,
            clock,
        }
    }
}

/// What one [`Reconciler::pass`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PassReport {
    /// Whether this reconciler held the lease. If not, nothing else was done.
    pub held: bool,
    /// The objects copied to the cold tier, or found there with the same content. An object
    /// whose copy was taken back because the hot tier no longer holds that object is not
    /// counted.
    pub copied: usize,
    /// The objects removed from the hot tier.
    pub evicted: usize,
    /// The journal objects deleted.
    pub pruned: usize,
    /// The calls that failed, in the order they did. The steps that depend on a failed one
    /// were left for the next pass.
    pub failed: Vec<Error>,
}

/// The outcome of the copies of one journal object: the key if it is in the cold tier now,
/// or nothing if the hot tier no longer holds it.
type Copies = Vec<Result<Option<String>, Error>>;

/// How many times a copy starts over when the hot object changes while it is copied.
const COPY_ATTEMPTS: usize = 3;

fn millis(duration: Duration) -> u64 {
    duration.as_millis() as u64
}

/// The runner that works through the journals the [`Proxy`](crate::Proxy)s write.
///
/// There is one runner: whoever holds the lease `_tiering/lease` in the hot tier (see
/// `Lease` in the source for how it is taken, renewed and taken over). Run
/// [`Reconciler::pass`] on an interval shorter than the lease ttl, on as many machines as
/// there are, and call [`Reconciler::stop`] to give the lease up. A pass that finds the lease
/// held by another does nothing; a pass that fails to renew it stops, and forgets what it
/// knew, since another reconciler may have worked meanwhile.
///
/// A pass does these steps, and never lists the data prefixes of either tier (it lists
/// `_tiering/proxies/`, one small object for each proxy, and, for each proxy, the journal
/// objects above the last one it read):
///
/// 1. **Tail the journals.** For each proxy, in the order of the journal objects from its
///    checkpoint, a journal object is read and applied: its puts are copied from the hot tier
///    to the cold tier, up to [`ReconcilerConfig::parallel`] at a time, create-only. A copy
///    reads the hot object's tag, then its bytes, writes them, and reads the tag again. If the
///    object is gone, the copy removes the cold object it wrote (the one with its own bytes);
///    if another object is there, it does the same and starts over (three times at most, then
///    the copy fails and the next pass tries again). A key the hot tier no longer holds is
///    skipped. A cold tier that holds the key already is compared: the same bytes count as
///    copied (the same size and tag, or, where the two tiers tag differently, the same bytes
///    read from the cold tier), and other bytes are replaced by the hot tier's, which is what a
///    get returns first, as long as the hot tier still holds the object that was read. A
///    delete entry changes only what the reconciler knows of the hot tier: the proxy's own
///    delete removed the key from both tiers, the hot tier first, and a copy that finished
///    after that took itself back. Then the journal object counts as applied.
/// 2. **Evict.** The reconciler adds up the bytes of the objects the hot tier holds from the
///    journals alone: the sizes of the puts, less the deletes and the evictions. Above
///    [`ReconcilerConfig::high_water`] of the budget it deletes objects from the hot tier,
///    oldest first by the time in their journal entries (journal order within a proxy), down
///    to [`ReconcilerConfig::low_water`]. Only an object of an applied journal object is a
///    candidate, so the cold tier holds it, and the reconciler neither lists the hot tier nor
///    asks it about the object. Before it deletes anything it reads the journals once more,
///    so that an object that was deleted or put again since is skipped.
/// 3. **Checkpoint.** For each proxy, `_tiering/checkpoint/<proxy_id>` is written (an `If-Match`
///    put, after the first): every journal object up to a number is applied, and every entry
///    before a position is done with (the eviction folds its progress into the checkpoint,
///    so a restart does not count an eviction twice). After a crash the reconciler resumes
///    from the checkpoints; whatever it had done since is done again, which is harmless.
/// 4. **Prune.** A journal object is deleted when the stored checkpoint has applied it and
///    moved past its entries, and it was written at least [`ReconcilerConfig::retention`]
///    ago. A journal object that holds an object the hot tier may still hold is kept: the
///    journal is how a restarted reconciler learns what the hot tier holds, so it has about
///    one entry for each object in the hot tier.
/// 5. **Publish.** `_tiering/usage` is rewritten with the bytes the hot tier holds, the copy
///    lag (the bytes of those that the cold tier does not hold yet) and the time. The same
///    two numbers are in [`Reconciler::hot_bytes`] and [`Reconciler::copy_lag`], for metrics.
///
/// A journal object that cannot be read or does not decode stops the journal of its proxy
/// there; the failure is in the report of every pass until it is dealt with.
///
/// Known limits:
///
/// - An eviction deletes the key from the hot tier without asking it. The journals are read
///   again just before the eviction, but the copies of a pass can take long, so an object that
///   is deleted and put again through a proxy after that read (or whose journal object the
///   listing does not show yet) is deleted from the hot tier before it is copied. The window
///   is the time of that read and the deletes that follow it. Closing it takes a delete that
///   is conditional on the content tag, which [`Backend`] does not have.
/// - A pass that takes longer than the lease ttl loses the lease, and goes on to the end:
///   it may store an older checkpoint over the one of the reconciler that took over. Keep a
///   pass shorter than the ttl (set `parallel` and the ttl accordingly), and do not cancel a
///   pass.
/// - A copy whose outcome the reconciler never learns (the process died, or the lease was lost,
///   between its write to the cold tier and its second read of the hot tier, or the cold
///   tier took the write after the call timed out) can leave a cold copy of a key that was
///   deleted meanwhile. Nothing removes it afterwards, because a delete entry does not touch
///   the cold tier, and a get of the key then finds it. An operator removes it by hand.
/// - A copy removes its own cold object by the tag it reads between its write and the delete,
///   so a put that went straight to the cold tier with the same bytes inside that window is
///   removed too.
/// - A copy that cannot replace a cold object of other bytes (the cold tier refuses the
///   delete) fails in every pass, and the journal of its proxy stops there.
/// - After a restart the journal objects before the position of the stored checkpoint that
///   are still younger than the retention are not read again, and are never deleted.
pub struct Reconciler {
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
    config: ReconcilerConfig,
    lease: Lease,
    books: Books,
}

impl Reconciler {
    /// A reconciler over `hot` and `cold`. Nothing is read or written until a pass.
    #[must_use]
    pub fn new(hot: Arc<dyn Backend>, cold: Arc<dyn Backend>, config: ReconcilerConfig) -> Self {
        let ttl = millis(config.lease_ttl);
        let clock = Arc::clone(&config.clock);
        let lease = Lease::new(Arc::clone(&hot), clock, &config.holder, ttl);
        Self {
            hot,
            cold,
            config,
            lease,
            books: Books::default(),
        }
    }

    /// The bytes of the objects the hot tier holds, by the journals read so far.
    #[must_use]
    pub fn hot_bytes(&self) -> u64 {
        self.books.usage()
    }

    /// The bytes of the objects the hot tier holds that the cold tier does not hold yet.
    #[must_use]
    pub fn copy_lag(&self) -> u64 {
        self.books.lag()
    }

    /// Gives the lease up, if it is held, and forgets what was read.
    pub async fn stop(&mut self) {
        self.lease.release().await;
        self.books = Books::default();
    }

    /// Takes or renews the lease and, if it holds it, does the steps above once.
    pub async fn pass(&mut self) -> PassReport {
        let mut report = PassReport::default();
        if !self.lease.ensure().await {
            self.books = Books::default();
            return report;
        }
        report.held = true;
        self.discover(&mut report).await;
        let ids: Vec<String> = self.books.streams.keys().cloned().collect();
        for id in &ids {
            self.advance(id, &mut report).await;
        }
        self.books.trim_stale();
        self.evict(&ids, &mut report).await;
        for id in &ids {
            self.persist(id, &mut report).await;
            self.prune(id, &mut report).await;
        }
        self.publish(&mut report).await;
        report
    }

    /// Learns the proxies that have a journal, and where each one's checkpoint is.
    async fn discover(&mut self, report: &mut PassReport) {
        let listed = match self.hot.list(PROXIES_PREFIX).await {
            Ok(listed) => listed,
            Err(error) => {
                report.failed.push(error);
                return;
            }
        };
        let ids = listed
            .iter()
            .filter_map(|object| object.key.strip_prefix(PROXIES_PREFIX));
        for id in ids {
            if self.books.streams.contains_key(id) {
                continue;
            }
            match load_checkpoint(self.hot.as_ref(), id).await {
                Ok(checkpoint) => {
                    let stream = Stream::resume(checkpoint);
                    self.books.streams.insert(id.to_owned(), stream);
                }
                Err(error) => report.failed.push(error),
            }
        }
    }

    async fn advance(&mut self, id: &str, report: &mut PassReport) {
        if let Err(error) = self.read_new(id).await {
            report.failed.push(error);
        }
        self.apply_pending(id, report).await;
    }

    /// Reads the journal objects of a proxy that follow the last one read, as far as their
    /// numbers go on without a gap (a listing need not show the latest writes).
    async fn read_new(&mut self, id: &str) -> Result<(), Error> {
        let prefix = journal_prefix(id);
        let page_size = self.config.page_size.max(1);
        loop {
            let after = journal_key(id, self.books.stream(id).read_through);
            let listing = self.hot.list_page(&prefix, Some(&after), page_size);
            let page = listing.await?;
            let last_page = page.len() < page_size;
            for object in page {
                let want = self.books.stream(id).read_through + 1;
                if seq_of(&prefix, &object.key) != Some(want) {
                    return Ok(());
                }
                let body = self.hot.get(&object.key, None).await?;
                let entries = decode_journal(&body)?;
                self.books.read_object(id, want, object.modified, entries);
            }
            if last_page {
                return Ok(());
            }
        }
    }

    /// Applies the journal objects that are read and not applied, in order, until one fails.
    async fn apply_pending(&mut self, id: &str, report: &mut PassReport) {
        loop {
            let Some((seq, entries)) = self.books.stream(id).pending.front().cloned() else {
                return;
            };
            let copied = match self.apply_object(&entries).await {
                Ok(copied) => copied,
                Err(error) => {
                    report.failed.push(error);
                    return;
                }
            };
            report.copied += copied.len();
            for (index, entry) in entries.iter().enumerate() {
                if entry.op != JournalOp::Put {
                    continue;
                }
                let at = (seq, index as u64);
                if copied.contains(&entry.key) {
                    self.books.copied(id, at, &entry.key, entry.time);
                } else {
                    self.books.gone(id, at, &entry.key);
                }
            }
            let stream = self.books.stream(id);
            stream.checkpoint = seq;
            stream.pending.pop_front();
        }
    }

    /// Copies the puts of one journal object. Says which keys are in the cold tier now. Its
    /// deletes change nothing here: the books forget the key when they read the entry, and
    /// the proxy already removed the key from both tiers.
    async fn apply_object(&self, entries: &[JournalEntry]) -> Result<BTreeSet<String>, Error> {
        let puts = entries.iter().filter(|entry| entry.op == JournalOp::Put);
        let keys: BTreeSet<&str> = puts.map(|entry| entry.key.as_str()).collect();
        let copies = stream::iter(keys).map(|key| self.copy_key(key));
        let parallel = self.config.parallel.max(1);
        let outcomes: Copies = copies.buffer_unordered(parallel).collect().await;
        let mut copied = BTreeSet::new();
        for outcome in outcomes {
            if let Some(key) = outcome? {
                copied.insert(key);
            }
        }
        Ok(copied)
    }

    /// Copies one object from the hot tier to the cold tier, create-only, and says whether the
    /// cold tier holds it now (`None`: the hot tier no longer holds the key).
    ///
    /// The tag is read before the bytes and again after the write. A proxy deletes from the
    /// hot tier before the cold tier, so a copy that lands in the cold tier after the proxy's
    /// delete of the cold tier finds, at its second read, that the hot tier has no such
    /// object, and removes what it wrote. That order of the proxy's two deletes is what makes
    /// this sound, and the hot tier must show a delete to the next read.
    async fn copy_key(&self, key: &str) -> Result<Option<String>, Error> {
        for _ in 0..COPY_ATTEMPTS {
            let seen = match self.hot.head(key).await {
                Ok(meta) => meta,
                Err(Error::NotFound(_)) => return Ok(None),
                Err(error) => return Err(error),
            };
            let body = match self.hot.get(key, None).await {
                Ok(body) => body,
                Err(Error::NotFound(_)) => return Ok(None),
                Err(error) => return Err(error),
            };
            self.store_cold(key, &body, &seen).await?;
            match self.hot.head(key).await {
                Ok(now) if now.same_content(&seen) => return Ok(Some(key.to_owned())),
                // Another object is there, and the bytes read may be of either: take the copy
                // back and start over with what the hot tier holds now.
                Ok(_) => self.remove_own_cold_copy(key, &body).await?,
                Err(Error::NotFound(_)) => {
                    self.remove_own_cold_copy(key, &body).await?;
                    return Ok(None);
                }
                Err(error) => {
                    // The hot tier may have lost the object, and a retry would then skip the
                    // key and never look at the cold tier: take the copy back now. If the
                    // object is still there, the next pass copies it again.
                    let _ = self.remove_own_cold_copy(key, &body).await;
                    return Err(error);
                }
            }
        }
        Err(Error::Other(format!(
            "{key} changed in the hot tier at each of {COPY_ATTEMPTS} attempts to copy it"
        )))
    }

    /// Writes `body` to the cold tier, create-only. A cold tier that holds the key is
    /// compared with `body` ([`Reconciler::settle_existing`]).
    async fn store_cold(&self, key: &str, body: &Bytes, seen: &Meta) -> Result<(), Error> {
        match self.cold.put(key, body.clone()).await {
            Err(Error::AlreadyExists(_)) => self.settle_existing(key, body, seen).await,
            other => other,
        }
    }

    /// The cold tier holds `key` already. If its bytes are `body`'s there is nothing to do (a
    /// copy that was done before a crash, or a copy-back whose source is the cold tier).
    /// Otherwise the hot tier's object replaces it, because a get returns the hot tier's, but
    /// only if the hot tier still holds the object that was read: if it does not, the cold
    /// object may be a put that was acknowledged after a delete, and it is left alone (the
    /// caller reads the hot tier again and decides what the copy is).
    async fn settle_existing(&self, key: &str, body: &Bytes, seen: &Meta) -> Result<(), Error> {
        let there = self.cold.head(key).await?;
        if self.same_bytes(key, body, &there, Some(seen)).await? {
            return Ok(());
        }
        match self.hot.head(key).await {
            Ok(now) if now.same_content(seen) => {}
            Ok(_) | Err(Error::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        }
        match self.cold.delete_if_match(key, &there.tag).await {
            Ok(()) | Err(Error::NotFound(_) | Error::PreconditionFailed(_)) => {}
            Err(error) => return Err(error),
        }
        self.cold.put(key, body.clone()).await
    }

    /// Removes the cold tier's object under `key` if its bytes are `body`'s, by its tag, so
    /// that an object that replaced it meanwhile stays. An object of other bytes is somebody
    /// else's and stays.
    async fn remove_own_cold_copy(&self, key: &str, body: &Bytes) -> Result<(), Error> {
        let there = match self.cold.head(key).await {
            Ok(meta) => meta,
            Err(Error::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        if !self.same_bytes(key, body, &there, None).await? {
            return Ok(());
        }
        match self.cold.delete_if_match(key, &there.tag).await {
            Ok(()) | Err(Error::NotFound(_) | Error::PreconditionFailed(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Whether the cold object `there` has the bytes of `body`. The sizes are compared first.
    /// With the tag of the hot object that `body` was read as (`hot`), equal tags settle it
    /// (the tiers tag by the same function); otherwise the cold object is read, chunk by
    /// chunk, and compared with `body`, so that it is never held whole. An object that is gone
    /// by then does not have them.
    async fn same_bytes(
        &self,
        key: &str,
        body: &Bytes,
        there: &Meta,
        hot: Option<&Meta>,
    ) -> Result<bool, Error> {
        if there.size != body.len() as u64 {
            return Ok(false);
        }
        if hot.is_some_and(|hot| hot.tag == there.tag) {
            return Ok(true);
        }
        // An object that is gone since its tag was read is not the same bytes.
        let mut chunks = match self.cold.get_stream(key, None).await {
            Ok(chunks) => chunks,
            Err(Error::NotFound(_)) => return Ok(false),
            Err(error) => return Err(error),
        };
        let mut at = 0;
        while let Some(chunk) = chunks.try_next().await? {
            let end = at + chunk.len();
            if body.get(at..end) != Some(&chunk[..]) {
                return Ok(false);
            }
            at = end;
        }
        Ok(at == body.len())
    }

    /// Deletes objects from the hot tier, oldest first, while it holds more than the low
    /// water, if it holds more than the high water. The journals of `ids` are read once more
    /// first, so that what a proxy did meanwhile is taken into account.
    async fn evict(&mut self, ids: &[String], report: &mut PassReport) {
        let budget = self.config.hot_budget as f64;
        let used = self.books.usage() as f64;
        if used <= budget * self.config.high_water {
            return;
        }
        for id in ids {
            if let Err(error) = self.read_new(id).await {
                report.failed.push(error);
                return;
            }
        }
        self.books.trim_stale();
        let low = (budget * self.config.low_water) as u64;
        while self.books.usage() > low {
            let Some((proxy, victim)) = self.books.next_victim() else {
                return;
            };
            if self.books.is_current(&proxy, victim.at, &victim.key) {
                if let Err(error) = self.hot.delete(&victim.key).await {
                    report.failed.push(error);
                    return;
                }
                self.books.forget(&victim.key);
                report.evicted += 1;
            }
            self.books.pop(&proxy);
        }
    }

    /// Stores the checkpoint of a proxy if it moved.
    async fn persist(&mut self, id: &str, report: &mut PassReport) {
        let wanted = self.books.stream(id).snapshot();
        if wanted == self.books.stream(id).persisted {
            return;
        }
        let key = checkpoint_key(id);
        match write_object(self.hot.as_ref(), &key, to_body(&wanted)).await {
            Ok(()) => self.books.stream(id).persisted = wanted,
            Err(error) => report.failed.push(error),
        }
    }

    /// Deletes the journal objects of a proxy that the stored checkpoint is done with and
    /// that are older than the retention, oldest first.
    async fn prune(&mut self, id: &str, report: &mut PassReport) {
        let now = self.config.clock.now();
        let retention = millis(self.config.retention);
        loop {
            let stream = self.books.stream(id);
            let Some(&(seq, modified)) = stream.retained.front() else {
                return;
            };
            if seq >= stream.persisted.evict_seq {
                return;
            }
            if modified.saturating_add(retention) > now {
                return;
            }
            if let Err(error) = self.hot.delete(&journal_key(id, seq)).await {
                report.failed.push(error);
                return;
            }
            self.books.stream(id).retained.pop_front();
            report.pruned += 1;
        }
    }

    async fn publish(&self, report: &mut PassReport) {
        let usage = Usage {
            hot_bytes: self.books.usage(),
            copy_lag: self.books.lag(),
            time: self.config.clock.now(),
        };
        let body = to_body(&usage);
        if let Err(error) = write_object(self.hot.as_ref(), USAGE_KEY, body).await {
            report.failed.push(error);
        }
    }
}
