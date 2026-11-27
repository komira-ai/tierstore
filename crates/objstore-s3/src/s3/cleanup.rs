//! The work that follows a write that failed: the look at the key of a single PUT that got no
//! answer (`cut_put.rs`) and the abort of a multipart upload that failed or was dropped
//! (`multipart.rs`).
//!
//! The work never holds up the write: the write gives its own error as soon as it has it (and
//! with it the caller's hold on anything, a write permit of a server above this store), and
//! the work runs in a task of its own. What bounds it:
//!
//! - at most [`CLEANUPS_AT_ONCE`] tasks of a store and its clones run at once, each in a place
//!   it takes before it is spawned; none is spawned without a place, so a store that stalls
//!   every write has at most that many tasks, whatever the rate of the writes;
//! - each task ends at [`CLEANUP_DEADLINE`] after its requests began, whatever they are doing
//!   (a request of the store may take `request_timeout`, which is longer);
//! - a look that finds every place taken is not made, and is counted
//!   ([`Cleanups::looks_skipped`]); the object of the wrong length it might have removed stays,
//!   as one whose look failed does;
//! - an abort that finds every place taken, that fails, or that its deadline ends is kept on a
//!   list of at most [`FAILED_ABORTS_KEPT`] uploads ([`Cleanups::failed_aborts`], the size of
//!   the list now). The list is full: the oldest is dropped to make room; an upload tried
//!   [`ABORT_TRIES`] times is dropped too ([`Cleanups::aborts_dropped`]). The bucket's lifecycle
//!   rule (`AbortIncompleteMultipartUpload`) removes a dropped upload.
//!
//! The list is sent again when the store answers a request again: any answer, of any status,
//! proves the store is there. One pass at a time, in a place like any other task: it aborts the
//! uploads of the list one by one, oldest first, and stops at the first that fails (back on the
//! list, at its end). After a pass that stopped so, the next one waits: 1 s
//! ([`RETRY_FIRST`]) after the first such pass, twice the wait before after each next one, at
//! most 60 s ([`RETRY_CAP`]), each wait cut by up to half at random so that stores that failed
//! together do not ask again together. A pass that empties the list starts the waits again.

use std::collections::VecDeque;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use super::Conn;
use super::multipart::try_abort;
use crate::ObjectKey;

/// How many tasks of the work after failed writes a store and its clones run at once.
pub const CLEANUPS_AT_ONCE: usize = 16;

/// How long the requests of one task of that work may take in all.
pub const CLEANUP_DEADLINE: Duration = Duration::from_secs(10);

/// The most failed aborts a store keeps to send again.
pub const FAILED_ABORTS_KEPT: usize = 1024;

/// How many times an abort is sent before its upload is left to the bucket's lifecycle rule.
pub const ABORT_TRIES: u32 = 8;

/// The wait after the first pass over the failed aborts that stopped on a failure.
pub const RETRY_FIRST: Duration = Duration::from_secs(1);

/// The longest wait between two passes over the failed aborts.
pub const RETRY_CAP: Duration = Duration::from_secs(60);

/// What the work after failed writes did and holds, since the store was made (module docs). An
/// [`S3Store`](super::S3Store) and its clones count together.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cleanups {
    /// Looks after single PUTs that got no answer that were not made: every place was taken.
    pub looks_skipped: u64,
    /// The multipart uploads whose abort failed (or found no place), kept to be aborted again:
    /// the size of the list now, at most [`FAILED_ABORTS_KEPT`].
    pub failed_aborts: u64,
    /// Failed aborts dropped from that list, the oldest when it was full or one tried
    /// [`ABORT_TRIES`] times: left to the bucket's lifecycle rule.
    pub aborts_dropped: u64,
}

/// An upload to abort, and how many times its abort was sent.
#[derive(Debug, PartialEq, Eq)]
struct Upload {
    key: ObjectKey,
    id: String,
    tries: u32,
}

/// The failed aborts, oldest first, at most `bound` of them; and the waits between passes.
#[derive(Debug)]
struct Kept {
    list: VecDeque<Upload>,
    bound: usize,
    dropped: u64,
    /// A pass over the list runs.
    passing: bool,
    /// The wait after the last pass that stopped on a failure; zero when the last pass emptied
    /// the list, or none ran.
    wait: Duration,
    /// No pass begins before this.
    not_before: Option<Instant>,
}

impl Kept {
    fn new(bound: usize) -> Self {
        Self {
            list: VecDeque::new(),
            bound,
            dropped: 0,
            passing: false,
            wait: Duration::ZERO,
            not_before: None,
        }
    }

