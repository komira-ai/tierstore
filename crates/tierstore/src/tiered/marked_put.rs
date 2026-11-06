//! The puts of a proxy whose format says `active-marks`: a put of a key hot does not hold takes
//! the fast path of `docs/design/put-without-lock.md`, section 5.1, with no entry and no lock. It
//! covers every create-only put the proxy receives (a put, a streamed put, a presigned put, and
//! the completion of a multipart upload), since [`Backend::put`] only creates; the overwrite of a
//! key hot holds is pull request 5.
//!
//! A put, in order:
//!
//! 1. The proxy must be live (`Proxy::is_live`) and the format must route it here
//!    (`Inner::route_write`): the format is the cached one and is never read for this put alone
//!    but as for any write, at most every `format_refresh`.
//! 2. **Wave 1**, in parallel: a hot HEAD, a cold HEAD and a GET of the marker. The key exists if
//!    hot has it, or cold has it and the marker is not `gone` (a deleted key whose cold copy
//!    remains is free to create). A cold tier that errs, with hot absent and no `gone` marker, is
//!    [`Error::Unavailable`]: the one cost a create-only put pays for S3's rule that it sees a key
//!    that lives only in cold. A wave that took longer than `read_window` is discarded and run
//!    again, `read_retries` times at most, and then the put is [`Error::Unavailable`].
//! 3. A put that is about to answer [`Error::AlreadyExists`] **re-checks once**: it reads the
//!    marker. `LOCK(DELETE)` is [`Error::Conflict`] (a delete is in flight). `gone` is followed
//!    by one hot HEAD: present is [`Error::AlreadyExists`] (the key was recreated), absent is
//!    [`Error::Conflict`] (the key was deleted). Any other marker, or none, is
//!    [`Error::AlreadyExists`].
//! 4. **Wave 2**, in parallel: the object (create-only, to the hot tier alone, whatever the level:
//!    a hot tier that answers [`Error::Full`] is a 503) and the marker (`DIRTY`, `gone = false`,
//!    create-only). The put is acknowledged when the object landed and the marker is `DIRTY`,
//!    ours or another writer's, after the table of 5.1:
//!    - the marker exists: the put reads it. None now: it creates it. `DIRTY` or `WANT`: it moves
//!      it to `DIRTY` keeping `gone` (a compare-and-swap, after the object landed by
//!      construction). `LOCK` or `CLEARING`: it waits for it to end (5.2) and then asks hot: an
//!      object there is marked as before, and none means a delete or an eviction came after the
//!      put, which is the order put, delete, and is acknowledged. At most `entry_rounds`
//!      contentions, then [`Error::Unavailable`], an outcome that is not known: the object
//!      landed.
//!    - the marker could not be written: the put reads and writes it again until
//!      `marker_deadline`, then [`Error::Unavailable`] likewise.
//! 5. **A slow put marks again.** If more than `reassert_after` passed between the sending of the
//!    marker's creation and the end of wave 2, the copier may have cleared the marker as an orphan
//!    while the body streamed: the put reads the marker and writes it as in step 4 before it
//!    acknowledges.
//!
//! A put that fails for good after its marker landed (the object was refused, or a completion
//! named a part that is not there) removes the marker it made: `CLEARING` by a compare-and-swap on
//! the version it made, then a delete. A failed compare-and-swap means another writer took the
//! marker over, and it stays. A put whose object may have landed (the store did not answer) leaves
//! its marker, and is [`Error::Unavailable`]: nothing more is written, and the copier or the
//! stray sweep adopts what landed.

use std::future::Future;
use std::time::Duration;

use bytes::Bytes;

use super::Inner;
use super::entry_put::{Failed, landing, retry};
use super::put::Offered;
use super::uploads::{Route as UploadRoute, route};
use crate::deadline::millis;
use crate::{
    Creation, Error, MarkBirth, MarkMove, MarkSeen, MarkerState, MarkerWhy, Markers, MarkersConfig,
    Outcome, PartBody, PutAnswer, Waited, Written,
};

/// The settings of the puts made on markers: the waits of a lock (`lock_wait`, `lock_poll_*`,
/// `lock_lease`, `lock_refresh`, `queued_call_margin`), and `marker_deadline`, `reassert_after`,
/// `read_window` and `read_retries` of section 9 of the design. The looks a put takes at a marker
/// that keeps changing are `ProxyConfig::entry_rounds`, and the pauses between the tries of a
/// marker write that failed are those of `ProxyConfig::step_retry`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkedPutConfig {
    /// The waits of a lock and its lease.
    pub markers: MarkersConfig,
    /// The longest the tries of a marker write or read run before the put is answered 503 "may
    /// not have landed" (30 s).
    pub marker_deadline: Duration,
    /// A put whose object landed this long after its marker was created marks again (30 s). It is
    /// below the copier's `orphan_after`, which is checked where both are set.
    pub reassert_after: Duration,
    /// A wave 1 slower than this is read again (5 s).
    pub read_window: Duration,
    /// How often a wave 1 is read again before the put is answered 503 (3).
    pub read_retries: u32,
}

