//! The fake of the launchd arm's ports, in one world: the one job of the system domain (loaded
//! or not, and the release it runs), the copy of a release's binary the plist runs, the node's
//! configuration files, the record and its lock. A job that is loaded runs the program of the
//! plist's `ProgramArguments` as its `UserName`: when that program is [`SERVICE_BINARY`] it runs
//! the release whose binary the hook put there (`install_binary`); with none there, or with any
//! other program (the binary inside a release, which hostkeeper keeps closed to every user but
//! root), the job is loaded and runs nothing. It answers `/readyz` and `/metricz` on the public
//! port from what runs. hostkeeper's link `current` is not in the world: the hook never reads it.
//! The clock is a number that `pause` moves on; nothing sleeps.
//!
//! A release lists no capabilities at first (`set_capabilities` gives it some) and accepts the
//! node's file unless `set_refusal` says otherwise, so an update is a restart unless a test makes
//! it a hand-over. `hand_to_next_image` models `launchctl kill SIGUSR2`: the job's process keeps
//! its place and runs the release whose binary the hook put at [`SERVICE_BINARY`].
//!
//! The job of `tierstore fs` is a second job of the domain (`FS_LABEL`, plist `FS_LAUNCHD_PLIST`)
//! that makes a mount (`FS_MOUNT`) when it is loaded and ends it when it is booted out, unless
//! `keep_mount` says the mount stays; `mounted` lists it.
//!
//! The main job serves the volume of the node's file: while it runs a release and the file at
//! [`CONFIG`] has an `fs` interface, the mount at that interface's mount point is listed and a
//! look-up of `trees` in it finds a directory (`is_dir`). `set_volume` makes that mount missing,
//! another file system's, or one whose server is gone.
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

use crate::server::update_hook::launchd::{run_step, run_step_with};
use crate::server::update_hook::provider_fake::FakeProvider;
use crate::server::update_hook::shape::fs_interface;
use crate::server::update_hook::{
    Clock, Done, FS_CONFIG_FILE, FS_LAUNCHD_PLIST, Host, Instances, LAUNCHD_PLIST, Launchd,
    LaunchdPorts, Lock, Outcome, Params, PortError, PortKind, Reason, Record, Reply,
    SERVICE_BINARY, StateDir, Step,
};

/// The label of the job, as the params name it.
pub const LABEL: &str = "com.komira.tierstore";
/// The label of the job of `tierstore fs`, as the params name it.
pub const FS_LABEL: &str = "tierstore-fs";
/// The mount point the fs job's configuration names.
pub const FS_MOUNT: &str = "/private/var/tierstore/fs";
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

/// The plist the package ships.
const PLIST: &str = include_str!("../../../../../../packaging/hostkeeper/units/tierstore.plist");

/// The program of the plist: the first string of its `ProgramArguments`.
pub fn plist_program() -> &'static str {
    PLIST
        .split("<key>ProgramArguments</key>")
        .nth(1)
        .and_then(|rest| rest.split("<string>").nth(1))
        .and_then(|rest| rest.split("</string>").next())
        .expect("the plist has a program")
}

/// What the main job's process makes of the volume of the node's file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Volume {
    /// It mounts tierstore's file system at the fs interface's mount point and serves it.
    Served,
    /// Nothing is mounted there.
    NotMounted,
    /// A mount is listed there, and it has no `trees`: another file system's.
    Foreign,
    /// The mount is listed, and every look-up in it fails: its server is gone.
    Dead,
}

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
    /// The release whose binary the hook put at [`SERVICE_BINARY`].
    installed: Option<PathBuf>,
    job: Option<PathBuf>,
    /// The job is loaded, and its program cannot be run.
    broken: bool,
    /// The params name the fs job.
    fs_named: bool,
    /// The fs job is loaded.
    fs_job: bool,
    /// Where a mount is listed, if any.
    mount: Option<PathBuf>,
    /// The mount stays when the fs job is booted out.
    mount_stays: bool,
    /// What the main job makes of the volume of the node's file.
    volume: Volume,
    /// The labels launchd was told to disable, and not enabled since.
    disabled: BTreeSet<String>,
    /// The bounds each `bootout_within` was given: label, call, looks.
    bounds: Vec<(String, Duration, u32)>,
    versions: BTreeMap<PathBuf, String>,
    lies: BTreeMap<PathBuf, String>,
    mute: BTreeSet<PathBuf>,
    read_only: bool,
    configs: BTreeMap<PathBuf, String>,
    /// The words each release lists; a release not here cannot be run.
    capabilities: BTreeMap<PathBuf, Vec<String>>,
    /// The releases that refuse the node's file, with the line they say.
    refusals: BTreeMap<PathBuf, String>,
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
    /// The release whose binary is at [`SERVICE_BINARY`].
    pub installed: Option<PathBuf>,
    pub job: Option<PathBuf>,
    /// The fs job is loaded.
    pub fs_job: bool,
    /// A mount is listed.
    pub mounted: bool,
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
    /// The node's mount provider.
    pub provider: FakeProvider,
}