    /// Keeps `upload` at the end of the list; drops it if it was tried [`ABORT_TRIES`] times,
    /// and the oldest if the list is full.
    fn keep(&mut self, upload: Upload) {
        if upload.tries >= ABORT_TRIES {
            tracing::warn!(key = %upload.key, tries = upload.tries, "a multipart upload could not be aborted; the bucket's lifecycle rule must remove it");
            self.dropped += 1;
            return;
        }
        if self.list.len() >= self.bound
            && let Some(oldest) = self.list.pop_front()
        {
            tracing::warn!(key = %oldest.key, "too many failed aborts kept; the oldest is left to the bucket's lifecycle rule");
            self.dropped += 1;
        }
        self.list.push_back(upload);
    }

    /// Whether a pass may begin at `now`: there is something to abort, none runs, and the wait
    /// after the last failed pass is over. Marks the pass begun if so.
    fn begin(&mut self, now: Instant) -> bool {
        let due = self.not_before.is_none_or(|at| now >= at);
        let begins = !self.list.is_empty() && !self.passing && due;
        self.passing |= begins;
        begins
    }

    /// The pass ended at `now`, on the failure of `upload` if given (kept again; the next pass
    /// waits `jitter` of the next wait), or with the list empty.
    fn end(&mut self, now: Instant, failed: Option<Upload>, jitter: f64) {
        self.passing = false;
        let Some(upload) = failed else {
            self.wait = Duration::ZERO;
            self.not_before = None;
            return;
        };
        self.keep(upload);
        self.wait = next_wait(self.wait);
        self.not_before = Some(now + self.wait.mul_f64(jitter));
    }
}

/// The wait after `last`: [`RETRY_FIRST`] after none, then twice the last, at most
/// [`RETRY_CAP`].
fn next_wait(last: Duration) -> Duration {
    if last.is_zero() {
        RETRY_FIRST
    } else {
        last.saturating_mul(2).min(RETRY_CAP)
    }
}

/// A number between 0.5 and 1, drawn at random for each call: the part of a wait that is
/// waited.
fn jitter() -> f64 {
    let drawn = RandomState::new().hash_one(0u8) % 1000;
    0.5 + drawn as f64 / 2000.0
}

/// The work after failed writes of a store and its clones (module docs).
pub(super) struct Cleaner {
    /// The store's connection, for the tasks to send their requests on.
    conn: Weak<Conn>,
    places: Arc<Semaphore>,
    kept: Mutex<Kept>,
    looks_skipped: AtomicU64,
}

impl Cleaner {
    /// The work of the store whose connection `conn` will be.
    pub(super) fn new(conn: Weak<Conn>) -> Self {
        Self::bounded(conn, CLEANUPS_AT_ONCE, FAILED_ABORTS_KEPT)
    }

    fn bounded(conn: Weak<Conn>, places: usize, kept: usize) -> Self {
        Self {
            conn,
            places: Arc::new(Semaphore::new(places)),
            kept: Mutex::new(Kept::new(kept)),
            looks_skipped: AtomicU64::new(0),
        }
    }

    fn kept(&self) -> std::sync::MutexGuard<'_, Kept> {
        self.kept.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The counts so far.
    pub(super) fn read(&self) -> Cleanups {
        let kept = self.kept();
        Cleanups {
            looks_skipped: self.looks_skipped.load(Ordering::Relaxed),
            failed_aborts: kept.list.len() as u64,
            aborts_dropped: kept.dropped,
        }
    }

    /// A place and the connection and runtime to run a task in, or none: every place is taken,
    /// the store is gone, or this is not on a runtime.
    fn place(&self) -> Option<(OwnedSemaphorePermit, Arc<Conn>, tokio::runtime::Handle)> {
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        let conn = self.conn.upgrade()?;
        let place = Arc::clone(&self.places).try_acquire_owned().ok()?;
        Some((place, conn, runtime))
    }

    /// Runs `look` in a task of its own once `after` has passed (`look` keeps to
    /// [`CLEANUP_DEADLINE`] itself); with no place for it, counts it skipped. `look` is given the
    /// connection.
    pub(super) fn look<F, Fut>(&self, after: Duration, look: F)
    where
        F: FnOnce(Arc<Conn>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let Some((place, conn, runtime)) = self.place() else {
            self.looks_skipped.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("too many looks after PUTs that got no answer; this one is not made");
            return;
        };
        runtime.spawn(async move {
            tokio::time::sleep(after).await;
            look(conn).await;
            drop(place);
        });
    }

    /// Aborts the upload `id` of `key` in a task of its own, or keeps it on the list of failed
    /// aborts when there is no place for one, or when it fails or its deadline ends.
    pub(super) fn abort(&self, key: ObjectKey, id: String) {
        let upload = Upload { key, id, tries: 0 };
        let Some((place, conn, runtime)) = self.place() else {
            self.kept().keep(upload);
            return;
        };
        runtime.spawn(async move {
            if let Err(upload) = sent(&conn, upload).await {
                conn.cleaner.kept().keep(upload);
            }
            drop(place);
        });
    }

