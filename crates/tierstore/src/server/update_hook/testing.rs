//! The fakes of the ports, in one world: a host with two slots, the nftables rules, the units,
//! the links and the state directory, in memory. Instances answer `/readyz` and `/metricz` from
//! what runs: the first to start holds the cache lock and is read-write, the others are
//! read-only until the holder is gone and the fake promotes one. The clock is a number that
//! `pause` moves on; nothing sleeps.
//!
//! Every call of a port except the clock is counted and logged. `fail_at(n)` makes the n-th call
//! (counted from 1, from the moment it is set) fail without effect, which is a crash at that
//! boundary seen from the step: the tests run a step with each call failing in turn and then
//! run it again.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use super::Step;
use super::declaration::PUBLIC_PORT;
use super::params::Params;
use super::ports::{
    Clock, Conntrack, Host, Instances, Lock, Nft, PortError, PortKind, Ports, Reply, Slots,
    StateDir, Units,
};
use super::record::Record;
use super::report::Done;
use super::ruleset::{
    FORWARD_TABLE, GUARD_TABLE, guard_ruleset, redirect_ruleset, redirect_target,
};
use super::steps::{HANDOVER_CAPABILITY, run_step};

/// The declaration the fake host holds.
pub const DECLARATION: &str = "state_dir = \"/var/lib/updater/hooks/tierstore\"\n\
health_ports = [9301, 9302]\n\
[hook]\n\
units = [\"tierstore@9301.service\", \"tierstore@9302.service\"]\n";

/// The release that runs at first.
pub const OLD: &str = "/releases/old";
/// The release the first update installs.
pub const NEW: &str = "/releases/new";
/// The release a second update installs.
pub const NEWER: &str = "/releases/newer";

/// How the fake promotes a read-only instance when the lock is free.
#[derive(Clone, Copy, Debug)]
pub enum Promotion {
    /// At the next look.
    Instant,
    /// Never.
    Stuck,
    /// At the n-th look.
    AfterPolls(u32),
}

/// What the world looks like, without the counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub record: Option<Record>,
    pub redirect: Option<u16>,
    pub guard: bool,
    pub running: BTreeMap<u16, PathBuf>,
    pub links: BTreeMap<u16, PathBuf>,
    pub holder: Option<u16>,
}

struct World {
    record: Option<Vec<u8>>,
    declaration: String,
    busy: bool,
    rules: BTreeMap<String, String>,
    applied: Vec<String>,
    reject_redirect_once: bool,
    reject_guard: bool,
    running: BTreeMap<u16, PathBuf>,
    links: BTreeMap<u16, PathBuf>,
    holder: Option<u16>,
    promotion: Promotion,
    free_polls: u32,
    releases: BTreeMap<PathBuf, (String, Vec<String>)>,
    lies: BTreeMap<u16, String>,
    muted: BTreeSet<u16>,
    public_503: bool,
    slot_label: bool,
    conntrack: Conntrack,
    now: u64,
    pauses: u32,
    boot: u32,
    errors: Vec<String>,
    halt_from: Option<usize>,
    log: Vec<String>,
    calls: usize,
    fail_at: Option<usize>,
}

impl World {
    /// `port` has exited: if it held the lock the kernel frees it, and with `Instant` another
    /// running instance takes it at once.
    fn let_go(&mut self, port: u16) {
        if self.holder == Some(port) {
            self.holder = None;
            self.free_polls = 0;
            if matches!(self.promotion, Promotion::Instant) {
                self.holder = self.running.keys().next().copied();
            }
        }
    }
}

/// Counts the locks that are held; the lock is released when this is dropped.
struct Held(Rc<Cell<u32>>);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

/// The world, and every port over it.
pub struct Fake {
    w: RefCell<World>,
    locks: Rc<Cell<u32>>,
}

fn port_of(unit: &str) -> u16 {
    unit.trim_start_matches("tierstore@")
        .trim_end_matches(".service")
        .parse()
        .expect("a slot unit")
}

fn release(version: &str, caps: &[&str]) -> (String, Vec<String>) {
    (
        version.to_owned(),
        caps.iter().map(|c| (*c).to_owned()).collect(),
    )
}

