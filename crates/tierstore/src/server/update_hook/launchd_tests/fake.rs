//! The fake of the launchd arm's ports, in one world: the one job of the system domain (loaded
//! or not, and the release it runs), the release link `current` that hostkeeper moves, the
//! node's configuration files, the record and its lock. The job runs the release `current` named
//! when it was loaded, and answers `/readyz` and `/metricz` on the public port from what runs.
//! The clock is a number that `pause` moves on; nothing sleeps.
//!
//! Every call of a port except the clock is counted and logged. `fail_at(n)` makes the n-th call
//! fail without effect (a failure the step sees and may recover from); `halt_at(n)` makes that
//! call and every later one fail (a crash: no recovery code of the step gets through);
//! `fail_late_at(n)` makes the n-th call fail after its effect. `fail_nth` and `fail_late_nth`
//! pick the n-th call whose log line starts with a prefix.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use crate::server::update_hook::launchd::run_step;
use crate::server::update_hook::{
    Clock, Done, Host, Instances, LAUNCHD_PLIST, Launchd, LaunchdPorts, Lock, Outcome, Params,
    PortError, PortKind, Reason, Record, Reply, StateDir, Step,
};

/// The label of the job, as the params name it.
pub const LABEL: &str = "com.komira.tierstore";
/// The node's configuration file, as the params name it.
pub const CONFIG: &str = "/usr/local/tierstore/etc/tierstore.toml";
/// The release that runs at first (0.3.0).
pub const OLD: &str = "/var/lib/hostkeeper/packages/tierstore/0.3.0+000000000003";
/// The release the first update installs (0.4.0).
pub const NEW: &str = "/var/lib/hostkeeper/packages/tierstore/0.4.0+000000000004";
/// The version of `OLD`.
pub const OLD_VERSION: &str = "0.3.0+000000000003";
/// The version of `NEW`.
pub const NEW_VERSION: &str = "0.4.0+000000000004";

/// A failure picked by what a call's log line starts with.
struct Fault {
    prefix: String,
    /// Which matching call fails, from 1.
    nth: usize,
    seen: usize,
    late: bool,
}

struct World {
    record: Option<Vec<u8>>,
    busy: bool,
    current: Option<PathBuf>,
    job: Option<PathBuf>,
    versions: BTreeMap<PathBuf, String>,
    lies: BTreeMap<PathBuf, String>,
    mute: BTreeSet<PathBuf>,
    read_only: bool,
    configs: BTreeMap<PathBuf, String>,
    strays: Vec<String>,
    now: u64,
    pauses: u32,
    log: Vec<String>,
    calls: usize,
    fail_at: Option<usize>,
    halt_from: Option<usize>,
    late_at: Option<usize>,
    fault: Option<Fault>,
}

/// What the world looks like, without the counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub record: Option<Record>,
    pub current: Option<PathBuf>,
    pub job: Option<PathBuf>,
}

/// Counts the locks that are held; the lock is released when this is dropped.
struct Held(Rc<Cell<u32>>);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

/// The world, the params of its update, and every port over the world.
pub struct Fake {
    w: RefCell<World>,
    locks: Rc<Cell<u32>>,
    first: bool,
}

/// The params of an update to `release` (`version`), from `previous` when there is one.
pub fn params_for(release: &str, version: &str, previous: Option<(&str, &str)>) -> Params {
    let mut text = format!(
        "service_manager = \"launchd\"\n\
         launchd_label = \"{LABEL}\"\n\
         config_file = \"{CONFIG}\"\n\
         \"updater.staged_release\" = \"{release}\"\n\
         \"updater.target_version\" = \"{version}\"\n"
    );
    if let Some((release, version)) = previous {
        text.push_str(&format!(
            "\"updater.previous_release\" = \"{release}\"\n\
             \"updater.previous_version\" = \"{version}\"\n"
        ));
    }
    Params::parse(&text).expect("params")
}

impl Fake {
    fn world(record: Option<Vec<u8>>, current: Option<&str>, job: Option<&str>) -> World {
        World {
            record,
            busy: false,
            current: current.map(PathBuf::from),
            job: job.map(PathBuf::from),
            versions: BTreeMap::from([
                (PathBuf::from(OLD), OLD_VERSION.to_owned()),
                (PathBuf::from(NEW), NEW_VERSION.to_owned()),
            ]),
            lies: BTreeMap::new(),
            mute: BTreeSet::new(),
            read_only: false,
            configs: BTreeMap::from([(PathBuf::from(CONFIG), "[server]\n".to_owned())]),
            strays: Vec::new(),
            now: 0,
            pauses: 0,
            log: Vec::new(),
            calls: 0,
            fail_at: None,
            halt_from: None,
            late_at: None,
            fault: None,
        }
    }

