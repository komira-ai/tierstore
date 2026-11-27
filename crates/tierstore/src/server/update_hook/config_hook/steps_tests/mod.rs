//! The steps of the config hook over the fakes of the two arms: a config update by hand-over and
//! by restart, the refusals of `prepare` and `verify`, the guard, the rollback of a config
//! package and of the node's first, what `cleanup` writes into the release's record, and the
//! same steps on a Mac.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::super::ports::{Control, Guard, Instances, Reply};
use super::super::systemd_tests::fake as lin;
use super::super::{
    CACHE_HANDOFF_CAPABILITY, Done, HANDOFF_CAPABILITY, HANDOFF_STATE_DIR_CAPABILITY, Host,
    Instance, Lock, Marker, Mode, Params, PortError, PortKind, Record, StateDir, Step,
    SystemdPorts,
};
use super::{Common, Running, staged_copy, systemd_step};

use Step::{Cleanup, Cutover, Prepare, Verify};

pub(super) const PKG_OLD: &str = "/var/lib/hostkeeper/packages/tierstore-config/1.0.0";
pub(super) const PKG_NEW: &str = "/var/lib/hostkeeper/packages/tierstore-config/1.1.0";

/// The files a node reads, as the host port of the config hook sees them: the text of each path,
/// the lines the serving binary refuses a path with, and the paths that cannot be read.
struct Files {
    texts: RefCell<BTreeMap<PathBuf, String>>,
    refusals: RefCell<BTreeMap<PathBuf, String>>,
    unreadable: RefCell<BTreeSet<PathBuf>>,
    checks_fail: Cell<bool>,
    mount_fails: Cell<bool>,
    checked: RefCell<Vec<(PathBuf, PathBuf)>>,
    mounts: RefCell<BTreeSet<PathBuf>>,
    capabilities: Vec<String>,
}

impl Files {
    fn new(capabilities: &[&str]) -> Files {
        Files {
            texts: RefCell::new(BTreeMap::new()),
            refusals: RefCell::new(BTreeMap::new()),
            unreadable: RefCell::new(BTreeSet::new()),
            checks_fail: Cell::new(false),
            mount_fails: Cell::new(false),
            checked: RefCell::new(Vec::new()),
            mounts: RefCell::new(BTreeSet::new()),
            capabilities: capabilities.iter().map(|word| (*word).to_owned()).collect(),
        }
    }

    fn put(&self, path: &Path, text: &str) {
        self.texts
            .borrow_mut()
            .insert(path.to_path_buf(), text.to_owned());
    }

    fn refuse(&self, path: &Path, line: &str) {
        self.refusals
            .borrow_mut()
            .insert(path.to_path_buf(), line.to_owned());
    }

    fn checked(&self) -> Vec<(PathBuf, PathBuf)> {
        self.checked.borrow().clone()
    }
}

impl Host for Files {
    fn config(&self, path: &Path) -> Result<Option<String>, PortError> {
        if self.unreadable.borrow().contains(path) {
            return Err(PortError::failed(PortKind::Host, "unreadable"));
        }
        Ok(self.texts.borrow().get(path).cloned())
    }

    fn mounted(&self, path: &Path) -> Result<bool, PortError> {
        if self.mount_fails.get() {
            return Err(PortError::failed(
                PortKind::Host,
                "the mount list is unreadable",
            ));
        }
        Ok(self.mounts.borrow().contains(path))
    }

    fn capabilities(&self, _release: &Path) -> Result<Vec<String>, PortError> {
        Ok(self.capabilities.clone())
    }

    fn check_config(&self, release: &Path, config: &Path) -> Result<Option<String>, PortError> {
        self.checked
            .borrow_mut()
            .push((release.to_path_buf(), config.to_path_buf()));
        if self.checks_fail.get() {
            return Err(PortError::failed(PortKind::Host, "the binary did not run"));
        }
        Ok(self.refusals.borrow().get(config).cloned())
    }

    // The config hook's host never asks for these: its [`Running`] host answers them itself.
    fn install_binary(&self, _release: &Path) -> Result<(), PortError> {
        Err(PortError::failed(
            PortKind::Host,
            "no binary is put in place",
        ))
    }

    fn remove_binary(&self) -> Result<(), PortError> {
        Err(PortError::failed(PortKind::Host, "no binary is removed"))
    }
}

