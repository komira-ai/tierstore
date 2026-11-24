//! The fake machine of the systemd arm: both instances, their start markers, the copy of a
//! release's binary the units run, the node's configuration, the releases' capabilities, the
//! record and the clock, in one world in memory. hostkeeper's link `current` is not in it: the
//! hook never reads the link, only the releases the params name.
//!
//! `start(x)` models systemd and the binary: without the marker the template's
//! `ConditionPathExists=` names (read from the packaged unit file) the unit stays inactive;
//! otherwise the instance runs the program of the template's `ExecStart=` as its `User=`. When
//! that program is [`SERVICE_BINARY`], it runs the release whose binary the hook put there
//! (`install_binary`); with none there, or with any other program (the binary inside a release,
//! which hostkeeper keeps closed to every user but root), the unit fails. When the other instance serves, a
//! hand-over (the configuration names `server.handoff_socket` and both releases have the
//! capabilities, and for a `[local_cache]` table the cache one too) makes the new one serve and the old one drain, which is running without
//! serving for a chosen number of `state` looks and then inactive (exit 0, not restarted); with
//! no hand-over the new instance cannot bind and fails. `set_takeover_looks(n)` makes a hand-over
//! slow: the new instance runs without serving, the old one answering, for `n` looks at `/readyz`
//! (a stop of either during them is not modelled). The instances answer `/readyz` and
//! `/metricz` of port 9300 from the serving one. The clock is a number that `pause` moves on.
//!
//! Every call of a port except the clock and `Markers::present` is counted and logged.
//! `halt_at(n)` makes that call and every later one fail without effect (a crash);
//! `fail_on(prefix)` makes the calls whose log line starts with `prefix` fail without effect,
//! and `late_on(prefix)` makes them fail after their effect.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use crate::server::build_info::gauge;
use crate::server::update_hook::systemd::run_step;
use crate::server::update_hook::{
    CACHE_HANDOFF_CAPABILITY, Clock, Done, HANDOFF_CAPABILITY, HANDOFF_STATE_DIR_CAPABILITY, Host,
    Instance, Instances, LOCAL_CACHE_TABLE, Lock, Markers, Outcome, Params, PortError, PortKind,
    Reason, Record, Reply, SERVICE_BINARY, StateDir, Step, SystemdPorts, UnitState, Units,
    marker_path,
};

/// The release that runs at first.
pub const OLD: &str = "/var/lib/hostkeeper/packages/tierstore/0.3.0";
/// The release the first update installs.
pub const NEW: &str = "/var/lib/hostkeeper/packages/tierstore/0.4.0";
/// The release a second update installs.
pub const NEWER: &str = "/var/lib/hostkeeper/packages/tierstore/0.5.0";
/// The node's configuration file.
pub const CONFIG: &str = "/etc/tierstore/config.toml";
/// A configuration that names the hand-over socket.
pub const WITH_SOCKET: &str = "[server]\nhandoff_socket = \"/var/lib/tierstore/handoff.sock\"\n";
/// A configuration of a build node or a storage node with a cache that names the hand-over
/// socket.
pub const WITH_CACHE_SOCKET: &str = "[server]\nhandoff_socket = \"/var/lib/tierstore/handoff.sock\"\n\n[local_cache]\npath = \"/var/lib/tierstore/cache\"\n";
/// A configuration that leaves the hand-over socket out: its default is the socket.
pub const WITHOUT_SOCKET: &str = "[server]\nproxy_id = \"p1\"\n";
/// A configuration with a directory as the hot tier, and the hand-over socket named.
pub const DIR_HOT: &str = "[server]\nhandoff_socket = \"/var/lib/tierstore/handoff.sock\"\n\n[hot_store]\nkind = \"dir\"\npath = \"/var/lib/tierstore/hot\"\n";
/// The commit of every release's build.
const COMMIT: &str = "0123456789ab";
/// The unit file the package ships.
const TEMPLATE: &str =
    include_str!("../../../../../../packaging/hostkeeper/units/tierstore@.service");

/// The path the unit file's `ConditionPathExists=` names, with `%i` as the file spells it.
pub fn unit_condition() -> &'static str {
    TEMPLATE
        .lines()
        .find_map(|line| line.strip_prefix("ConditionPathExists="))
        .expect("the unit file has a ConditionPathExists")
}

