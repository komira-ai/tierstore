//! The puts of a proxy whose format is active (the format says `active`): every put, held or
//! streamed, presigned or the completion of a multipart upload, is made on the journal entry of its
//! key. The entry decides which of several puts of one key wins. The entry reconciler
//! (`EntryReconciler`, implemented in the library) copies the object to the cold tier, removes the
//! entry, and reaps the ones left behind by a proxy that departed. The binary starts entry
//! reconciliation when a round observes the active format with an epoch of 2 or more.
//!
//! A put, in order:
//!
//! 1. The proxy must be live (`Proxy::is_live`): else the put is refused with
//!    [`Error::Unavailable`], and nothing is read or written.
//! 2. The hot tier has the key: [`Error::AlreadyExists`], and no entry is made (a put of a key
//!    that exists is one request).
//! 3. The entry is created, create-only, as `(PUT, HELD)` owned by this proxy. If one exists, the
//!    put is refused: with [`Error::AlreadyExists`] when the entry is settled (pending or sealed)
//!    and a tier holds the key, and with [`Error::Unavailable`] otherwise (a put, delete or copy
//!    of the key is in flight). It looks again if the entry was gone in between, at most
//!    [`ProxyConfig::entry_rounds`](super::ProxyConfig::entry_rounds) times.
//! 4. Under the entry, the hot tier and the cold tier are asked once more, together. The first
//!    answer of the hot tier (step 2) may be stale by now: another put may have landed and been
//!    released since, and a put at `Full` would land in the cold tier beside it. A cold tier that
//!    cannot be asked is an [`Error::Unavailable`]: a new key is not accepted while it is down.
//! 5. The object lands, create-only: in the hot tier, or in the cold tier when the hot tier is
//!    full (a presigned put answers [`Error::Full`] instead). The completion of a multipart upload
//!    lands in the tier that holds the upload (`uploads.rs`): the hot tier unless the upload was
//!    begun while it was full.
//! 6. The entry becomes `(PUT, PENDING)`, which is the acknowledgement of the put. If the entry
//!    changed, the put is not acknowledged and touches nothing again. If the proxy is no longer
//!    live when the replace has answered, the put is not acknowledged either ([`Error::Unavailable`]),
//!    and the entry stays pending.
//!
//! The replace of step 6 and the removal of an entry (below) are tried again when the hot tier
//! fails them ambiguously ([`StepRetry`](super::StepRetry); `entry_retry.rs` says how). The put is
//! acknowledged only when a try is known to have landed, by the store's answer or by the nonce
//! of the entry read back.
//!
//! A put that fails before the object landed removes its entry and leaves nothing. A put whose
//! landing may have happened (the store did not answer) makes the entry pending all the same, and
//! fails, and the entry stays pending. A put that cannot reach the entry in any of its tries
//! leaves it held. Copying, removing and taking over such entries is the work of
//! `EntryReconciler`: it copies a pending one, finishes a held one once its owner has departed or
//! it has stood unchanged for `held_ttl`, and removes it.

use std::future::Future;

use bytes::Bytes;

use super::put::Offered;
use super::{Inner, Level};
use crate::entry::{Created, Entry, EntryOp, EntryState, Outcome, Seen, Step, Take};
use crate::{Error, PartBody, PutAnswer};

/// Why an object did not land.
pub(super) enum Failed {
    /// Nothing landed, and nothing will: the put ends and its entry goes.
    Definite(Error),
    /// The store did not say whether the object landed: the entry is handed to the reconciler.
    Maybe(Error),
}

/// Sorts the error of a landing: a store that did not answer may have written.
pub(super) fn landing(error: Error) -> Failed {
    match error {
        Error::Unavailable(_) | Error::Other(_) => Failed::Maybe(error),
        definite => Failed::Definite(definite),
    }
}

