//! The journal entry of one key: the object `_entries/<sha256hex(key)>` in the hot tier, which says
//! who is working on the key and what is to be done to it next.
//!
//! An entry is created create-only, changed only by a replace that names the content tag it was
//! read at (`If-Match`), and removed by the actor whose work it was (or by the reconciler, when
//! the entry is sealed). That makes every change of an entry one atomic store call, and a call
//! that lost a race fails instead of overwriting. This module is the entry's body, its
//! transitions and the store calls that carry them out. The puts and deletes of a proxy whose
//! format is active (`tiered/entry_put.rs`, `tiered/entry_delete.rs`) use it, so do its reads of a
//! cold object, which ask for a copy-back (`tiered/copy_back.rs`), and so does the entry
//! reconciler; a read never looks at an entry.
//!
//! - The body is JSON: `v` (1), `key`, `op` ([`EntryOp`]), `st` ([`EntryState`]), `owner` (the
//!   id of the proxy or reconciler working on the key, or `null`), `nonce` (written afresh by
//!   every write) and `created_ms` (wall clock, for metrics, never for a decision). A reader
//!   ignores fields it does not know, and reads an `op` or `st` it does not know as `Unknown`,
//!   which no transition accepts.
//! - [`Step`] is a transition of the entry. [`Step::apply`] is the guard: it says what an entry
//!   in a given shape becomes, or that the step does not apply to it.
//! - [`Entries`] makes the store calls: [`Entries::create`], [`Entries::advance`] (a replace on
//!   the tag), [`Entries::release`] (a delete on the tag where the store honours it) and
//!   [`Entries::read`].
//!
//! # Ambiguous answers
//!
//! A write whose answer was lost (the store was unavailable, or failed in a way that does not say
//! whether it did the write) is not a failure: the entry is read again, and the nonce of the body
//! says whether it was this call's write. A create that landed is [`Created::Taken`], a replace
//! that landed is [`Outcome::Done`], a delete that landed is [`Outcome::Done`]. When the nonce
//! says it did not land, a create is [`Created::Exists`] if another body holds the entry and the
//! original error if none does; a replace is [`Outcome::Lost`]; a delete is [`Outcome::Lost`] if
//! another entry has replaced the one it meant, and the original error if that entry is still
//! there. When the read itself fails, the original error is returned.
//!
//! A caller that retries a step uses [`Entries::try_advance`], which tells a replace that
//! failed and was read back as not landed ([`Attempt::Unchanged`]) from one that could not be read
//! back ([`Attempt::Unknown`]), and [`Entries::settle`] to read again. [`Entries::advance`] is the
//! same call with the first folded into [`Outcome::Lost`] and the second into the error.
//!
//! # Reading
//!
//! An entry is read with one tagged get ([`Backend::get_tagged`]): the body and the tag are those of
//! one object. A store without that call is read with a head (its tag) and then a get (its body). A
//! write may come between the two, so the body can be newer than the tag. That is safe: the tag is
//! the older one, and a replace or delete that names it fails against the newer entry, which is
//! [`Outcome::Lost`]. Nobody acts on a body without going through such a call.
//!
//! The one exception is a store without a delete on the tag: there [`Entries::release`] falls back to
//! a plain delete, which does not check the tag, so a stale [`Seen`] could remove a newer entry.
//! It does so only for a sealed entry, which no step changes (the reconciler seals before it
//! releases, and abandons the release when its seal is [`Outcome::Lost`]). A held entry is changed
//! under its holder (a delete amends a held put with [`Step::AmendCopy`], the reconciler reaps a
//! held entry), so there release returns the store's `Unsupported` and deletes nothing.
//!
//! # Nonce
//!
//! The nonce is `<actor id>.<boot>.<counter>`, the counter being per [`Entries`]. The actor id is
//! the configured one and is reused when a process restarts, so it cannot make the nonce unique
//! by itself: each [`Entries`] takes a `boot` of its own, 128 random bits from the operating system
//! (the helper of the lease nonce), so that a counter that starts again under a known actor id, in
//! this process or in another with the same process id and clock, still writes nonces no earlier
//! instance wrote.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::journal::{ENTRIES_PREFIX, to_body};
use crate::{Backend, Clock, Error};