impl Fake {
    /// A host that serves release `OLD` on 9301 with nothing in flight; `NEW` and `NEWER` are
    /// staged and both have `handover-v1`.
    pub fn new() -> Fake {
        let caps = [HANDOVER_CAPABILITY];
        let releases = BTreeMap::from([
            (PathBuf::from(OLD), release("0.3.0", &caps)),
            (PathBuf::from(NEW), release("0.4.0", &caps)),
            (PathBuf::from(NEWER), release("0.5.0", &caps)),
        ]);
        let rules = BTreeMap::from([
            (
                FORWARD_TABLE.to_owned(),
                redirect_ruleset(PUBLIC_PORT, 9301),
            ),
            (
                GUARD_TABLE.to_owned(),
                guard_ruleset(PUBLIC_PORT, [9301, 9302]),
            ),
        ]);
        let world = World {
            record: Some(Record::idle(9301).encode()),
            declaration: DECLARATION.to_owned(),
            busy: false,
            rules,
            applied: Vec::new(),
            reject_redirect_once: false,
            reject_guard: false,
            running: BTreeMap::from([(9301, PathBuf::from(OLD))]),
            links: BTreeMap::from([(9301, PathBuf::from(OLD))]),
            holder: Some(9301),
            promotion: Promotion::Instant,
            free_polls: 0,
            releases,
            lies: BTreeMap::new(),
            muted: BTreeSet::new(),
            public_503: false,
            slot_label: true,
            conntrack: Conntrack {
                tcp_timeout_established_secs: 432_000,
                count: 10,
                max: 1000,
            },
            now: 0,
            pauses: 0,
            boot: 1,
            errors: Vec::new(),
            halt_from: None,
            log: Vec::new(),
            calls: 0,
            fail_at: None,
        };
        Fake {
            w: RefCell::new(world),
            locks: Rc::new(Cell::new(0)),
        }
    }

    /// A host whose running release lacks `handover-v1`: an update to `NEW` is a restart.
    pub fn legacy() -> Fake {
        let fake = Fake::new();
        fake.w
            .borrow_mut()
            .releases
            .insert(PathBuf::from(OLD), release("0.3.0", &[]));
        fake
    }