/// The program of the unit file's `ExecStart=`.
pub fn unit_program() -> &'static str {
    TEMPLATE
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart="))
        .and_then(|command| command.split_whitespace().next())
        .expect("the unit file has an ExecStart")
}

/// The version of a release: its directory is named after the crate version.
pub fn version_of(release: &str) -> String {
    let name = release.rsplit('/').next().expect("a release has a name");
    format!("{name}+{COMMIT}")
}

/// The params of an update to `staged` (a first install when `previous` is `None`), with the
/// optional lines `optional`.
pub fn params_with(staged: &str, previous: Option<&str>, optional: &str) -> Params {
    Params::parse(&params_text(staged, previous, optional)).expect("params")
}

/// The text of the params file of [`params_with`], as hostkeeper writes it.
pub fn params_text(staged: &str, previous: Option<&str>, optional: &str) -> String {
    let mut text = format!(
        "\"updater.staged_release\" = \"{staged}\"\n\"updater.target_version\" = \"{}\"\n",
        version_of(staged)
    );
    if let Some(previous) = previous {
        text.push_str(&format!(
            "\"updater.previous_release\" = \"{previous}\"\n\
             \"updater.previous_version\" = \"{}\"\n",
            version_of(previous)
        ));
    }
    text.push_str(optional);
    text
}

/// The params of an update to `staged`, with the node's configuration named.
pub fn params(staged: &str, previous: Option<&str>) -> Params {
    params_with(staged, previous, &format!("config_file = \"{CONFIG}\"\n"))
}

/// The words a step says of what it did.
pub fn detail(done: &Done) -> &str {
    done.report.detail.as_deref().unwrap_or_default()
}

/// Asserts that the step succeeded.
pub fn assert_ok(done: &Done) {
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
}

/// Asserts that the step failed (exit 1) with `reason`.
pub fn assert_failed(done: &Done, reason: Reason) {
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(reason), "{done:?}");
}

/// Asserts that the step deferred (exit 75) with `reason`, to be run again after `retry` seconds.
pub fn assert_deferred(done: &Done, reason: Reason, retry: u64) {
    assert_eq!(done.outcome, Outcome::Deferred, "{done:?}");
    assert_eq!(done.report.reason, Some(reason), "{done:?}");
    assert_eq!(done.report.retry_after_secs, Some(retry), "{done:?}");
}

/// What the world looks like, without the counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub record: Option<Record>,
    pub units: Vec<(Instance, UnitState, Option<PathBuf>)>,
    pub markers: Vec<Instance>,
    /// The units and markers of `tierstore-fs`.
    pub fs_units: Vec<(Instance, UnitState, Option<PathBuf>)>,
    pub fs_markers: Vec<Instance>,
    /// The release whose binary is at [`SERVICE_BINARY`].
    pub installed: Option<PathBuf>,
}

#[path = "fake_fs.rs"]
mod fake_fs;
pub use fake_fs::{FsTake, fs_marker_path, fs_unit_condition, fs_unit_program};

#[derive(Clone)]
struct Unit {
    state: UnitState,
    release: Option<PathBuf>,
    serving: bool,
    /// While draining: the `state` looks that still see it running.
    left: u32,
}

impl Unit {
    fn off() -> Unit {
        Unit {
            state: UnitState::Inactive,
            release: None,
            serving: false,
            left: 0,
        }
    }
}

struct World {
    units: BTreeMap<Instance, Unit>,
    markers: BTreeSet<PathBuf>,
    /// The release whose binary the hook put at [`SERVICE_BINARY`].
    installed: Option<PathBuf>,
    config: Option<String>,
    capabilities: BTreeMap<PathBuf, Vec<String>>,
    refusals: BTreeMap<PathBuf, String>,
    lies: BTreeMap<PathBuf, String>,
    mute: BTreeSet<PathBuf>,
    read_only: bool,
    /// The cache lock as the server holds it, when a test asks for it
    /// ([`SystemdFake::hold_cache_lock`]); `read_only` then says nothing.
    cache_lock: Option<CacheLock>,
    record: Option<Vec<u8>>,
    busy: bool,
    now: u64,
    log: Vec<String>,
    calls: usize,
    halt_from: Option<usize>,
    fail_prefix: Option<String>,
    late_prefix: Option<String>,
    drain_looks: u32,
    /// How many of the next looks at `/readyz` the old instance answers, still draining on the
    /// same sockets.
    draining_looks: u32,
    /// How many looks at `/readyz` a new instance of a hand-over takes to open its cache and
    /// take the sockets; the old instance answers them.
    takeover_looks: u32,
    /// A hand-over under way: the new instance, the old one, and the looks left before the new
    /// one takes the sockets.
    taking: Option<(Instance, Instance, u32)>,
    /// The second unit, `tierstore-fs`: see [`fake_fs`].
    fs: fake_fs::FsWorld,
}