/// The `v` of the bodies this release writes.
const ENTRY_VERSION: u32 = 1;

/// The key of the entry of `key` in the hot tier: the prefix and the SHA-256 of the key in hex, so
/// that the name has a fixed length whatever the key.
#[must_use]
pub fn entry_key(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    format!("{ENTRIES_PREFIX}{}", hex::encode(digest))
}

/// What an entry says is being done to its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum EntryOp {
    /// A put in flight, or landed and waiting to be copied to the cold tier.
    Put,
    /// A delete in flight, or handed over for the reconciler to finish.
    Del,
    /// A copy of the cold tier's object into the hot tier, which a read asked for.
    Cb,
    /// An eviction of the hot tier's object.
    Evict,
    /// Anything a newer writer adds: no transition accepts it.
    #[serde(other)]
    Unknown,
}

/// Whose turn it is on an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum EntryState {
    /// The owner is working on the key.
    Held,
    /// The work landed or was asked for, and it is the reconciler's turn.
    Pending,
    /// The reconciler is finishing, and nobody else may act.
    Sealed,
    /// Anything a newer writer adds: no transition accepts it.
    #[serde(other)]
    Unknown,
}

/// The body of an entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The version of the format, 1.
    pub v: u32,
    /// The key the entry is about.
    pub key: String,
    /// What is being done to the key.
    pub op: EntryOp,
    /// Whose turn it is.
    pub st: EntryState,
    /// The id of the actor working on the key, for a held entry; `None` otherwise.
    pub owner: Option<String>,
    /// A value written afresh by every write of the entry, so that a read can tell whether a
    /// write it did not get an answer for is the one that is stored.
    pub nonce: String,
    /// When the entry was created, in milliseconds of the wall clock. For metrics only.
    pub created_ms: u64,
}

/// The body of `entry`.
#[must_use]
pub fn encode_entry(entry: &Entry) -> Bytes {
    to_body(entry)
}

/// The entry in a body.
///
/// # Errors
/// [`Error::Other`] if the body is not the encoding of an entry.
pub fn decode_entry(body: &[u8]) -> Result<Entry, Error> {
    serde_json::from_slice(body).map_err(|error| Error::Other(format!("corrupt entry: {error}")))
}

/// An entry as it was read, and the content tag it had.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    /// The body.
    pub entry: Entry,
    /// The content tag of the object the body was read from, which a replace or a delete names.
    pub tag: String,
}

/// What a client or the reconciler takes an entry for when none exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Take {
    /// A put: held by the caller.
    Put,
    /// A delete: held by the caller.
    Del,
    /// A copy-back that a read asks for: pending, no owner.
    CopyBack,
    /// An eviction: sealed, no owner.
    Evict,
    /// The adoption of an object in the hot tier that no entry and no cold object accompanies:
    /// a pending put, no owner.
    Stray,
}

impl Take {
    /// The operation and state of the new entry, and whether the caller owns it.
    fn shape(self) -> (EntryOp, EntryState, bool) {
        match self {
            Take::Put => (EntryOp::Put, EntryState::Held, true),
            Take::Del => (EntryOp::Del, EntryState::Held, true),
            Take::CopyBack => (EntryOp::Cb, EntryState::Pending, false),
            Take::Evict => (EntryOp::Evict, EntryState::Sealed, false),
            Take::Stray => (EntryOp::Put, EntryState::Pending, false),
        }
    }
}

/// How a [`Take`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Created {
    /// The caller made the entry.
    Taken(Seen),
    /// An entry exists already (or another body holds the key after an answer that was lost).
    Exists,
}

