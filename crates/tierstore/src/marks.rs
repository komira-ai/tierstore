//! The calls on the markers of one actor, a proxy or the leader, named by `me`: create, change by a
//! compare-and-swap, read, remove, wait for a lock to end, and keep a lock alive
//! (`docs/design/put-without-lock.md`, sections 4.3 and 5.2). The body of a marker is the module
//! `marker`; the marker of K is the object named exactly K in the marks bucket of the hot store,
//! which is the [`Backend`] handed to [`Markers::new`]. Only the leader's eviction, copy-back and
//! stray sweep (`marks_evict`, `marks_copy_back`, `marks_sweep`) call it, and nothing the server
//! runs does yet.
//!
//! - [`MarkBirth`] is the marker a create-only write makes where none exists (M1, M3, M10, M12).
//! - [`MarkMove`] is a transition of a marker that exists (M2, M4, M4', M5 to M7, M9, M11, M13 and
//!   M14). [`MarkMove::apply`] is the guard: it says what a marker in a given shape becomes under
//!   the move, or that the move does not apply to it, and the table of 4.3 is the definition of
//!   it. A move that does not apply makes no store call and is [`Outcome::Refused`]. `none`,
//!   `DIRTY` and `WANT` are written by anyone; `LOCK` and `CLEARING` are written only by their
//!   owner, or by the leader reaping a lock whose owner has departed or whose lease ran out.
//!   Nobody else writes them: a writer who finds one waits ([`Markers::wait`]).
//! - [`Markers::create`], [`Markers::advance`] (an `If-Match` write on the content tag the marker
//!   was read at, which is its version), [`Markers::remove`] and [`Markers::read`] are the store
//!   calls. A writer that lost a race gets [`Creation::Exists`] or [`Outcome::Lost`] and reads
//!   again.
//!
//! # Ambiguous answers
//!
//! A write whose answer was lost (the store was unavailable, or failed in a way that does not say
//! whether it did the write) is not assumed to have landed or to have failed: the marker is read
//! back, and its nonce, which every write makes afresh, says whether it was this call's write.
//! A create that landed is [`Creation::Made`], a change that landed is [`Outcome::Done`], a remove
//! that landed is [`Outcome::Done`]. When the nonce says it did not land, a create is
//! [`Creation::Exists`] if another body holds the key and the original error if none does; a
//! change is [`Outcome::Lost`]; a remove is [`Outcome::Lost`] if another marker has replaced the
//! one it meant and the original error if that marker is still there. When the read-back fails,
//! the original error is returned, and the caller must not assume anything: a lock whose creation
//! ended so is not held by the caller, and the leader reaps it by the lease if it landed.
//!
//! # The lease
//!
//! A holder times its lock on a monotonic clock of its own, from the moment it *sent* the request
//! that wrote the lock (a reply that comes late says nothing about when the store saw the
//! request): [`Written::sent_at`]. It refreshes the lock when [`Markers::refresh_due`] says
//! `lock_refresh` has passed (a [`MarkMove::Refresh`], which writes the same lock with a fresh
//! nonce), and starts a call that destroys only while [`Markers::may_destroy`] says its last write
//! of the lock is younger than `lock_lease - queued_call_margin`, so that a call it started
//! cannot land after the leader reaped the lock, unless the process itself was frozen. The
//! leader's side of the lease (a lock unchanged for `lock_lease` is reaped even if its holder
//! lives) is the leader's, and uses [`MarkMove::Reap`].
//!
//! # Nonce
//!
//! The nonce is `<boot>.<counter>`, the counter being per [`Markers`] and the `boot` 128 random
//! bits from the operating system (the helper of the lease nonce) that each [`Markers`] makes for
//! itself, so that a counter that starts again in a restarted process writes nonces no earlier
//! instance wrote. The content tag of the stored object, which is the marker's version, is
//! therefore unique per write.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Map, Value};

use crate::deadline::millis;
use crate::entry::{ambiguous, found};
use crate::marker::{MARKER_VERSION, Marker, MarkerState, MarkerWhy, decode_marker, encode_marker};
use crate::{Backend, Clock, Error, Outcome, Sleeper};