    /// The store answered a request: begins a pass over the failed aborts, if one is due and
    /// a place is free (module docs).
    pub(super) fn answered(&self) {
        if !self.kept().begin(Instant::now()) {
            return;
        }
        let Some((place, conn, runtime)) = self.place() else {
            self.kept().passing = false;
            return;
        };
        runtime.spawn(async move {
            let cleaner = &conn.cleaner;
            let mut failed = None;
            loop {
                let next = cleaner.kept().list.pop_front();
                let Some(upload) = next else { break };
                if let Err(upload) = sent(&conn, upload).await {
                    failed = Some(upload);
                    break;
                }
            }
            cleaner.kept().end(Instant::now(), failed, jitter());
            drop(place);
        });
    }

    /// Waits until no task of the work runs.
    pub(super) async fn idle(&self) {
        let all = u32::try_from(CLEANUPS_AT_ONCE).unwrap_or(u32::MAX);
        let _all = self.places.acquire_many(all).await;
    }

    /// Takes every place, until the permit given is dropped.
    #[cfg(test)]
    pub(super) fn take_places(&self) -> OwnedSemaphorePermit {
        let all = u32::try_from(CLEANUPS_AT_ONCE).unwrap_or(u32::MAX);
        Arc::clone(&self.places)
            .try_acquire_many_owned(all)
            .expect("every place is free")
    }
}

impl super::S3Store {
    /// What the work after failed writes did and holds so far (module docs).
    #[must_use]
    pub fn cleanups(&self) -> Cleanups {
        self.conn.cleaner.read()
    }

    /// Waits until no task of the work after failed writes runs (a pass over the failed aborts
    /// that begins later is not waited for): for a test that must see what the work came to, or
    /// a shutdown that lets it end.
    pub async fn cleanups_idle(&self) {
        self.conn.cleaner.idle().await;
    }
}

