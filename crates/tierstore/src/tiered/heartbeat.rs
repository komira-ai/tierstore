//! The heartbeat of a proxy: it rewrites its registration every [`ProxyConfig::heartbeat`], and
//! each beat that is acknowledged moves the proxy's `live_until` on.
//!
//! `live_until` is the send time of the last acknowledged beat on the proxy's own monotonic
//! clock, plus [`ProxyConfig::departure`] less [`ProxyConfig::deadline_margin`]: past it the
//! proxy must not destroy or acknowledge anything that another process could take as done by a
//! live owner. This is the only use of `departure` and the margin today: no request path reads
//! `live_until` or refuses anything by it yet (enforcing it on destroying calls is future work),
//! and [`Proxy::is_live`] is how a caller sees it.
//!
//! A stopping process keeps beating until its drain is over and then calls
//! [`Proxy::stop_heartbeat`]: there is no flag in the registration for a closing process, it just
//! stops changing, and the reconciler removes it once it has stayed unchanged for
//! `ReconcilerConfig::registration_retention` (which is at least `departure`) and the proxy left
//! no journal object.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::future::{Either, select};
use tokio::sync::watch;

use super::{Inner, Proxy, locked};
use crate::deadline::millis;
use crate::lease::new_nonce;
use crate::registration::{PROTO, Registration};
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
    /// Random at the start of the process, in every body of its registration.
    pub(super) boot: String,
    state: Mutex<State>,
}

impl Heartbeat {
    pub(super) fn new(config: &ProxyConfig) -> Self {
        Self {
            deadline: Arc::new(Deadline::new(
                Arc::clone(&config.mono),
                config.deadline_margin,
            )),
            boot: new_nonce(),
            state: Mutex::new(State::Idle),
        }
    }

    /// The registration of a process that has not beaten yet.
    pub(super) fn first_body(&self) -> Bytes {
        let (proto, boot, hb) = (PROTO, self.boot.clone(), 0);
        Registration { proto, boot, hb }.encode()
    }

    /// Writes this process's registration at open: create-only. One that exists is left as it is,
    /// except that of an earlier process that beat: this process does not beat until it starts
    /// its heartbeat, perhaps never, so that registration is replaced by this process's own count
    /// of zero, which the leader never removes. Left, it would stop changing, be taken for the
    /// registration of a departed owner and be removed while this process, which may write a
    /// journal, is alive.
    pub(super) async fn register(&self, hot: &dyn Backend, key: &str) -> Result<(), Error> {
        match hot.put(key, self.first_body()).await {
            Err(Error::AlreadyExists(_)) => {}
            other => return other,
        }
        let meta = hot.head(key).await?;
        let body = hot.get(key, None).await?;
        if Registration::decode(&body).is_some_and(|found| found.beats()) {
            hot.put_if_match(key, self.first_body(), &meta.tag).await?;
        }
        Ok(())
    }
}

/// One beat at a time: what is needed to rewrite the registration.
struct Beater {
    hot: Bounded,
    key: String,
    boot: String,
    mono: Arc<dyn Clock>,
    deadline: Arc<Deadline>,
    /// How long a beat that is acknowledged gives: the departure time less the margin, in
    /// milliseconds.
    span: u64,
    /// The beats written so far.
    count: u64,
    /// The content tag of the registration as this process last wrote it. None before the first
    /// beat and after a beat that failed: the next beat reads the object again.
    etag: Option<String>,
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
        let (proto, boot, hb) = (PROTO, self.boot.clone(), count);
        let body = Registration { proto, boot, hb }.encode();
        let tag = match self.etag.take() {
            Some(etag) => self.hot.put_if_match(&self.key, body, &etag).await?,
            None => self.adopt(body).await?,
        };
        self.etag = Some(tag);
        self.count = count;
        self.deadline.extend_to(sent.saturating_add(self.span));
        Ok(())
    }

    /// Writes `body` over the registration that is there, whoever wrote it, or creates it.
    async fn adopt(&self, body: Bytes) -> Result<String, Error> {
        match self.hot.head(&self.key).await {
            Ok(meta) => self.hot.put_if_match(&self.key, body, &meta.tag).await,
            Err(Error::NotFound(_)) => self.hot.put_tagged(&self.key, body).await,
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
            boot: inner.heartbeat.boot.clone(),
            mono: Arc::clone(&config.mono),
            deadline: Arc::clone(&inner.heartbeat.deadline),
            span: millis(config.departure).saturating_sub(millis(config.deadline_margin)),
            count: 0,
            etag: None,
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
    /// registration is left as it is: it stops changing, and the reconciler removes it after
    /// `ReconcilerConfig::registration_retention` if no journal object is left. Afterwards the heartbeat cannot be started
    /// again, whether it was running or not.
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
        while ended.changed().await.is_ok() {}
    }

    /// Whether the proxy may still destroy and acknowledge: the time on its own monotonic clock
    /// has not reached `live_until`. False until the first beat is acknowledged, and again when
    /// beats stop being acknowledged for the departure time less the margin.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.inner.heartbeat.deadline.is_live()
    }
}