/// The times of the waiting and of the lease: `lock_wait`, `lock_poll_min`, `lock_poll_max`,
/// `lock_poll_factor`, `lock_lease`, `lock_refresh` and `queued_call_margin` of the design
/// (section 9), with the defaults it proposes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkersConfig {
    /// The longest a writer waits for a lock to end (5 s).
    pub lock_wait: Duration,
    /// The first pause between two polls of a lock (20 ms).
    pub lock_poll_min: Duration,
    /// The longest pause between two polls (1 s).
    pub lock_poll_max: Duration,
    /// How many times longer each pause is than the one before it (2); 1 polls at a steady pace.
    pub lock_poll_factor: u32,
    /// How long a lock may stand unchanged before the leader reaps it though its holder lives
    /// (120 s).
    pub lock_lease: Duration,
    /// How often a holder writes its lock again while it works (30 s).
    pub lock_refresh: Duration,
    /// How long a hot DELETE that ended unknown may still land at the store (60 s): a holder
    /// starts no destructive call in the last of this time of its lease.
    pub queued_call_margin: Duration,
}

impl Default for MarkersConfig {
    fn default() -> Self {
        Self {
            lock_wait: Duration::from_secs(5),
            lock_poll_min: Duration::from_millis(20),
            lock_poll_max: Duration::from_secs(1),
            lock_poll_factor: 2,
            lock_lease: Duration::from_secs(120),
            lock_refresh: Duration::from_secs(30),
            queued_call_margin: Duration::from_secs(60),
        }
    }
}

/// Why a [`MarkersConfig`] is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MarkersConfigError {
    /// `lock_poll_min` is zero: the pauses would all be zero and a wait would never end.
    #[error("lock_poll_min must be more than zero")]
    PollMinZero,
    /// `lock_poll_min` is above `lock_poll_max`.
    #[error("lock_poll_min ({min:?}) is above lock_poll_max ({max:?})")]
    PollOrder {
        /// `lock_poll_min`.
        min: Duration,
        /// `lock_poll_max`.
        max: Duration,
    },
    /// `lock_poll_factor` is zero: the pauses would shrink to zero.
    #[error("lock_poll_factor must be at least 1")]
    PollFactorZero,
    /// `lock_refresh` is zero: a lock would be due for a refresh at every look.
    #[error("lock_refresh must be more than zero")]
    RefreshZero,
    /// `lock_lease` is below two refreshes plus the margin of a queued call, so a holder that
    /// misses one refresh would have its lock reaped, or have no time left to destroy in.
    #[error(
        "lock_lease ({lease:?}) is below 2 x lock_refresh ({refresh:?}) + queued_call_margin \
         ({margin:?})"
    )]
    LeaseTooShort {
        /// `lock_lease`.
        lease: Duration,
        /// `lock_refresh`.
        refresh: Duration,
        /// `queued_call_margin`.
        margin: Duration,
    },
}

impl MarkersConfig {
    /// Whether the values hold together.
    ///
    /// # Errors
    /// The first [`MarkersConfigError`] that applies.
    pub fn check(&self) -> Result<(), MarkersConfigError> {
        if self.lock_poll_min.is_zero() {
            return Err(MarkersConfigError::PollMinZero);
        }
        if self.lock_poll_min > self.lock_poll_max {
            return Err(MarkersConfigError::PollOrder {
                min: self.lock_poll_min,
                max: self.lock_poll_max,
            });
        }
        if self.lock_poll_factor == 0 {
            return Err(MarkersConfigError::PollFactorZero);
        }
        if self.lock_refresh.is_zero() {
            return Err(MarkersConfigError::RefreshZero);
        }
        let needed = self
            .lock_refresh
            .checked_mul(2)
            .and_then(|twice| twice.checked_add(self.queued_call_margin));
        if needed.is_none_or(|needed| self.lock_lease < needed) {
            return Err(MarkersConfigError::LeaseTooShort {
                lease: self.lock_lease,
                refresh: self.lock_refresh,
                margin: self.queued_call_margin,
            });
        }
        Ok(())
    }
}

/// A marker as it was read or written, and the content tag of the object that holds it, which is
/// its version and which a change or a remove of it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkSeen {
    /// The body.
    pub marker: Marker,
    /// The content tag of the object the body was read from, or the tag the store gave the write.
    pub tag: String,
}