/// The lock of the local cache, as the server takes it: one instance holds it until its process
/// ends, and an instance that serves without it is read-only. It is free once the holder's unit
/// runs no more, and the instance that serves then takes it after `promote_secs` seconds of the
/// clock (its next tick and a scan of the cache; at the first second for 0). The clock alone moves it: a pause of a second also
/// counts down the drain of each old instance, as the `state` looks do.
#[derive(Clone, Copy)]
struct CacheLock {
    holder: Option<Instance>,
    promote_secs: u32,
    /// The seconds still to go of the scan of an instance that found the lock free.
    promoting: Option<u32>,
}

impl World {
    /// The instance that answers on the public port.
    fn serving(&self) -> Option<Instance> {
        Instance::ALL.into_iter().find(|i| {
            let unit = &self.units[i];
            unit.serving && unit.state == UnitState::Active
        })
    }

    /// Whether the store that answers says `ready read-only`.
    pub(super) fn store_read_only(&self) -> bool {
        match self.cache_lock {
            Some(lock) => lock.holder.is_none() || lock.holder != self.serving(),
            None => self.read_only,
        }
    }

    /// A second of the clock under the cache lock: the old instances drain on, a holder whose
    /// unit runs no more lets the lock go, and the instance that serves scans its cache and takes
    /// it.
    fn tick_second(&mut self) {
        let Some(mut lock) = self.cache_lock else {
            return;
        };
        for unit in self.units.values_mut() {
            if unit.state == UnitState::Deactivating {
                if unit.left == 0 {
                    *unit = Unit::off();
                } else {
                    unit.left -= 1;
                }
            }
        }
        if lock.holder.is_some_and(|h| !self.units[&h].state.running()) {
            lock.holder = None;
        }
        if lock.holder.is_none() {
            let left = lock
                .promoting
                .unwrap_or(lock.promote_secs)
                .saturating_sub(1);
            lock.promoting = None;
            match self.serving() {
                Some(taker) if left == 0 => lock.holder = Some(taker),
                Some(_) => lock.promoting = Some(left),
                None => {}
            }
        }
        self.cache_lock = Some(lock);
    }

    /// The path of the marker the unit file's condition names for `instance`.
    fn condition_path(instance: Instance) -> PathBuf {
        PathBuf::from(unit_condition().replace("%i", instance.letter()))
    }

    /// Whether the hand-over works between the releases of the two instances.
    fn can_hand_over(&self, old: Option<&Path>, new: &Path) -> bool {
        // A configuration with a `[local_cache]` table is a cache shape, which hands over only
        // between releases that list the cache word too.
        let with_cache = self
            .config
            .as_deref()
            .and_then(|text| text.parse::<toml::Table>().ok())
            .is_some_and(|table| table.contains_key(LOCAL_CACHE_TABLE));
        let capable = |release: &Path| {
            self.capabilities.get(release).is_some_and(|list| {
                let has = |need: &str| list.iter().any(|word| word == need);
                has(HANDOFF_CAPABILITY) && (!with_cache || has(CACHE_HANDOFF_CAPABILITY))
            })
        };
        // A node with no configuration cannot start a successor that takes the sockets. The
        // socket key may be left out: the process defaults it (`prepare` judged the releases).
        // Both releases, checked together: an instance serving is always one that was started
        // from a release.
        self.config.is_some() && [old, Some(new)].into_iter().all(|r| r.is_some_and(capable))
    }

