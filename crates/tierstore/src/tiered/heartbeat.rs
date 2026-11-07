//! The heartbeat of a proxy: it rewrites its registration every [`ProxyConfig::heartbeat`], and
//! each beat that is acknowledged moves the proxy's `live_until` on.
//!
//! `live_until` is the send time of the last acknowledged beat on the proxy's own monotonic
//! clock, plus [`ProxyConfig::departure`] less [`ProxyConfig::deadline_margin`]: past it the
//! proxy must not destroy or acknowledge anything that another process could take as done by a
//! live owner. The only request paths that read `live_until` are the puts and deletes made on entries
//! (they start only if the margin is left, acknowledge only while it is live, and remove an
//! entry only within the margin); [`Proxy::is_live`] is how a caller sees it.
//!
//! After each beat the task reads the format object (`_tiering/format`) too, within the heartbeat
//! period, and keeps what it says as a write does (see `pause`): a proxy takes up the epoch of the
//! entry protocol within one beat of its being written, and one that starts after it, at its first
//! beat, whether or not a request comes. A read that fails changes nothing.
//!
//! Each beat reports two things beside the protocol: `phase`, the phase of the format object the
//! proxy acts on, and `writes_in_flight`, the number of writes it has begun and not ended
//! (`pause::Writing`). The operator's rollback from `active-marks` waits for both
//! (`tierstore format-epoch`). The registration written at the start, before the first beat, reports
//! neither.
//!
//! A stopping process keeps beating until its drain is over and then calls
//! [`Proxy::stop_heartbeat`]: there is no flag in the registration for a closing process, it just
//! stops changing, and the reconciler counts its owner as departed once it has stayed unchanged for
//! the departure time.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::future::{Either, select};
use tokio::sync::{Mutex as AsyncMutex, watch};

use super::pause::{self, Pause};
use super::peers::Peers;
use super::{Inner, Proxy, locked};
use crate::deadline::millis;
use crate::lease::new_nonce;
use crate::registration::{PROTO, PROTO_MARKS, Registration};
use crate::{Backend, Bounded, Clock, Deadline, Error, ProxyConfig, Sleeper};

/// Where the heartbeat of a proxy is.
enum State {
    /// Not started.
    Idle,
    /// Running: the way to stop it, and the end of the task, which closes when the task is over.
    Running(watch::Sender<bool>, watch::Receiver<()>),
    /// Stop called, the task may still be ending: every caller of stop waits for the same end.
    Stopping(watch::Receiver<()>),
    /// Stopped for good (or never started and then stopped): a proxy that has said it is leaving
    /// does not come back.
    Stopped,
}

/// What a proxy keeps of its heartbeat.
pub(super) struct Heartbeat {
    pub(super) deadline: Arc<Deadline>,
    /// The `proto` of every body of the registration: [`PROTO_MARKS`] for a proxy that reads
    /// markers (it has a marks bucket), [`PROTO`] for one that does not.
    pub(super) proto: u32,
    /// Random at the start of the process, in every body of its registration.
    pub(super) boot: String,
    /// The other processes of this proxy id the registration found at open names, which the
    /// heartbeat takes over when it starts.
    peers: Mutex<Peers>,
    state: Mutex<State>,
}

impl Heartbeat {
    pub(super) fn new(config: &ProxyConfig) -> Self {
        Self {
            deadline: Arc::new(Deadline::new(
                Arc::clone(&config.mono),
                config.deadline_margin,
            )),
            proto: if config.marks.is_some() {
                PROTO_MARKS
            } else {
                PROTO
            },
            boot: new_nonce(),
            peers: Mutex::new(Peers::new(beats_in(config.departure, config.heartbeat))),
            state: Mutex::new(State::Idle),
        }
    }

    /// The registration of a process that has not beaten yet.
    pub(super) fn first_body(&self) -> Bytes {
        Registration {
            proto: self.proto,
            boot: self.boot.clone(),
            hb: 0,
            peers: Some(locked(&self.peers).names()),
            phase: None,
            writes_in_flight: None,
        }
        .encode()
    }