/// A marker this actor wrote, and when it sent the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Written {
    /// The marker as it is stored.
    pub seen: MarkSeen,
    /// The reading of the actor's monotonic clock just before the request that wrote it was sent.
    /// The age of a lock is counted from it.
    pub sent_at: u64,
}

/// The shape a marker takes: the fields a write decides, the others being the actor's nonce, the
/// key, `v` and what a later release added.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Next {
    pub(crate) st: MarkerState,
    pub(crate) gone: bool,
    pub(crate) dirty: bool,
    pub(crate) why: Option<MarkerWhy>,
    pub(crate) owner: Option<String>,
}

impl Next {
    /// `DIRTY`: hot has a change cold lacks. Nobody owns it.
    fn dirty(gone: bool) -> Self {
        Self {
            st: MarkerState::Dirty,
            gone,
            dirty: true,
            why: None,
            owner: None,
        }
    }

    /// `CLEARING`, owned by `me`: nothing is owed to the cold tier any more.
    fn clearing(gone: bool, me: &str) -> Self {
        Self {
            st: MarkerState::Clearing,
            gone,
            dirty: false,
            why: None,
            owner: Some(me.to_owned()),
        }
    }

    /// `LOCK` for `why`, owned by `me`.
    fn lock(why: MarkerWhy, gone: bool, dirty: bool, me: &str) -> Self {
        Self {
            st: MarkerState::Lock,
            gone,
            dirty,
            why: Some(why),
            owner: Some(me.to_owned()),
        }
    }

    /// What a lock ends in: `DIRTY` if the key owes cold a copy, `CLEARING` (owned by `me`) if it
    /// does not. `gone` is carried over either way; it ends only when the marker is removed.
    fn after_lock(from: &Marker, changed: bool, me: &str) -> Self {
        if from.dirty || changed {
            Self::dirty(from.gone)
        } else {
            Self::clearing(from.gone, me)
        }
    }
}

/// The marker a create-only write makes where none exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkBirth {
    /// `DIRTY`, `gone = false`: a put in flight, or hot holding a change cold lacks (M1, and M12
    /// of the stray sweep).
    Dirty,
    /// `WANT`: cold holds the truth and a read asked for a copy into hot (M10).
    Want,
    /// `LOCK` for the reason, owned by the writer, `gone = false` (M3). The lock owes cold a copy
    /// unless the reason is one that leaves the two tiers equal ([`MarkerWhy::owes_cold_a_copy`]).
    Lock(MarkerWhy),
}

impl MarkBirth {
    pub(crate) fn shape(self, me: &str) -> Next {
        match self {
            Self::Dirty => Next::dirty(false),
            Self::Want => Next {
                st: MarkerState::Want,
                gone: false,
                dirty: false,
                why: None,
                owner: None,
            },
            Self::Lock(why) => {
                let dirty = why.owes_cold_a_copy();
                Next::lock(why, false, dirty, me)
            }
        }
    }
}

/// A transition of a marker that exists. The rows of the table of 4.3 it carries out are named.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkMove {
    /// A put whose object landed, or the re-assertion of a slow put: `DIRTY` or `WANT` to
    /// `DIRTY`, **keeping `gone`** (M2).
    Bump,
    /// Take the lock for the reason: `DIRTY` or `WANT` to `LOCK`, owned by the writer, keeping
    /// `gone` as found and `dirty` unless the reason owes cold a copy (M4, and M11 with
    /// [`MarkerWhy::CopyBack`]).
    Lock(MarkerWhy),
    /// A deleter that found hot under its lock decides the key is deleted: its own `LOCK(DELETE)`
    /// to the same with `gone = true` (M4').
    Gone,
    /// End the lock, which is the writer's own: to `DIRTY` if the key owes cold a copy, to
    /// `CLEARING` if it does not (M5, M6). `changed` says that the holder changed hot though the
    /// lock did not say it would (an eviction that found other bytes), and then the key is
    /// `DIRTY` whatever the lock said. `gone` is kept.
    Release {
        /// Whether the holder left hot different from cold.
        changed: bool,
    },
    /// The holder writes its own lock again with a fresh nonce, so that the leader sees it move
    /// (M14).
    Refresh,
    /// The copier is done: `DIRTY` to `CLEARING`, owned by the copier (M7). The caller names the
    /// version it read before the copy by passing that marker.
    Clear,
    /// The leader clears an orphan: `DIRTY` with `gone = false` to `CLEARING` (M13).
    ClearOrphan,
    /// The leader reaps a lock whose owner has departed or whose lease ran out, or a lock of a
    /// word this release does not know: to `DIRTY` if it owes cold a copy, `CLEARING` otherwise,
    /// keeping `gone` (M9). The caller has judged the owner.
    Reap,
}

