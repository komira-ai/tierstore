//! Which endpoints of a store take requests now. An endpoint that stalled (no byte of an answer
//! within `request_timeout`) or whose connection could not be made is set aside for a while, and
//! the requests go to the other endpoints, in turn, meanwhile.
//!
//! - The first failure sets the endpoint aside for [`FIRST_ASIDE`]; each failure after it with no
//!   answer between doubles that, up to [`MAX_ASIDE`]. Each time is shortened by up to a quarter,
//!   by a number drawn from the store's seed, so the clients of one store do not all come back to
//!   it at the same moment.
//! - When the time is up, the next request that picks an endpoint starts a probe of it
//!   ([`probe`]): one unsigned `GET /` on a task of its own, so no caller's request waits for it.
//!   An answer of any status puts the endpoint back in turn; no answer sets it aside for the
//!   next, longer, time. While the probe is out the endpoint stays aside, for at most the probe's
//!   own hold, after which another probe is started (one whose task ended unheard of does not
//!   keep the endpoint aside for good).
//! - The last endpoint in turn is never set aside: its failure is the request's error, as with a
//!   store of one endpoint, and the requests go on to it.
//! - An answer of any status, to any request, puts its endpoint back in turn and clears its
//!   failures: an endpoint that answers is alive, whatever it answered.
//! - A failure of a request sent to an endpoint before it was set aside (one that was already on
//!   its way) changes nothing: one stall seen by many requests at once is one failure.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use reqwest::Url;
use tokio::time::Instant;

use super::{Conn, S3Store};
use crate::ObjectStoreError;

/// How long an endpoint is set aside after its first failure.
pub(super) const FIRST_ASIDE: Duration = Duration::from_secs(2);

/// The longest an endpoint is set aside at a time, however many failures it had.
pub(super) const MAX_ASIDE: Duration = Duration::from_secs(30);

/// What is known of one endpoint.
#[derive(Clone, Copy, Debug, Default)]
struct State {
    /// The failures since the endpoint last answered.
    failures: u32,
    /// Until when the endpoint is set aside; `None` while it is in turn.
    aside_until: Option<Instant>,
}

/// The endpoints of one store and which of them take requests.
pub(super) struct Health {
    inner: Mutex<Inner>,
    /// What each time aside is shortened by is drawn from this.
    seed: u64,
    /// How long an endpoint stays aside, at most, while its probe is out.
    probe_hold: Duration,
}

struct Inner {
    states: Vec<State>,
    /// Where the next pick starts looking: the position after the endpoint picked last.
    next: usize,
}

/// The endpoint a request goes to, and the endpoints to probe now.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Pick {
    /// The index of the endpoint, among those of the store.
    pub(super) index: usize,
    /// The endpoints whose time aside is up, each now held aside for its probe.
    pub(super) probes: Vec<usize>,
}

/// What a stall or a failed connect did to its endpoint ([`Health::failed`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// The endpoint is set aside for this long.
    SetAside(Duration),
    /// The endpoint was set aside already: the request was on its way to it before. Nothing
    /// changed.
    AlreadyAside,
    /// The endpoint stays in turn: it is the last one in turn.
    LastInTurn,
}

impl Verdict {
    /// Whether the endpoint is aside now, so that the next pick is another endpoint.
    pub(super) fn aside(self) -> bool {
        self != Self::LastInTurn
    }
}

impl Health {
    /// `endpoints` endpoints (at least one), all in turn. `seed` decides what each time aside is
    /// shortened by; `probe_hold` is how long a probe may take before another is started.
    pub(super) fn new(endpoints: usize, seed: u64, probe_hold: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner {
                states: vec![State::default(); endpoints],
                next: 0,
            }),
            seed,
            probe_hold,
        }
    }

    /// The table, even if a thread panicked while it held it: every change leaves it whole.
    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The endpoint for the next request at `now`: the first endpoint in turn at or after the one
    /// after the endpoint picked last, so the picks go round the endpoints in their order, one
    /// step for each call, and round those in turn alone while some are set aside. An endpoint
    /// whose time aside is up is among the probes, and held aside for its [`probe`].
    pub(super) fn pick(&self, now: Instant) -> Pick {
        let mut inner = self.inner();
        let mut probes = Vec::new();
        for (index, state) in inner.states.iter_mut().enumerate() {
            if state.aside_until.is_some_and(|until| until <= now) {
                state.aside_until = Some(now + self.probe_hold);
                probes.push(index);
            }
        }
        let count = inner.states.len();
        // There is always one endpoint in turn (`failed` never sets the last one aside).
        let index = (0..count)
            .map(|step| (inner.next + step) % count)
            .find(|&i| inner.states[i].aside_until.is_none())
            .unwrap_or(0);
        inner.next = (index + 1) % count;
        Pick { index, probes }
    }

    /// Endpoint `index` answered: it is in turn, with no failures. Whether it was aside.
    pub(super) fn answered(&self, index: usize) -> bool {
        let mut inner = self.inner();
        let state = &mut inner.states[index];
        let was_aside = state.aside_until.is_some();
        *state = State::default();
        was_aside
    }

    /// A request to endpoint `index` stalled or could not connect, at `now`: what that did to
    /// the endpoint.
    pub(super) fn failed(&self, index: usize, now: Instant) -> Verdict {
        let mut inner = self.inner();
        if inner.states[index].aside_until.is_some() {
            return Verdict::AlreadyAside;
        }
        let last = inner.in_turn() == 1;
        let state = &mut inner.states[index];
        state.failures = state.failures.saturating_add(1);
        if last {
            return Verdict::LastInTurn;
        }
        let aside = self.aside_for(index, state.failures);
        state.aside_until = Some(now + aside);
        Verdict::SetAside(aside)
    }

    /// The probe of endpoint `index` got no answer, at `now`: it is set aside again, for longer.
    /// The time it is set aside for, or `None` when it is in turn already: another request it
    /// answered while the probe was out proved it alive, and the probe changes nothing (so it
    /// never sets aside an endpoint in turn, the last one included).
    pub(super) fn probe_failed(&self, index: usize, now: Instant) -> Option<Duration> {
        let mut inner = self.inner();
        let state = &mut inner.states[index];
        state.aside_until?;
        state.failures = state.failures.saturating_add(1);
        let aside = self.aside_for(index, state.failures);
        state.aside_until = Some(now + aside);
        Some(aside)
    }

    /// For each endpoint, whether it is set aside.
    pub(super) fn aside(&self) -> Vec<bool> {
        let inner = self.inner();
        inner
            .states
            .iter()
            .map(|s| s.aside_until.is_some())
            .collect()
    }

    /// How long endpoint `index` is set aside after its `failures`-th failure in a row (at least
    /// one): [`FIRST_ASIDE`] doubled for each failure after the first, at most [`MAX_ASIDE`],
    /// shortened by a part of up to a quarter of it drawn from the seed, the endpoint and the
    /// failure.
    fn aside_for(&self, index: usize, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(16);
        let full = FIRST_ASIDE.saturating_mul(1 << doublings).min(MAX_ASIDE);
        let drawn = mix(self.seed ^ ((index as u64) << 32) ^ u64::from(failures));
        // A quarter of `full` times a fraction in [0, 1], in steps of 2^-32.
        let cut = full / 4 * (drawn >> 32) as u32 / u32::MAX;
        full - cut
    }
}