/// A refusal that a client retries.
pub(super) fn retry(key: &str, why: &str) -> Error {
    Error::Unavailable(format!("{why} for {key}; retry later"))
}

/// Whether a put may be answered "exists" from the heads of the tiers: the entry is settled.
fn is_settled(entry: &Entry) -> bool {
    matches!(
        (entry.op, entry.st),
        (_, EntryState::Sealed) | (EntryOp::Put | EntryOp::Cb, EntryState::Pending)
    )
}

impl Inner {
    /// Whether the hot tier has an object under `key`.
    async fn hot_has(&self, key: &str) -> Result<bool, Error> {
        match self.hot.head(key).await {
            Ok(_) => Ok(true),
            Err(Error::NotFound(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Whether the cold tier has an object under `key`; a cold tier that cannot be asked is
    /// [`Error::Unavailable`].
    async fn cold_has(&self, key: &str) -> Result<bool, Error> {
        match self.cold.head(key).await {
            Ok(_) => Ok(true),
            Err(Error::NotFound(_)) => Ok(false),
            Err(error @ Error::Unavailable(_)) => Err(error),
            Err(error) => Err(Error::Unavailable(format!(
                "the cold tier did not answer for {key}: {error}"
            ))),
        }
    }

    /// Fails with [`Error::Unavailable`] unless this proxy may destroy and acknowledge now.
    pub(super) fn require_live(&self, key: &str) -> Result<(), Error> {
        if self.heartbeat.deadline.allows_start() {
            Ok(())
        } else {
            Err(retry(key, "this proxy's registration is not live"))
        }
    }

    /// Takes the entry of `key` for a put, or says why the put is refused.
    async fn take_put(&self, key: &str) -> Result<Seen, Error> {
        for _ in 0..self.config.entry_rounds {
            match self.entries.create(key, Take::Put).await? {
                Created::Taken(seen) => return Ok(seen),
                Created::Exists => {}
            }
            let Some(found) = self.entries.read(key).await? else {
                continue;
            };
            if is_settled(&found.entry) && (self.hot_has(key).await? || self.cold_has(key).await?) {
                return Err(Error::AlreadyExists(key.to_owned()));
            }
            return Err(retry(key, "a put, delete or copy is in flight"));
        }
        Err(retry(key, "the entry keeps changing"))
    }

    /// Removes the entry of a put that ended before landing, if this proxy may still destroy:
    /// each try is cut when it has not answered within the margin, and a failed one is tried
    /// again. An entry that is left is taken by the reconciler once this proxy is gone or the entry
    /// has stood unchanged for `held_ttl`.
    async fn drop_entry(&self, key: &str, seen: &Seen) {
        let _ = self.release_retrying(key, seen).await;
    }

    /// The acknowledgement of a put: `(PUT, HELD, me)` to `(PUT, PENDING)`, if this proxy is still
    /// live and the entry is still the one it holds. Liveness is checked again once the replace
    /// has answered: a proxy that is no longer live then does not acknowledge, and the entry stays
    /// pending for the reconciler.
    async fn land_put(&self, key: &str, seen: &Seen) -> Result<(), Error> {
        let once = || async {
            if !self.heartbeat.deadline.is_live() {
                return Err(retry(key, "this proxy's registration is no longer live"));
            }
            self.entries.try_advance(seen, Step::LandPut).await
        };
        match self.advance_retrying(seen, once).await {
            Ok(Outcome::Done(_)) if !self.heartbeat.deadline.is_live() => Err(retry(
                key,
                "this proxy's registration stopped being live while the entry was written",
            )),
            Ok(Outcome::Done(_)) => Ok(()),
            Ok(Outcome::Lost | Outcome::Refused) => Err(retry(key, "the entry changed")),
            Err(error @ Error::Unavailable(_)) => Err(error),
            Err(error) => Err(retry(
                key,
                &format!("the entry could not be written: {error}"),
            )),
        }
    }

    /// A put of `key` on its entry whose object `land` writes. `land` runs under the entry, after
    /// the tiers were found not to hold the key, and says what it wrote.
    pub(super) async fn put_on_entry<T, F, Fut>(&self, key: &str, land: F) -> Result<T, Error>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, Failed>>,
    {
        self.require_live(key)?;
        if self.hot_has(key).await? {
            return Err(Error::AlreadyExists(key.to_owned()));
        }
        let seen = self.take_put(key).await?;
        let held = self.check_tiers(key).await;
        let landed = match held {
            Ok(()) => land().await,
            Err(failed) => Err(failed),
        };
        match landed {
            Ok(value) => self.land_put(key, &seen).await.map(|()| value),
            Err(Failed::Definite(error)) => {
                self.drop_entry(key, &seen).await;
                Err(error)
            }
            Err(Failed::Maybe(error)) => {
                let _ = self.land_put(key, &seen).await;
                Err(retry(
                    key,
                    &format!("the object may not have landed: {error}"),
                ))
            }
        }
    }

    /// Under the entry: neither tier holds the key. The two heads are asked together, since neither
    /// answer is needed to ask the other; the hot tier's answer is judged first, so the errors and
    /// the refusals are the ones of asking them one after the other.
    async fn check_tiers(&self, key: &str) -> Result<(), Failed> {
        let (hot, cold) = futures::join!(self.hot_has(key), self.cold_has(key));
        if hot.map_err(Failed::Definite)? {
            return Err(Failed::Definite(Error::AlreadyExists(key.to_owned())));
        }
        if cold.map_err(Failed::Definite)? {
            return Err(Failed::Definite(Error::AlreadyExists(key.to_owned())));
        }
        Ok(())
    }

    /// The put of a held body on its entry.
    pub(super) async fn put_held_on_entry(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.put_on_entry(key, || async move {
            match self.level().await {
                Level::Full => self.cold.put(key, body).await.map_err(landing),
                Level::Critical | Level::Open => match self.hot.put(key, body.clone()).await {
                    Err(Error::Full(_)) => {
                        self.latch_full();
                        self.cold.put(key, body).await.map_err(landing)
                    }
                    other => other.map_err(landing),
                },
            }
        })
        .await
    }

    /// The put of a streamed body on its entry; `hot_only` is a presigned put, which is never
    /// written to the cold tier. Gives what the hot tier's answer to the write says of the object
    /// (see [`Backend::put_stream_answered`]), or `None` when the object went to the cold tier or
    /// the hot tier's answer says nothing.
    pub(super) async fn put_streamed_on_entry(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        hot_only: bool,
    ) -> Result<Option<PutAnswer>, Error> {
        let offered = Offered::new(body);
        self.put_on_entry(key, || async move {
            let full = || Failed::Definite(Error::Full(key.to_owned()));
            if matches!(self.level().await, Level::Full) {
                if hot_only {
                    return Err(full());
                }
                let cold = self.cold.put_stream(key, len, offered.view()).await;
                return cold
                    .map(|()| None)
                    .map_err(|error| self.sort(&offered, error));
            }
            let hot = self
                .hot
                .put_stream_answered(key, len, offered.view(), false)
                .await;
            match hot {
                Err(Error::Full(_)) => {
                    self.latch_full();
                    if hot_only || offered.taken() > 0 {
                        return Err(full());
                    }
                    let cold = self.cold.put_stream(key, len, offered.view()).await;
                    cold.map(|()| None)
                        .map_err(|error| self.sort(&offered, error))
                }
                other => other.map_err(|error| self.sort(&offered, error)),
            }
        })
        .await
    }

    /// The error of a streamed landing: one that the body gave left nothing, and any other may
    /// have landed.
    pub(super) fn sort(&self, offered: &Offered<'_>, error: Error) -> Failed {
        if offered.failed() {
            Failed::Definite(error)
        } else {
            landing(error)
        }
    }
}