impl MarkMove {
    /// What `from` becomes under this move for the actor `me`, or `None` when the move does not
    /// apply to a marker in that shape.
    #[must_use]
    pub(crate) fn apply(self, from: &Marker, me: &str) -> Option<Next> {
        let free = matches!(from.st, MarkerState::Dirty | MarkerState::Want);
        let own_lock = from.st == MarkerState::Lock && from.owner.as_deref() == Some(me);
        let next = match self {
            Self::Bump if free => Next::dirty(from.gone),
            Self::Lock(why) if free => {
                let dirty = from.dirty || why.owes_cold_a_copy();
                Next::lock(why, from.gone, dirty, me)
            }
            Self::Gone if own_lock && from.why == Some(MarkerWhy::Delete) => {
                Next::lock(MarkerWhy::Delete, true, true, me)
            }
            Self::Release { changed } if own_lock => Next::after_lock(from, changed, me),
            Self::Refresh if own_lock => Next {
                st: MarkerState::Lock,
                gone: from.gone,
                dirty: from.dirty,
                why: from.why.clone(),
                owner: from.owner.clone(),
            },
            Self::Clear if from.st == MarkerState::Dirty => Next::clearing(from.gone, me),
            Self::ClearOrphan if from.st == MarkerState::Dirty && !from.gone => {
                Next::clearing(false, me)
            }
            Self::Reap if from.st == MarkerState::Lock => Next::after_lock(from, false, me),
            Self::Reap if matches!(from.st, MarkerState::Other(_)) => Next::dirty(from.gone),
            _ => return None,
        };
        Some(next)
    }
}

/// How a [`Markers::create`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Creation {
    /// The caller made the marker.
    Made(Written),
    /// A marker exists already (or another body holds the key after an answer that was lost).
    Exists,
}

/// How a [`Markers::wait`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Waited {
    /// The key has no marker (`None`) or one that is neither a lock nor a clearing, as it was
    /// read last.
    Free(Option<MarkSeen>),
    /// `lock_wait` passed with the lock still there: the writer answers 503.
    TimedOut,
}

/// The store calls on the markers of one actor.
///
/// `me` may be the same in every process of the actor, so the nonces it writes are made unique by
/// `boot`, which is random for every [`Markers`] made.
pub struct Markers {
    marks: Arc<dyn Backend>,
    me: String,
    boot: String,
    config: MarkersConfig,
    mono: Arc<dyn Clock>,
    sleeper: Arc<dyn Sleeper>,
    writes: AtomicU64,
}

impl Markers {
    /// The calls of the actor `me` on the markers in `marks`, the marks bucket of the hot store.
    /// `mono` is a monotonic clock (its readings are compared with each other only), and
    /// `sleeper` is where a wait pauses.
    ///
    /// # Errors
    /// [`MarkersConfigError`] if `config` does not hold together ([`MarkersConfig::check`]).
    pub fn new(
        marks: Arc<dyn Backend>,
        me: impl Into<String>,
        config: MarkersConfig,
        mono: Arc<dyn Clock>,
        sleeper: Arc<dyn Sleeper>,
    ) -> Result<Self, MarkersConfigError> {
        Self::with_boot(marks, me, config, mono, sleeper, crate::lease::new_nonce())
    }

    /// As [`Markers::new`] with the `boot` named, for a test that needs the nonces to be known.
    pub(crate) fn with_boot(
        marks: Arc<dyn Backend>,
        me: impl Into<String>,
        config: MarkersConfig,
        mono: Arc<dyn Clock>,
        sleeper: Arc<dyn Sleeper>,
        boot: impl Into<String>,
    ) -> Result<Self, MarkersConfigError> {
        config.check()?;
        Ok(Self {
            marks,
            me: me.into(),
            boot: boot.into(),
            config,
            mono,
            sleeper,
            writes: AtomicU64::new(0),
        })
    }