    /// `old` gives its sockets away and drains.
    fn hand_over(&mut self, old: Instance) {
        let drain = self.drain_looks;
        let old_release = self.units[&old].release.clone();
        self.units.insert(
            old,
            Unit {
                state: UnitState::Deactivating,
                release: old_release,
                serving: false,
                left: drain,
            },
        );
    }

    /// One look at `/readyz` during a hand-over under way: the old instance answers the looks
    /// left, and the first look after them finds the new instance holding the sockets.
    fn tick_takeover(&mut self) {
        let Some((new, old, left)) = self.taking else {
            return;
        };
        if left > 0 {
            self.taking = Some((new, old, left - 1));
            return;
        }
        self.taking = None;
        self.units.get_mut(&new).expect("both instances").serving = true;
        self.hand_over(old);
    }

    /// systemd starting `instance`.
    fn start_unit(&mut self, instance: Instance) {
        if !self.markers.contains(&World::condition_path(instance)) {
            self.log.push(format!("skipped {instance}: no marker"));
            return;
        }
        if self.units[&instance].state == UnitState::Active {
            return;
        }
        // The service account runs the program; only the copy at `SERVICE_BINARY` is one it can
        // reach.
        let reached = (unit_program() == SERVICE_BINARY)
            .then(|| self.installed.clone())
            .flatten();
        let Some(release) = reached else {
            self.log
                .push(format!("failed {instance}: {} not run", unit_program()));
            self.units.insert(
                instance,
                Unit {
                    state: UnitState::Failed,
                    ..Unit::off()
                },
            );
            return;
        };
        let serving = Instance::ALL.into_iter().find(|other| {
            *other != instance
                && self.units[other].serving
                && self.units[other].state == UnitState::Active
        });
        let started = Unit {
            state: UnitState::Active,
            release: Some(release.clone()),
            serving: true,
            left: 0,
        };
        let Some(old) = serving else {
            self.units.insert(instance, started);
            return;
        };
        let old_release = self.units[&old].release.clone();
        if self.can_hand_over(old_release.as_deref(), &release) {
            if self.takeover_looks > 0 {
                // Still opening its cache: running, not serving; the old instance answers.
                self.units.insert(
                    instance,
                    Unit {
                        serving: false,
                        ..started
                    },
                );
                self.taking = Some((instance, old, self.takeover_looks));
            } else {
                self.units.insert(instance, started);
                self.hand_over(old);
            }
        } else {
            self.units.insert(
                instance,
                Unit {
                    state: UnitState::Failed,
                    ..Unit::off()
                },
            );
        }
    }
}

/// The world, and every port over it.
pub struct SystemdFake {
    w: Rc<RefCell<World>>,
    /// The ports of the second unit, over the same world.
    fs: fake_fs::FsPorts,
}

impl SystemdFake {
    fn world() -> SystemdFake {
        let capable = vec![
            HANDOFF_CAPABILITY.to_owned(),
            CACHE_HANDOFF_CAPABILITY.to_owned(),
            HANDOFF_STATE_DIR_CAPABILITY.to_owned(),
        ];
        let w = Rc::new(RefCell::new(World {
            units: Instance::ALL
                .into_iter()
                .map(|i| (i, Unit::off()))
                .collect(),
            markers: BTreeSet::new(),
            installed: None,
            config: Some(WITH_SOCKET.to_owned()),
            capabilities: [OLD, NEW, NEWER]
                .into_iter()
                .map(|release| (PathBuf::from(release), capable.clone()))
                .collect(),
            refusals: BTreeMap::new(),
            lies: BTreeMap::new(),
            mute: BTreeSet::new(),
            read_only: false,
            cache_lock: None,
            record: None,
            busy: false,
            now: 0,
            log: Vec::new(),
            calls: 0,
            halt_from: None,
            fail_prefix: None,
            late_prefix: None,
            drain_looks: 0,
            draining_looks: 0,
            takeover_looks: 0,
            taking: None,
            fs: fake_fs::FsWorld::new(),
        }));
        SystemdFake {
            fs: fake_fs::FsPorts::new(Rc::clone(&w)),
            w,
        }
    }

    /// A host with nothing installed: no instance, no marker, no binary, no record.
    pub fn empty() -> SystemdFake {
        SystemdFake::world()
    }