/// A network guard that counts its reloads and can refuse them.
struct CountingGuard {
    reloads: Cell<u32>,
    broken: Cell<bool>,
}

impl CountingGuard {
    fn new() -> CountingGuard {
        CountingGuard {
            reloads: Cell::new(0),
            broken: Cell::new(false),
        }
    }
}

impl Guard for CountingGuard {
    fn reload(&self) -> Result<(), PortError> {
        self.reloads.set(self.reloads.get() + 1);
        if self.broken.get() {
            return Err(PortError::failed(PortKind::Units, "nft: syntax error"));
        }
        Ok(())
    }
}

/// The update hook's record of the release, in memory.
struct Mem {
    bytes: RefCell<Option<Vec<u8>>>,
    fail_load: Cell<bool>,
    fail_save: Cell<bool>,
}

impl Mem {
    fn holding(record: Option<&Record>) -> Mem {
        Mem {
            bytes: RefCell::new(record.map(Record::encode)),
            fail_load: Cell::new(false),
            fail_save: Cell::new(false),
        }
    }

    fn set(&self, record: &Record) {
        *self.bytes.borrow_mut() = Some(record.encode());
    }

    fn get(&self) -> Option<Record> {
        self.bytes
            .borrow()
            .as_deref()
            .map(|bytes| Record::decode(bytes).expect("a record"))
    }
}

impl StateDir for Mem {
    fn lock(&self) -> Result<Lock, PortError> {
        Ok(Lock::new(()))
    }

    fn load(&self) -> Result<Option<Vec<u8>>, PortError> {
        if self.fail_load.get() {
            return Err(PortError::failed(PortKind::State, "unreadable"));
        }
        Ok(self.bytes.borrow().clone())
    }

    fn save(&self, bytes: &[u8]) -> Result<(), PortError> {
        if self.fail_save.get() {
            return Err(PortError::failed(PortKind::State, "disk full"));
        }
        *self.bytes.borrow_mut() = Some(bytes.to_vec());
        Ok(())
    }
}

fn build() -> String {
    lin::version_of(lin::OLD)
}

/// The release's record as the update hook left it: serving `live`, with the update that
/// installed the binary still named in `to` and `version`.
fn release_record(live: Instance) -> Record {
    Record {
        markers: BTreeSet::from([Marker::Swapped, Marker::Verified, Marker::Stopping]),
        to: Some(PathBuf::from(lin::OLD)),
        version: Some(build()),
        ..Record::idle(Some(live))
    }
}

/// A record with an update in flight and no marker yet.
fn in_flight(live: Option<Instance>) -> Record {
    Record {
        updating: true,
        to: Some(PathBuf::from(PKG_NEW)),
        version: Some(build()),
        ..Record::idle(live)
    }
}

fn staged(config: &str) -> PathBuf {
    staged_copy(Path::new(PKG_NEW), Path::new(config))
}

fn with_interface(interface: &str) -> String {
    format!("interface = \"{interface}\"\n{}", lin::WITH_SOCKET)
}

fn with_socket(socket: &str) -> String {
    format!("[server]\nhandoff_socket = \"{socket}\"\n")
}

/// The mount the fake control socket names, and the one of a process before it, whose gauge the
/// node still shows.
const MOUNT: &str = "mount-7";
const OLD_MOUNT: &str = "mount-6";

/// The control socket of the instance that serves: it records each request, takes a `set` of the
/// cap of the tree lists, which the node's gauge then shows (unless the gauge `lags`), and answers
/// `answer` when one is given, or fails when `broken`.
struct FakeControl {
    asked: RefCell<Vec<(PathBuf, String)>>,
    gauge: Cell<Option<u64>>,
    lags: Cell<bool>,
    answer: RefCell<Option<String>>,
    broken: Cell<bool>,
}

impl FakeControl {
    fn new() -> FakeControl {
        FakeControl {
            asked: RefCell::new(Vec::new()),
            gauge: Cell::new(None),
            lags: Cell::new(false),
            answer: RefCell::new(None),
            broken: Cell::new(false),
        }
    }

    /// The requests, in order, each with the socket it went to.
    fn asked(&self) -> Vec<(PathBuf, String)> {
        self.asked.borrow().clone()
    }
}