/// A transition of an entry that is already there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// A put that landed (or failed in a way that may have landed): `(PUT, HELD, me)` to
    /// `(PUT, PENDING)`. Its success is the acknowledgement of the put.
    LandPut,
    /// A delete that amends a landed put or a requested copy-back: `(PUT|CB, PENDING)` to
    /// `(DEL, HELD, me)`. It does not wait for anyone.
    Amend,
    /// A delete that amends a put whose copy the reconciler may be running: `(PUT, HELD, other)`
    /// to `(DEL, HELD, me)`. The caller says that `other` is the reconciler. This delete waits for
    /// the release of the entry.
    AmendCopy,
    /// A delete that claims a delete handed over: `(DEL, PENDING)` to `(DEL, HELD, me)`. It waits
    /// for the release of the entry.
    Claim,
    /// A delete that hands its entry to the reconciler, after an amend or a claim or after a call
    /// that failed: `(DEL, HELD, me)` to `(DEL, PENDING)`.
    Handover,
    /// The reconciler's hold on a put before it copies: `(PUT, PENDING)` to `(PUT, HELD, me)`. It
    /// fails when a delete amended the entry, and then no copy starts.
    HoldForCopy,
    /// The reconciler seals its work: `(PUT|DEL|CB, PENDING)` or its own `(PUT, HELD, me)` to
    /// `(op, SEALED)`.
    Seal,
    /// The reconciler takes a held entry whose owner has departed: `(PUT|DEL, HELD, other)` to
    /// `(op, PENDING)`. The caller has judged that `other` is gone.
    Reap,
}

/// The shape an entry takes: operation, state and owner.
type Shape<'a> = (EntryOp, EntryState, Option<&'a str>);

impl Step {
    /// What `from` becomes under this step for the actor `me`, or `None` when the step does not
    /// apply to an entry in that shape.
    #[must_use]
    pub fn apply<'a>(self, from: &Entry, me: &'a str) -> Option<Shape<'a>> {
        let mine = from.owner.as_deref() == Some(me);
        let theirs = from.owner.as_deref().is_some_and(|owner| owner != me);
        let op = from.op;
        let next = match (self, op, from.st) {
            (Step::LandPut, EntryOp::Put, EntryState::Held) if mine => {
                (EntryOp::Put, EntryState::Pending, None)
            }
            (Step::Amend, EntryOp::Put | EntryOp::Cb, EntryState::Pending) => {
                (EntryOp::Del, EntryState::Held, Some(me))
            }
            (Step::AmendCopy, EntryOp::Put, EntryState::Held) if theirs => {
                (EntryOp::Del, EntryState::Held, Some(me))
            }
            (Step::Claim, EntryOp::Del, EntryState::Pending) => {
                (EntryOp::Del, EntryState::Held, Some(me))
            }
            (Step::Handover, EntryOp::Del, EntryState::Held) if mine => {
                (EntryOp::Del, EntryState::Pending, None)
            }
            (Step::HoldForCopy, EntryOp::Put, EntryState::Pending) => {
                (EntryOp::Put, EntryState::Held, Some(me))
            }
            (Step::Seal, EntryOp::Put | EntryOp::Del | EntryOp::Cb, EntryState::Pending) => {
                (op, EntryState::Sealed, None)
            }
            (Step::Seal, EntryOp::Put, EntryState::Held) if mine => (op, EntryState::Sealed, None),
            (Step::Reap, EntryOp::Put | EntryOp::Del, EntryState::Held) if theirs => {
                (op, EntryState::Pending, None)
            }
            _ => return None,
        };
        Some(next)
    }

    /// Whether a delete that took this step waits for the release of the entry before it
    /// acknowledges: the entry may be a copy in flight, which must end first.
    #[must_use]
    pub fn waits(self) -> bool {
        matches!(self, Step::AmendCopy | Step::Claim)
    }
}

