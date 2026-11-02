//! The fakes of the single-instance ports, in one world: a host with one service under launchd,
//! the release links `current` and `previous`, and the state directory, in memory. The service
//! runs the release `current` named when it was started, and answers `/readyz` and `/metricz` on
//! the one port from what runs. The clock is a number that `pause` moves on; nothing sleeps.
//!
//! Every call of a port except the clock is counted and logged. `fail_at(n)` makes the n-th call
//! fail without effect (a failure the step sees and may recover from); `halt_at(n)` makes that
//! call and every later one fail (a crash: no recovery code of the step gets through).

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use super::Step;
use super::params::Params;
use super::ports::{
    Clock, Conntrack, Host, Instances, Link, Lock, PortError, PortKind, Releases, Reply, Service,
    SinglePorts, StateDir,
};
use super::report::Done;
use super::single::{SingleRecord, run_step};

/// The port the one instance serves on.
pub const PORT: u16 = 9300;

/// The declaration the fake host holds.
pub const DECLARATION: &str = "uid = \"_tierstore\"\n\
state_dir = \"/var/db/updater/hooks/tierstore\"\n\
health_ports = [9300]\n\
[hook]\n\
service = \"com.komira.tierstore\"\n";

/// The release that runs at first.
pub const OLD: &str = "/usr/local/tierstore/releases/old";
/// The release the first update installs.
pub const NEW: &str = "/usr/local/tierstore/releases/new";
/// The release a second update installs.
pub const NEWER: &str = "/usr/local/tierstore/releases/newer";

/// How the fake's instance takes the cache lock once started.
#[derive(Clone, Copy, Debug)]
pub enum Promotion {
    /// At once.
    Instant,
    /// Never.
    Stuck,
}

/// What the world looks like, without the counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub record: Option<SingleRecord>,
    pub current: Option<PathBuf>,
    pub previous: Option<PathBuf>,
    pub running: Option<PathBuf>,
}

struct World {
    record: Option<Vec<u8>>,
    declaration: String,
    busy: bool,
    current: Option<PathBuf>,
    previous: Option<PathBuf>,
    running: Option<PathBuf>,
    versions: BTreeMap<PathBuf, String>,
    lies: BTreeMap<PathBuf, String>,
    mute: BTreeSet<PathBuf>,
    promotion: Promotion,
    now: u64,
    pauses: u32,
    starts: u32,
    log: Vec<String>,
    calls: usize,
    fail_at: Option<usize>,
    halt_from: Option<usize>,
}