    /// The ports over this world.
    pub fn ports(&self) -> Ports<'_> {
        Ports {
            nft: self,
            units: self,
            slots: self,
            instances: self,
            host: self,
            clock: self,
            state: self,
            public_port: PUBLIC_PORT,
        }
    }

    /// The params of an update to `NEW`, with `promotion_bound_secs` at 30.
    pub fn params(&self) -> Params {
        self.params_for(NEW, "0.4.0")
    }

    /// The params of an update to `release`, which is `version`.
    pub fn params_for(&self, release: &str, version: &str) -> Params {
        Params::parse(&format!(
            "promotion_bound_secs = 30\n\
             \"updater.staged_release\" = \"{release}\"\n\
             \"updater.target_version\" = \"{version}\"\n\
             \"updater.promotion_ceiling_secs\" = 3600\n"
        ))
        .expect("params")
    }

    /// Runs `step` as an update to `NEW`.
    pub fn run(&self, step: Step) -> Done {
        run_step(step, Some(&self.params()), &self.ports())
    }

    /// Runs `steps` in order, each of which must succeed.
    pub fn advance(&self, steps: &[Step]) {
        for step in steps {
            let done = self.run(*step);
            assert_eq!(done.outcome.code(), 0, "{step}: {done:?}");
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let w = self.w.borrow();
        let record = w
            .record
            .as_ref()
            .map(|b| Record::decode(b).expect("a record"));
        Snapshot {
            record,
            redirect: w.rules.get(FORWARD_TABLE).and_then(|t| redirect_target(t)),
            guard: w.rules.contains_key(GUARD_TABLE),
            running: w.running.clone(),
            links: w.links.clone(),
            holder: w.holder,
        }
    }

    pub fn record(&self) -> Record {
        self.snapshot().record.expect("a record")
    }

    /// Every call so far, in order: `nft.apply <table>`, `units.start <unit>`, `get 9302
    /// /readyz`, and so on.
    pub fn log(&self) -> Vec<String> {
        self.w.borrow().log.clone()
    }

    /// The calls that change something, in order.
    pub fn changes(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|c| {
                [
                    "nft.apply",
                    "nft.delete",
                    "units.",
                    "slots.point",
                    "slots.remove",
                    "state.save",
                ]
                .iter()
                .any(|p| c.starts_with(p))
            })
            .collect()
    }

    /// The number of calls so far.
    pub fn calls(&self) -> usize {
        self.w.borrow().calls
    }

    /// The rule texts applied so far, in order.
    pub fn applied(&self) -> Vec<String> {
        self.w.borrow().applied.clone()
    }

    pub fn pauses(&self) -> u32 {
        self.w.borrow().pauses
    }

    pub fn live_locks(&self) -> u32 {
        self.locks.get()
    }

    /// The n-th call from now fails without effect; `None` clears it.
    pub fn fail_at(&self, n: Option<usize>) {
        let mut w = self.w.borrow_mut();
        w.fail_at = n.map(|n| w.calls + n);
    }

    pub fn set_promotion(&self, promotion: Promotion) {
        self.w.borrow_mut().promotion = promotion;
    }

    /// The instance on `port` dies without being stopped; the kernel frees the lock.
    pub fn crash(&self, port: u16) {
        let mut w = self.w.borrow_mut();
        w.running.remove(&port);
        w.let_go(port);
    }

    /// The instance on `port` reports `version` in its build gauge, whatever it runs.
    pub fn lie(&self, port: u16, version: &str) {
        self.w.borrow_mut().lies.insert(port, version.to_owned());
    }

    /// `/readyz` of the instance on `port` does not answer.
    pub fn mute(&self, port: u16) {
        self.w.borrow_mut().muted.insert(port);
    }

    /// The running instances lose the lock, as if the holder had let go of it.
    pub fn drop_lock(&self) {
        self.w.borrow_mut().holder = None;
    }

    /// Gives `release` exactly these capabilities.
    pub fn set_caps(&self, release: &str, caps: &[&str]) {
        let mut w = self.w.borrow_mut();
        let entry = w.releases.get_mut(Path::new(release)).expect("a release");
        entry.1 = caps.iter().map(|c| (*c).to_owned()).collect();
    }

    /// The public port answers 503 to everything.
    pub fn public_busy(&self) {
        self.w.borrow_mut().public_503 = true;
    }

    /// The build gauge names no slot.
    pub fn no_slot_label(&self) {
        self.w.borrow_mut().slot_label = false;
    }

    pub fn set_busy(&self, busy: bool) {
        self.w.borrow_mut().busy = busy;
    }

    pub fn set_conntrack(&self, conntrack: Conntrack) {
        self.w.borrow_mut().conntrack = conntrack;
    }

    pub fn reject_redirect_once(&self) {
        self.w.borrow_mut().reject_redirect_once = true;
    }

    pub fn reject_guard(&self) {
        self.w.borrow_mut().reject_guard = true;
    }

    pub fn set_declaration(&self, text: &str) {
        self.w.borrow_mut().declaration = text.to_owned();
    }

    pub fn set_record_bytes(&self, bytes: Option<&[u8]>) {
        self.w.borrow_mut().record = bytes.map(<[u8]>::to_vec);
    }

    pub fn set_record(&self, record: &Record) {
        self.set_record_bytes(Some(&record.encode()));
    }

    /// Removes the link of `port` as if something else had.
    pub fn drop_link(&self, port: u16) {
        self.w.borrow_mut().links.remove(&port);
    }

    /// A host that rebooted: nothing runs, the lock is free, the rules are gone.
    pub fn reboot(&self) {
        let mut w = self.w.borrow_mut();
        w.running.clear();
        w.holder = None;
        w.free_polls = 0;
        w.rules.clear();
        w.boot += 1;
    }

    /// The world stops from the n-th call on: that call and every later one fail without effect,
    /// so no recovery code of the step can run. `None` lets it go on.
    pub fn halt_at(&self, n: Option<usize>) {
        let mut w = self.w.borrow_mut();
        w.halt_from = n.map(|n| w.calls + n);
    }

    /// The host has booted again, and nothing else has changed.
    pub fn new_boot(&self) {
        self.w.borrow_mut().boot += 1;
    }

    /// The guard table is not there.
    pub fn remove_guard(&self) {
        self.w.borrow_mut().rules.remove(GUARD_TABLE);
    }

    /// Wrong things that happened in the world, such as two instances of a release that has no
    /// lock running together.
    pub fn errors(&self) -> Vec<String> {
        self.w.borrow().errors.clone()
    }

    /// Counts one call and fails it if it is the chosen one.
    fn tick(&self, what: String, kind: PortKind) -> Result<(), PortError> {
        let mut w = self.w.borrow_mut();
        w.calls += 1;
        w.log.push(what);
        if w.fail_at == Some(w.calls) || w.halt_from.is_some_and(|from| w.calls >= from) {
            Err(PortError::failed(kind, "injected failure"))
        } else {
            Ok(())
        }
    }
}

