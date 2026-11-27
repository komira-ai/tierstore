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
//!   (a request of the store may take `request_timeout`, which is longer); a pass over the failed
//!   aborts (below) too, as a whole: the abort under way then counts as failed;
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

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use super::Conn;
use super::multipart::try_abort;
use crate::ObjectKey;

/// How many tasks of the work after failed writes a store and its clones run at once.
pub(super) const CLEANUPS_AT_ONCE: usize = 16;

/// How long the requests of one task of that work may take in all (a pass over the failed aborts
/// as a whole).
pub(super) const CLEANUP_DEADLINE: Duration = Duration::from_secs(10);

/// The most failed aborts a store keeps to send again.
pub(super) const FAILED_ABORTS_KEPT: usize = 1024;

/// How many times an abort is sent before its upload is left to the bucket's lifecycle rule.
pub(super) const ABORT_TRIES: u32 = 8;

/// The wait after the first pass over the failed aborts that stopped on a failure.
pub(super) const RETRY_FIRST: Duration = Duration::from_secs(1);

/// The longest wait between two passes over the failed aborts.
pub(super) const RETRY_CAP: Duration = Duration::from_secs(60);

/// What the work after failed writes did and holds, since the store was made (module docs). An
/// [`S3Store`](super::S3Store) and its clones count together.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cleanups {
    /// Looks after single PUTs that got no answer that were not made: every place was taken.
    pub looks_skipped: u64,
    /// The multipart uploads whose abort failed (or found no place), kept to be aborted again:
    /// the size of the list now, at most 1024.
    pub failed_aborts: u64,
    /// Failed aborts dropped from that list, the oldest when it was full or one tried
    /// 8 times: left to the bucket's lifecycle rule.
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
    /// A list of at most `bound` uploads, and at least one.
    fn new(bound: usize) -> Self {
        Self {
            list: VecDeque::new(),
            bound: bound.max(1),
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
        if self.list.len() >= self.bound {
            // The bound is at least 1, so the list has an oldest.
            let oldest = self.list.pop_front().map(|oldest| oldest.key);
            tracing::warn!(key = ?oldest, "too many failed aborts kept; the oldest is left to the bucket's lifecycle rule");
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

/// The place a task of the work holds while it runs: given back when the task ends, and then
/// told to [`Cleaner::idle`].
struct Place {
    permit: Option<OwnedSemaphorePermit>,
    ended: Arc<Notify>,
}

impl Drop for Place {
    fn drop(&mut self) {
        // The place is free before the waiters look at the free places.
        drop(self.permit.take());
        self.ended.notify_waiters();
    }
}

/// The work after failed writes of a store and its clones (module docs).
pub(super) struct Cleaner {
    /// The store's connection, for the tasks to send their requests on.
    conn: Weak<Conn>,
    places: Arc<Semaphore>,
    /// Told each time a task ends.
    ended: Arc<Notify>,
    kept: Mutex<Kept>,
    looks_skipped: AtomicU64,
}

impl Cleaner {
    /// The work of the store whose connection `conn` will be.
    pub(super) fn new(conn: Weak<Conn>) -> Self {
        Self {
            conn,
            places: Arc::new(Semaphore::new(CLEANUPS_AT_ONCE)),
            ended: Arc::new(Notify::new()),
            kept: Mutex::new(Kept::new(FAILED_ABORTS_KEPT)),
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
    fn place(&self) -> Option<(Place, Arc<Conn>, tokio::runtime::Handle)> {
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        let conn = self.conn.upgrade()?;
        let permit = Arc::clone(&self.places).try_acquire_owned().ok()?;
        let place = Place {
            permit: Some(permit),
            ended: Arc::clone(&self.ended),
        };
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
            let until = Instant::now() + CLEANUP_DEADLINE;
            if let Err(upload) = sent(&conn, upload, until).await {
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
            let until = Instant::now() + CLEANUP_DEADLINE;
            let mut failed = None;
            loop {
                let next = cleaner.kept().list.pop_front();
                let Some(upload) = next else { break };
                if let Err(upload) = sent(&conn, upload, until).await {
                    failed = Some(upload);
                    break;
                }
            }
            cleaner.kept().end(Instant::now(), failed, jitter());
            drop(place);
        });
    }

    /// Waits until no task of the work runs. It takes no place, so it holds up no task.
    pub(super) async fn idle(&self) {
        loop {
            let ended = self.ended.notified();
            let mut ended = std::pin::pin!(ended);
            // Registered before the look, so a task that ends in between is not missed.
            ended.as_mut().enable();
            if self.places.available_permits() == CLEANUPS_AT_ONCE {
                return;
            }
            ended.await;
        }
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

/// Sends the abort of `upload` once, ended at `until` (the deadline of its task); gives the
/// upload back, its tries counted, if it failed.
async fn sent(conn: &Conn, mut upload: Upload, until: Instant) -> Result<(), Upload> {
    upload.tries += 1;
    let aborted = tokio::time::timeout_at(until, try_abort(conn, &upload.key, &upload.id));
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
    use bytes::Bytes;

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
        let until = Instant::now() + CLEANUP_DEADLINE;
        let aborting = tokio::spawn(async move { sent(&conn, upload(0, 0), until).await });
        let (_held, _) = listener.accept().await.unwrap();
        tokio::time::pause();
        let given = aborting.await.unwrap().unwrap_err();
        assert_eq!(given, upload(0, 1));
    }

    /// How many aborts the loopback service took.
    fn aborts(s3: &crate::s3::fake::FakeS3) -> usize {
        let taken = s3.requests();
        // The test sends no other request with an upload id.
        taken.iter().filter(|r| r.contains("uploadId=")).count()
    }

    /// Asserts what a real pass does with the list: over two kept aborts, it stops at the first
    /// that fails (one abort sent, that upload put back at the end, the wait set); once the wait
    /// is over it sends both, oldest first, and empties the list (the waits start again); and
    /// an upload whose abort fails its last try is dropped and counted. Catches: a pass that goes
    /// on sending to a store that has just refused an abort, that loses the upload it failed, or
    /// that keeps an upload for ever.
    #[tokio::test]
    async fn a_pass_stops_at_its_first_failure_and_drops_an_upload_out_of_tries() {
        let s3 = crate::s3::fake::FakeS3::start().await;
        let store = s3.store(Capabilities::default());
        let cleaner = &store.conn.cleaner;
        cleaner.kept().keep(upload(0, 1));
        cleaner.kept().keep(upload(1, 1));
        s3.fail(crate::s3::fake::Failure::Abort);
        cleaner.answered();
        store.cleanups_idle().await;
        assert_eq!(aborts(&s3), 1);
        assert_eq!(ids(&cleaner.kept()), ["u1", "u0"]);
        assert_eq!(cleaner.kept().wait, RETRY_FIRST);
        cleaner.kept().not_before = None;
        cleaner.answered();
        store.cleanups_idle().await;
        assert_eq!(
            aborts(&s3),
            3,
            "both sent, the store no longer knows either"
        );
        assert_eq!(cleaner.read(), Cleanups::default());
        assert_eq!(cleaner.kept().wait, Duration::ZERO);
        cleaner.kept().keep(upload(2, ABORT_TRIES - 1));
        s3.fail(crate::s3::fake::Failure::Abort);
        cleaner.answered();
        store.cleanups_idle().await;
        let dropped = Cleanups {
            aborts_dropped: 1,
            ..Cleanups::default()
        };
        assert_eq!(cleaner.read(), dropped);
    }

    /// Asserts, through the crate's own copy of the write paths, the three ways a failed write
    /// hands its work over: a PUT that never reached the store (every connect refused) is not
    /// looked after; a PUT whose connection the store closed without an answer is looked after
    /// in a task of its own, whose HEAD fails too (counted `failed`); and the abort of an upload
    /// runs in a task of its own while a place is free, kept when the store fails it and not
    /// kept when it is done.
    #[tokio::test]
    async fn a_failed_write_hands_its_look_or_its_abort_to_a_task() {
        use crate::ObjectStore as _;
        use tokio::io::AsyncReadExt as _;

        let body =
            || futures::StreamExt::boxed(futures::stream::iter([Ok(Bytes::from_static(b"abcd"))]));
        let never = store();
        let unsent = never.put_stream(&k(), 4, body()).await;
        assert!(unsent.is_err());
        never.cleanups_idle().await;
        assert_eq!(never.cut_puts(), crate::s3::CutPuts::default());

        // A store that reads each request's head and closes the connection unanswered.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut head = [0_u8; 1024];
                let _ = sock.read(&mut head).await;
            }
        });
        let tuning = crate::s3::S3Tuning {
            cut_put_look_after: Duration::ZERO,
            endpoint_attempts: 1,
            ..crate::s3::S3Tuning::default()
        };
        let closing = S3Store::new_tuned(store_config(endpoint), tuning).unwrap();
        let cut = closing.put_stream(&k(), 4, body()).await;
        assert!(cut.is_err());
        closing.cleanups_idle().await;
        assert_eq!(closing.cut_puts().failed, 1);

        let s3 = crate::s3::fake::FakeS3::start().await;
        let store = s3.store(Capabilities::default());
        s3.fail(crate::s3::fake::Failure::Abort);
        store.conn.cleaner.abort(k(), "u1".to_owned());
        store.cleanups_idle().await;
        assert_eq!(store.cleanups().failed_aborts, 1);
        let other = S3Store::new(s3.config(Capabilities::default())).unwrap();
        other.conn.cleaner.abort(k(), "u2".to_owned());
        other.cleanups_idle().await;
        assert_eq!(other.cleanups(), Cleanups::default());
        assert_eq!(aborts(&s3), 2);
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
        let mut one = Kept::new(0);
        one.keep(upload(0, 1));
        one.keep(upload(1, 1));
        assert_eq!((ids(&one), one.dropped), (vec!["u1".to_owned()], 1));
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