    /// A host that runs `OLD` on instance a, with nothing in flight.
    pub fn installed() -> SystemdFake {
        let fake = SystemdFake::world();
        {
            let mut w = fake.w.borrow_mut();
            w.installed = Some(PathBuf::from(OLD));
            w.markers.insert(World::condition_path(Instance::A));
            w.start_unit(Instance::A);
            w.record = Some(Record::idle(Some(Instance::A)).encode());
            w.log.clear();
        }
        fake
    }

    /// The ports over this world.
    pub fn ports(&self) -> SystemdPorts<'_> {
        SystemdPorts {
            units: self,
            markers: self,
            fs_units: &self.fs,
            fs_markers: &self.fs,
            instances: self,
            host: self,
            clock: self,
            state: self,
        }
    }

    /// Runs `step` with `params`.
    pub fn run(&self, step: Step, params: &Params) -> Done {
        run_step(step, params, &self.ports())
    }

    /// Runs `steps` in order as hostkeeper does. Each must succeed.
    pub fn drive(&self, params: &Params, steps: &[Step]) {
        for step in steps {
            let done = self.run_as_hostkeeper(*step, params);
            assert_ok(&done);
        }
    }

    /// The four steps of an update that succeeds.
    pub fn complete(&self, params: &Params) {
        self.drive(
            params,
            &[Step::Prepare, Step::Cutover, Step::Verify, Step::Cleanup],
        );
    }

    /// Runs `step` as `drive` does, and gives its result. hostkeeper moves `current` to the
    /// staged release before `cutover` and back to the previous one before `rollback`; the
    /// world has no link to move, since the hook reads the releases from the params alone.
    pub fn run_as_hostkeeper(&self, step: Step, params: &Params) -> Done {
        self.run(step, params)
    }

    /// The node's configuration; `None` is no file.
    pub fn set_config(&self, text: Option<&str>) {
        self.w.borrow_mut().config = text.map(str::to_owned);
    }

    /// What `release` says it can do.
    pub fn set_capabilities(&self, release: &str, words: &[&str]) {
        self.w.borrow_mut().capabilities.insert(
            PathBuf::from(release),
            words.iter().map(|word| (*word).to_owned()).collect(),
        );
    }

    /// `release` refuses the node's configuration when asked to check it, with `line`; every
    /// release accepts it unless this says otherwise.
    pub fn set_refusal(&self, release: &str, line: &str) {
        self.w
            .borrow_mut()
            .refusals
            .insert(PathBuf::from(release), line.to_owned());
    }

    /// The `(release, configuration)` pairs the step asked to check, in order.
    pub fn config_checks(&self) -> Vec<(String, String)> {
        self.log()
            .iter()
            .filter_map(|line| line.strip_prefix("host.check_config "))
            .filter_map(|rest| rest.split_once(' '))
            .map(|(release, config)| (release.to_owned(), config.to_owned()))
            .collect()
    }

    /// The record on disk, as bytes.
    pub fn set_record_bytes(&self, bytes: Option<&[u8]>) {
        self.w.borrow_mut().record = bytes.map(<[u8]>::to_vec);
    }

    /// The record on disk.
    pub fn set_record(&self, record: &Record) {
        self.set_record_bytes(Some(&record.encode()));
    }

    /// The record on disk, decoded.
    pub fn record(&self) -> Record {
        let w = self.w.borrow();
        Record::decode(w.record.as_deref().expect("a record")).expect("a record that decodes")
    }

    /// Another step holds the lock.
    pub fn set_busy(&self, busy: bool) {
        self.w.borrow_mut().busy = busy;
    }

    /// How many `state` looks a draining instance is still seen running.
    pub fn set_drain_looks(&self, looks: u32) {
        self.w.borrow_mut().drain_looks = looks;
    }

    /// The next `looks` looks at `/readyz` reach the old instance, which drains on the same
    /// sockets and says 503 `draining`.
    pub fn answer_draining(&self, looks: u32) {
        self.w.borrow_mut().draining_looks = looks;
    }

    /// A new instance of a hand-over opens its cache and takes the sockets after `looks` looks at
    /// `/readyz`, which the old instance answers; 0 (the default) is at once.
    pub fn set_takeover_looks(&self, looks: u32) {
        self.w.borrow_mut().takeover_looks = looks;
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

    /// Whether `/readyz` says the cache is read-only.
    pub fn set_read_only(&self, read_only: bool) {
        self.w.borrow_mut().read_only = read_only;
    }

    /// From now on `/readyz` says what the cache lock says ([`CacheLock`]): the instance that
    /// serves now holds it, and an instance that finds it free takes it `promote_secs` seconds
    /// later.
    pub fn hold_cache_lock(&self, promote_secs: u32) {
        let mut w = self.w.borrow_mut();
        let holder = w.serving();
        w.cache_lock = Some(CacheLock {
            holder,
            promote_secs,
            promoting: None,
        });
    }

    /// Puts `instance` in a state, running `release` and serving or not; a hand-over under way ends
    /// there.
    pub fn set_unit(
        &self,
        instance: Instance,
        state: UnitState,
        release: Option<&str>,
        serving: bool,
    ) {
        let mut w = self.w.borrow_mut();
        // The state set is the end of any hand-over under way.
        w.taking = None;
        w.units.insert(
            instance,
            Unit {
                state,
                release: release.map(PathBuf::from),
                serving,
                left: 0,
            },
        );
    }

    /// Lays down or removes the marker of `instance`, as a person or an old run did.
    pub fn set_marker(&self, instance: Instance, present: bool) {
        let mut w = self.w.borrow_mut();
        let path = marker_path(instance);
        if present {
            w.markers.insert(path);
        } else {
            w.markers.remove(&path);
        }
    }

    /// The host boots: no instance runs, and systemd starts every instance whose unit file's
    /// condition holds.
    pub fn reboot(&self) {
        let mut w = self.w.borrow_mut();
        for instance in Instance::ALL {
            w.units.insert(instance, Unit::off());
        }
        for instance in Instance::ALL {
            w.start_unit(instance);
        }
        w.reboot_fs();
    }

    /// The state of `instance`, without a look that counts.
    pub fn peek(&self, instance: Instance) -> UnitState {
        self.w.borrow().units[&instance].state
    }

    /// The instances whose process may run.
    pub fn running(&self) -> Vec<Instance> {
        let w = self.w.borrow();
        Instance::ALL
            .into_iter()
            .filter(|i| w.units[i].state.running())
            .collect()
    }

    /// The instances that have their marker.
    pub fn markers(&self) -> Vec<Instance> {
        Instance::ALL
            .into_iter()
            .filter(|i| Markers::present(self, *i).expect("a look at a marker"))
            .collect()
    }

    /// The world without the counters.
    pub fn snapshot(&self) -> Snapshot {
        let record = self
            .w
            .borrow()
            .record
            .as_deref()
            .map(|bytes| Record::decode(bytes).expect("a record that decodes"));
        let w = self.w.borrow();
        Snapshot {
            record,
            units: Instance::ALL
                .into_iter()
                .map(|i| (i, w.units[&i].state, w.units[&i].release.clone()))
                .collect(),
            markers: Instance::ALL
                .into_iter()
                .filter(|i| w.markers.contains(&marker_path(*i)))
                .collect(),
            installed: w.installed.clone(),
            fs_units: w.fs_snapshot(),
            fs_markers: w.fs_marker_set(),
        }
    }

    /// The release whose binary is at [`SERVICE_BINARY`], as a person or an old run left it.
    pub fn set_installed(&self, release: Option<&str>) {
        self.w.borrow_mut().installed = release.map(PathBuf::from);
    }

    /// The calls that put a binary in place, remove it, or start an instance, in order.
    pub fn binary_acts(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|line| {
                ["host.install_binary", "host.remove_binary", "units.start"]
                    .iter()
                    .any(|prefix| line.starts_with(prefix))
            })
            .collect()
    }

    /// Every call so far, in order.
    pub fn log(&self) -> Vec<String> {
        self.w.borrow().log.clone()
    }

    /// The calls that change a unit or a marker, in order.
    pub fn actions(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|line| {
                ["units.start", "units.stop", "markers.set", "markers.clear"]
                    .iter()
                    .any(|prefix| line.starts_with(prefix))
            })
            .collect()
    }

    /// The paths the step asked the host for the configuration of.
    pub fn config_paths(&self) -> Vec<String> {
        self.log()
            .iter()
            .filter_map(|line| line.strip_prefix("host.config "))
            .map(str::to_owned)
            .collect()
    }

    /// How many calls so far.
    pub fn calls(&self) -> usize {
        self.w.borrow().calls
    }

    /// The n-th call from now fails, and so does every later one, without effect; `None` lets
    /// every call through.
    pub fn halt_at(&self, n: Option<usize>) {
        let mut w = self.w.borrow_mut();
        w.halt_from = n.map(|n| w.calls + n);
    }

    /// The calls whose log line starts with `prefix` fail without effect.
    pub fn fail_on(&self, prefix: Option<&str>) {
        self.w.borrow_mut().fail_prefix = prefix.map(str::to_owned);
    }

    /// The calls whose log line starts with `prefix` fail after their effect.
    pub fn late_on(&self, prefix: Option<&str>) {
        self.w.borrow_mut().late_prefix = prefix.map(str::to_owned);
    }

    /// Counts and logs one call, and says whether it is to fail after its effect. A call that is
    /// to fail without effect is the error.
    fn count(&self, what: String, kind: PortKind) -> Result<bool, PortError> {
        count_in(&self.w, what, kind)
    }

    /// The failure of a call whose effect has happened, if it is to fail.
    fn after(late: bool, kind: PortKind) -> Result<(), PortError> {
        if late {
            Err(PortError::failed(kind, "injected failure after its effect"))
        } else {
            Ok(())
        }
    }
}