impl Nft for Fake {
    fn apply(&self, ruleset: &str) -> Result<(), PortError> {
        let table = ruleset
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("add table inet "))
            .ok_or_else(|| PortError::failed(PortKind::Nft, "not a ruleset"))?
            .to_owned();
        self.tick(format!("nft.apply {table}"), PortKind::Nft)?;
        let mut w = self.w.borrow_mut();
        if table == FORWARD_TABLE && std::mem::take(&mut w.reject_redirect_once) {
            return Err(PortError::failed(PortKind::Nft, "rejected"));
        }
        if table == GUARD_TABLE && w.reject_guard {
            return Err(PortError::failed(PortKind::Nft, "rejected"));
        }
        w.applied.push(ruleset.to_owned());
        w.rules.insert(table, ruleset.to_owned());
        Ok(())
    }

    fn list_table(&self, table: &str) -> Result<Option<String>, PortError> {
        self.tick(format!("nft.list {table}"), PortKind::Nft)?;
        Ok(self.w.borrow().rules.get(table).cloned())
    }

    fn delete_table(&self, table: &str) -> Result<(), PortError> {
        self.tick(format!("nft.delete {table}"), PortKind::Nft)?;
        self.w.borrow_mut().rules.remove(table);
        Ok(())
    }
}

impl Units for Fake {
    fn start(&self, unit: &str) -> Result<(), PortError> {
        self.tick(format!("units.start {unit}"), PortKind::Units)?;
        let port = port_of(unit);
        let mut w = self.w.borrow_mut();
        // The unit's condition: a slot with no link does not start.
        if let Some(link) = w.links.get(&port).cloned()
            && !w.running.contains_key(&port)
        {
            w.running.insert(port, link);
            let lockless = w.running.values().any(|release| {
                !w.releases[release]
                    .1
                    .iter()
                    .any(|c| c == HANDOVER_CAPABILITY)
            });
            if w.running.len() > 1 && lockless {
                let note = format!("two instances run and one has no lock (started {unit})");
                w.errors.push(note);
            }
            if w.holder.is_none() {
                w.holder = Some(port);
            }
        }
        Ok(())
    }

    fn stop(&self, unit: &str) -> Result<(), PortError> {
        self.tick(format!("units.stop {unit}"), PortKind::Units)?;
        let port = port_of(unit);
        let mut w = self.w.borrow_mut();
        w.running.remove(&port);
        w.let_go(port);
        Ok(())
    }
}

impl Slots for Fake {
    fn target(&self, port: u16) -> Result<Option<PathBuf>, PortError> {
        self.tick(format!("slots.target {port}"), PortKind::Slots)?;
        Ok(self.w.borrow().links.get(&port).cloned())
    }

    fn point(&self, port: u16, release: &Path) -> Result<(), PortError> {
        self.tick(format!("slots.point {port}"), PortKind::Slots)?;
        self.w.borrow_mut().links.insert(port, release.to_owned());
        Ok(())
    }

    fn remove(&self, port: u16) -> Result<(), PortError> {
        self.tick(format!("slots.remove {port}"), PortKind::Slots)?;
        self.w.borrow_mut().links.remove(&port);
        Ok(())
    }
}