impl Inner {
    /// How many endpoints are in turn.
    fn in_turn(&self) -> usize {
        self.states
            .iter()
            .filter(|s| s.aside_until.is_none())
            .count()
    }
}

/// SplitMix64's finaliser: a well-spread 64-bit number for each input.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Whether the endpoint at `base` answers an unsigned `GET /` within `timeout`. Any answer
/// counts, whatever its status: a running S3 service refuses an unsigned request with an error
/// of its own, and that error is an answer.
pub(super) async fn probe(client: reqwest::Client, base: Url, timeout: Duration) -> bool {
    client.get(base).timeout(timeout).send().await.is_ok()
}

/// One endpoint of a store and whether requests go to it now ([`S3Store::endpoints`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct EndpointState {
    /// The endpoint as the store was given it: `S3Config::endpoint` or an entry of `also`
    /// ([`S3Store::new_rotating`]).
    pub endpoint: String,
    /// Whether the endpoint is set aside: it stalled or refused a connection, and the requests
    /// go to the other endpoints until a probe of it is answered. A gauge, 1 while set aside.
    pub set_aside: bool,
}

impl S3Store {
    /// Each endpoint of the store, in the order it was given, and whether it is set aside now.
    /// The last endpoint in turn is never set aside, so at least one is not.
    pub fn endpoints(&self) -> Vec<EndpointState> {
        let aside = self.conn.health.aside();
        self.conn
            .targets
            .iter()
            .zip(aside)
            .map(|(target, set_aside)| EndpointState {
                endpoint: target.endpoint.clone(),
                set_aside,
            })
            .collect()
    }
}

impl Conn {
    /// The index of the endpoint for the next request. A probe is started for each endpoint whose
    /// time aside is up.
    pub(super) fn pick(&self) -> usize {
        let Pick { index, probes } = self.health.pick(Instant::now());
        for probed in probes {
            self.start_probe(probed);
        }
        index
    }

    /// Probes endpoint `index` on a task of its own, limited to `request_timeout`, and puts it
    /// back in turn or sets it aside again by the answer.
    fn start_probe(&self, index: usize) {
        let health = Arc::clone(&self.health);
        let client = self.client.clone();
        let target = &self.targets[index];
        let (base, endpoint) = (target.base.clone(), target.endpoint.clone());
        let timeout = self.request_timeout;
        drop(tokio::spawn(async move {
            if probe(client, base, timeout).await {
                health.answered(index);
                tracing::info!(%endpoint, "the endpoint answered its probe: back in turn");
            } else if let Some(aside) = health.probe_failed(index, Instant::now()) {
                tracing::warn!(%endpoint, ?aside, "the endpoint did not answer its probe: set aside again");
            }
        }));
    }

    /// Endpoint `index` answered a request.
    pub(super) fn answered(&self, index: usize) {
        if self.health.answered(index) {
            let endpoint = &self.targets[index].endpoint;
            tracing::info!(%endpoint, "the endpoint answered: back in turn");
        }
    }

    /// A request to endpoint `index` stalled or could not connect, with `error`. Whether the
    /// endpoint is aside now, so that a request sent again goes to another one.
    pub(super) fn endpoint_failed(&self, index: usize, error: &ObjectStoreError) -> bool {
        let verdict = self.health.failed(index, Instant::now());
        if let Verdict::SetAside(aside) = verdict {
            let endpoint = &self.targets[index].endpoint;
            tracing::warn!(
                %endpoint,
                %error,
                ?aside,
                "the endpoint stalled or refused the connection: set aside, requests go to the other endpoints"
            );
        }
        verdict.aside()
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod wire_tests;
