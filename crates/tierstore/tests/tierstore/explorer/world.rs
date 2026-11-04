//! The world the explorer runs: one hot and one cold in-memory tier, a few clients (each a real
//! [`Proxy`]) and the real [`EntryReconciler`], every one of them behind its own pair of
//! [`GateBackend`]s. A gate holds each call an actor makes on the key or on an entry, so an actor
//! runs only until its next store call and the driver decides, one call at a time, who goes on.
//!
//! No test here reads a clock or waits on one: actors are polled only while the driver waits for
//! them to park or end, the tokio clock is paused (the delete's waits pass at once), and the
//! monotonic clock moves only when the quiet phase moves it.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::FutureExt;
use tierstore::{
    Backend, DeleteWait, Entries, EntryOp, EntryReconciler, EntryReconcilerConfig, EntryState,
    Error, Gate, GateBackend, MemoryBackend, Mono, Op, Phase, Proxy, ProxyConfig, ReconcilerConfig,
    Sleeper, TestClock,
};

use super::check::{End, Event, Kind, SEED, judge};

/// The one key every run is about.
pub const KEY: &str = "k";

/// The hot budget of a proxy: far above what a run writes, so only the reconciler's budget
/// decides whether the hot tier is full.
const PROXY_BUDGET: u64 = 1 << 20;

/// The most calls one run makes before it is called a livelock.
const STEP_LIMIT: usize = 800;

/// The most steps the reconciler takes in the quiet phase to reach one that changes nothing.
const QUIET_STEPS: usize = 8;

/// The calls an actor is held at: every kind a client or the reconciler makes on the key or on an
/// entry. Calls on `_tiering/` (the format, the lease, the usage, the registrations) pass.
const OPS: [Op; 7] = [
    Op::Put,
    Op::PutIfMatch,
    Op::Get,
    Op::Head,
    Op::Delete,
    Op::DeleteIfMatch,
    Op::List,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Hot,
    Cold,
}

/// Where a call is made: on the key, or on an entry (always in the hot tier).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    Data,
    Entries,
}

impl Space {
    fn prefix(self) -> &'static str {
        match self {
            Space::Data => KEY,
            Space::Entries => "_entries/",
        }
    }
}