    /// Writes this process's registration at open: create-only. One that exists is left as it is,
    /// except that of an earlier process of this release (a body that reads as a registration):
    /// this process does not beat until it starts its heartbeat, perhaps never, so that
    /// registration is replaced by this process's own, with its own boot and a count of zero. Left,
    /// one that beat would stop changing and be taken for the registration of a departed owner
    /// while this process, which may hold entries, is alive; and one that did not beat would keep
    /// the boot of the earlier process, so that the entries of this one, which name its own boot,
    /// would look like those of a process that has restarted. The boot of the earlier process,
    /// and those it names, stay named in `peers` (see `peers.rs`): it may be alive.
    pub(super) async fn register(&self, hot: &dyn Backend, key: &str) -> Result<(), Error> {
        match hot.put(key, self.first_body()).await {
            Err(Error::AlreadyExists(_)) => {}
            other => return other,
        }
        let meta = hot.head(key).await?;
        let body = hot.get(key, None).await?;
        if let Some(found) = Registration::decode(&body) {
            locked(&self.peers).found_at_open(&self.boot, &found);
            hot.put_if_match(key, self.first_body(), &meta.tag).await?;
        }
        Ok(())
    }
}

/// One beat at a time: what is needed to rewrite the registration.
struct Beater {
    hot: Bounded,
    key: String,
    proto: u32,
    boot: String,
    mono: Arc<dyn Clock>,
    deadline: Arc<Deadline>,
    /// How long a beat that is acknowledged gives: the departure time less the margin, in
    /// milliseconds.
    span: u64,
    /// The beats written so far.
    count: u64,
    /// The other processes of this proxy id that may be alive, for `peers` of every body.
    peers: Peers,
    /// The content tag of the registration as this process last wrote it. None before the first
    /// beat and after a beat that failed: the next beat reads the object again.
    etag: Option<String>,
    /// The hot tier as it is, for the look at the format object after each beat.
    plain: Arc<dyn Backend>,
    /// What the proxy keeps of the format object, which that look renews.
    seen: Arc<Mutex<Pause>>,
    /// The gate that the reads of the format by the writes go through, so that the two never
    /// overlap.
    gate: Arc<AsyncMutex<()>>,
    /// The longest that look may take: the heartbeat period, as for a beat.
    limit: Duration,
    /// The writes begun and not ended, which each beat reports.
    writing: Arc<AtomicU64>,
}

impl Beater {
    /// Rewrites the registration once. `live_until` moves on only if the store acknowledged it,
    /// and then to the time the request was sent plus the span, never to a time of the answer.
    ///
    /// A beat that fails (the store did not answer in time, or the object is not the one this
    /// process wrote) forgets the tag, so the next beat reads the object and replaces what it
    /// finds, or creates it if it is gone.
    async fn beat(&mut self) -> Result<(), Error> {
        let sent = self.mono.now();
        let count = self.count + 1;
        let tag = match self.etag.take() {
            Some(etag) => {
                let body = self.body(count);
                self.hot.put_if_match(&self.key, body, &etag).await?
            }
            None => self.adopt(count).await?,
        };
        self.etag = Some(tag);
        self.count = count;
        self.peers.beat();
        self.deadline.extend_to(sent.saturating_add(self.span));
        Ok(())
    }

    /// Reads the format object and keeps what it says, so that the proxy takes up the epoch at its
    /// next beat, and a proxy that starts after the epoch was written takes it up at its first.
    async fn look_at_format(&self) {
        pause::refresh(&self.seen, &self.gate, self.plain.as_ref(), self.limit).await;
    }

    /// The body of beat `count`.
    fn body(&self, count: u64) -> Bytes {
        // The phase is read before the count of writes, never after it (see `pause::Writing`).
        let phase = locked(&self.seen).reported();
        let in_flight = self.writing.load(Ordering::SeqCst);
        Registration {
            proto: self.proto,
            boot: self.boot.clone(),
            hb: count,
            peers: Some(self.peers.names()),
            phase,
            writes_in_flight: Some(in_flight),
        }
        .encode()
    }

    /// Writes beat `count` over the registration that is there, whoever wrote it, or creates it.
    /// A registration of another boot that is there is heard: this process wrote its own at open,
    /// so only a process that is alive could have written the other.
    async fn adopt(&mut self, count: u64) -> Result<String, Error> {
        match self.hot.head(&self.key).await {
            Ok(meta) => {
                let found = self.hot.get(&self.key, None).await?;
                if let Some(found) = Registration::decode(&found) {
                    self.peers.heard(&self.boot, &found);
                }
                let body = self.body(count);
                self.hot.put_if_match(&self.key, body, &meta.tag).await
            }
            Err(Error::NotFound(_)) => {
                let body = self.body(count);
                self.hot.put_tagged(&self.key, body).await
            }
            Err(error) => Err(error),
        }
    }
}