    /// A host that runs `OLD` under the job, with nothing in flight; `NEW` is staged.
    pub fn new() -> Fake {
        Fake {
            w: RefCell::new(Fake::world(
                Some(Record::idle(None).encode()),
                Some(OLD),
                Some(OLD),
            )),
            locks: Rc::new(Cell::new(0)),
            first: false,
        }
    }

    /// A host with nothing installed: no record, no release linked, no job; `NEW` is staged.
    pub fn first_install() -> Fake {
        Fake {
            w: RefCell::new(Fake::world(None, None, None)),
            locks: Rc::new(Cell::new(0)),
            first: true,
        }
    }

    /// The ports over this world.
    pub fn ports(&self) -> LaunchdPorts<'_> {
        LaunchdPorts {
            launchd: self,
            instances: self,
            host: self,
            clock: self,
            state: self,
        }
    }

    /// The params of the update to `NEW`: from `OLD`, or of a first install.
    pub fn params(&self) -> Params {
        let previous = (!self.first).then_some((OLD, OLD_VERSION));
        params_for(NEW, NEW_VERSION, previous)
    }

    /// Runs `step` as it is, with the params of this host's update.
    pub fn run(&self, step: Step) -> Done {
        self.run_with(step, &self.params(), LABEL)
    }

    /// Runs `step` with `params` and the job `label`.
    pub fn run_with(&self, step: Step, params: &Params, label: &str) -> Done {
        run_step(step, params, label, &self.ports())
    }

    /// Runs `step` as hostkeeper does: `current` is moved to the staged release before
    /// `cutover` and back to the previous one before `rollback`.
    pub fn host_run(&self, step: Step) -> Done {
        match step {
            Step::Cutover => self.switch_current(NEW),
            Step::Rollback if !self.first => self.switch_current(OLD),
            _ => {}
        }
        self.run(step)
    }

    /// Runs `steps` in order as hostkeeper does, each of which must succeed.
    pub fn advance(&self, steps: &[Step]) {
        for step in steps {
            let done = self.host_run(*step);
            assert_eq!(done.outcome.code(), 0, "{step}: {done:?}");
        }
    }

    /// What the world looks like.
    pub fn snapshot(&self) -> Snapshot {
        let w = self.w.borrow();
        Snapshot {
            record: w
                .record
                .as_ref()
                .map(|b| Record::decode(b).expect("a record")),
            current: w.current.clone(),
            job: w.job.clone(),
        }
    }

    /// The record on disk.
    pub fn record(&self) -> Record {
        self.snapshot().record.expect("a record")
    }

    /// Every call so far, in order.
    pub fn log(&self) -> Vec<String> {
        self.w.borrow().log.clone()
    }

    /// The calls to the launchd domain so far, in order.
    pub fn launchd_calls(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|call| call.starts_with("launchd."))
            .collect()
    }

    /// The calls of the launchd domain that named a job or a plist other than the job's.
    pub fn strays(&self) -> Vec<String> {
        self.w.borrow().strays.clone()
    }

    /// How many calls so far.
    pub fn calls(&self) -> usize {
        self.w.borrow().calls
    }

    /// How many pauses so far.
    pub fn pauses(&self) -> u32 {
        self.w.borrow().pauses
    }

    /// The locks held now.
    pub fn live_locks(&self) -> u32 {
        self.locks.get()
    }

    /// The n-th call from now fails without effect.
    pub fn fail_at(&self, n: usize) {
        let mut w = self.w.borrow_mut();
        w.fail_at = Some(w.calls + n);
    }

    /// The world stops from the n-th call on: that call and every later one fail without effect.
    pub fn halt_at(&self, n: usize) {
        let mut w = self.w.borrow_mut();
        w.halt_from = Some(w.calls + n);
    }

    /// The n-th call from now fails after its effect (a start, a stop or a record written has
    /// happened, and the step is told it failed). A call that changes nothing just fails.
    pub fn fail_late_at(&self, n: usize) {
        let mut w = self.w.borrow_mut();
        w.late_at = Some(w.calls + n);
    }

    /// The `nth` call (from 1) from now whose log line starts with `prefix`
    /// fails without effect.
    pub fn fail_nth(&self, prefix: &str, nth: usize) {
        self.set_fault(prefix, nth, false);
    }

    /// The `nth` call from now whose log line starts with `prefix` fails after its effect.
    pub fn fail_late_nth(&self, prefix: &str, nth: usize) {
        self.set_fault(prefix, nth, true);
    }

    fn set_fault(&self, prefix: &str, nth: usize, late: bool) {
        self.w.borrow_mut().fault = Some(Fault {
            prefix: prefix.to_owned(),
            nth,
            seen: 0,
            late,
        });
    }

    /// Every call goes through again.
    pub fn heal(&self) {
        let mut w = self.w.borrow_mut();
        w.fail_at = None;
        w.halt_from = None;
        w.late_at = None;
        w.fault = None;
    }

    /// The job of `release` reports `version` instead of its own.
    pub fn lie(&self, release: &str, version: &str) {
        self.w
            .borrow_mut()
            .lies
            .insert(PathBuf::from(release), version.to_owned());
    }

    /// The job of `release` never answers.
    pub fn mute(&self, release: &str) {
        self.w.borrow_mut().mute.insert(PathBuf::from(release));
    }

    /// Whether the job answers `/readyz` as a build node that has not taken the cache lock.
    pub fn set_read_only(&self, read_only: bool) {
        self.w.borrow_mut().read_only = read_only;
    }

    /// Whether another step holds the lock.
    pub fn set_busy(&self, busy: bool) {
        self.w.borrow_mut().busy = busy;
    }

    /// The bytes of the record on disk.
    pub fn set_record_bytes(&self, bytes: Option<&[u8]>) {
        self.w.borrow_mut().record = bytes.map(<[u8]>::to_vec);
    }

    /// The record on disk.
    pub fn set_record(&self, record: &Record) {
        self.set_record_bytes(Some(&record.encode()));
    }

    /// The configuration file at `path`: `Some(text)` or none at all.
    pub fn set_config(&self, path: &str, text: Option<&str>) {
        let mut w = self.w.borrow_mut();
        match text {
            Some(text) => w.configs.insert(PathBuf::from(path), text.to_owned()),
            None => w.configs.remove(Path::new(path)),
        };
    }

    /// The job is loaded, running `release`, or is not loaded.
    pub fn set_job(&self, release: Option<&str>) {
        self.w.borrow_mut().job = release.map(PathBuf::from);
    }

    /// hostkeeper moves the link `current`.
    pub fn switch_current(&self, release: &str) {
        self.w.borrow_mut().current = Some(PathBuf::from(release));
    }

    /// The host has booted, and launchd has not loaded the job yet.
    pub fn reboot(&self) {
        self.w.borrow_mut().job = None;
    }

    /// Counts one call and fails it if it is the chosen one.
    fn tick(&self, what: String, kind: PortKind) -> Result<(), PortError> {
        if self.tick_changing(what, kind)? {
            return Err(PortError::failed(kind, "injected failure"));
        }
        Ok(())
    }

    /// Counts one call that changes something: fails it before its effect if it is the chosen
    /// one, and says whether it is to fail after its effect.
    fn tick_changing(&self, what: String, kind: PortKind) -> Result<bool, PortError> {
        let mut w = self.w.borrow_mut();
        w.calls += 1;
        let call = w.calls;
        let picked = match &mut w.fault {
            Some(fault) if what.starts_with(&fault.prefix) => {
                fault.seen += 1;
                (fault.seen == fault.nth).then_some(fault.late)
            }
            _ => None,
        };
        w.log.push(what);
        if w.fail_at == Some(call)
            || w.halt_from.is_some_and(|from| call >= from)
            || picked == Some(false)
        {
            Err(PortError::failed(kind, "injected failure"))
        } else {
            Ok(w.late_at == Some(call) || picked == Some(true))
        }
    }

    /// The failure of a call whose effect has happened, if it is the chosen one.
    fn after(late: bool, kind: PortKind) -> Result<(), PortError> {
        if late {
            Err(PortError::failed(kind, "injected failure after its effect"))
        } else {
            Ok(())
        }
    }

    /// Notes a call that named something other than the job.
    fn stray(&self, what: &str) {
        self.w.borrow_mut().strays.push(what.to_owned());
    }
}