/// Counts and logs one call of the world `w`; see [`SystemdFake::count`].
fn count_in(w: &RefCell<World>, what: String, kind: PortKind) -> Result<bool, PortError> {
    let mut w = w.borrow_mut();
    w.calls += 1;
    let n = w.calls;
    let matches = |prefix: &Option<String>| {
        prefix
            .as_deref()
            .is_some_and(|prefix| what.starts_with(prefix))
    };
    let refused = w.halt_from.is_some_and(|from| n >= from) || matches(&w.fail_prefix);
    let late = matches(&w.late_prefix);
    w.log.push(what);
    if refused {
        Err(PortError::failed(kind, "injected failure"))
    } else {
        Ok(late)
    }
}

impl Units for SystemdFake {
    fn start(&self, instance: Instance) -> Result<(), PortError> {
        let late = self.count(format!("units.start {instance}"), PortKind::Units)?;
        self.w.borrow_mut().start_unit(instance);
        SystemdFake::after(late, PortKind::Units)
    }

    fn stop(&self, instance: Instance) -> Result<(), PortError> {
        let late = self.count(format!("units.stop {instance}"), PortKind::Units)?;
        self.w.borrow_mut().units.insert(instance, Unit::off());
        SystemdFake::after(late, PortKind::Units)
    }