impl Entry {
    /// Whether `me` may remove this entry: it is sealed, or it is a put or a delete that `me`
    /// holds.
    fn releasable_by(&self, me: &str) -> bool {
        let mine = self.owner.as_deref() == Some(me);
        match (self.op, self.st) {
            (EntryOp::Unknown, _) => false,
            (_, EntryState::Sealed) => true,
            (EntryOp::Put | EntryOp::Del, EntryState::Held) => mine,
            _ => false,
        }
    }
}

/// How a replace or a delete of an entry ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome<T> {
    /// It was done.
    Done(T),
    /// The entry changed since it was read, or is gone: read it again and decide again.
    Lost,
    /// The step does not apply to the entry as it was read, so no call was made.
    Refused,
}

/// Whether an answer leaves it open that the write was done: the store was unavailable, or it
/// failed in a way that does not say. Such an answer is also the only kind a retry of a step may
/// follow, since any other says the store refused the call.
pub(crate) fn ambiguous(error: &Error) -> bool {
    matches!(error, Error::Unavailable(_) | Error::Other(_))
}

/// How a replace ended for a caller that may try it again ([`Entries::try_advance`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attempt {
    /// The replace has an outcome: it was done, the entry changed, or the step did not apply.
    Settled(Outcome<Seen>),
    /// The store failed the replace and the entry was read back as it was read before: the
    /// replace did not land, and the same step on the same [`Seen`] may be tried again.
    Unchanged(Error),
    /// The store failed the replace and the entry could not be read back, so whether it landed is
    /// not known: [`Entries::settle`] reads again.
    Unknown(Unsettled),
}

/// A replace whose answer was lost, and the nonce of the body it wrote if it landed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unsettled {
    error: Error,
    nonce: String,
}

impl Unsettled {
    /// The error the store answered with.
    #[must_use]
    pub fn into_error(self) -> Error {
        self.error
    }
}