/// Beats, then waits for `period` through `sleeper`, until the stop is signalled. A stop that
/// comes during a beat ends the loop after that beat.
async fn run(
    mut beater: Beater,
    sleeper: Arc<dyn Sleeper>,
    period: Duration,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        // A beat that fails is tried again after the period, and says nothing else: `live_until`
        // does not move, which is what the failure means.
        let _ = beater.beat().await;
        beater.look_at_format().await;
        let stopped = Box::pin(stop.changed());
        let waited = Box::pin(sleeper.sleep(period));
        if matches!(select(stopped, waited).await, Either::Left(_)) {
            return;
        }
    }
}

impl Proxy {
    /// Starts the heartbeat: a task that rewrites this proxy's registration now and then every
    /// [`ProxyConfig::heartbeat`], waiting through `sleeper`. Gives whether it started: a
    /// heartbeat that is running, or was stopped, is not started again.
    ///
    /// Call it inside a Tokio runtime, and call [`Proxy::stop_heartbeat`] when the drain is over.
    /// A proxy that is dropped ends its heartbeat too.
    pub fn start_heartbeat(&self, sleeper: Arc<dyn Sleeper>) -> bool {
        let inner: &Inner = &self.inner;
        let config = &inner.config;
        let mut state = locked(&inner.heartbeat.state);
        if !matches!(*state, State::Idle) {
            return false;
        }
        let beater = Beater {
            hot: Bounded::new(Arc::clone(&inner.hot), config.heartbeat),
            key: crate::journal::proxy_key(&config.proxy_id),
            proto: inner.heartbeat.proto,
            boot: inner.heartbeat.boot.clone(),
            mono: Arc::clone(&config.mono),
            deadline: Arc::clone(&inner.heartbeat.deadline),
            span: millis(config.departure).saturating_sub(millis(config.deadline_margin)),
            count: 0,
            peers: locked(&inner.heartbeat.peers).clone(),
            etag: None,
            plain: Arc::clone(&inner.hot),
            seen: Arc::clone(&inner.pause),
            gate: Arc::clone(&inner.format_gate),
            limit: config.heartbeat,
            writing: Arc::clone(&inner.writing),
        };
        let (stop, stopped) = watch::channel(false);
        let (over, ended) = watch::channel(());
        let period = config.heartbeat;
        tokio::spawn(async move {
            run(beater, sleeper, period, stopped).await;
            drop(over);
        });
        *state = State::Running(stop, ended);
        true
    }

    /// Stops the heartbeat and waits until the task has ended, so that no beat follows; a second
    /// caller, at the same time or later, waits for the same end. The
    /// registration is left as it is: it stops changing, and the reconciler counts its owner as
    /// departed after the departure time. Afterwards the heartbeat cannot be started again,
    /// whether it was running or not.
    pub async fn stop_heartbeat(&self) {
        let mut ended = {
            let mut state = locked(&self.inner.heartbeat.state);
            match std::mem::replace(&mut *state, State::Stopped) {
                State::Idle | State::Stopped => return,
                State::Running(stop, ended) => {
                    stop.send_replace(true);
                    *state = State::Stopping(ended.clone());
                    ended
                }
                State::Stopping(ended) => {
                    *state = State::Stopping(ended.clone());
                    ended
                }
            }
        };
        // Nothing is ever sent on it: this returns when the task has dropped its end.
        let _ = ended.changed().await;
    }

    /// Whether the proxy may still destroy and acknowledge: the time on its own monotonic clock
    /// has not reached `live_until`. False until the first beat is acknowledged, and again when
    /// beats stop being acknowledged for the departure time less the margin.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.inner.heartbeat.deadline.is_live()
    }
}

/// How many beats of `heartbeat` the time `departure` is, rounded up.
fn beats_in(departure: Duration, heartbeat: Duration) -> u64 {
    millis(departure).div_ceil(millis(heartbeat).max(1))
}