    fn state(&self, instance: Instance) -> Result<UnitState, PortError> {
        self.count(format!("units.state {instance}"), PortKind::Units)?;
        let mut w = self.w.borrow_mut();
        let unit = w.units.get_mut(&instance).expect("both instances");
        if unit.state == UnitState::Deactivating {
            if unit.left == 0 {
                *unit = Unit::off();
            } else {
                unit.left -= 1;
            }
        }
        Ok(unit.state)
    }
}

impl Markers for SystemdFake {
    fn present(&self, instance: Instance) -> Result<bool, PortError> {
        Ok(self.w.borrow().markers.contains(&marker_path(instance)))
    }

    fn set(&self, instance: Instance) -> Result<(), PortError> {
        let late = self.count(format!("markers.set {instance}"), PortKind::Markers)?;
        self.w.borrow_mut().markers.insert(marker_path(instance));
        SystemdFake::after(late, PortKind::Markers)
    }

    fn clear(&self, instance: Instance) -> Result<(), PortError> {
        let late = self.count(format!("markers.clear {instance}"), PortKind::Markers)?;
        self.w.borrow_mut().markers.remove(&marker_path(instance));
        SystemdFake::after(late, PortKind::Markers)
    }
}

impl Instances for SystemdFake {
    fn get(&self, _port: u16, path: &str) -> Result<Reply, PortError> {
        self.count(format!("get {path}"), PortKind::Instances)?;
        let mut w = self.w.borrow_mut();
        if path == "/readyz" {
            w.tick_takeover();
        }
        if path == "/readyz" && w.draining_looks > 0 {
            w.draining_looks -= 1;
            return Ok(Reply {
                status: 503,
                body: "draining".to_owned(),
            });
        }
        let release = w
            .units
            .values()
            .filter(|unit| unit.serving && unit.state == UnitState::Active)
            .find_map(|unit| unit.release.as_ref())
            .ok_or_else(|| PortError::failed(PortKind::Instances, "connection refused"))?;
        if w.mute.contains(release) {
            return Err(PortError::failed(PortKind::Instances, "timed out"));
        }
        let body = if path == "/readyz" {
            if w.store_read_only() {
                "ready read-only".to_owned()
            } else {
                "ready read-write".to_owned()
            }
        } else {
            let full = w
                .lies
                .get(release)
                .cloned()
                .unwrap_or_else(|| version_of(&release.to_string_lossy()));
            let (version, commit) = full.split_once('+').expect("a version with a commit");
            // The text the server renders, so that the fake and the server cannot drift.
            gauge(version, commit, None)
        };
        Ok(Reply { status: 200, body })
    }
}