/// The params of an update to `release` (`version`), from `previous` when there is one.
pub fn params_for(release: &str, version: &str, previous: Option<(&str, &str)>) -> Params {
    params_with(release, version, previous, "")
}

/// [`params_for`], with `extra` lines of params.
pub fn params_with(
    release: &str,
    version: &str,
    previous: Option<(&str, &str)>,
    extra: &str,
) -> Params {
    let mut text = format!(
        "service_manager = \"launchd\"\n\
         launchd_label = \"{LABEL}\"\n\
         {extra}\
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
    fn world(record: Option<Vec<u8>>, installed: Option<&str>, job: Option<&str>) -> World {
        World {
            record,
            busy: false,
            installed: installed.map(PathBuf::from),
            job: job.map(PathBuf::from),
            broken: false,
            fs_named: false,
            fs_job: false,
            mount: None,
            mount_stays: false,
            volume: Volume::Served,
            disabled: BTreeSet::new(),
            bounds: Vec::new(),
            versions: BTreeMap::from([
                (PathBuf::from(OLD), OLD_VERSION.to_owned()),
                (PathBuf::from(NEW), NEW_VERSION.to_owned()),
            ]),
            lies: BTreeMap::new(),
            mute: BTreeSet::new(),
            read_only: false,
            configs: BTreeMap::from([(PathBuf::from(CONFIG), "[server]\n".to_owned())]),
            capabilities: BTreeMap::from([
                (PathBuf::from(OLD), Vec::new()),
                (PathBuf::from(NEW), Vec::new()),
            ]),
            refusals: BTreeMap::new(),
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
            provider: FakeProvider::under(FS_MOUNT),
        }
    }

    /// A host with nothing installed: no record, no binary, no job; `NEW` is staged.
    pub fn first_install() -> Fake {
        Fake {
            w: RefCell::new(Fake::world(None, None, None)),
            locks: Rc::new(Cell::new(0)),
            first: true,
            provider: FakeProvider::under(FS_MOUNT),
        }
    }

    /// The ports over this world.
    pub fn ports(&self) -> LaunchdPorts<'_> {
        LaunchdPorts {
            launchd: self,
            instances: self,
            host: self,
            provider: &self.provider,
            clock: self,
            state: self,
        }
    }

    /// The params of the update to `NEW`: from `OLD`, or of a first install.
    pub fn params(&self) -> Params {
        let previous = (!self.first).then_some((OLD, OLD_VERSION));
        let extra = if self.w.borrow().fs_named {
            format!("fs_launchd_label = \"{FS_LABEL}\"\n")
        } else {
            String::new()
        };
        params_with(NEW, NEW_VERSION, previous, &extra)
    }

    /// The params of this host's update name the fs job, whose configuration names the mount
    /// point [`FS_MOUNT`]; `loaded` says whether the fs job is loaded and has made its mount.
    pub fn with_fs(&self, loaded: bool) {
        let mut w = self.w.borrow_mut();
        w.fs_named = true;
        w.fs_job = loaded;
        w.mount = loaded.then(|| PathBuf::from(FS_MOUNT));
        w.configs.insert(
            PathBuf::from(FS_CONFIG_FILE),
            format!("[fs]\nmountpoint = \"{FS_MOUNT}\"\n"),
        );
    }

    /// Whether the params of this host's update name the fs job.
    pub fn name_fs_job(&self, named: bool) {
        self.w.borrow_mut().fs_named = named;
    }

    /// Whether the mount stays listed after the fs job is booted out.
    pub fn keep_mount(&self, stays: bool) {
        self.w.borrow_mut().mount_stays = stays;
    }

    /// What the main job's process makes of the volume of the node's file.
    pub fn set_volume(&self, volume: Volume) {
        self.w.borrow_mut().volume = volume;
    }

    /// The bounds of each `bootout_within` call so far: label, call time and looks.
    pub fn bootout_bounds(&self) -> Vec<(String, Duration, u32)> {
        self.w.borrow().bounds.clone()
    }

    /// Runs `step` as it is, with the params of this host's update.
    pub fn run(&self, step: Step) -> Done {
        self.run_with(step, &self.params(), LABEL)
    }

    /// Runs `step` with `params` and the job `label`.
    pub fn run_with(&self, step: Step, params: &Params, label: &str) -> Done {
        run_step_with(step, params, label, &self.ports(), true)
    }

    /// Runs `step` as the config hook drives the arm: for a package that carries no binary.
    pub fn run_for_config(&self, step: Step) -> Done {
        run_step(step, &self.params(), LABEL, &self.ports())
    }

    /// Runs `step` as hostkeeper does. hostkeeper moves `current` to the staged release before
    /// `cutover` and back to the previous one before `rollback`; the world has no link to move,
    /// since the hook reads the releases from the params alone.
    pub fn host_run(&self, step: Step) -> Done {
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
            installed: w.installed.clone(),
            job: w.job.clone(),
            fs_job: w.fs_job,
            mounted: w.mount.is_some(),
        }
    }

    /// The labels that are disabled.
    pub fn disabled(&self) -> Vec<String> {
        self.w.borrow().disabled.iter().cloned().collect()
    }

    /// Disables `label`, as an earlier step would have.
    pub fn set_disabled(&self, label: &str) {
        self.w.borrow_mut().disabled.insert(label.to_owned());
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

    /// The words `release` lists in `--capabilities`; none at first.
    pub fn set_capabilities(&self, release: &str, words: &[&str]) {
        self.w.borrow_mut().capabilities.insert(
            PathBuf::from(release),
            words.iter().map(|word| (*word).to_owned()).collect(),
        );
    }

    /// `release` refuses the node's file when it checks it, with `line`.
    pub fn set_refusal(&self, release: &str, line: &str) {
        self.w
            .borrow_mut()
            .refusals
            .insert(PathBuf::from(release), line.to_owned());
    }

    /// The job is loaded, running `release`, or is not loaded.
    pub fn set_job(&self, release: Option<&str>) {
        let mut w = self.w.borrow_mut();
        w.job = release.map(PathBuf::from);
        w.broken = false;
    }

    /// The job is loaded and runs nothing: its program exited and launchd has not started it
    /// again, or cannot.
    pub fn set_job_idle(&self) {
        let mut w = self.w.borrow_mut();
        w.job = None;
        w.broken = true;
    }

    /// The release whose binary is at [`SERVICE_BINARY`], as a person or an old run left it.
    pub fn set_installed(&self, release: Option<&str>) {
        self.w.borrow_mut().installed = release.map(PathBuf::from);
    }

    /// The calls that put a binary in place, remove it, or load the job, in order.
    pub fn binary_acts(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|line| {
                [
                    "host.install_binary",
                    "host.remove_binary",
                    "launchd.bootstrap",
                ]
                .iter()
                .any(|prefix| line.starts_with(prefix))
            })
            .collect()
    }

    /// The host has booted, and launchd has not loaded the job, or the fs job, yet; no mount is
    /// listed.
    pub fn reboot(&self) {
        let mut w = self.w.borrow_mut();
        w.job = None;
        w.broken = false;
        w.fs_job = false;
        w.mount = None;
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
        if label != LABEL && label != FS_LABEL {
            self.stray(label);
        }
        let w = self.w.borrow();
        Ok(match label {
            LABEL => w.job.is_some() || w.broken,
            FS_LABEL => w.fs_job,
            _ => false,
        })
    }

    fn bootstrap(&self, plist: &Path) -> Result<(), PortError> {
        let late = self.tick_changing(
            format!("launchd.bootstrap {}", plist.display()),
            PortKind::Launchd,
        )?;
        if plist == Path::new(FS_LAUNCHD_PLIST) {
            let mut w = self.w.borrow_mut();
            if w.disabled.contains(FS_LABEL) {
                return Err(PortError::failed(
                    PortKind::Launchd,
                    "Bootstrap failed: 119: Service is disabled",
                ));
            }
            if w.fs_job {
                return Err(PortError::failed(
                    PortKind::Launchd,
                    "Bootstrap failed: 5: Input/output error (the fs job is loaded)",
                ));
            }
            w.fs_job = true;
            w.mount = Some(PathBuf::from(FS_MOUNT));
            drop(w);
            return Fake::after(late, PortKind::Launchd);
        }
        if plist != Path::new(LAUNCHD_PLIST) {
            self.stray(&plist.display().to_string());
        }
        let mut w = self.w.borrow_mut();
        if w.job.is_some() || w.broken {
            return Err(PortError::failed(
                PortKind::Launchd,
                "Bootstrap failed: 5: Input/output error (the job is loaded)",
            ));
        }
        // The job's account reaches only the copy at `SERVICE_BINARY`.
        let reached = (plist_program() == SERVICE_BINARY)
            .then(|| w.installed.clone())
            .flatten();
        w.broken = reached.is_none();
        w.job = reached;
        drop(w);
        Fake::after(late, PortKind::Launchd)
    }

    fn bootout(&self, label: &str) -> Result<(), PortError> {
        let late = self.tick_changing(format!("launchd.bootout {label}"), PortKind::Launchd)?;
        if label == LABEL {
            let mut w = self.w.borrow_mut();
            w.job = None;
            w.broken = false;
        } else if label == FS_LABEL {
            let mut w = self.w.borrow_mut();
            w.fs_job = false;
            if !w.mount_stays {
                w.mount = None;
            }
        } else {
            self.stray(label);
        }
        Fake::after(late, PortKind::Launchd)
    }

    fn bootout_within(
        &self,
        label: &str,
        call: Duration,
        gone_attempts: u32,
    ) -> Result<(), PortError> {
        self.w
            .borrow_mut()
            .bounds
            .push((label.to_owned(), call, gone_attempts));
        self.bootout(label)
    }

    /// The job's process executes the program of the plist again, under the same pid: it then
    /// runs the release whose binary is at [`SERVICE_BINARY`]. A job that is not loaded is
    /// `launchctl`'s error.
    fn hand_to_next_image(&self, label: &str) -> Result<(), PortError> {
        let late =
            self.tick_changing(format!("launchd.kill SIGUSR2 {label}"), PortKind::Launchd)?;
        if label != LABEL {
            self.stray(label);
            return Err(PortError::failed(PortKind::Launchd, "No such process"));
        }
        let mut w = self.w.borrow_mut();
        if w.job.is_none() {
            return Err(PortError::failed(PortKind::Launchd, "No such process"));
        }
        let reached = (plist_program() == SERVICE_BINARY)
            .then(|| w.installed.clone())
            .flatten();
        w.broken = reached.is_none();
        w.job = reached;
        drop(w);
        Fake::after(late, PortKind::Launchd)
    }

    fn disable(&self, label: &str) -> Result<(), PortError> {
        let late = self.tick_changing(format!("launchd.disable {label}"), PortKind::Launchd)?;
        self.w.borrow_mut().disabled.insert(label.to_owned());
        Fake::after(late, PortKind::Launchd)
    }

    fn enable(&self, label: &str) -> Result<(), PortError> {
        let late = self.tick_changing(format!("launchd.enable {label}"), PortKind::Launchd)?;
        self.w.borrow_mut().disabled.remove(label);
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

impl World {
    /// Where the main job lists a mount: the mount point of the `fs` interface of the node's file,
    /// while the job runs a release that mounts it.
    fn job_mount(&self) -> Option<PathBuf> {
        if self.job.is_none() || self.volume == Volume::NotMounted {
            return None;
        }
        let text = self.configs.get(Path::new(CONFIG))?;
        fs_interface(text).map(|shape| shape.mountpoint)
    }

    fn lists(&self, path: &Path) -> bool {
        self.mount.as_deref() == Some(path) || self.job_mount().as_deref() == Some(path)
    }
}

impl Host for Fake {
    fn mounted(&self, path: &Path) -> Result<bool, PortError> {
        self.tick(format!("host.mounted {}", path.display()), PortKind::Host)?;
        Ok(self.w.borrow().lists(path))
    }

    /// `trees` just below a mount that is listed is a directory, unless the mount is another file
    /// system's; a look-up in the main job's mount fails when its server is gone.
    fn is_dir(&self, path: &Path) -> Result<bool, PortError> {
        self.tick(format!("host.is_dir {}", path.display()), PortKind::Host)?;
        let w = self.w.borrow();
        let Some(mount) = path.parent().filter(|mount| w.lists(mount)) else {
            return Ok(false);
        };
        if w.volume == Volume::Dead && w.job_mount().as_deref() == Some(mount) {
            return Err(PortError::failed(
                PortKind::Host,
                format!("stat {}: Input/output error (os error 5)", path.display()),
            ));
        }
        Ok(w.volume != Volume::Foreign && path.file_name() == Some("trees".as_ref()))
    }

    fn config(&self, path: &Path) -> Result<Option<String>, PortError> {
        self.tick(format!("host.config {}", path.display()), PortKind::Host)?;
        Ok(self.w.borrow().configs.get(path).cloned())
    }

    fn capabilities(&self, release: &Path) -> Result<Vec<String>, PortError> {
        self.tick(
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
        self.tick(
            format!(
                "host.check_config {} {}",
                release.display(),
                config.display()
            ),
            PortKind::Host,
        )?;
        Ok(self.w.borrow().refusals.get(release).cloned())
    }

    fn install_binary(&self, release: &Path) -> Result<(), PortError> {
        let late = self.tick_changing(
            format!("host.install_binary {}", release.display()),
            PortKind::Host,
        )?;
        self.w.borrow_mut().installed = Some(release.to_path_buf());
        Fake::after(late, PortKind::Host)
    }

    fn remove_binary(&self) -> Result<(), PortError> {
        let late = self.tick_changing("host.remove_binary".to_owned(), PortKind::Host)?;
        self.w.borrow_mut().installed = None;
        Fake::after(late, PortKind::Host)
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