impl Control for FakeControl {
    fn ask(&self, socket: &Path, line: &str) -> Result<String, PortError> {
        self.asked
            .borrow_mut()
            .push((socket.to_path_buf(), line.to_owned()));
        if self.broken.get() {
            return Err(PortError::failed(PortKind::Instances, "connection refused"));
        }
        if let Some(answer) = self.answer.borrow().clone() {
            return Ok(answer);
        }
        let bytes: u64 = line
            .strip_prefix("set tree_list_ram_bytes ")
            .and_then(|bytes| bytes.parse().ok())
            .unwrap_or_else(|| panic!("not a set: {line}"));
        if !self.lags.get() {
            self.gauge.set(Some(bytes));
        }
        Ok(format!(
            "{{\"ok\":true,\"tree_list_ram_bytes\":{bytes},\"mount\":\"{MOUNT}\"}}"
        ))
    }
}

/// The instances' answers of the fake, with the gauges of the cap of the tree lists on
/// `/metricz`: the mount of the process before at 1 MiB, and [`MOUNT`] at what the control
/// socket set.
struct Gauged<'a> {
    fake: &'a lin::SystemdFake,
    control: &'a FakeControl,
}

impl Instances for Gauged<'_> {
    fn get(&self, port: u16, path: &str) -> Result<Reply, PortError> {
        let mut reply = self.fake.get(port, path)?;
        if path == "/metricz" {
            reply.body.push_str(&format!(
                "tierstore_fs_tree_list_ram_bytes{{mount=\"{OLD_MOUNT}\"}} 1048576\n"
            ));
            if let Some(bytes) = self.control.gauge.get() {
                reply.body.push_str(&format!(
                    "tierstore_fs_tree_list_ram_bytes{{mount=\"{MOUNT}\"}} {bytes}\n"
                ));
            }
        }
        Ok(reply)
    }
}

/// A node of two systemd instances: `A` serves the binary of `lin::OLD`, the config hook's record
/// is the fake's, the release's record is `service`.
struct Linux {
    fake: lin::SystemdFake,
    files: Files,
    guard: CountingGuard,
    control: FakeControl,
    service: Mem,
}

impl Linux {
    /// A node serving with the file `serving` (none: the file is missing) and a config package
    /// that stages `staged_text`.
    fn new(serving: Option<&str>, staged_text: &str) -> Linux {
        Linux::on(lin::SystemdFake::installed(), &[], serving, staged_text)
    }

    /// The same on the machine `fake`, whose running release lists `extra` words besides the
    /// three of the hand-over of the store and `handoff-service-v1`, as a release from this one on
    /// does.
    fn on(
        fake: lin::SystemdFake,
        extra: &[&str],
        serving: Option<&str>,
        staged_text: &str,
    ) -> Linux {
        let mut words = vec![
            HANDOFF_CAPABILITY,
            CACHE_HANDOFF_CAPABILITY,
            HANDOFF_STATE_DIR_CAPABILITY,
            crate::server::update_hook::HANDOFF_SERVICE_CAPABILITY,
        ];
        words.extend_from_slice(extra);
        Linux::listing(fake, &words, serving, staged_text)
    }

    /// The same, whose running release lists exactly `words`.
    fn listing(
        fake: lin::SystemdFake,
        words: &[&str],
        serving: Option<&str>,
        staged_text: &str,
    ) -> Linux {
        let files = Files::new(words);
        if let Some(text) = serving {
            files.put(Path::new(lin::CONFIG), text);
        }
        files.put(&staged(lin::CONFIG), staged_text);
        Linux {
            fake,
            files,
            guard: CountingGuard::new(),
            control: FakeControl::new(),
            service: Mem::holding(Some(&release_record(Instance::A))),
        }
    }

    /// A node like [`Linux::alike`] that also has an fs process, instance `A`, and whose running
    /// release lists `handoff-fs-v1`.
    fn with_fs() -> Linux {
        Linux::on(
            lin::SystemdFake::fs_installed(),
            &[crate::server::update_hook::HANDOFF_FS_CAPABILITY],
            Some(lin::WITH_SOCKET),
            &format!("{}proxy_id = \"p2\"\n", lin::WITH_SOCKET),
        )
    }

    /// A node whose config package changes nothing a hand-over cares about.
    fn alike() -> Linux {
        Linux::new(
            Some(lin::WITH_SOCKET),
            &format!("{}proxy_id = \"p2\"\n", lin::WITH_SOCKET),
        )
    }