impl Launchd for Fake {
    fn loaded(&self, label: &str) -> Result<bool, PortError> {
        self.tick(format!("launchd.loaded {label}"), PortKind::Launchd)?;
        if label != LABEL {
            self.stray(label);
        }
        Ok(label == LABEL && self.w.borrow().job.is_some())
    }

    fn bootstrap(&self, plist: &Path) -> Result<(), PortError> {
        let late = self.tick_changing(
            format!("launchd.bootstrap {}", plist.display()),
            PortKind::Launchd,
        )?;
        if plist != Path::new(LAUNCHD_PLIST) {
            self.stray(&plist.display().to_string());
        }
        let mut w = self.w.borrow_mut();
        if w.job.is_some() {
            return Err(PortError::failed(
                PortKind::Launchd,
                "Bootstrap failed: 5: Input/output error (the job is loaded)",
            ));
        }
        let current = w.current.clone();
        w.job = current;
        drop(w);
        Fake::after(late, PortKind::Launchd)
    }

    fn bootout(&self, label: &str) -> Result<(), PortError> {
        let late = self.tick_changing(format!("launchd.bootout {label}"), PortKind::Launchd)?;
        if label == LABEL {
            self.w.borrow_mut().job = None;
        } else {
            self.stray(label);
        }
        Fake::after(late, PortKind::Launchd)
    }
}