/// Sends the abort of `upload` once, within [`CLEANUP_DEADLINE`]; gives the upload back, its
/// tries counted, if it failed.
async fn sent(conn: &Conn, mut upload: Upload) -> Result<(), Upload> {
    upload.tries += 1;
    let aborted = tokio::time::timeout(CLEANUP_DEADLINE, try_abort(conn, &upload.key, &upload.id));
    match aborted.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            tracing::warn!(key = %upload.key, %error, "a multipart upload could not be aborted; it is kept to be aborted again");
            Err(upload)
        }
        Err(_) => {
            tracing::warn!(key = %upload.key, "the abort of a multipart upload ran out of time; it is kept to be aborted again");
            Err(upload)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Capabilities;
    use crate::s3::{Addressing, Credentials, S3Config, S3Store};

    /// A store whose every connect is refused.
    fn store() -> S3Store {
        let endpoint = format!("http://{}", tierstore_testkit::port::dead_addr());
        S3Store::new(store_config(endpoint)).unwrap()
    }

    fn k() -> ObjectKey {
        ObjectKey::new("k").unwrap()
    }

    /// Asserts the bound on the tasks: with every place taken, a look is not made (counted
    /// skipped), an abort goes straight to the list, an answer of the store begins no pass, and
    /// no task is spawned for any of them (the places are all free again as soon as the test
    /// gives them back). Catches: a task spawned for each failed write, however many fail.
    #[tokio::test]
    async fn with_every_place_taken_no_task_is_spawned() {
        let store = store();
        let cleaner = &store.conn.cleaner;
        let all = cleaner.take_places();
        store.look_after_cut(&k(), 4, "t".to_owned());
        cleaner.abort(k(), "u1".to_owned());
        cleaner.answered();
        assert!(
            !cleaner.kept().passing,
            "a pass marked begun with no place for it"
        );
        let counted = Cleanups {
            looks_skipped: 1,
            failed_aborts: 1,
            aborts_dropped: 0,
        };
        assert_eq!(store.cleanups(), counted);
        drop(all);
        assert_eq!(cleaner.places.available_permits(), CLEANUPS_AT_ONCE);
    }

    /// Asserts that outside a runtime (a write dropped as its runtime shuts down) an abort is
    /// kept and a look is counted skipped, and that a store gone leaves nothing to run them on.
    #[test]
    fn outside_a_runtime_an_abort_is_kept() {
        let store = store();
        let cleaner = &store.conn.cleaner;
        cleaner.abort(k(), "u1".to_owned());
        store.look_after_cut(&k(), 4, "t".to_owned());
        let counted = Cleanups {
            looks_skipped: 1,
            failed_aborts: 1,
            aborts_dropped: 0,
        };
        assert_eq!(store.cleanups(), counted);
        let orphan = Cleaner::new(Weak::new());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async { orphan.abort(k(), "u2".to_owned()) });
        assert_eq!(orphan.read().failed_aborts, 1);
    }

    /// Asserts that an abort whose store never answers is ended at its deadline and given back,
    /// its try counted. The store takes the connection and never reads it; the clock is paused
    /// once the connection is taken, so the deadline passes in no real time.
    #[tokio::test]
    async fn an_abort_the_store_never_answers_ends_at_its_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = store_config(format!("http://{}", listener.local_addr().unwrap()));
        config.request_timeout = Duration::from_secs(3600);
        let store = S3Store::new(config).unwrap();
        let conn = Arc::clone(&store.conn);
        let aborting = tokio::spawn(async move { sent(&conn, upload(0, 0)).await });
        let (_held, _) = listener.accept().await.unwrap();
        tokio::time::pause();
        let given = aborting.await.unwrap().unwrap_err();
        assert_eq!(given, upload(0, 1));
    }

    fn store_config(endpoint: String) -> S3Config {
        S3Config {
            endpoint,
            region: "us-east-1".to_owned(),
            bucket: "test-bucket".to_owned(),
            addressing: Addressing::Path,
            ca_pem: None,
            credentials: Credentials::new("AKIDEXAMPLE", "secret"),
            capabilities: Capabilities::default(),
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(60),
            min_body_rate: None,
        }
    }

    fn upload(n: u32, tries: u32) -> Upload {
        Upload {
            key: ObjectKey::new(format!("k{n}")).unwrap(),
            id: format!("u{n}"),
            tries,
        }
    }

    fn ids(kept: &Kept) -> Vec<String> {
        kept.list.iter().map(|u| u.id.clone()).collect()
    }

    /// Asserts that the list never holds more than its bound: past it, the oldest is dropped
    /// and counted, the newest kept. Catches: a list of failed aborts that grows without
    /// bound while the store fails them.
    #[test]
    fn the_failed_aborts_stay_at_their_bound_and_drop_the_oldest() {
        let mut kept = Kept::new(3);
        for n in 0..5 {
            kept.keep(upload(n, 1));
        }
        assert_eq!(ids(&kept), ["u2", "u3", "u4"]);
        assert_eq!(kept.dropped, 2);
        let mut full = Kept::new(FAILED_ABORTS_KEPT);
        for n in 0..(FAILED_ABORTS_KEPT as u32 + 7) {
            full.keep(upload(n, 1));
        }
        assert_eq!(full.list.len(), FAILED_ABORTS_KEPT);
        assert_eq!(full.dropped, 7);
        assert_eq!(full.list.front().map(|u| u.id.as_str()), Some("u7"));
    }

    /// Asserts that an upload tried [`ABORT_TRIES`] times is dropped and counted, and one tried
    /// once less is kept.
    #[test]
    fn an_upload_tried_every_time_is_left_to_the_lifecycle_rule() {
        let mut kept = Kept::new(4);
        kept.keep(upload(0, ABORT_TRIES - 1));
        kept.keep(upload(1, ABORT_TRIES));
        assert_eq!(ids(&kept), ["u0"]);
        assert_eq!(kept.dropped, 1);
    }

    /// Asserts the waits between passes: none before the first failure; 1 s, then doubling, at
    /// most 60 s, each cut by the jitter given; a pass that empties the list starts them again;
    /// and no pass begins while one runs, before its wait is over, or with nothing to abort.
    #[test]
    fn passes_wait_a_growing_capped_jittered_delay_after_a_failure() {
        let now = Instant::now();
        let mut kept = Kept::new(8);
        assert!(!kept.begin(now), "nothing to abort");
        kept.keep(upload(0, 1));
        assert!(kept.begin(now));
        assert!(!kept.begin(now), "a pass runs");
        kept.end(now, Some(upload(0, 2)), 1.0);
        assert_eq!(kept.wait, RETRY_FIRST);
        assert!(!kept.begin(now + RETRY_FIRST - Duration::from_millis(1)));
        assert!(kept.begin(now + RETRY_FIRST));
        kept.end(now, Some(upload(0, 3)), 0.5);
        assert_eq!(kept.wait, 2 * RETRY_FIRST);
        assert_eq!(kept.not_before, Some(now + RETRY_FIRST));
        let mut wait = kept.wait;
        for _ in 0..10 {
            wait = next_wait(wait);
        }
        assert_eq!(wait, RETRY_CAP);
        assert!(kept.begin(now + RETRY_FIRST));
        kept.list.clear();
        kept.end(now, None, 1.0);
        assert_eq!((kept.wait, kept.not_before), (Duration::ZERO, None));
    }

    /// Asserts that the jitter keeps between half and all of a wait.
    #[test]
    fn the_jitter_keeps_half_to_all_of_a_wait() {
        for _ in 0..64 {
            let j = jitter();
            assert!((0.5..=1.0).contains(&j), "{j}");
        }
    }
}