    /// A nonce no other write has: this instance's boot and the count of its writes.
    fn nonce(&self) -> String {
        let count = self.writes.fetch_add(1, Ordering::SeqCst);
        format!("{}.{count}", self.boot)
    }

    /// The marker of `key` that has the shape `next`, with a fresh nonce.
    fn marker(&self, key: &str, v: u32, next: Next, extra: Map<String, Value>) -> Marker {
        Marker {
            v,
            key: key.to_owned(),
            st: next.st,
            gone: next.gone,
            dirty: next.dirty,
            why: next.why,
            owner: next.owner,
            nonce: self.nonce(),
            extra,
        }
    }

    /// Reads the marker of `key` with its version, in one request: `None` if there is none.
    ///
    /// # Errors
    /// The backend's error for a failed read, and [`Error::Other`] for a body that is not a
    /// marker or names another key.
    pub async fn read(&self, key: &str) -> Result<Option<MarkSeen>, Error> {
        let Some((body, tag)) = found(self.marks.get_tagged(key, None).await)? else {
            return Ok(None);
        };
        let marker = decode_marker(&body)?;
        if marker.key != key {
            return Err(Error::Other(format!(
                "corrupt marker: the object {key} names the key {}",
                marker.key
            )));
        }
        Ok(Some(MarkSeen { marker, tag }))
    }

    /// Creates the marker of `key` as `birth` says, create-only.
    ///
    /// # Errors
    /// The backend's error for a failed write (see the module for the ambiguous ones).
    pub async fn create(&self, key: &str, birth: MarkBirth) -> Result<Creation, Error> {
        let marker = self.marker(key, MARKER_VERSION, birth.shape(&self.me), Map::new());
        let sent_at = self.mono.now();
        let written = self.marks.put_tagged(key, encode_marker(&marker)).await;
        match written {
            Ok(tag) => Ok(Creation::Made(Written {
                seen: MarkSeen { marker, tag },
                sent_at,
            })),
            Err(Error::AlreadyExists(_)) => Ok(Creation::Exists),
            Err(error) if ambiguous(&error) => self.settle_create(marker, sent_at, error).await,
            Err(error) => Err(error),
        }
    }

    /// The answer of a create whose reply was lost, from what is stored.
    async fn settle_create(
        &self,
        marker: Marker,
        sent_at: u64,
        error: Error,
    ) -> Result<Creation, Error> {
        match self.read(&marker.key).await {
            Ok(Some(seen)) if seen.marker.nonce == marker.nonce => {
                Ok(Creation::Made(Written { seen, sent_at }))
            }
            Ok(Some(_)) => Ok(Creation::Exists),
            Ok(None) | Err(_) => Err(error),
        }
    }

    /// Takes `mv` on the marker as `seen` has it, writing only if it still has the tag of
    /// `seen`. The new body keeps `key`, `v` and the fields a later release added, and has a
    /// fresh nonce.
    ///
    /// # Errors
    /// The backend's error for a failed write (see the module for the ambiguous ones).
    pub async fn advance(&self, seen: &MarkSeen, mv: MarkMove) -> Result<Outcome<Written>, Error> {
        let from = &seen.marker;
        let Some(next) = mv.apply(from, &self.me) else {
            return Ok(Outcome::Refused);
        };
        let marker = self.marker(&from.key, from.v, next, from.extra.clone());
        let sent_at = self.mono.now();
        let body = encode_marker(&marker);
        let written = self.marks.put_if_match(&marker.key, body, &seen.tag).await;
        match written {
            Ok(tag) => Ok(Outcome::Done(Written {
                seen: MarkSeen { marker, tag },
                sent_at,
            })),
            Err(Error::PreconditionFailed(_) | Error::NotFound(_)) => Ok(Outcome::Lost),
            Err(error) if ambiguous(&error) => self.settle_advance(marker, sent_at, error).await,
            Err(error) => Err(error),
        }
    }

    /// The answer of a change whose reply was lost, from what is stored.
    async fn settle_advance(
        &self,
        marker: Marker,
        sent_at: u64,
        error: Error,
    ) -> Result<Outcome<Written>, Error> {
        match self.read(&marker.key).await {
            Ok(Some(seen)) if seen.marker.nonce == marker.nonce => {
                Ok(Outcome::Done(Written { seen, sent_at }))
            }
            Ok(_) => Ok(Outcome::Lost),
            Err(_) => Err(error),
        }
    }