/// What a client does with the key; its bytes are the text.
#[derive(Clone, Copy, Debug)]
pub enum ClientOp {
    Put(&'static str),
    /// A get spawns the proxy's copy-back task when it reads the cold tier, so it is the last
    /// operation of a client: the task is then the only call that actor has in flight.
    Get,
    Delete,
}

/// The state the run starts from.
#[derive(Clone, Copy, Debug)]
pub enum Seed {
    Empty,
    /// A put acknowledged by a proxy and not yet copied: the hot tier holds the bytes and the
    /// entry is `(PUT, PENDING)`.
    Pending(&'static str),
    /// Both tiers hold the bytes and there is no entry.
    Replicated(&'static str),
    /// Only the cold tier holds the bytes, as after an eviction.
    ColdOnly(&'static str),
}

#[derive(Clone, Debug)]
pub struct Scenario {
    pub seed: Seed,
    /// The hot budget of the reconciler: the eviction starts above 0.8 of it.
    pub budget: u64,
    pub clients: Vec<Vec<ClientOp>>,
    /// The steps the reconciler takes while the clients run.
    pub reconcile: usize,
}

/// One choice of the driver: who could go, and who did.
#[derive(Clone, Debug)]
pub struct Decision {
    pub enabled: Vec<usize>,
    pub chosen: usize,
}

struct Watch {
    side: Side,
    op: Op,
    space: Space,
    phase: Phase,
    gate: Gate,
}

type Run = Pin<Box<dyn Future<Output = Option<EntryReconciler>>>>;

struct Actor {
    name: String,
    hot: Arc<GateBackend>,
    cold: Arc<GateBackend>,
    watches: Vec<Watch>,
    /// What the actor has left to do; none once it ended.
    run: Option<Run>,
    /// The reconciler, handed back when its steps are done.
    result: Option<EntryReconciler>,
}

/// A wait that never ends: the heartbeat of a proxy and the renewal of the lease then make no
/// call after the first, so the only calls are the ones the run is about.
struct Never;

#[async_trait]
impl Sleeper for Never {
    async fn sleep(&self, _duration: Duration) {
        std::future::pending::<()>().await;
    }
}

#[derive(Clone)]
struct Recorder {
    events: Arc<Mutex<Vec<Event>>>,
    who: usize,
}

impl Recorder {
    fn log(&self, kind: Kind) {
        let event = Event {
            who: self.who,
            kind,
        };
        self.events.lock().unwrap().push(event);
    }
}

fn text(body: &Bytes) -> String {
    String::from_utf8_lossy(body).into_owned()
}

fn bytes(text: &'static str) -> Bytes {
    Bytes::from_static(text.as_bytes())
}

fn watch(backend: &GateBackend, side: Side, op: Op, space: Space, phase: Phase) -> Watch {
    Watch {
        side,
        op,
        space,
        phase,
        gate: backend.gate(op, space.prefix(), phase),
    }
}

/// A gate for every kind of call, before the call reaches the store.
fn arm(hot: &GateBackend, cold: &GateBackend) -> Vec<Watch> {
    let mut watches = Vec::new();
    for op in OPS {
        for space in [Space::Data, Space::Entries] {
            watches.push(watch(hot, Side::Hot, op, space, Phase::Pre));
        }
        watches.push(watch(cold, Side::Cold, op, Space::Data, Phase::Pre));
    }
    watches
}

fn is_parked(watch: &Watch) -> bool {
    watch.gate.parked().now_or_never().is_some()
}

/// A proxy that is live: its first heartbeat is acknowledged and it makes no other.
async fn live_proxy(
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
    id: &str,
    clock: &Arc<TestClock>,
    mono: &Arc<Mono>,
) -> Proxy {
    let mut config = ProxyConfig::new(id, PROXY_BUDGET, clock.clone());
    config.mono = mono.clone();
    config.usage_refresh = Duration::ZERO;
    config.format_refresh = Duration::ZERO;
    config.departure = Duration::from_secs(1000);
    config.deadline_margin = Duration::from_secs(100);
    config.delete_wait = DeleteWait {
        wait: Duration::from_secs(3),
        poll_min: Duration::from_secs(1),
        poll_max: Duration::from_secs(1),
    };
    let proxy = Proxy::open(hot, cold, config).await.unwrap();
    assert!(proxy.start_heartbeat(Arc::new(Never)));
    while !proxy.is_live() {
        tokio::task::yield_now().await;
    }
    proxy
}

async fn client(proxy: Proxy, ops: Vec<ClientOp>, log: Recorder) -> Option<EntryReconciler> {
    for op in ops {
        match op {
            ClientOp::Put(put) => {
                log.log(Kind::PutStart {
                    text: put.to_owned(),
                });
                let acked = proxy.put(KEY, bytes(put)).await.is_ok();
                log.log(Kind::PutDone {
                    text: put.to_owned(),
                    acked,
                });
            }
            ClientOp::Get => {
                log.log(Kind::GetStart);
                log.log(match proxy.get(KEY, None).await {
                    Ok(body) => Kind::GetDone {
                        got: Some(text(&body)),
                    },
                    Err(Error::NotFound(_)) => Kind::GetDone { got: None },
                    Err(_) => Kind::GetFailed,
                });
            }
            ClientOp::Delete => {
                log.log(Kind::DeleteStart);
                let acked = proxy.delete(KEY).await.is_ok();
                log.log(Kind::DeleteDone { acked });
            }
        }
    }
    None
}

async fn leader(
    mut reconciler: EntryReconciler,
    steps: usize,
    log: Recorder,
) -> Option<EntryReconciler> {
    for _ in 0..steps {
        let report = reconciler.step().await;
        log.log(Kind::Step {
            mismatched: report.mismatched.len(),
        });
    }
    Some(reconciler)
}

/// Steps until one changes nothing, moving the monotonic clock on by a second after each, as the
/// loop's own waits would.
async fn quiet(
    mut reconciler: EntryReconciler,
    mono: Arc<Mono>,
    log: Recorder,
) -> Option<EntryReconciler> {
    let mut reached = false;
    for _ in 0..QUIET_STEPS {
        let report = reconciler.step().await;
        log.log(Kind::Step {
            mismatched: report.mismatched.len(),
        });
        mono.advance(Duration::from_secs(1));
        let idle = report.changed == 0 && report.pinned == 0 && report.backed_off == 0;
        if idle && report.failed.is_empty() {
            reached = true;
            break;
        }
    }
    log.log(Kind::Quiet { reached });
    Some(reconciler)
}

async fn tier_text(tier: &MemoryBackend) -> Option<String> {
    match tier.get(KEY, None).await {
        Ok(body) => Some(text(&body)),
        Err(Error::NotFound(_)) => None,
        Err(error) => panic!("a tier answered a read of the key with {error:?}"),
    }
}

pub struct World {
    clock: Arc<TestClock>,
    mono: Arc<Mono>,
    hot: Arc<MemoryBackend>,
    cold: Arc<MemoryBackend>,
    events: Arc<Mutex<Vec<Event>>>,
    actors: Vec<Actor>,
    /// The index of the reconciler's actor, the last.
    leader: usize,
}

impl World {
    /// A world in the scenario's seed state, with every actor run to its first store call.
    pub async fn start(scenario: &Scenario) -> Self {
        let clock = Arc::new(TestClock::new());
        clock.set(6_000_000);
        let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let cold = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let format = Bytes::from_static(br#"{"epoch":2,"phase":"active"}"#);
        hot.put("_tiering/format", format).await.unwrap();
        let mut world = Self {
            clock,
            mono: Arc::new(Mono::new()),
            hot,
            cold,
            events: Arc::new(Mutex::new(Vec::new())),
            actors: Vec::new(),
            leader: scenario.clients.len(),
        };
        world.plant(scenario.seed).await;
        for (index, ops) in scenario.clients.iter().enumerate() {
            world.add_client(index, ops.clone()).await;
        }
        world.add_leader(scenario).await;
        for actor in 0..world.actors.len() {
            world.settle(actor).await;
        }
        world
    }

    fn record(&self, who: usize, kind: Kind) {
        let log = Recorder {
            events: self.events.clone(),
            who,
        };
        log.log(kind);
    }

    /// Makes the seed state, and records the put it stands for as acknowledged.
    async fn plant(&self, seed: Seed) {
        let (Seed::Pending(held) | Seed::Replicated(held) | Seed::ColdOnly(held)) = seed else {
            return;
        };
        self.record(
            SEED,
            Kind::PutStart {
                text: held.to_owned(),
            },
        );
        match seed {
            Seed::Pending(_) => {
                let hot: Arc<dyn Backend> = self.hot.clone();
                let cold: Arc<dyn Backend> = self.cold.clone();
                let proxy = live_proxy(hot, cold, "seed", &self.clock, &self.mono).await;
                proxy.put(KEY, bytes(held)).await.unwrap();
            }
            Seed::Replicated(_) => {
                self.hot.put(KEY, bytes(held)).await.unwrap();
                self.cold.put(KEY, bytes(held)).await.unwrap();
            }
            Seed::ColdOnly(_) | Seed::Empty => self.cold.put(KEY, bytes(held)).await.unwrap(),
        }
        let done = Kind::PutDone {
            text: held.to_owned(),
            acked: true,
        };
        self.record(SEED, done);
    }

    fn gated_pair(&self) -> (Arc<GateBackend>, Arc<GateBackend>) {
        (
            Arc::new(GateBackend::new(self.hot.clone())),
            Arc::new(GateBackend::new(self.cold.clone())),
        )
    }

    async fn add_client(&mut self, index: usize, ops: Vec<ClientOp>) {
        let (hot, cold) = self.gated_pair();
        let id = format!("c{index}");
        let proxy = live_proxy(hot.clone(), cold.clone(), &id, &self.clock, &self.mono).await;
        let watches = arm(&hot, &cold);
        let log = Recorder {
            events: self.events.clone(),
            who: index,
        };
        let run: Run = Box::pin(client(proxy, ops, log));
        self.actors.push(Actor {
            name: id,
            hot,
            cold,
            watches,
            run: Some(run),
            result: None,
        });
    }

    async fn add_leader(&mut self, scenario: &Scenario) {
        let (hot, cold) = self.gated_pair();
        let mut base = ReconcilerConfig::new("r1", scenario.budget, self.clock.clone());
        base.mono = self.mono.clone();
        base.sleeper = Arc::new(Never);
        base.parallel = 1;
        let mut config = EntryReconcilerConfig::new(base);
        config.waiter = Arc::new(Never);
        config.retry_min = Duration::from_millis(1);
        config.retry_max = Duration::from_millis(1);
        let reconciler = EntryReconciler::new(hot.clone(), cold.clone(), config);
        let watches = arm(&hot, &cold);
        let log = Recorder {
            events: self.events.clone(),
            who: self.leader,
        };
        let run: Run = Box::pin(leader(reconciler, scenario.reconcile, log));
        self.actors.push(Actor {
            name: "r1".to_owned(),
            hot,
            cold,
            watches,
            run: Some(run),
            result: None,
        });
    }

    /// The index of the actor that is the reconciler.
    pub fn reconciler(&self) -> usize {
        self.leader
    }

    pub fn name(&self, actor: usize) -> &str {
        &self.actors[actor].name
    }

    /// The call `actor` is held at, if it is held at one.
    pub fn parked_at(&self, actor: usize) -> Option<(Side, Op, Space, Phase)> {
        let held = self.actors[actor]
            .watches
            .iter()
            .find(|held| is_parked(held));
        held.map(|held| (held.side, held.op, held.space, held.phase))
    }

    /// Whether the actor has no operation left to do.
    pub fn ended(&self, actor: usize) -> bool {
        self.actors[actor].run.is_none()
    }

    /// The actors that are held at a call, in order.
    pub fn enabled(&self) -> Vec<usize> {
        (0..self.actors.len())
            .filter(|actor| self.parked_at(*actor).is_some())
            .collect()
    }

    /// Holds the next call of the kind, after the store has done it, so that a test can answer
    /// that call with a failure (see [`World::advance_failing`]).
    pub fn hold_after(&mut self, actor: usize, side: Side, op: Op, space: Space) {
        let actor = &mut self.actors[actor];
        let backend = match side {
            Side::Hot => &actor.hot,
            Side::Cold => &actor.cold,
        };
        let held = watch(backend, side, op, space, Phase::Post);
        actor.watches.push(held);
    }

    /// Lets the call `actor` is held at go on, and runs the actor to its next call or its end.
    pub async fn advance(&mut self, actor: usize) {
        self.release(actor, false).await;
    }

    /// Answers the call `actor` is held after with a failure, as if the store did it and the
    /// answer was lost, and runs the actor on.
    pub async fn advance_failing(&mut self, actor: usize) {
        self.release(actor, true).await;
    }

    async fn release(&mut self, index: usize, failing: bool) {
        let actor = &mut self.actors[index];
        let Some(at) = actor.watches.iter().position(is_parked) else {
            panic!("{} is held at no call", actor.name);
        };
        let spent = actor.watches.remove(at);
        if spent.phase == Phase::Pre {
            let backend = match spent.side {
                Side::Hot => &actor.hot,
                Side::Cold => &actor.cold,
            };
            let fresh = watch(backend, spent.side, spent.op, spent.space, Phase::Pre);
            actor.watches.insert(at, fresh);
        }
        let released = if failing {
            spent.gate.fail_post()
        } else {
            spent.gate.release()
        };
        assert!(released, "{} gave up its parked call", actor.name);
        self.settle(index).await;
    }

    /// Runs the actor until it is held at a call or has ended, and lets the tasks it spawned (the
    /// copy-back of a get) run to their own first call.
    async fn settle(&mut self, index: usize) {
        loop {
            let actor = &mut self.actors[index];
            if actor.watches.iter().any(is_parked) {
                break;
            }
            let Some(run) = actor.run.as_mut() else {
                break;
            };
            let parked: Vec<Pin<Box<dyn Future<Output = ()> + '_>>> = actor
                .watches
                .iter()
                .map(|held| {
                    let waiting: Pin<Box<dyn Future<Output = ()> + '_>> =
                        Box::pin(held.gate.parked());
                    waiting
                })
                .collect();
            let ended = tokio::select! {
                biased;
                out = run => Some(out),
                _ = futures::future::select_all(parked) => None,
            };
            if let Some(out) = ended {
                actor.run = None;
                actor.result = out;
                break;
            }
        }
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
    }

    /// Runs the actors to their ends, `choose` picking among the enabled ones at each call (it is
    /// given the number of the step and the enabled actors, in order).
    pub async fn run(&mut self, choose: &mut dyn FnMut(usize, &[usize]) -> usize) -> Vec<Decision> {
        let mut trace = Vec::new();
        loop {
            let enabled = self.enabled();
            if enabled.is_empty() {
                return trace;
            }
            assert!(
                trace.len() < STEP_LIMIT,
                "a run of {STEP_LIMIT} calls is a livelock"
            );
            let chosen = choose(trace.len(), &enabled);
            trace.push(Decision { enabled, chosen });
            self.advance(chosen).await;
        }
    }

    /// Runs `actor` until it is held at the call, which it has not made.
    pub async fn run_until_parked(&mut self, actor: usize, side: Side, op: Op, space: Space) {
        loop {
            match self.parked_at(actor) {
                Some((s, o, p, Phase::Pre)) if (s, o, p) == (side, op, space) => return,
                Some(_) => self.advance(actor).await,
                None => panic!(
                    "{} ended before it reached {side:?} {op:?} {space:?}",
                    self.name(actor)
                ),
            }
        }
    }

    /// Runs `actor` to its end.
    pub async fn run_to_end(&mut self, actor: usize) {
        while self.parked_at(actor).is_some() {
            self.advance(actor).await;
        }
        assert!(
            self.ended(actor),
            "{} is neither held nor ended",
            self.name(actor)
        );
    }

    /// The shape of the entry of the key: operation, state and owner.
    pub async fn shape(&self) -> Option<(EntryOp, EntryState, Option<String>)> {
        let entries = Entries::new(self.hot.clone(), "reader", self.clock.clone());
        let seen = entries.read(KEY).await.unwrap()?;
        Some((seen.entry.op, seen.entry.st, seen.entry.owner))
    }

    pub async fn hot_has(&self) -> bool {
        tier_text(&self.hot).await.is_some()
    }

    pub async fn cold_has(&self) -> bool {
        tier_text(&self.cold).await.is_some()
    }

    /// What the clients were answered, in order.
    pub fn history(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    /// Finishes the run: lets every actor that is left go on to its end (in order), has the
    /// reconciler step until one changes nothing, reads what is left in the tiers, and gives the
    /// rules the run broke.
    pub async fn verdict(&mut self) -> Vec<String> {
        let mut first = |_: usize, enabled: &[usize]| enabled[0];
        self.run(&mut first).await;
        let reconciler = self.actors[self.leader]
            .result
            .take()
            .expect("the reconciler ended its steps");
        let log = Recorder {
            events: self.events.clone(),
            who: self.leader,
        };
        let steps: Run = Box::pin(quiet(reconciler, self.mono.clone(), log));
        self.actors[self.leader].run = Some(steps);
        self.settle(self.leader).await;
        self.run(&mut first).await;
        let end = self.read_end().await;
        judge(&self.history(), &end)
    }

    async fn read_end(&self) -> End {
        let listed = self.hot.list("_entries/").await.unwrap();
        let entries = listed.into_iter().map(|object| object.key).collect();
        let hot = tier_text(&self.hot).await;
        let cold = tier_text(&self.cold).await;
        let front_hot: Arc<dyn Backend> = self.hot.clone();
        let front_cold: Arc<dyn Backend> = self.cold.clone();
        let reader = live_proxy(front_hot, front_cold, "reader", &self.clock, &self.mono).await;
        let read = match reader.get(KEY, None).await {
            Ok(body) => Ok(Some(text(&body))),
            Err(Error::NotFound(_)) => Ok(None),
            Err(error) => Err(error.to_string()),
        };
        End {
            hot,
            cold,
            read,
            entries,
        }
    }
}
