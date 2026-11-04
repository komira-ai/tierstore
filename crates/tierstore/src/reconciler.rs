//! The reconciler: copies what the proxies journal to the cold tier, and frees the hot tier.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use tokio::time::Instant;

use crate::backend::ByteRange;
use crate::journal::{
    PROXIES_PREFIX, USAGE_KEY, Usage, decode_journal, journal_key, journal_prefix, load_checkpoint,
    seq_of, store_checkpoint, to_body, write_object,
};
use crate::lease::Lease;
use crate::state::Candidate;
use crate::state::{Books, Stream};
use crate::{Backend, Clock, Error, JournalEntry, JournalOp, Meta};

mod drain;

/// How many times a copy reads the cold tier's object again after it changed under the copy.
const COPY_ROUNDS: u32 = 3;

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
    /// How long the format object must have said `drain` (by the monotonic clock of the
    /// reconciler) before it writes `drained`: more than the time a proxy takes to read the format
    /// (`ProxyConfig::format_refresh`), so that every proxy has read the format again.
    pub drain_settle: Duration,
    /// The time, for the lease, the retention and the usage object. It must agree with the
    /// times the hot tier gives its objects to within the lease ttl and the retention.
    pub clock: Arc<dyn Clock>,
}

impl ReconcilerConfig {
    /// The settings for a reconciler `holder` and a hot tier of `hot_budget` bytes, with
    /// high and low water of 0.8 and 0.7, 16 parallel copies, a lease ttl of 30 seconds, a
    /// retention of an hour, listings of 100 journal objects, and a drain settle time of 15 seconds.
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
            drain_settle: Duration::from_secs(15),
            clock,
        }
    }
}

/// What one [`Reconciler::pass`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PassReport {
    /// Whether this reconciler held the lease at the start of the pass. If not, nothing else was
    /// done. If the check made after the copies and evictions finds the lease gone, nothing is
    /// stored after that check (no checkpoint, no pruning, no usage), and that is not in
    /// `failed`; `copied` and `evicted` still count the work done before the check. A takeover
    /// after the check is not seen. The next pass tries to take or renew the lease again, and
    /// takes it at once if it was deleted.
    pub held: bool,
    /// The objects copied to the cold tier, or found there already.
    pub copied: usize,
    /// The objects removed from the hot tier.
    pub evicted: usize,
    /// The journal objects deleted.
    pub pruned: usize,
    /// The calls that failed, in the order they did. The steps that depend on a failed one
    /// were left for the next pass.
    pub failed: Vec<Error>,
}

/// The outcome of the copies of one journal object: the key and the content tag of the hot
/// tier's object if the cold tier is settled with it now, or nothing if the hot tier no longer
/// holds it or the key is another put's.
type Copies = Vec<Result<Option<(String, String)>, Error>>;

/// What the entries of one journal object say of one key.
struct Wanted<'a> {
    /// The op of the last entry that puts the key.
    kind: JournalOp,
    /// The cold tags that the reserved puts of the key name.
    reservations: Vec<&'a str>,
    /// The cold tag of the last mirror entry of the key.
    mirror: Option<&'a str>,
    /// The hot tag of the last mirror entry of the key: the tag the hot tier gave the copy. It is
    /// only ever compared with the hot tier's tags, never with a cold tag.
    mirror_hot: Option<&'a str>,
    /// The index, in the journal object, of the last entry that puts the key.
    index: u64,
    /// Whether that entry is still the latest word on the key in the journals read: no later
    /// put or delete of the key was read, from this proxy or another.
    current: bool,
}

/// What each key put by the entries is wanted as.
fn wanted(entries: &[JournalEntry]) -> BTreeMap<&str, Wanted<'_>> {
    let mut wanted: BTreeMap<&str, Wanted<'_>> = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if !entry.op.is_put() {
            continue;
        }
        let slot = wanted.entry(entry.key.as_str()).or_insert(Wanted {
            kind: entry.op,
            reservations: Vec::new(),
            mirror: None,
            mirror_hot: None,
            index: 0,
            current: true,
        });
        slot.kind = entry.op;
        slot.index = index as u64;
        let tag = entry.cold_tag.as_deref();
        match entry.op {
            JournalOp::PutReserved => slot.reservations.extend(tag),
            JournalOp::PutMirror => {
                slot.mirror = tag;
                slot.mirror_hot = entry.hot_tag.as_deref();
            }
            _ => {}
        }
    }
    wanted
}