impl Host for SystemdFake {
    fn config(&self, path: &Path) -> Result<Option<String>, PortError> {
        self.count(format!("host.config {}", path.display()), PortKind::Host)?;
        Ok(self.w.borrow().config.clone())
    }

    // The systemd arm lists no mounts.
    fn mounted(&self, path: &Path) -> Result<bool, PortError> {
        self.count(format!("host.mounted {}", path.display()), PortKind::Host)?;
        Ok(false)
    }

    fn capabilities(&self, release: &Path) -> Result<Vec<String>, PortError> {
        self.count(
            format!("host.capabilities {}", release.display()),
            PortKind::Host,
        )?;
        self.w
            .borrow()
            .capabilities
            .get(release)
            .cloned()
            .ok_or_else(|| PortError::failed(PortKind::Host, "not a release of this host"))
    }

    fn check_config(&self, release: &Path, config: &Path) -> Result<Option<String>, PortError> {
        self.check_config_of(release, config)
    }

    fn install_binary(&self, release: &Path) -> Result<(), PortError> {
        let late = self.count(
            format!("host.install_binary {}", release.display()),
            PortKind::Host,
        )?;
        self.w.borrow_mut().installed = Some(release.to_path_buf());
        SystemdFake::after(late, PortKind::Host)
    }

    fn remove_binary(&self) -> Result<(), PortError> {
        let late = self.count("host.remove_binary".to_owned(), PortKind::Host)?;
        self.w.borrow_mut().installed = None;
        SystemdFake::after(late, PortKind::Host)
    }
}

impl SystemdFake {
    fn check_config_of(&self, release: &Path, config: &Path) -> Result<Option<String>, PortError> {
        self.count(
            format!(
                "host.check_config {} {}",
                release.display(),
                config.display()
            ),
            PortKind::Host,
        )?;
        Ok(self.w.borrow().refusals.get(release).cloned())
    }
}

impl Clock for SystemdFake {
    fn now_ms(&self) -> u64 {
        self.w.borrow().now
    }

    fn pause(&self, span: Duration) {
        let mut w = self.w.borrow_mut();
        w.now += u64::try_from(span.as_millis()).unwrap_or(u64::MAX);
        for _ in 0..span.as_secs() {
            w.tick_second();
        }
    }
}

impl StateDir for SystemdFake {
    fn lock(&self) -> Result<Lock, PortError> {
        self.count("state.lock".to_owned(), PortKind::State)?;
        if self.w.borrow().busy {
            return Err(PortError::Busy);
        }
        Ok(Lock::new(()))
    }

    fn load(&self) -> Result<Option<Vec<u8>>, PortError> {
        self.count("state.load".to_owned(), PortKind::State)?;
        Ok(self.w.borrow().record.clone())
    }

    fn save(&self, bytes: &[u8]) -> Result<(), PortError> {
        let late = self.count("state.save".to_owned(), PortKind::State)?;
        self.w.borrow_mut().record = Some(bytes.to_vec());
        SystemdFake::after(late, PortKind::State)
    }
}