impl World {
    /// The link `link`, to read or to set.
    fn link(&mut self, link: Link) -> &mut Option<PathBuf> {
        match link {
            Link::Current => &mut self.current,
            Link::Previous => &mut self.previous,
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
pub struct SingleFake {
    w: RefCell<World>,
    locks: Rc<Cell<u32>>,
}

impl SingleFake {
    /// A host whose service runs `OLD` (version 0.3.0) with nothing in flight; `NEW` (0.4.0) and
    /// `NEWER` (0.5.0) are staged.
    pub fn new() -> SingleFake {
        let versions = BTreeMap::from([
            (PathBuf::from(OLD), "0.3.0".to_owned()),
            (PathBuf::from(NEW), "0.4.0".to_owned()),
            (PathBuf::from(NEWER), "0.5.0".to_owned()),
        ]);
        SingleFake {
            w: RefCell::new(World {
                record: Some(SingleRecord::idle().encode()),
                declaration: DECLARATION.to_owned(),
                busy: false,
                current: Some(PathBuf::from(OLD)),
                previous: None,
                running: Some(PathBuf::from(OLD)),
                versions,
                lies: BTreeMap::new(),
                mute: BTreeSet::new(),
                promotion: Promotion::Instant,
                now: 0,
                pauses: 0,
                starts: 0,
                log: Vec::new(),
                calls: 0,
                fail_at: None,
                halt_from: None,
            }),
            locks: Rc::new(Cell::new(0)),
        }
    }

    /// The ports over this world.
    pub fn ports(&self) -> SinglePorts<'_> {
        SinglePorts {
            service: self,
            releases: self,
            instances: self,
            host: self,
            clock: self,
            state: self,
        }
    }

    /// The params of an update to `NEW`, with `promotion_bound_secs` at 30.
    pub fn params(&self) -> Params {
        params_for(NEW, "0.4.0")
    }

    /// Runs `step` as an update to `NEW`.
    pub fn run(&self, step: Step) -> Done {
        run_step(step, Some(&self.params()), &self.ports())
    }

    /// Runs `step` with `params`.
    pub fn run_with(&self, step: Step, params: Option<&Params>) -> Done {
        run_step(step, params, &self.ports())
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
        Snapshot {
            record: w
                .record
                .as_ref()
                .map(|b| SingleRecord::decode(b).expect("a record")),
            current: w.current.clone(),
            previous: w.previous.clone(),
            running: w.running.clone(),
        }
    }

    /// The record on disk.
    pub fn record(&self) -> SingleRecord {
        self.snapshot().record.expect("a record")
    }

    /// Every call so far, in order.
    pub fn log(&self) -> Vec<String> {
        self.w.borrow().log.clone()
    }

    /// How many calls so far.
    pub fn calls(&self) -> usize {
        self.w.borrow().calls
    }

    /// How many pauses so far.
    pub fn pauses(&self) -> u32 {
        self.w.borrow().pauses
    }

    /// How many times a stopped service was started.
    pub fn starts(&self) -> u32 {
        self.w.borrow().starts
    }

    /// The locks held now.
    pub fn live_locks(&self) -> u32 {
        self.locks.get()
    }

    /// The n-th call from now fails without effect; `None` lets every call through.
    pub fn fail_at(&self, n: Option<usize>) {
        let mut w = self.w.borrow_mut();
        w.fail_at = n.map(|n| w.calls + n);
    }

    /// The world stops from the n-th call on: that call and every later one fail without effect.
    pub fn halt_at(&self, n: Option<usize>) {
        let mut w = self.w.borrow_mut();
        w.halt_from = n.map(|n| w.calls + n);
    }

    /// The instance of `release` reports `version` instead of its own.
    pub fn lie(&self, release: &str, version: &str) {
        self.w
            .borrow_mut()
            .lies
            .insert(PathBuf::from(release), version.to_owned());
    }

    /// The instance of `release` never answers.
    pub fn mute(&self, release: &str) {
        self.w.borrow_mut().mute.insert(PathBuf::from(release));
    }

    pub fn set_promotion(&self, promotion: Promotion) {
        self.w.borrow_mut().promotion = promotion;
    }

    pub fn set_busy(&self, busy: bool) {
        self.w.borrow_mut().busy = busy;
    }

    pub fn set_declaration(&self, text: &str) {
        text.clone_into(&mut self.w.borrow_mut().declaration);
    }

    pub fn set_record_bytes(&self, bytes: Option<&[u8]>) {
        self.w.borrow_mut().record = bytes.map(<[u8]>::to_vec);
    }

    pub fn set_record(&self, record: &SingleRecord) {
        self.set_record_bytes(Some(&record.encode()));
    }

    pub fn set_link(&self, link: Link, release: Option<&str>) {
        *self.w.borrow_mut().link(link) = release.map(PathBuf::from);
    }

    /// The host has booted, and launchd has not started the service yet.
    pub fn reboot(&self) {
        self.w.borrow_mut().running = None;
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

/// The params of an update to `release`, which is `version`.
pub fn params_for(release: &str, version: &str) -> Params {
    Params::parse(&format!(
        "promotion_bound_secs = 30\n\
         mode = \"restart\"\n\
         \"updater.staged_release\" = \"{release}\"\n\
         \"updater.target_version\" = \"{version}\"\n"
    ))
    .expect("params")
}

impl Service for SingleFake {
    fn start(&self) -> Result<(), PortError> {
        self.tick("service.start".to_owned(), PortKind::Units)?;
        let mut w = self.w.borrow_mut();
        if w.running.is_none() {
            w.running = w.current.clone();
            w.starts += 1;
        }
        Ok(())
    }

    fn stop(&self) -> Result<(), PortError> {
        self.tick("service.stop".to_owned(), PortKind::Units)?;
        self.w.borrow_mut().running = None;
        Ok(())
    }
}

impl Releases for SingleFake {
    fn target(&self, link: Link) -> Result<Option<PathBuf>, PortError> {
        self.tick(format!("links.target {}", link.name()), PortKind::Slots)?;
        Ok(self.w.borrow_mut().link(link).clone())
    }

    fn point(&self, link: Link, release: &Path) -> Result<(), PortError> {
        self.tick(
            format!("links.point {} {}", link.name(), release.display()),
            PortKind::Slots,
        )?;
        *self.w.borrow_mut().link(link) = Some(release.to_owned());
        Ok(())
    }

    fn remove(&self, link: Link) -> Result<(), PortError> {
        self.tick(format!("links.remove {}", link.name()), PortKind::Slots)?;
        *self.w.borrow_mut().link(link) = None;
        Ok(())
    }
}

impl Instances for SingleFake {
    fn get(&self, port: u16, path: &str) -> Result<Reply, PortError> {
        self.tick(format!("get {port} {path}"), PortKind::Instances)?;
        let w = self.w.borrow();
        let refused = || PortError::failed(PortKind::Instances, "connection refused");
        let running = w
            .running
            .as_ref()
            .filter(|_| port == PORT)
            .ok_or_else(refused)?;
        if w.mute.contains(running) {
            return Err(PortError::failed(PortKind::Instances, "timed out"));
        }
        let body = match path {
            "/readyz" => match w.promotion {
                Promotion::Instant => "ready read-write",
                Promotion::Stuck => "ready read-only",
            }
            .to_owned(),
            "/metricz" => {
                let version = w
                    .lies
                    .get(running)
                    .or_else(|| w.versions.get(running))
                    .cloned()
                    .unwrap_or_default();
                // The text the server renders, so that the fake and the server cannot drift.
                crate::server::build_info::gauge(&version, "abc", Some(PORT))
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

impl Host for SingleFake {
    fn declaration(&self) -> Result<String, PortError> {
        self.tick("host.declaration".to_owned(), PortKind::Host)?;
        Ok(self.w.borrow().declaration.clone())
    }

    // A single-instance step asks the host for nothing but its declaration: these three fail,
    // and are logged, so that a step that asked would fail and show it in the log.
    fn boot_id(&self) -> Result<String, PortError> {
        self.tick("host.boot_id".to_owned(), PortKind::Host)?;
        Err(PortError::failed(PortKind::Host, "no boot on this host"))
    }

    fn conntrack(&self) -> Result<Conntrack, PortError> {
        self.tick("host.conntrack".to_owned(), PortKind::Host)?;
        Err(PortError::failed(
            PortKind::Host,
            "no conntrack on this host",
        ))
    }

    fn capabilities(&self, release: &Path) -> Result<Vec<String>, PortError> {
        self.tick(
            format!("host.capabilities {}", release.display()),
            PortKind::Host,
        )?;
        Err(PortError::failed(
            PortKind::Host,
            "no capabilities asked on this host",
        ))
    }
}

impl Clock for SingleFake {
    fn now_ms(&self) -> u64 {
        self.w.borrow().now
    }

    fn pause(&self, span: Duration) {
        let mut w = self.w.borrow_mut();
        w.now += u64::try_from(span.as_millis()).unwrap_or(u64::MAX);
        w.pauses += 1;
    }
}

impl StateDir for SingleFake {
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