    fn step(&self, step: Step, params: &Params) -> Done {
        let build = build();
        let running = Running {
            host: &self.files,
            release: Path::new(lin::OLD),
        };
        let ports = SystemdPorts {
            units: &self.fake,
            markers: &self.fake,
            fs_units: self.fake.fs_ports(),
            fs_markers: self.fake.fs_ports(),
            fs_processes: self.fake.fs_ports(),
            instances: &Gauged {
                fake: &self.fake,
                control: &self.control,
            },
            host: &running,
            provider: &self.fake.provider,
            clock: &self.fake,
            state: &self.fake,
        };
        let common = Common {
            service: &self.service,
            release: Path::new(lin::OLD),
            build: &build,
        };
        systemd_step(step, params, &ports, &self.guard, &self.control, &common)
    }

    /// Runs `steps`, each of which must succeed.
    fn go(&self, params: &Params, steps: &[Step]) {
        for step in steps {
            lin::assert_ok(&self.step(*step, params));
        }
    }

    fn reloads(&self) -> u32 {
        self.guard.reloads.get()
    }
}

fn update() -> Params {
    lin::params(PKG_NEW, Some(PKG_OLD))
}

fn first() -> Params {
    lin::params(PKG_NEW, None)
}

fn committed_record(live: Instance) -> Record {
    Record {
        updating: true,
        markers: BTreeSet::from([Marker::Swapped, Marker::Verified, Marker::Stopping]),
        to: Some(PathBuf::from(PKG_NEW)),
        version: Some(build()),
        live: Some(live),
        next: Some(live.other()),
        mode: Some(Mode::Handover),
        ..Record::idle(None)
    }
}

/// Runs a config update from a node serving `serving` to the file `staged_text` and asserts that
/// it is a restart: the old instance is stopped before the new one starts, and cleanup leaves the
/// new one alone.
fn assert_restarts(serving: Option<&str>, staged_text: &str, params: &Params) {
    let node = Linux::new(serving, staged_text);
    node.go(params, &[Prepare]);
    assert_eq!(node.fake.record().mode, Some(Mode::Restart));
    node.go(params, &[Cutover]);
    assert_eq!(
        node.fake.actions(),
        ["units.stop a", "markers.set b", "units.start b"]
    );
    assert_eq!(node.fake.running(), [Instance::B]);
    node.go(params, &[Verify, Cleanup]);
    assert_eq!(node.fake.running(), [Instance::B]);
    assert_eq!(node.fake.markers(), [Instance::B]);
    assert_eq!(node.fake.record().live, Some(Instance::B));
}

mod macos;
mod reload;
mod switch_macos;
mod switch_systemd;
mod systemd_rollback;
mod systemd_update;

/// Asserts that the write-back after `cleanup` leaves the release's record alone when the config
/// hook's record is absent, cannot be decoded, or names no serving instance.
#[test]
fn a_config_record_that_names_no_instance_writes_nothing_back() {
    let release = Record::idle(Some(super::super::Instance::A));
    let unreadable = Mem::holding(None);
    *unreadable.bytes.borrow_mut() = Some(b"not json".to_vec());
    for config in [
        Mem::holding(None),
        unreadable,
        Mem::holding(Some(&Record::idle(None))),
    ] {
        let service = Mem::holding(Some(&release));
        super::steps::write_back(&config, &service).unwrap();
        assert_eq!(service.get(), Some(release.clone()));
    }
}

/// Asserts that the config hook's host puts no binary in place and removes none, and does not
/// ask the host to: a config package carries no binary, so the binary that serves stays. (The
/// fake host itself refuses both, so the answers are the config hook's own.) Catches a config
/// update that copies a file of the config package over the service's binary, or removes it.
#[test]
fn the_config_hook_host_never_puts_or_removes_a_binary() {
    let files = Files::new(&[]);
    let running = Running {
        host: &files,
        release: Path::new("/"),
    };
    assert_eq!(running.install_binary(Path::new(PKG_NEW)), Ok(()));
    assert_eq!(running.remove_binary(), Ok(()));
    assert!(files.install_binary(Path::new(PKG_NEW)).is_err());
    assert!(files.remove_binary().is_err());
}
