//! The lease that makes one reconciler the runner.

use std::collections::hash_map::RandomState;
use std::convert::Infallible;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::future::{Either, select};
use tokio::sync::Mutex;

use crate::journal::LEASE_KEY;
use crate::{Backend, Clock, Error, Sleeper};

/// 128 random bits as hex, from the keys the standard library seeds each `RandomState` with
/// from the operating system's randomness, so that no crate is needed for it. It only has to
/// differ between two leases, and a guess of it gains nothing: the content tag is not a secret.
fn new_nonce() -> String {
    let bits = |salt: u8| {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u8(salt);
        hasher.finish()
    };
    format!("{:016x}{:016x}", bits(0), bits(1))
}

/// A lease on the object `_tiering/lease` of the hot tier. Its body is the holder's name, the
/// nonce of this lease and the count of bodies written so far, one to a line.
///
/// It is taken by a create-only put, and renewed by a put that replaces the object only if it
/// still has the content tag the last write gave (`If-Match`). No time is read from the hot
/// tier and none is compared across machines; every time here is a reading of the monotonic
/// clock of this process (`mono`):
///
/// - **Takeover.** A contender that finds the lease object taken writes down the content tag
///   it saw and when it first saw it. It may replace the object, again with `If-Match`, so of
///   two that try at once one fails, once the same tag has stood for `ttl` on its own clock.
///   A renewal gives the object a new tag, so a lease that is renewed is never taken. A
///   contender that has just started therefore waits up to `ttl` before it takes a lease its
///   holder has left, as long as a holder that stopped without giving it up.
/// - **Validity.** A holder is valid until `valid_until`, the time before it sent its last
///   successful write, plus `ttl`, less `margin`. [`Lease::margin_ok`] says whether at least
///   `margin` of that is left, which is the room a call that must end inside the lease needs.
/// - **Renewal.** [`Lease::renewing`] renews on a timer of its own, every `renew`, whatever
///   the holder is doing: a pass that is busy with one long copy does not delay it.
///
/// A holder whose renewal fails (it lost the lease, or the hot tier did not answer) stops
/// holding it at once, and does not use it again before it takes it anew. A holder that stops
/// deletes the lease object only if it still has the content tag the holder wrote.
///
/// The lease keeps two reconcilers from doing the same work. The copies are create-only and
/// every other step can be done twice, but a holder only learns that it lost the lease when
/// it renews it, so a pass can run beside another holder's for up to `renew` (and for as long
/// as the hot tier does not answer the renewals).
#[derive(Clone)]
pub(crate) struct Lease {
    shared: Arc<Shared>,
}

struct Shared {
    hot: Arc<dyn Backend>,
    mono: Arc<dyn Clock>,
    holder: String,
    ttl: u64,
    /// How long the timer of [`Lease::renewing`] waits between two renewals.
    period: u64,
    margin: u64,
    /// Random at `new`, in every body: another process of the same holder name counts its
    /// bodies from the same start, and without the nonce would write the same bodies and so
    /// the same content tags.
    nonce: String,
    /// The time on `mono` until which the lease is held, or 0 when it is not held. Read
    /// without the lock, by the pass that works while the timer renews.
    valid_until: AtomicU64,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Counts the bodies written, so that every body of this lease differs from the one
    /// before. With the nonce, no holder is mistaken for another by the content tag.
    generation: u64,
    /// The content tag of the lease object as this holder last wrote it, while it holds it.
    etag: Option<String>,
    /// The tag of the lease object of another holder, and when this contender first saw it.
    seen: Option<Seen>,
}

struct Seen {
    tag: String,
    since: u64,
}