impl Instances for Fake {
    fn get(&self, port: u16, path: &str) -> Result<Reply, PortError> {
        self.tick(format!("get {port} {path}"), PortKind::Instances)?;
        let w = self.w.borrow();
        let refused = || PortError::failed(PortKind::Instances, "connection refused");
        let running = w
            .job
            .as_ref()
            .filter(|_| port == 9300)
            .ok_or_else(refused)?;
        if w.mute.contains(running) {
            return Err(PortError::failed(PortKind::Instances, "timed out"));
        }
        let body = match path {
            "/readyz" if w.read_only => "ready read-only".to_owned(),
            "/readyz" => "ready read-write".to_owned(),
            "/metricz" => {
                let version = w
                    .lies
                    .get(running)
                    .or_else(|| w.versions.get(running))
                    .cloned()
                    .unwrap_or_default();
                let (crate_version, commit) = version
                    .split_once('+')
                    .unwrap_or((version.as_str(), "none"));
                // The text the server renders, so that the fake and the server cannot drift.
                crate::server::build_info::gauge(crate_version, commit, None)
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
    fn config(&self, path: &Path) -> Result<Option<String>, PortError> {
        self.tick(format!("host.config {}", path.display()), PortKind::Host)?;
        Ok(self.w.borrow().configs.get(path).cloned())
    }

    // A launchd step runs no release to ask: this fails, and is logged, so that a step that
    // asked would fail and show it in the log.
    fn capabilities(&self, release: &Path) -> Result<Vec<String>, PortError> {
        self.tick(
            format!("host.capabilities {}", release.display()),
            PortKind::Host,
        )?;
        Err(PortError::failed(
            PortKind::Host,
            "no release is run by a launchd step",
        ))
    }

    fn check_config(&self, release: &Path, config: &Path) -> Result<Option<String>, PortError> {
        self.no_check(release, config)
    }
}

impl Fake {
    // A launchd step runs no release to ask, here as for the capabilities.
    fn no_check(&self, release: &Path, config: &Path) -> Result<Option<String>, PortError> {
        self.tick(
            format!(
                "host.check_config {} {}",
                release.display(),
                config.display()
            ),
            PortKind::Host,
        )?;
        Err(PortError::failed(
            PortKind::Host,
            "no release is run by a launchd step",
        ))
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
        let late = self.tick_changing("state.save".to_owned(), PortKind::State)?;
        self.w.borrow_mut().record = Some(bytes.to_vec());
        Fake::after(late, PortKind::State)
    }
}

/// Asserts that `done` is a failure (exit 1) for `reason`.
pub fn assert_failed(done: &Done, reason: Reason) {
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(reason), "{done:?}");
}

/// Asserts that `done` is a success (exit 0).
pub fn assert_ok(done: &Done) {
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
}

/// `release` as the path a record holds.
pub fn path(release: &str) -> Option<PathBuf> {
    Some(PathBuf::from(release))
}

/// The record of the update from `OLD` to `NEW` that `prepare` lays down; a first install has no
/// `from`.
pub fn in_flight(first: bool) -> Record {
    Record {
        updating: true,
        from: if first { None } else { path(OLD) },
        to: path(NEW),
        version: Some(NEW_VERSION.to_owned()),
        ..Record::idle(None)
    }
}