    /// Removes the marker as `seen` has it (M8), if it is a `CLEARING` this actor owns. The delete
    /// is a plain one, as the store has no delete on the tag it can be relied on for: nobody
    /// writes a `CLEARING` but its owner (I10), so the marker removed is the one that was read.
    ///
    /// # Errors
    /// The backend's error for a failed delete (see the module for the ambiguous ones).
    pub async fn remove(&self, seen: &MarkSeen) -> Result<Outcome<()>, Error> {
        let marker = &seen.marker;
        let own =
            marker.st == MarkerState::Clearing && marker.owner.as_deref() == Some(self.me.as_str());
        if own {
            self.delete(seen).await
        } else {
            Ok(Outcome::Refused)
        }
    }

    /// As [`Markers::remove`], for the leader finishing a `CLEARING` whose owner it has judged
    /// departed (M9).
    ///
    /// # Errors
    /// As [`Markers::remove`].
    pub async fn remove_departed(&self, seen: &MarkSeen) -> Result<Outcome<()>, Error> {
        if seen.marker.st == MarkerState::Clearing {
            self.delete(seen).await
        } else {
            Ok(Outcome::Refused)
        }
    }

    async fn delete(&self, seen: &MarkSeen) -> Result<Outcome<()>, Error> {
        match self.marks.delete(&seen.marker.key).await {
            Ok(()) => Ok(Outcome::Done(())),
            Err(error) if ambiguous(&error) => self.settle_remove(seen, error).await,
            Err(error) => Err(error),
        }
    }

    /// The answer of a delete whose reply was lost, from what is stored.
    async fn settle_remove(&self, seen: &MarkSeen, error: Error) -> Result<Outcome<()>, Error> {
        match self.read(&seen.marker.key).await {
            Ok(None) => Ok(Outcome::Done(())),
            Ok(Some(now)) if now.marker.nonce != seen.marker.nonce => Ok(Outcome::Lost),
            Ok(Some(_)) | Err(_) => Err(error),
        }
    }

    /// Polls the marker of `key` until it is neither a lock nor a clearing, or `lock_wait` has
    /// passed (5.2): a read, then pauses of `lock_poll_min` that grow by `lock_poll_factor` up to
    /// `lock_poll_max`, the last one cut short so that the wait ends at `lock_wait`. The pauses
    /// are the sleeper's and the time the monotonic clock's.
    ///
    /// # Errors
    /// The backend's error for a failed read, which ends the wait.
    pub async fn wait(&self, key: &str) -> Result<Waited, Error> {
        let started = self.mono.now();
        let limit = millis(self.config.lock_wait);
        let mut pause = self.config.lock_poll_min;
        loop {
            let seen = self.read(key).await?;
            if seen.as_ref().is_none_or(|seen| !seen.marker.st.is_held()) {
                return Ok(Waited::Free(seen));
            }
            let left = limit.saturating_sub(self.mono.now().saturating_sub(started));
            if left == 0 {
                return Ok(Waited::TimedOut);
            }
            self.sleeper
                .sleep(pause.min(Duration::from_millis(left)))
                .await;
            let grown = pause.saturating_mul(self.config.lock_poll_factor);
            pause = grown.min(self.config.lock_poll_max);
        }
    }

    /// How long ago the request that wrote `held` was sent, in milliseconds.
    fn age(&self, held: &Written) -> u64 {
        self.mono.now().saturating_sub(held.sent_at)
    }

    /// Whether the holder of the lock `held` is due to write it again: `lock_refresh` has passed
    /// since its last write was sent.
    #[must_use]
    pub fn refresh_due(&self, held: &Written) -> bool {
        self.age(held) >= millis(self.config.lock_refresh)
    }

    /// Whether the holder of the lock `held` may start a call that destroys (a hot DELETE, a
    /// replace, a cold write): its last write of the lock is younger than `lock_lease -
    /// queued_call_margin`, so the call is over or cut before the leader may reap the lock.
    #[must_use]
    pub fn may_destroy(&self, held: &Written) -> bool {
        let within = self
            .config
            .lock_lease
            .saturating_sub(self.config.queued_call_margin);
        self.age(held) < millis(within)
    }
}