/// What one look at a key came to.
enum Round {
    /// Settled: with the content tag of the hot tier's object if the cold tier holds the key's
    /// object now, or without if there is nothing to copy.
    Done(Option<String>),
    /// The cold tier's object changed during the look: look again.
    Again,
}

/// What an eviction did to an object of the hot tier.
enum Eviction {
    /// It deleted the object.
    Removed,
    /// The hot tier no longer had it.
    Gone,
    /// The object was left: the hot tier holds another object under the key now, or the cold tier
    /// does not hold a real object of the hot object's content (nothing, a reservation, or
    /// another object), so the hot object may be the only copy. It stays a candidate.
    Kept,
}

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
/// 1. **Tail the journals.** The journal objects of every proxy that follow its checkpoint are read
///    first; then, for each proxy, in the order of the journal objects from its checkpoint, a
///    journal object is applied: its puts are settled with the cold
///    tier, up to [`ReconcilerConfig::parallel`] at a time, one look at a time for each key
///    (the hot tier's object, then the cold tier's), by the rules below; then its deletes are
///    applied to the cold tier, by the last rule below. Then the object counts as applied.
///
///    A cold object that a *reservation* is (a marked 56-byte object; see
///    [`Backend::put_reservation`]) holds a key for a put that the proxy acknowledged. The rules
///    for a key, by the last put entry of the key in the journal object:
///    - a `PutMirror` that a later put or delete of the key supersedes is skipped: the hot object
///      is not the one it mirrors, so it is neither dropped nor counted as copied. A later entry
///      of the same proxy supersedes it; so does a put of *another* proxy that the journals read
///      so far make the word on the key, whichever proxy's journal was read first: the mirror of
///      a proxy is never the word on a key that another proxy's put holds. The journals do not
///      order the writes of different proxies, so the guarantee is about ownership and not about
///      time: a mirror deletes the hot object only if the object has the content tag the
///      mirror's own entry names as the hot tier's tag of its copy (`JournalEntry::hot_tag`;
///      `If-Match`, [`Backend::delete_if_match`]). The proxy took that tag from the hot tier's
///      create-only write that made the copy ([`Backend::put_tagged`]: one operation, no HEAD
///      after it), so it is the tag of the copy itself, and an object of other bytes that
///      another proxy put under the key since (before or after the entry was written) is never
///      deleted by a mirror. A mirror that names no hot tag owns nothing: it deletes nothing, is
///      never an eviction candidate, and stays so when the books are rebuilt from a checkpoint.
///      Tags are never compared across tiers (a directory tier tags by SHA-256, an S3 tier by
///      ETag): the cold tag is compared with the cold tier's, the hot tag with the hot tier's. A
///      tag names content, so a put of the *same bytes* by another proxy has the copy's tag and
///      is not told from it, whether or not its journal object has been read;
///    - the hot tier no longer holds the key: nothing to copy; for a `PutReserved`, the
///      reservations its entries name are deleted by their tags (a put and a delete that were
///      both acknowledged leave no reservation behind), and no other cold object is touched;
///    - the cold tier holds an object (not a reservation): a `PutReserved` or a `Put` counts as
///      copied; a `PutMirror` counts as copied, and so becomes an eviction candidate, only if the
///      cold object has the cold tag its entry names *and* the hot object has the hot tag its
///      entry names; if the cold object has another tag the hot copy is a stale mirror and is
///      deleted by the hot tag of its entry; if the hot object has another tag it is not the
///      mirror's, and is left and not counted;
///    - the cold tier holds a reservation: a `PutReserved` whose entries name its tag replaces it
///      with the hot bytes in one conditional write (if the reservation changed meanwhile, the
///      key is looked at again, three times at most, then the pass reports it); a reservation
///      its entries do not name is another put's, and nothing is touched; a `Put` leaves it
///      alone; a `PutMirror` is an orphan and the hot copy is deleted by the hot tag of its entry;
///    - the cold tier holds nothing: a `PutReserved` keeps the hot object and the pass reports
///      the key (a cold bucket that was repointed or restored is what it means, and the
///      key stays refused until someone looks); a `PutMirror` deletes the hot copy by the hot tag of its entry;
///      a `Put` is copied create-only (an object that appears meanwhile counts as copied);
///    - delete entries: one is applied to the cold tier (the cold object goes by its tag, unless
///      the hot tier holds the key again and the cold object is of that object) for a key whose
///      last put in the same journal object is a plain `Put`, or that has none there and whose
///      latest put entry the reconciler read in the last [`ReconcilerConfig::retention`] is not
///      a `PutReserved` or a `PutMirror`; never for another key, and never for a reservation. A
///      proxy that reserves removes the cold tier's object itself before it deletes from the hot
///      tier, and an entry applied late could take an object that another put made since.
/// 2. **Evict.** The reconciler adds up the bytes of the objects the hot tier holds from the
///    journals alone: the sizes of the puts, less the deletes and the evictions. Above
///    [`ReconcilerConfig::high_water`] of the budget it deletes objects from the hot tier,
///    oldest first by the time in their journal entries (journal order within a proxy), down
///    to [`ReconcilerConfig::low_water`]. Only an object of an applied journal object is a
///    candidate, so the cold tier holds it, and the reconciler does not list the hot tier. Before
///    it deletes anything it reads the journals once more, so that an object that was deleted or
///    put again since is skipped. The delete is conditional on the content tag the copy found
///    (`If-Match`, [`Backend::delete_if_match`]), so an object that was put again after the
///    copy, with other bytes, is kept; for an entry applied by an earlier reconciler the tag is
///    read from the hot tier just before the delete, and only if the cold tier holds a real
///    object of that content then (not nothing, not a reservation, not another object: the hot
///    object may be the only copy, and stays, and is asked about again by the next pass).
/// 3. **Checkpoint.** The lease is checked once more first (a HEAD, no renewal): a pass that
///    finds it lost stores nothing more, and forgets what it read. Then, for each proxy,
///    `_tiering/checkpoint/<proxy_id>` is read back, joined with what this pass has (the
///    larger of each number), and written (an `If-Match` put, after the first) only if that is
///    more than what is stored, so a stored checkpoint never moves backwards: every journal
///    object up to a number is applied, and every entry before a position is done with (the
///    eviction folds its progress into the checkpoint, so a restart does not count an eviction
///    twice). After a crash the reconciler resumes from the checkpoints; whatever it had done
///    since is done again, which is harmless.
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
/// - A tag names content, not a write: an object that is deleted and put again with the same
///   bytes, after the copy that found it and before its eviction, has the tag the eviction
///   deletes on, and is deleted from the hot tier unless its own journal entry shows in the
///   read that precedes the eviction. It is in the cold tier by then in the common case (the
///   copy found it), so only the hot copy goes.
/// - A pass that takes longer than the lease ttl loses the lease. It still copies, and may still
///   evict, beside the reconciler that took over (there is no fencing token; copies are
///   create-only), but once it finds the lease gone, before the checkpoint, it stores no
///   checkpoint, prunes nothing and publishes no usage. A checkpoint is never stored lower. The
///   check is a HEAD, not a fence: a pass that takes the lease over between the check and the
///   stores can still prune or publish usage, and the checkpoint it stores is joined with the
///   stored one. Keep a pass shorter than the ttl (set `parallel` and the ttl accordingly), and
///   do not cancel a pass.
/// - A checkpoint object that does not decode fails the pass of its proxy each time, until the
///   object is deleted (the proxy then starts over from journal object 1).
/// - A delete entry of a key that a reserved put or a mirror last put (by what the reconciler read
///   in the last retention) is not applied to the cold tier: the proxy removed the cold object
///   itself. One that is applied after more than the retention since that put is applied as a
///   plain key's is, which can take an object that a put that went straight to the cold tier made
///   after the delete.
/// - While `_tiering/format` says `drain`, the last step of a pass that had no failure, after
///   [`ReconcilerConfig::drain_settle`] of seeing it, replaces it by `drained` (see the module
///   `format`).
/// - After a restart the journal objects before the position of the stored checkpoint that
///   are still younger than the retention are not read again, and are never deleted.
pub struct Reconciler {
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
    config: ReconcilerConfig,
    lease: Lease,
    books: Books,
    /// Since when the format object has said `drain`, as this reconciler has seen it.
    draining: Option<Instant>,
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
            draining: None,
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
        self.forget_drain();
    }

    /// Takes or renews the lease and, if it holds it, does the steps above once.
    pub async fn pass(&mut self) -> PassReport {
        let mut report = PassReport::default();
        if !self.lease.ensure().await {
            self.books = Books::default();
            self.forget_drain();
            return report;
        }
        report.held = true;
        self.discover(&mut report).await;
        let ids: Vec<String> = self.books.streams.keys().cloned().collect();
        // Every proxy's journal is read before any is applied, so that a mirror of one proxy
        // is judged against the later puts and deletes of all of them.
        for id in &ids {
            if let Err(error) = self.read_new(id).await {
                report.failed.push(error);
            }
        }
        for id in &ids {
            self.apply_pending(id, &mut report).await;
        }
        self.books.trim_stale();
        let kept = millis(self.config.retention);
        let now = self.config.clock.now();
        self.books.forget_reserved_before(now.saturating_sub(kept));
        self.evict(&ids, &mut report).await;
        match self.lease.still_held().await {
            Ok(true) => {}
            Ok(false) => {
                self.books = Books::default();
                self.forget_drain();
                return report;
            }
            Err(error) => {
                report.failed.push(error);
                return report;
            }
        }
        for id in &ids {
            self.persist(id, &mut report).await;
            self.prune(id, &mut report).await;
        }
        self.settle_drain(&mut report).await;
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
            let copied = match self.apply_object(id, seq, &entries).await {
                Ok(copied) => copied,
                Err(error) => {
                    report.failed.push(error);
                    return;
                }
            };
            report.copied += copied.len();
            for (index, entry) in entries.iter().enumerate() {
                if !entry.op.is_put() {
                    continue;
                }
                let at = (seq, index as u64);
                match copied.get(&entry.key) {
                    Some(tag) => {
                        let tag = Some(tag.clone());
                        self.books.copied(id, at, &entry.key, entry.time, tag);
                    }
                    None => self.books.gone(id, at, &entry.key),
                }
            }
            let stream = self.books.stream(id);
            stream.checkpoint = seq;
            stream.pending.pop_front();
        }
    }

    /// Settles the puts of one journal object with the cold tier, then applies the deletes that
    /// are applied. Says which keys the cold tier is settled with, and the content tag of the hot
    /// tier's object for each.
    async fn apply_object(
        &self,
        proxy: &str,
        seq: u64,
        entries: &[JournalEntry],
    ) -> Result<BTreeMap<String, String>, Error> {
        let mut wanted = wanted(entries);
        for (key, want) in &mut wanted {
            want.current = self.books.is_current(proxy, (seq, want.index), key);
        }
        let copies = stream::iter(&wanted).map(|(key, want)| self.copy_key(key, want));
        let parallel = self.config.parallel.max(1);
        let outcomes: Copies = copies.buffer_unordered(parallel).collect().await;
        let mut copied = BTreeMap::new();
        for outcome in outcomes {
            if let Some((key, tag)) = outcome? {
                copied.insert(key, tag);
            }
        }
        for entry in entries.iter().filter(|e| e.op == JournalOp::Delete) {
            if self.applies_to_cold(&entry.key, wanted.get(entry.key.as_str())) {
                self.delete_cold(&entry.key).await?;
            }
        }
        Ok(copied)
    }

    /// Whether the delete entry of `key` is applied to the cold tier: if the object puts the key,
    /// by the kind of its last put (a plain `Put` is, a reserved put or a mirror is not); if it
    /// does not, unless the latest put entry read for the key is a reserved put or a mirror. A
    /// proxy that writes those removes the cold tier's object itself, before it deletes from the
    /// hot tier, so an entry applied late could take an object that another put made since.
    fn applies_to_cold(&self, key: &str, put: Option<&Wanted<'_>>) -> bool {
        match put {
            Some(want) => want.kind == JournalOp::Put,
            None => !self.books.is_reserved(key),
        }
    }

    /// Settles one key with the cold tier (see the type), looking again if the cold tier's object
    /// changes under a replacement, up to [`COPY_ROUNDS`] looks.
    async fn copy_key(
        &self,
        key: &str,
        want: &Wanted<'_>,
    ) -> Result<Option<(String, String)>, Error> {
        for _ in 0..COPY_ROUNDS {
            if let Round::Done(tag) = self.copy_round(key, want).await? {
                return Ok(tag.map(|tag| (key.to_owned(), tag)));
            }
        }
        let text = format!("the cold copy of {key} changed under the copy");
        Err(Error::Unavailable(text))
    }

    /// One look at a key: the hot tier's object, then the cold tier's.
    async fn copy_round(&self, key: &str, want: &Wanted<'_>) -> Result<Round, Error> {
        if want.kind == JournalOp::PutMirror && !want.current {
            // A put or a delete of the key was journaled after the mirror: the hot tier's object
            // is that put's, not the one the mirror was made of, and the mirror owns nothing.
            return Ok(Round::Done(None));
        }
        let hot = match self.hot.head(key).await {
            Ok(hot) => hot,
            Err(Error::NotFound(_)) => {
                self.release_reservations(key, want).await?;
                return Ok(Round::Done(None));
            }
            Err(error) => return Err(error),
        };
        match self.cold.head(key).await {
            Ok(cold) if cold.reservation => self.replace_reservation(key, want, &hot, &cold).await,
            Ok(cold) => self.settle(key, want, &hot, &cold).await,
            Err(Error::NotFound(_)) => self.fill_empty(key, want, &hot).await,
            Err(error) => Err(error),
        }
    }

    /// The hot tier no longer holds `key`: deletes the reservations that the reserved puts of the
    /// key name, each by its own tag, so that a put and a delete that were both acknowledged leave
    /// no reservation. A reservation with another tag, or none, is not the put's and stays.
    async fn release_reservations(&self, key: &str, want: &Wanted<'_>) -> Result<(), Error> {
        if want.kind != JournalOp::PutReserved {
            return Ok(());
        }
        for tag in &want.reservations {
            match self.cold.delete_if_match(key, tag).await {
                Ok(()) | Err(Error::NotFound(_) | Error::PreconditionFailed(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// The cold tier holds an object that is not a reservation. A put counts as copied; a mirror
    /// does only if the object is the one it mirrors, and else it is a stale copy to drop.
    async fn settle(
        &self,
        key: &str,
        want: &Wanted<'_>,
        hot: &Meta,
        cold: &Meta,
    ) -> Result<Round, Error> {
        if want.kind != JournalOp::PutMirror {
            return Ok(Round::Done(Some(hot.tag.clone())));
        }
        if want.mirror != Some(cold.tag.as_str()) {
            return self.drop_mirror(key, want).await;
        }
        // The cold tier holds the mirrored object. The hot object is the mirror's copy of it, and
        // so an eviction candidate, only if it has the tag the hot tier gave that copy; any other
        // hot object (a put whose journal was not read yet) is not the mirror's, and is left and
        // not counted as copied. An entry that names no hot tag owns nothing.
        if want.mirror_hot == Some(hot.tag.as_str()) {
            return Ok(Round::Done(Some(hot.tag.clone())));
        }
        Ok(Round::Done(None))
    }

    /// The cold tier holds a reservation. The put it was made for replaces it with the hot bytes,
    /// in one conditional write on its tag; a reservation that is another put's is left; a mirror
    /// of an object that is gone from the cold tier is dropped.
    async fn replace_reservation(
        &self,
        key: &str,
        want: &Wanted<'_>,
        hot: &Meta,
        cold: &Meta,
    ) -> Result<Round, Error> {
        match want.kind {
            JournalOp::PutMirror => self.drop_mirror(key, want).await,
            JournalOp::PutReserved if want.reservations.contains(&cold.tag.as_str()) => {
                let body = match self.hot.get_stream(key, None).await {
                    Ok(body) => body,
                    Err(Error::NotFound(_)) => return Ok(Round::Again),
                    Err(error) => return Err(error),
                };
                // The hot object goes to the cold tier as it is read, never held whole.
                let sent = self
                    .cold
                    .put_stream_if_match(key, Some(hot.size), body, &cold.tag)
                    .await;
                match sent {
                    Ok(_) => Ok(Round::Done(Some(hot.tag.clone()))),
                    Err(Error::PreconditionFailed(_) | Error::NotFound(_)) => Ok(Round::Again),
                    Err(error) => Err(error),
                }
            }
            _ => Ok(Round::Done(None)),
        }
    }

    /// The cold tier holds nothing under the key. A reserved put keeps its object and reports the
    /// key; a mirror is dropped; a plain put is copied create-only.
    async fn fill_empty(&self, key: &str, want: &Wanted<'_>, hot: &Meta) -> Result<Round, Error> {
        match want.kind {
            JournalOp::PutReserved => {
                let text = format!("the cold tier holds nothing for {key}; the hot object is kept");
                Err(Error::Other(text))
            }
            JournalOp::PutMirror => self.drop_mirror(key, want).await,
            _ => {
                let body = match self.hot.get_stream(key, None).await {
                    Ok(body) => body,
                    Err(Error::NotFound(_)) => return Ok(Round::Done(None)),
                    Err(error) => return Err(error),
                };
                match self.cold.put_stream(key, Some(hot.size), body).await {
                    Ok(()) | Err(Error::AlreadyExists(_)) => Ok(Round::Done(Some(hot.tag.clone()))),
                    Err(error) => Err(error),
                }
            }
        }
    }

    /// Deletes a mirror from the hot tier, by the hot tag its own entry names (the tag the hot tier
    /// gave the copy, never the cold tag): only an object that has that tag is the mirror's. Any
    /// other object (a put of another proxy, replaced or removed meanwhile) stays, and so does
    /// the object of a mirror entry that names no hot tag (one written by a release before the
    /// field), which is left to the eviction's normal path.
    async fn drop_mirror(&self, key: &str, want: &Wanted<'_>) -> Result<Round, Error> {
        let Some(tag) = want.mirror_hot else {
            return Ok(Round::Done(None));
        };
        match self.hot.delete_if_match(key, tag).await {
            Ok(()) | Err(Error::NotFound(_) | Error::PreconditionFailed(_)) => {
                Ok(Round::Done(None))
            }
            Err(error) => Err(error),
        }
    }

    /// Applies the delete of a key of a release without reservations: the cold tier's object goes, by its tag, unless the hot tier holds the key again and the cold
    /// tier's object is of that object (the same size and content tag). A reservation, which is
    /// another put's, and an absent key are left alone.
    async fn delete_cold(&self, key: &str) -> Result<(), Error> {
        let in_hot = match self.hot.head(key).await {
            Ok(meta) => Some(meta),
            Err(Error::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        let in_cold = match self.cold.head(key).await {
            Ok(meta) if !meta.reservation => meta,
            Ok(_) | Err(Error::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        if in_hot.is_some_and(|hot| hot.same_content(&in_cold)) {
            return Ok(());
        }
        match self.cold.delete_if_match(key, &in_cold.tag).await {
            Ok(()) | Err(Error::NotFound(_) | Error::PreconditionFailed(_)) => Ok(()),
            Err(error) => Err(error),
        }
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
        let mut kept = Vec::new();
        self.evict_down(low, report, &mut kept).await;
        // A candidate that was left stays a candidate (oldest first, as before), so that a later
        // pass asks again once the cold tier holds the object.
        for (proxy, candidate) in kept.into_iter().rev() {
            self.books.requeue(&proxy, candidate);
        }
    }

    /// Evicts candidates, oldest first, until the hot tier holds no more than `low` bytes, there
    /// is no candidate left, or a call failed. The candidates that were left are put in `kept`,
    /// with the name of their proxy.
    async fn evict_down(
        &mut self,
        low: u64,
        report: &mut PassReport,
        kept: &mut Vec<(String, Candidate)>,
    ) {
        while self.books.usage() > low {
            let Some((proxy, victim)) = self.books.next_victim() else {
                return;
            };
            if self.books.is_current(&proxy, victim.at, &victim.key) {
                match self.evict_one(&victim).await {
                    Ok(Eviction::Removed) => {
                        self.books.forget(&victim.key);
                        report.evicted += 1;
                    }
                    Ok(Eviction::Gone) => self.books.forget(&victim.key),
                    Ok(Eviction::Kept) => kept.push((proxy.clone(), victim)),
                    Err(error) => {
                        report.failed.push(error);
                        return;
                    }
                }
            }
            self.books.pop(&proxy);
        }
    }

    /// Deletes one candidate from the hot tier, on the content tag the copy found. An entry that an
    /// earlier reconciler applied has none: the tag is read from the hot tier, and the cold tier is
    /// asked first whether it holds a real object of that content (not nothing, not a reservation,
    /// not another object), since nothing here has seen the copy.
    async fn evict_one(&self, victim: &Candidate) -> Result<Eviction, Error> {
        let tag = match &victim.tag {
            Some(tag) => tag.clone(),
            None => {
                let hot = match self.hot.head(&victim.key).await {
                    Ok(meta) => meta,
                    Err(Error::NotFound(_)) => return Ok(Eviction::Gone),
                    Err(error) => return Err(error),
                };
                match self.cold_holds_copy(&victim.key, &hot).await? {
                    Some(true) => hot.tag,
                    Some(false) => return Ok(Eviction::Kept),
                    None => return Ok(Eviction::Gone),
                }
            }
        };
        match self.hot.delete_if_match(&victim.key, &tag).await {
            Ok(()) => Ok(Eviction::Removed),
            Err(Error::NotFound(_)) => Ok(Eviction::Gone),
            Err(Error::PreconditionFailed(_)) => Ok(Eviction::Kept),
            Err(error) => Err(error),
        }
    }

    /// Whether the cold tier holds a real object under `key` of the content of `hot`: not a
    /// reservation, not nothing, and not another object. The tags are not compared (two tiers of
    /// different kinds tag the same bytes differently): the sizes are, and if they agree the bytes.
    /// `None` if the hot object is gone by the time it is read.
    async fn cold_holds_copy(&self, key: &str, hot: &Meta) -> Result<Option<bool>, Error> {
        match self.cold.head(key).await {
            Ok(cold) if !cold.reservation && cold.size == hot.size => {
                match self.same_bytes(key, hot, &cold).await? {
                    Some(true) => self.still_the_object(key, &cold).await.map(Some),
                    other => Ok(other),
                }
            }
            Ok(_) => Ok(Some(false)),
            Err(Error::NotFound(_)) => Ok(Some(false)),
            Err(error) => Err(error),
        }
    }

    /// Whether the cold tier still holds, under `key`, the real object of `seen` (what the HEAD
    /// before the comparison gave): the same content tag and not a reservation. The comparison
    /// reads bind each range to that tag, so this look only finds an object that was removed or
    /// replaced after the last range was read.
    async fn still_the_object(&self, key: &str, seen: &Meta) -> Result<bool, Error> {
        match self.cold.head(key).await {
            Ok(now) => Ok(!now.reservation && now.tag == seen.tag),
            Err(Error::NotFound(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Whether the cold tier holds the bytes of `hot`, which is what a HEAD of the hot tier gave,
    /// in the object `cold` (what a HEAD of the cold tier gave). Both tiers are read in ranges of
    /// at most [`COMPARE_RANGE`] bytes, each read with its content tag and mark in one call, so
    /// that every byte compared is of the object whose tag a HEAD gave: a range of another object
    /// (a key replaced between the HEAD and the read, and put back before the next look) has
    /// another tag, a range of a reservation is marked, and either ends the comparison with
    /// `Some(false)`, as does a range that is not the length asked for; a read ends at the first
    /// difference. Neither tier is held whole. An object that is not there to be read is not an
    /// error: the cold one is not held (`Some(false)`), and the hot one is gone (`None`).
    async fn same_bytes(&self, key: &str, hot: &Meta, cold: &Meta) -> Result<Option<bool>, Error> {
        let mut offset = 0u64;
        while offset < hot.size {
            let len = (hot.size - offset).min(COMPARE_RANGE);
            let range = ByteRange::new(offset, len);
            let (mine, _, tag) = match self.hot.get_marked(key, Some(range)).await {
                Ok(read) => read,
                Err(Error::NotFound(_)) => return Ok(None),
                Err(error) => return Err(error),
            };
            if tag != hot.tag || mine.len() as u64 != len {
                return Ok(Some(false));
            }
            let (theirs, marked, tag) = match self.cold.get_marked(key, Some(range)).await {
                Ok(read) => read,
                Err(Error::NotFound(_)) => return Ok(Some(false)),
                Err(error) => return Err(error),
            };
            if marked || tag != cold.tag || theirs != mine {
                return Ok(Some(false));
            }
            offset += len;
        }
        Ok(Some(true))
    }

    /// Stores the checkpoint of a proxy if it moved past the stored one. What is stored after
    /// the call (the join of the two) is what the prune follows.
    async fn persist(&mut self, id: &str, report: &mut PassReport) {
        let wanted = self.books.stream(id).snapshot();
        let persisted = self.books.stream(id).persisted.clone();
        if persisted.joined(&wanted) == persisted {
            return;
        }
        match store_checkpoint(self.hot.as_ref(), id, &wanted).await {
            Ok(stored) => self.books.stream(id).persisted = stored,
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

/// The most bytes of the hot object that one read of a comparison holds.
const COMPARE_RANGE: u64 = 1 << 20;