impl MarkedPutConfig {
    /// The defaults of the design.
    #[must_use]
    pub fn new() -> Self {
        Self {
            markers: MarkersConfig::default(),
            marker_deadline: Duration::from_secs(30),
            reassert_after: Duration::from_secs(30),
            read_window: Duration::from_secs(5),
            read_retries: 3,
        }
    }

    /// Whether the times are more than zero. (The lock waits are checked by [`Markers::new`] when
    /// the proxy has a marks bucket.)
    ///
    /// # Errors
    /// The text of the first value refused.
    pub fn check(&self) -> Result<(), String> {
        let times = [
            ("marker_deadline", self.marker_deadline),
            ("reassert_after", self.reassert_after),
            ("read_window", self.read_window),
        ];
        match times.iter().find(|(_, time)| time.is_zero()) {
            Some((name, _)) => Err(format!("{name} must be more than zero")),
            None => Ok(()),
        }
    }
}

impl Default for MarkedPutConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// How one look at the marker after the object landed ended.
enum Step {
    /// The marker is `DIRTY`, or the put is known to have come before a delete.
    Done,
    /// The marker changed under the put: look again.
    Again,
    /// A lock was waited for and has ended with this marker (none if it is gone): look at hot,
    /// and mark.
    Freed(Option<MarkSeen>),
    /// `lock_wait` passed with the lock there.
    TimedOut,
}

impl Inner {
    /// The put of a held body of a key hot does not hold, on markers.
    pub(super) async fn put_held_marked(
        &self,
        markers: &Markers,
        key: &str,
        body: Bytes,
    ) -> Result<(), Error> {
        self.put_marked(markers, key, || async move {
            self.hot.put(key, body).await.map_err(landing)
        })
        .await
    }