/// An answer of a read, with a missing object as `None`.
pub(crate) fn found<T>(answer: Result<T, Error>) -> Result<Option<T>, Error> {
    match answer {
        Ok(value) => Ok(Some(value)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The store calls on the entries of one actor: a proxy or a reconciler, named by `me`.
///
/// `me` may be the same in every process of the actor, so the nonces it writes are made unique by
/// `boot`, which is random for every [`Entries`] made.
pub struct Entries {
    hot: Arc<dyn Backend>,
    me: String,
    boot: String,
    clock: Arc<dyn Clock>,
    writes: AtomicU64,
}

impl Entries {
    /// The calls of the actor `me` on the entries in `hot`; `clock` is the wall clock for
    /// `created_ms`.
    #[must_use]
    pub fn new(hot: Arc<dyn Backend>, me: impl Into<String>, clock: Arc<dyn Clock>) -> Self {
        let boot = crate::lease::new_nonce();
        Self::with_boot(hot, me, clock, boot)
    }

    /// As [`Entries::new`] with the `boot` named, for a test that needs the nonces to be known.
    #[must_use]
    pub(crate) fn with_boot(
        hot: Arc<dyn Backend>,
        me: impl Into<String>,
        clock: Arc<dyn Clock>,
        boot: impl Into<String>,
    ) -> Self {
        Self {
            hot,
            me: me.into(),
            boot: boot.into(),
            clock,
            writes: AtomicU64::new(0),
        }
    }

    /// A nonce no other write has: this actor's id, this instance's boot and the count of its
    /// writes.
    fn nonce(&self) -> String {
        let count = self.writes.fetch_add(1, Ordering::SeqCst);
        format!("{}.{}.{count}", self.me, self.boot)
    }

    /// Reads the entry of `key`: its body and tag, or `None` if there is none.
    ///
    /// # Errors
    /// The backend's error for a failed head or get, and [`Error::Other`] for a body that is not
    /// an entry.
    pub async fn read(&self, key: &str) -> Result<Option<Seen>, Error> {
        self.read_path(&entry_key(key)).await
    }

    /// Reads the entry object at `path`, a name from [`Entries::list`]: its body and tag, or
    /// `None` if there is none.
    ///
    /// # Errors
    /// As [`Entries::read`].
    pub async fn read_path(&self, path: &str) -> Result<Option<Seen>, Error> {
        let (body, tag) = match self.hot.get_tagged(path, None).await {
            Ok(read) => read,
            Err(Error::Unsupported(_)) => {
                let Some(meta) = found(self.hot.head(path).await)? else {
                    return Ok(None);
                };
                let Some(body) = found(self.hot.get(path, None).await)? else {
                    return Ok(None);
                };
                (body, meta.tag)
            }
            Err(Error::NotFound(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(Some(Seen {
            entry: decode_entry(&body)?,
            tag,
        }))
    }

    /// The names of the entries in the hot tier, in ascending order, listed `page_size` at a time
    /// (at least one). An entry that is made or removed while the listing is read may or may not
    /// be in it.
    ///
    /// # Errors
    /// The backend's error for a failed listing.
    pub async fn list(&self, page_size: usize) -> Result<Vec<String>, Error> {
        let page_size = page_size.max(1);
        let mut paths: Vec<String> = Vec::new();
        loop {
            let after = paths.last().cloned();
            let page = self
                .hot
                .list_page(ENTRIES_PREFIX, after.as_deref(), page_size)
                .await?;
            let last = page.len() < page_size;
            paths.extend(page.into_iter().map(|object| object.key));
            if last {
                return Ok(paths);
            }
        }
    }

    /// Creates the entry of `key` as `take` says, create-only.
    ///
    /// # Errors
    /// The backend's error for a failed write (see the module for the ambiguous ones).
    pub async fn create(&self, key: &str, take: Take) -> Result<Created, Error> {
        let (op, st, owned) = take.shape();
        let entry = Entry {
            v: ENTRY_VERSION,
            key: key.to_owned(),
            op,
            st,
            owner: owned.then_some(self.me.clone()),
            nonce: self.nonce(),
            created_ms: self.clock.now(),
        };
        let written = self
            .hot
            .put_tagged(&entry_key(key), encode_entry(&entry))
            .await;
        match written {
            Ok(tag) => Ok(Created::Taken(Seen { entry, tag })),
            Err(Error::AlreadyExists(_)) => Ok(Created::Exists),
            Err(error) if ambiguous(&error) => self.settle_create(&entry, error).await,
            Err(error) => Err(error),
        }
    }

    /// The answer of a create whose reply was lost, from what is stored.
    async fn settle_create(&self, entry: &Entry, error: Error) -> Result<Created, Error> {
        match self.read(&entry.key).await {
            Ok(Some(seen)) if seen.entry.nonce == entry.nonce => Ok(Created::Taken(seen)),
            Ok(Some(_)) => Ok(Created::Exists),
            Ok(None) | Err(_) => Err(error),
        }
    }

    /// Takes `step` on the entry as `seen` has it, replacing it only if it still has the tag of
    /// `seen`. The new body keeps `key` and `created_ms` and has a fresh nonce.
    ///
    /// # Errors
    /// The backend's error for a failed write (see the module for the ambiguous ones).
    pub async fn advance(&self, seen: &Seen, step: Step) -> Result<Outcome<Seen>, Error> {
        match self.try_advance(seen, step).await? {
            Attempt::Settled(outcome) => Ok(outcome),
            Attempt::Unchanged(_) => Ok(Outcome::Lost),
            Attempt::Unknown(unsettled) => Err(unsettled.into_error()),
        }
    }

    /// As [`Entries::advance`], for a caller that retries: a replace that failed ambiguously is
    /// not folded into [`Outcome::Lost`] or an error, but says whether the entry was read back as
    /// it was ([`Attempt::Unchanged`]) or could not be read back ([`Attempt::Unknown`]).
    ///
    /// # Errors
    /// The backend's error for a failed write that is not ambiguous.
    pub async fn try_advance(&self, seen: &Seen, step: Step) -> Result<Attempt, Error> {
        let Some((op, st, owner)) = step.apply(&seen.entry, &self.me) else {
            return Ok(Attempt::Settled(Outcome::Refused));
        };
        let next = Entry {
            v: ENTRY_VERSION,
            key: seen.entry.key.clone(),
            op,
            st,
            owner: owner.map(str::to_owned),
            nonce: self.nonce(),
            created_ms: seen.entry.created_ms,
        };
        let path = entry_key(&next.key);
        let written = self
            .hot
            .put_if_match(&path, encode_entry(&next), &seen.tag)
            .await;
        match written {
            Ok(tag) => Ok(Attempt::Settled(Outcome::Done(Seen { entry: next, tag }))),
            Err(Error::PreconditionFailed(_) | Error::NotFound(_)) => {
                Ok(Attempt::Settled(Outcome::Lost))
            }
            Err(error) if ambiguous(&error) => {
                let unsettled = Unsettled {
                    error,
                    nonce: next.nonce,
                };
                Ok(self.settle(seen, unsettled).await)
            }
            Err(error) => Err(error),
        }
    }

    /// Reads the entry again after a replace of `seen` whose answer was lost: it is
    /// [`Attempt::Settled`] `Done` if the body stored has the nonce of the replace, `Lost` if
    /// another body is stored or none is, [`Attempt::Unchanged`] if the entry has the tag of
    /// `seen` still, and [`Attempt::Unknown`] again if the read fails.
    pub async fn settle(&self, seen: &Seen, lost: Unsettled) -> Attempt {
        match self.read(&seen.entry.key).await {
            Ok(Some(now)) if now.entry.nonce == lost.nonce => Attempt::Settled(Outcome::Done(now)),
            Ok(Some(now)) if now.tag == seen.tag => Attempt::Unchanged(lost.error),
            Ok(_) => Attempt::Settled(Outcome::Lost),
            Err(_) => Attempt::Unknown(lost),
        }
    }

    /// Removes the entry as `seen` has it, if this actor may (it is sealed, or a put or a delete it
    /// holds) and it still has the tag of `seen`. A store that does not do a delete on the tag
    /// gets a plain delete, which does not check the tag, only for a sealed entry, which no step
    /// changes. For a held entry, which a delete may amend ([`Step::AmendCopy`]) or the reconciler
    /// reap, it gets nothing deleted and the `Unsupported` error: seal first, and give up the
    /// release when the seal is [`Outcome::Lost`].
    ///
    /// # Errors
    /// The backend's error for a failed delete (see the module for the ambiguous ones).
    pub async fn release(&self, seen: &Seen) -> Result<Outcome<()>, Error> {
        if !seen.entry.releasable_by(&self.me) {
            return Ok(Outcome::Refused);
        }
        let path = entry_key(&seen.entry.key);
        let deleted = match self.hot.delete_if_match(&path, &seen.tag).await {
            Err(Error::Unsupported(_)) if seen.entry.st == EntryState::Sealed => {
                self.hot.delete(&path).await
            }
            answer => answer,
        };
        match deleted {
            Ok(()) => Ok(Outcome::Done(())),
            Err(Error::PreconditionFailed(_) | Error::NotFound(_)) => Ok(Outcome::Lost),
            Err(error) if ambiguous(&error) => self.settle_release(seen, error).await,
            Err(error) => Err(error),
        }
    }

    /// The answer of a delete whose reply was lost, from what is stored.
    async fn settle_release(&self, seen: &Seen, error: Error) -> Result<Outcome<()>, Error> {
        match self.read(&seen.entry.key).await {
            Ok(None) => Ok(Outcome::Done(())),
            Ok(Some(now)) if now.entry.nonce != seen.entry.nonce => Ok(Outcome::Lost),
            Ok(Some(_)) | Err(_) => Err(error),
        }
    }
}
