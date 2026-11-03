//! The heartbeat of a proxy: it rewrites its registration every [`ProxyConfig::heartbeat`], and
//! each beat that is acknowledged moves the proxy's `live_until` on.
//!
//! `live_until` is the send time of the last acknowledged beat on the proxy's own monotonic
//! clock, plus [`ProxyConfig::departure`] less [`ProxyConfig::deadline_margin`]: past it the
//! proxy must not destroy or acknowledge anything that another process could take as done by a
//! live owner. Nothing in the request paths asks yet; [`Proxy::is_live`] is how a caller sees it.
//!
//! A stopping process keeps beating until its drain is over and then calls
//! [`Proxy::stop_heartbeat`]: there is no flag in the registration for a closing process, it just
//! stops changing, and the reconciler counts it as departed after [`ProxyConfig::departure`].

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::future::select;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::{Inner, Proxy, locked};
use crate::deadline::millis;
use crate::lease::new_nonce;
use crate::registration::{PROTO, Registration};
use crate::{Bounded, Clock, Deadline, Error, ProxyConfig, Sleeper};

/// Where the heartbeat of a proxy is.
enum State {
    /// Not started.
    Idle,
    /// Running: the way to stop it and to wait for it.
    Running(watch::Sender<bool>, JoinHandle<()>),
    /// Stopped for good: a proxy that has said it is leaving does not come back.
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
        if select(stopped, waited).await.is_left() {
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
        let task = tokio::spawn(run(beater, sleeper, config.heartbeat, stopped));
        *state = State::Running(stop, task);
        true
    }

    /// Stops the heartbeat and waits until the task has ended, so that no beat follows. The
    /// registration is left as it is: it stops changing, and the reconciler counts the process as
    /// departed after [`ProxyConfig::departure`]. Afterwards the heartbeat cannot be started
    /// again, whether it was running or not.
    pub async fn stop_heartbeat(&self) {
        let running = std::mem::replace(&mut *locked(&self.inner.heartbeat.state), State::Stopped);
        if let State::Running(stop, task) = running {
            stop.send_replace(true);
            let _ = task.await;
        }
    }

    /// Whether the proxy may still destroy and acknowledge: the time on its own monotonic clock
    /// has not reached `live_until`. False until the first beat is acknowledged, and again when
    /// beats stop being acknowledged for the departure time less the margin.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.inner.heartbeat.deadline.is_live()
    }
}