impl Lease {
    /// A lease for `holder`, taken over when the same object has stood for `ttl_millis` on the
    /// clock `mono`, renewed every `renew_millis` (at least a millisecond), and valid for
    /// `ttl_millis` less `margin_millis` after the start of its last renewal.
    pub(crate) fn new(
        hot: Arc<dyn Backend>,
        mono: Arc<dyn Clock>,
        holder: &str,
        ttl_millis: u64,
        renew_millis: u64,
        margin_millis: u64,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                hot,
                mono,
                holder: holder.to_owned(),
                ttl: ttl_millis,
                period: renew_millis.max(1),
                margin: margin_millis,
                nonce: new_nonce(),
                valid_until: AtomicU64::new(0),
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// Holds the lease after this call: if it is held with at least the margin left, nothing
    /// is written (the timer renews it); else it renews it if it is held, or takes it if it
    /// is free or has stood unchanged for the ttl.
    pub(crate) async fn ensure(&self) -> bool {
        if self.margin_ok() {
            return true;
        }
        let mut state = self.shared.state.lock().await;
        let sent = self.shared.mono.now();
        let held = match state.etag.clone() {
            Some(etag) => self.shared.renew(&mut state, &etag).await,
            None => self.shared.acquire(&mut state).await,
        };
        self.shared.settle(held, sent);
        held
    }

    /// Whether the lease is held and at least the margin is left of the time it is valid.
    pub(crate) fn margin_ok(&self) -> bool {
        let until = self.shared.valid_until.load(Ordering::SeqCst);
        until != 0 && until.saturating_sub(self.shared.mono.now()) >= self.shared.margin
    }

    /// Runs `work`, and while it runs renews the lease every `renew` on a timer of its own, as
    /// long as the lease is held; it never takes a lease: a holder that lost it stops renewing,
    /// and the pass finds out at its next check. The timer waits first, so nothing is written
    /// when `work` ends sooner than one period. It is the wait of `sleeper`, not a delay of
    /// `work`, that decides when a renewal is made, so a step that takes longer than the ttl
    /// does not keep the lease from being renewed.
    pub(crate) async fn renewing<T>(
        &self,
        sleeper: &dyn Sleeper,
        work: impl Future<Output = T>,
    ) -> T {
        let mut work = pin!(work);
        loop {
            let wait = sleeper.sleep(Duration::from_millis(self.shared.period));
            match select(work.as_mut(), wait).await {
                Either::Left((done, _)) => return done,
                Either::Right(((), _)) => self.renew_held().await,
            }
        }
    }

    /// Renews the lease if it is held.
    async fn renew_held(&self) {
        let mut state = self.shared.state.lock().await;
        if let Some(etag) = state.etag.clone() {
            let sent = self.shared.mono.now();
            let held = self.shared.renew(&mut state, &etag).await;
            self.shared.settle(held, sent);
        }
    }

    /// Whether the lease object still has the content tag this holder last wrote. Only reads: it
    /// does not renew. A holder that finds it gone or replaced stops holding it, as after a failed
    /// renewal. If the hot tier does not answer the holder keeps its tag and the error is given.
    ///
    /// # Errors
    /// What the HEAD gives other than not found.
    pub(crate) async fn still_held(&self) -> Result<bool, Error> {
        let mut state = self.shared.state.lock().await;
        let Some(etag) = state.etag.clone() else {
            return Ok(false);
        };
        match self.shared.hot.head(LEASE_KEY).await {
            Ok(meta) if meta.tag == etag => Ok(true),
            Ok(_) | Err(Error::NotFound(_)) => {
                state.etag = None;
                self.shared.valid_until.store(0, Ordering::SeqCst);
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// Gives the lease up, if it is held and the lease object is still the one this holder
    /// wrote: a holder that lost the lease without knowing leaves the new holder's alone.
    pub(crate) async fn release(&self) {
        let mut state = self.shared.state.lock().await;
        self.shared.valid_until.store(0, Ordering::SeqCst);
        if let Some(etag) = state.etag.take() {
            let ours = match self.shared.hot.head(LEASE_KEY).await {
                Ok(meta) => meta.tag == etag,
                Err(_) => false,
            };
            if ours {
                let _ = self.shared.hot.delete(LEASE_KEY).await;
            }
        }
    }
}

impl Shared {
    /// Records the result of a write that was sent at `sent`.
    fn settle(&self, held: bool, sent: u64) {
        let until = if held {
            sent.saturating_add(self.ttl)
                .saturating_sub(self.margin)
                .max(1)
        } else {
            0
        };
        self.valid_until.store(until, Ordering::SeqCst);
    }

    fn body(&self, state: &mut State) -> Bytes {
        state.generation += 1;
        Bytes::from(format!(
            "{}\n{}\n{}",
            self.holder, self.nonce, state.generation
        ))
    }

    async fn renew(&self, state: &mut State, etag: &str) -> bool {
        let body = self.body(state);
        state.etag = self.hot.put_if_match(LEASE_KEY, body, etag).await.ok();
        if state.etag.is_some() {
            state.seen = None;
        }
        state.etag.is_some()
    }

    async fn acquire(&self, state: &mut State) -> bool {
        let body = self.body(state);
        match self.hot.put(LEASE_KEY, body).await {
            Ok(()) => self.learn_tag(state).await,
            Err(Error::AlreadyExists(_)) => self.take_over(state).await,
            Err(_) => false,
        }
    }

    /// Reads the content tag of the lease object this holder just created. `Backend::put`
    /// returns no tag, so if the object was replaced since, this holder adopts the other holder's
    /// tag. The nonce does not close that; a create-only put that returns the tag would.
    async fn learn_tag(&self, state: &mut State) -> bool {
        state.etag = self.hot.head(LEASE_KEY).await.ok().map(|meta| meta.tag);
        state.seen = None;
        state.etag.is_some()
    }

    /// Takes the lease over if the object has stood with one content tag for the ttl on the
    /// clock of this process, by replacing the object that was seen. The first look at a tag
    /// only writes it down.
    async fn take_over(&self, state: &mut State) -> bool {
        let looked = self.mono.now();
        let Ok(meta) = self.hot.head(LEASE_KEY).await else {
            return false;
        };
        let since = match &state.seen {
            Some(seen) if seen.tag == meta.tag => seen.since,
            _ => {
                state.seen = Some(Seen {
                    tag: meta.tag.clone(),
                    since: looked,
                });
                looked
            }
        };
        if looked.saturating_sub(since) < self.ttl {
            return false;
        }
        self.renew(state, &meta.tag).await
    }
}