impl Instances for Fake {
    fn get(&self, port: u16, path: &str) -> Result<Reply, PortError> {
        self.tick(format!("get {port} {path}"), PortKind::Instances)?;
        if port == PUBLIC_PORT && self.w.borrow().public_503 {
            return Ok(Reply {
                status: 503,
                body: "busy".to_owned(),
            });
        }
        let mut w = self.w.borrow_mut();
        let target = if port == PUBLIC_PORT {
            w.rules.get(FORWARD_TABLE).and_then(|t| redirect_target(t))
        } else {
            Some(port)
        };
        let Some(target) = target.filter(|t| w.running.contains_key(t)) else {
            return Err(PortError::failed(PortKind::Instances, "connection refused"));
        };
        if path == "/readyz" && w.muted.contains(&target) {
            return Err(PortError::failed(PortKind::Instances, "timed out"));
        }
        let body = match path {
            "/readyz" => {
                if w.holder.is_none() {
                    match w.promotion {
                        Promotion::Instant => w.holder = Some(target),
                        Promotion::Stuck => {}
                        Promotion::AfterPolls(n) => {
                            w.free_polls += 1;
                            if w.free_polls >= n {
                                w.holder = Some(target);
                            }
                        }
                    }
                }
                if w.holder == Some(target) {
                    "ready read-write"
                } else {
                    "ready read-only"
                }
                .to_owned()
            }
            "/metricz" => {
                let running = w.running[&target].clone();
                let version = w
                    .lies
                    .get(&target)
                    .cloned()
                    .unwrap_or_else(|| w.releases[&running].0.clone());
                // The text the server renders, so that the fake and the server cannot drift.
                let slot = w.slot_label.then_some(target);
                crate::server::build_info::gauge(&version, "abc", slot)
            }
            _ => {
                return Ok(Reply {
                    status: 404,
                    body: String::new(),
                });
            }
        };
        Ok(Reply { status: 200, body })
    }
}

impl Host for Fake {
    fn declaration(&self) -> Result<String, PortError> {
        self.tick("host.declaration".to_owned(), PortKind::Host)?;
        Ok(self.w.borrow().declaration.clone())
    }

    fn boot_id(&self) -> Result<String, PortError> {
        self.tick("host.boot_id".to_owned(), PortKind::Host)?;
        Ok(format!("boot-{}", self.w.borrow().boot))
    }

    fn conntrack(&self) -> Result<Conntrack, PortError> {
        self.tick("host.conntrack".to_owned(), PortKind::Host)?;
        Ok(self.w.borrow().conntrack)
    }

    fn capabilities(&self, release: &Path) -> Result<Vec<String>, PortError> {
        self.tick(
            format!("host.capabilities {}", release.display()),
            PortKind::Host,
        )?;
        self.w
            .borrow()
            .releases
            .get(release)
            .map(|(_, caps)| caps.clone())
            .ok_or_else(|| PortError::failed(PortKind::Host, "unknown release"))
    }
}

impl Clock for Fake {
    fn now_ms(&self) -> u64 {
        self.w.borrow().now
    }

    fn pause(&self, span: Duration) {
        let mut w = self.w.borrow_mut();
        w.now += u64::try_from(span.as_millis()).unwrap_or(u64::MAX);
        w.pauses += 1;
    }
}

impl StateDir for Fake {
    fn lock(&self) -> Result<Lock, PortError> {
        self.tick("state.lock".to_owned(), PortKind::State)?;
        if self.w.borrow().busy {
            return Err(PortError::Busy);
        }
        self.locks.set(self.locks.get() + 1);
        Ok(Lock::new(Held(self.locks.clone())))
    }

    fn load(&self) -> Result<Option<Vec<u8>>, PortError> {
        self.tick("state.load".to_owned(), PortKind::State)?;
        Ok(self.w.borrow().record.clone())
    }

    fn save(&self, bytes: &[u8]) -> Result<(), PortError> {
        self.tick("state.save".to_owned(), PortKind::State)?;
        self.w.borrow_mut().record = Some(bytes.to_vec());
        Ok(())
    }
}

impl Fake {
    /// Runs `prepare` once, which the caller has arranged to be deferred.
    pub fn advance_to_deferral(&self) {
        let done = self.run(Step::Prepare);
        assert_eq!(done.outcome.code(), 75, "{done:?}");
    }
}