    /// The put of a streamed body, on markers. Gives what the hot tier's answer to the write says
    /// of the object (see [`Backend::put_stream_answered`]). A presigned put is the same: there is
    /// no other tier for it to be kept from.
    pub(super) async fn put_streamed_marked(
        &self,
        markers: &Markers,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<Option<PutAnswer>, Error> {
        let offered = Offered::new(body);
        self.put_marked(markers, key, || async {
            let written = self
                .hot
                .put_stream_answered(key, len, offered.view(), false)
                .await;
            written.map_err(|error| self.sort(&offered, error))
        })
        .await
    }

    /// The completion of the upload `id` of `key` in the hot tier, on markers. An upload begun in
    /// the cold tier is refused: its object would land in cold, which is the cold landing of 5.5
    /// (pull request 5), and none is begun there while the format says `active-marks`.
    pub(super) async fn complete_marked(
        &self,
        markers: &Markers,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        let UploadRoute::Hot(inner) = route(id) else {
            return Err(cold_upload(key));
        };
        self.put_marked(markers, key, || async move {
            let made = self.hot.complete_upload(key, inner, parts).await;
            made.map_err(landing)
        })
        .await
    }

    /// The put of `key` whose object `land` writes to the hot tier create-only, with its marker.
    async fn put_marked<T, F, Fut>(&self, markers: &Markers, key: &str, land: F) -> Result<T, Error>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, Failed>>,
    {
        self.require_live(key)?;
        self.require_absent(markers, key).await?;
        let (object, marker) = futures::join!(land(), markers.create(key, MarkBirth::Dirty));
        match object {
            Ok(value) => {
                self.mark_landed(markers, key, marker).await?;
                Ok(value)
            }
            Err(Failed::Definite(Error::AlreadyExists(_))) => {
                let answer = self.recheck(markers, key).await;
                self.forget(markers, marker).await;
                Err(answer)
            }
            Err(Failed::Definite(error)) => {
                self.forget(markers, marker).await;
                Err(error)
            }
            Err(Failed::Maybe(error)) => Err(retry(
                key,
                &format!("the object may not have landed: {error}"),
            )),
        }
    }

    /// Wave 1: the key must not exist.
    async fn require_absent(&self, markers: &Markers, key: &str) -> Result<(), Error> {
        let window = millis(self.config.marks_put.read_window);
        for _ in 0..=self.config.marks_put.read_retries {
            let started = self.config.mono.now();
            let (hot, cold, marker) =
                futures::join!(self.hot.head(key), self.cold.head(key), markers.read(key));
            match hot {
                Ok(_) => return Err(self.recheck(markers, key).await),
                Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
            if self.config.mono.now().saturating_sub(started) > window {
                continue;
            }
            let gone = marker?.is_some_and(|seen| seen.marker.gone);
            let exists = match cold {
                Ok(_) => !gone,
                Err(Error::NotFound(_)) => false,
                Err(_) if gone => false,
                Err(error) => {
                    let why = format!("the cold tier did not answer for {key}: {error}");
                    return Err(Error::Unavailable(why));
                }
            };
            if exists {
                return Err(self.recheck(markers, key).await);
            }
            return Ok(());
        }
        Err(retry(key, "the tiers and the marker were read too slowly"))
    }

    /// What a create-only put that found the key answers, after one more look at its marker: 409
    /// while a delete holds the key or has removed it, and 412 while it exists. An error of the
    /// look is the answer.
    async fn recheck(&self, markers: &Markers, key: &str) -> Error {
        let conflict = || Error::Conflict(key.to_owned());
        let exists = || Error::AlreadyExists(key.to_owned());
        let seen = match markers.read(key).await {
            Ok(seen) => seen,
            Err(error) => return error,
        };
        let Some(marker) = seen.map(|seen| seen.marker) else {
            return exists();
        };
        if marker.st == MarkerState::Lock && marker.why == Some(MarkerWhy::Delete) {
            return conflict();
        }
        if !marker.gone {
            return exists();
        }
        match self.hot.head(key).await {
            Ok(_) => exists(),
            Err(Error::NotFound(_)) => conflict(),
            Err(error) => error,
        }
    }

    /// Removes the marker a failed put made, if it made one and still holds it: `CLEARING` by a
    /// compare-and-swap on the version it made, then a delete. What goes wrong is left to the
    /// copier, which clears a marker with nothing behind it.
    async fn forget(&self, markers: &Markers, created: Result<Creation, Error>) {
        let Ok(Creation::Made(made)) = created else {
            return;
        };
        let cleared = markers.advance(&made.seen, MarkMove::Clear).await;
        if let Ok(Outcome::Done(clearing)) = cleared {
            let _ = markers.remove(&clearing.seen).await;
        }
    }

    /// The marker after an object landed: it is as the put made it, or the put makes it so.
    async fn mark_landed(
        &self,
        markers: &Markers,
        key: &str,
        created: Result<Creation, Error>,
    ) -> Result<(), Error> {
        if let Ok(Creation::Made(made)) = &created
            && !self.is_slow(made)
        {
            return Ok(());
        }
        self.mark_again(markers, key).await
    }

    /// Whether the marker of the put was made more than `reassert_after` ago.
    fn is_slow(&self, made: &Written) -> bool {
        let age = self.config.mono.now().saturating_sub(made.sent_at);
        age > millis(self.config.marks_put.reassert_after)
    }

    /// Reads the marker and writes it `DIRTY`, as long as it changes under the put, at most
    /// `entry_rounds` times, and as long as the store fails, until `marker_deadline`.
    async fn mark_again(&self, markers: &Markers, key: &str) -> Result<(), Error> {
        let settings = &self.config.marks_put;
        let (started, limit) = (self.config.mono.now(), millis(settings.marker_deadline));
        let mut pause = self.config.step_retry.delay;
        let (mut rounds, mut known) = (0, None);
        loop {
            let step = self.mark_step(markers, key, known.take()).await;
            match step {
                Ok(Step::Done) => return Ok(()),
                Ok(Step::Again) => {}
                Ok(Step::Freed(seen)) => known = Some(seen),
                Ok(Step::TimedOut) => {
                    return Err(retry(key, "the lock on the marker did not end in time"));
                }
                Err(error) => {
                    if self.config.mono.now().saturating_sub(started) >= limit {
                        return Err(retry(
                            key,
                            &format!("the marker could not be written: {error}"),
                        ));
                    }
                    self.config.sleeper.sleep(pause).await;
                    pause = pause
                        .saturating_mul(2)
                        .min(self.config.step_retry.delay_max);
                    continue;
                }
            }
            rounds += 1;
            if rounds >= self.config.entry_rounds {
                return Err(retry(key, "the marker kept changing"));
            }
        }
    }

    /// One look at the marker of `key` and the write it calls for. `known` is the marker as a
    /// wait for a lock last read it, which is then not read again.
    async fn mark_step(
        &self,
        markers: &Markers,
        key: &str,
        known: Option<Option<MarkSeen>>,
    ) -> Result<Step, Error> {
        let seen = match known {
            Some(seen) => seen,
            None => markers.read(key).await?,
        };
        let Some(seen) = seen else {
            return Ok(match markers.create(key, MarkBirth::Dirty).await? {
                Creation::Made(_) => Step::Done,
                Creation::Exists => Step::Again,
            });
        };
        if seen.marker.st.is_held() {
            return match markers.wait(key).await? {
                Waited::TimedOut => Ok(Step::TimedOut),
                Waited::Free(freed) => self.after_wait(key, freed).await,
            };
        }
        Ok(match markers.advance(&seen, MarkMove::Bump).await? {
            Outcome::Done(_) => Step::Done,
            Outcome::Lost | Outcome::Refused => Step::Again,
        })
    }

    /// A lock ended: if hot has no object a delete or an eviction came after the put and the put is
    /// done; else the marker is written from what the wait read.
    async fn after_wait(&self, key: &str, freed: Option<MarkSeen>) -> Result<Step, Error> {
        match self.hot.head(key).await {
            Ok(_) => Ok(Step::Freed(freed)),
            Err(Error::NotFound(_)) => Ok(Step::Done),
            Err(error) => Err(error),
        }
    }
}

/// The refusal of the completion of an upload begun in the cold tier.
pub(super) fn cold_upload(key: &str) -> Error {
    let why = "the upload was begun in the cold tier, which a put on markers does not complete; \
               abort it and begin it again";
    retry(key, why)
}
