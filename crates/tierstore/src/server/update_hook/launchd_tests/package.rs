//! A Mac that runs the macOS installer package, updated through the layout the release ships and
//! hostkeeper stages: the payload is the one `release macos-pkg` lays out (its binary at the
//! program of the package's plist, `packaging/macos-pkg/tierstore.plist`, mode 0755, and that plist
//! at `/Library/LaunchDaemons/tierstore.plist`, mode 0644, in directories of mode 0755; xtask's
//! tests hold its payload to the path the hook looks at), staged as hostkeeper's `macos-pkg`
//! installer stages a version (the expanded payload in the version directory's `tree/`, every
//! entry its owner's and writable by neither group nor others, the version directory 0700, root's
//! alone: hostkeeper's `VERSION_MODE`), and installed as its `activate` installs it (each file of
//! the payload put at its path under the root, a new file in place of the old). The params are
//! those of the manifest the release sends for a Mac (xtask's `macos_pkg::manifest_json`) and the node's
//! file is the Mac's own (`packaging/macos/tierstore.toml`, one process with an `fs` interface).
//!
//! What is real: the node's mount provider and the hook's client of it, on the socket of the
//! Mac's file, with the volume the provider's trees appear in stood in for
//! (`package_provider.rs`); the hook's steps, its host (`SystemHost` as `host_for` builds it for launchd,
//! with the ownership, mode and not-a-link checks of every file it runs), its launchd port
//! (`LaunchctlJobs`) and its record (`StateFiles`), all under a scratch root. What stands in: the
//! programs the hook runs, answered by [`Mac`] (`launchctl`, `dscl`, `sudo`, `mount` and a
//! release's binary), the job's HTTP answers and the clock. The stand-in of `sudo` and of
//! launchd's start of the job run a program as `_tierstore` only when every directory down to it
//! lets others search it and the file lets others execute it, as the kernel decides for an
//! account that is neither the owner nor in the group; nothing here runs on a real launchd.
//!
//! The Mac starts as the four farm Macs stand: hostkeeper's current release is 0.3.202, installed
//! from its package and served by the one job; the hook's record is an update from 0.3.202 to
//! 0.3.203 in flight with no marker; one Mac also has the 0.3.208 package staged, whose `prepare`
//! failed (`host_failed`) and wrote nothing.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::fake::{assert_failed, assert_ok};
use super::package_provider::{Arranged, FILE_FRONT, PackageProvider, SOCKET};
use crate::server::build_info::gauge;
use crate::server::provider::check::WITHOUT_SIDECAR;
use crate::server::update_hook::launchd::run_step_with;
use crate::server::update_hook::machine::testing::{exit, me};
use crate::server::update_hook::machine::{
    Call, DSCL_PROGRAM, LAUNCHCTL_PROGRAM, LaunchctlJobs, LaunchdTimes, Output, RunError, Runner,
    SUDO_PROGRAM, StateFiles,
};
use crate::server::update_hook::system::{
    BOOTOUT_TIMEOUT, GONE_ATTEMPTS, LAUNCHCTL_TIMEOUT, SYSTEMCTL_PROGRAM, SystemConfig, host_for,
};
use crate::server::update_hook::{
    Clock, Done, HANDOFF_EXEC_CAPABILITY, Instances, Invocation, LAUNCHD_PLIST, LaunchdPorts,
    Machine, Marker, Params, PortError, PortKind, Reason, Record, Reply, STATE_DIR, StateDir, Step,
};

/// Where hostkeeper keeps the versions of the service.
const PACKAGES: &str = "/var/lib/hostkeeper/packages/tierstore";
/// The release hostkeeper's `current` names on the farm Macs.
pub(super) const CURRENT: &str = "0.3.202";
/// The release of the update the hook's record holds in flight on them.
const STALE: &str = "0.3.203";
/// The release whose `prepare` failed on one of them.
const FAILED: &str = "0.3.208";
/// The release this repository ships next.
pub(super) const NEXT: &str = "0.3.209";
/// The commit every stand-in build names in its build gauge.
const COMMIT: &str = "0000000000d1";
/// The user id `dscl` gives `_tierstore`.
const ACCOUNT_UID: u32 = 299;
/// Where macOS has `mount` (`SystemHost::mounted` runs it).
const MOUNT_PROGRAM: &str = "/sbin/mount";
/// The node's file of a Mac: one process, with an `fs` interface.
const NODE_FILE: &str = include_str!("../../../../../../packaging/macos/tierstore.toml");
/// The mount point of that file's `fs` interface.
pub(super) const MOUNT: &str = "/private/var/tierstore/fs";
/// The plist the macOS installer package installs.
const PACKAGE_PLIST: &str = include_str!("../../../../../../packaging/macos-pkg/tierstore.plist");
/// The label of the package's job and the manifest's `launchd_label`.
const LABEL: &str = "tierstore";
/// The node's file the manifest's `config_file` names on a Mac.
const CONFIG_FILE: &str = "/usr/local/tierstore/etc/tierstore.toml";

/// Where the package installs its binary: the program of its plist.
fn package_binary() -> String {
    plist_program(PACKAGE_PLIST).expect("the package's plist names its program")
}

/// The directory of the copy the archive package's plist runs, which a Mac of the installer
/// package never runs.
const ARCHIVE_COPY_DIR: &str = "/usr/local/libexec/tierstore";

/// The version directory hostkeeper stages `version` in, as the params name it.
fn release(version: &str) -> String {
    format!("{PACKAGES}/{version}")
}

/// The bytes of the stand-in binary of `version`: what it reports and lists.
fn program_text(version: &str) -> String {
    format!(
        "tierstore stand-in\nversion {version}\ncommit {COMMIT}\ncapabilities {HANDOFF_EXEC_CAPABILITY}\n"
    )
}

/// What a stand-in binary is: its version and its capabilities.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Image {
    version: String,
    capabilities: Vec<String>,
}

/// The image of the stand-in binary `text`, or `None` for one that names no version.
fn image_of(text: &str) -> Option<Image> {
    let mut version = None;
    let mut capabilities = Vec::new();
    for line in text.lines() {
        match line.split_once(' ') {
            Some(("version", v)) => version = Some(v.to_owned()),
            Some(("capabilities", words)) => {
                capabilities = words.split(' ').map(str::to_owned).collect();
            }
            _ => {}
        }
    }
    Some(Image {
        version: version?,
        capabilities,
    })
}

/// The first string of the `ProgramArguments` of the plist `text`.
fn plist_program(text: &str) -> Option<String> {
    let rest = text.split("<key>ProgramArguments</key>").nth(1)?;
    let rest = rest.split("<string>").nth(1)?;
    Some(rest.split("</string>").next()?.to_owned())
}

fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .permissions()
        .mode()
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

/// Copies the tree `from` to `to` as hostkeeper's stage copies an expanded payload: each entry
/// with its setuid, setgid and sticky bits and its group and others' write bits taken away.
fn stage_copy(from: &Path, to: &Path) {
    let mode = mode_of(from) & 0o755;
    if fs::symlink_metadata(from).expect("stat").is_dir() {
        fs::create_dir(to).expect("mkdir");
        for entry in fs::read_dir(from).expect("read_dir") {
            let entry = entry.expect("entry");
            stage_copy(&entry.path(), &to.join(entry.file_name()));
        }
    } else {
        fs::copy(from, to).expect("copy");
    }
    set_mode(to, mode);
}

/// Puts every file of the tree `tree` at its path under `root`, as `installer` does: the
/// directories made when absent, each file a new one renamed over the old.
fn install_copy(tree: &Path, root: &Path) {
    for entry in fs::read_dir(tree).expect("read_dir") {
        let entry = entry.expect("entry");
        let from = entry.path();
        let to = root.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            if !to.exists() {
                fs::create_dir(&to).expect("mkdir");
                set_mode(&to, mode_of(&from));
            }
            install_copy(&from, &to);
        } else {
            let new = to.with_extension("installer-new");
            fs::copy(&from, &new).expect("copy");
            set_mode(&new, mode_of(&from));
            fs::rename(&new, &to).expect("rename");
        }
    }
}

/// What a stand-in program answers.
type Answer = Result<Output, RunError>;

/// The one job of the system domain: its process, the program its plist names, and what that
/// program runs, if `_tierstore` could execute it.
#[derive(Clone, Debug)]
struct Job {
    pid: u32,
    program: String,
    image: Option<Image>,
}

/// A Mac under a scratch root, and the stand-ins of its programs.
pub(super) struct Mac {
    _dir: TestDir,
    root: PathBuf,
    /// Where the packages are built, outside the root.
    work: PathBuf,
    job: RefCell<Option<Job>>,
    next_pid: Cell<u32>,
    calls: RefCell<Vec<Call>>,
    now: Cell<u64>,
    /// The node's mount provider, on the socket its file names.
    pub(super) provider: PackageProvider,
}

impl Mac {
    /// A Mac with hostkeeper's packages directory (0711) and service directory (0711), the node's
    /// file, and the namespace `trees` that tierstore's file system has at the mount point.
    fn new(name: &str) -> Mac {
        Mac::with_provider(name, Arranged::Built)
    }

    /// [`Mac::new`], whose provider's objects and placements stand as `arranged` says.
    pub(super) fn with_provider(name: &str, arranged: Arranged) -> Mac {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let root = dir.path().join("host");
        let work = dir.path().join("work");
        fs::create_dir_all(&work).expect("work");
        fs::create_dir_all(&root).expect("root");
        set_mode(&root, 0o755);
        let provider = PackageProvider::new(name, &root, MOUNT, arranged);
        let mac = Mac {
            _dir: dir,
            root,
            work,
            job: RefCell::new(None),
            next_pid: Cell::new(400),
            calls: RefCell::new(Vec::new()),
            now: Cell::new(0),
            provider,
        };
        let service = mac.at(PACKAGES);
        fs::create_dir_all(&service).expect("packages");
        set_mode(&service, 0o711);
        set_mode(service.parent().expect("packages"), 0o711);
        let config = mac.at(CONFIG_FILE);
        fs::create_dir_all(config.parent().expect("etc")).expect("etc");
        fs::write(&config, NODE_FILE).expect("node file");
        // Searchable by others whatever the umask, as on a Mac.
        for dir in ["/usr", "/usr/local"] {
            set_mode(&mac.at(dir), 0o755);
        }
        fs::create_dir_all(mac.at(MOUNT).join("trees")).expect("mount point");
        mac
    }

    /// `path` (absolute on the Mac) under the root.
    fn at(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    /// The payload of the package of `version`, laid out as `release macos-pkg` lays it out, and
    /// staged as hostkeeper's `macos-pkg` installer stages it.
    pub(super) fn stage(&self, version: &str) {
        let payload = self.work.join(format!("{version}.payload"));
        let files = [
            (package_binary(), 0o755, program_text(version)),
            (LAUNCHD_PLIST.to_owned(), 0o644, PACKAGE_PLIST.to_owned()),
        ];
        for (path, mode, text) in files {
            let at = payload.join(path.trim_start_matches('/'));
            let mut dir = payload.clone();
            for part in Path::new(&path)
                .parent()
                .expect("a directory")
                .components()
                .skip(1)
            {
                dir.push(part);
                fs::create_dir_all(&dir).expect("payload directory");
                set_mode(&dir, 0o755);
            }
            fs::write(&at, text).expect("payload file");
            set_mode(&at, mode);
        }
        set_mode(&payload, 0o755);
        let dir = self.at(&release(version));
        fs::create_dir(&dir).expect("version directory");
        stage_copy(&payload, &dir.join("tree"));
        set_mode(&dir, 0o700);
    }

    /// hostkeeper's `activate` of the staged `version` (and its rollback, the `activate` of the
    /// kept previous version): its payload installed at `/`.
    pub(super) fn activate(&self, version: &str) {
        install_copy(&self.at(&release(version)).join("tree"), &self.root);
    }

    /// The record of the hook, written as a step writes it.
    fn write_record(&self, record: &Record) {
        let state = self.state();
        let _lock = state.lock().expect("lock");
        state.save(&record.encode()).expect("save");
    }

    pub(super) fn record(&self) -> Record {
        let state = self.state();
        let bytes = state.load().expect("load").expect("a record");
        Record::decode(&bytes).expect("decode")
    }

    fn state(&self) -> StateFiles {
        StateFiles::new(self.at(STATE_DIR), me())
    }

    /// The job's process and the version it serves, if it serves.
    pub(super) fn serving(&self) -> Option<(u32, String)> {
        let job = self.job.borrow();
        let job = job.as_ref()?;
        Some((job.pid, job.image.as_ref()?.version.clone()))
    }

    /// The `launchctl` subcommands the hook ran, in order.
    fn launchctl_verbs(&self) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .filter(|call| call.program == LAUNCHCTL_PROGRAM)
            .map(|call| call.args[0].clone())
            .collect()
    }

    /// The files left under [`ARCHIVE_COPY_DIR`].
    fn archive_copies(&self) -> Vec<PathBuf> {
        let dir = self.at(ARCHIVE_COPY_DIR);
        if !dir.exists() {
            return Vec::new();
        }
        fs::read_dir(dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").path())
            .collect()
    }

    /// Runs `step` of the hook of a release, as hostkeeper runs it on a Mac (root).
    pub(super) fn step(&self, step: Step, params: &Params) -> Done {
        let config = SystemConfig {
            root: self.root.clone(),
            owner: me(),
            release_owner: me(),
            hook_uid: 0,
            systemctl: SYSTEMCTL_PROGRAM,
            launchctl: LAUNCHCTL_PROGRAM,
            poll: Duration::from_millis(1),
            runner: self,
        };
        let host = host_for(&config, &params.service_manager);
        let times = LaunchdTimes {
            call: LAUNCHCTL_TIMEOUT,
            bootout: BOOTOUT_TIMEOUT,
            gone_attempts: GONE_ATTEMPTS,
            poll: Duration::from_secs(1),
        };
        let launchd = LaunchctlJobs::new(self, self, times);
        let state = self.state();
        let ports = LaunchdPorts {
            launchd: &launchd,
            instances: self,
            host: &host,
            provider: &self.provider,
            clock: self,
            state: &state,
        };
        let label = LABEL;
        run_step_with(step, params, label, &ports, true)
    }

    /// Whether `_tierstore`, neither the owner of anything under the root nor in its group, can
    /// execute `path` (a path under the root): every directory from the root down lets others
    /// search it, and the file lets others execute it.
    fn account_reaches(&self, path: &Path) -> bool {
        let below = path
            .strip_prefix(&self.root)
            .expect("a path under the root");
        let mut at = self.root.clone();
        for part in below.components() {
            if mode_of(&at) & 0o001 == 0 {
                return false;
            }
            at.push(part);
        }
        fs::symlink_metadata(&at).is_ok_and(|meta| meta.permissions().mode() & 0o001 != 0)
    }

    /// What `program` (absolute on the Mac) runs when launchd starts it as `_tierstore`.
    fn image_for(&self, program: &str) -> Option<Image> {
        let at = self.at(program);
        let text = fs::read_to_string(&at)
            .ok()
            .filter(|_| self.account_reaches(&at))?;
        image_of(&text)
    }

    fn launchctl(&self, args: &[&str]) -> Answer {
        let target = format!("system/{}", LABEL);
        let loaded = self.job.borrow().is_some();
        match args {
            ["print", label] => {
                assert_eq!(*label, target);
                if loaded {
                    exit(0, "state = running\n", "")
                } else {
                    exit(113, "", "Could not find service")
                }
            }
            ["bootstrap", "system", plist] => {
                assert_eq!(*plist, LAUNCHD_PLIST);
                assert!(!loaded, "the hook bootstraps a job that is loaded");
                let text = fs::read_to_string(self.at(plist)).expect("the plist is installed");
                let program = plist_program(&text).expect("a program");
                let pid = self.next_pid.get();
                self.next_pid.set(pid + 1);
                let image = self.image_for(&program);
                *self.job.borrow_mut() = Some(Job {
                    pid,
                    program,
                    image,
                });
                exit(0, "", "")
            }
            ["bootout", label] => {
                assert_eq!(*label, target);
                *self.job.borrow_mut() = None;
                exit(0, "", "")
            }
            // The process executes its program again, under the same pid: the file now at the
            // plist's path.
            ["kill", "SIGUSR2", label] => {
                assert_eq!(*label, target);
                let program = self
                    .job
                    .borrow()
                    .as_ref()
                    .expect("a loaded job")
                    .program
                    .clone();
                let image = self.image_for(&program);
                self.job.borrow_mut().as_mut().expect("a loaded job").image = image;
                exit(0, "", "")
            }
            other => panic!("launchctl {other:?} is not a call of this hook"),
        }
    }

    /// `sudo -n -u #<uid> -- <program> --config <file> --check-config`: the program as
    /// `_tierstore`, which `sudo` refuses to execute when the account cannot reach it.
    fn sudo(&self, args: &[&str]) -> Answer {
        let uid = format!("#{ACCOUNT_UID}");
        assert_eq!(args.len(), 8, "sudo {args:?}");
        assert_eq!(args[..4], ["-n", "-u", uid.as_str(), "--"]);
        assert_eq!([args[5], args[7]], ["--config", "--check-config"]);
        let program = Path::new(args[4]);
        if !self.account_reaches(program) {
            return exit(
                1,
                "",
                &format!(
                    "sudo: unable to execute {}: Permission denied\n",
                    program.display()
                ),
            );
        }
        let text = fs::read_to_string(program).expect("the program is there");
        assert!(
            image_of(&text).is_some(),
            "{} is a binary",
            program.display()
        );
        assert!(Path::new(args[6]).is_file(), "the node's file is there");
        exit(0, "", "")
    }

    /// What `mount` lists: the root, and the mount of the node's `fs` interface while the job's
    /// process serves.
    fn mounts(&self) -> String {
        let served = self
            .serving()
            .map(|_| {
                format!("localhost:/ on {MOUNT} (nfs, nodev, nosuid, mounted by _tierstore)\n")
            })
            .unwrap_or_default();
        format!("/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n{served}")
    }

    /// A program the hook runs as itself (root): a release's binary, asked its capabilities.
    fn as_hook(&self, program: &str, args: &[&str]) -> Answer {
        assert_eq!(
            args,
            ["--capabilities"],
            "a root hook on a Mac runs a release's binary as itself only to ask its capabilities"
        );
        let text = fs::read_to_string(program).expect("the release's binary is there");
        let image = image_of(&text).expect("a stand-in binary");
        exit(0, &format!("{}\n", image.capabilities.join("\n")), "")
    }
}

impl Runner for Mac {
    fn run(&self, call: &Call) -> Result<Output, RunError> {
        self.calls.borrow_mut().push(call.clone());
        let args: Vec<&str> = call.args.iter().map(String::as_str).collect();
        match call.program.as_str() {
            LAUNCHCTL_PROGRAM => self.launchctl(&args),
            DSCL_PROGRAM => exit(
                0,
                &format!("UniqueID: {ACCOUNT_UID}\nPrimaryGroupID: {ACCOUNT_UID}\n"),
                "",
            ),
            SUDO_PROGRAM => self.sudo(&args),
            MOUNT_PROGRAM => exit(0, &self.mounts(), ""),
            program => self.as_hook(program, &args),
        }
    }
}

impl Instances for Mac {
    fn get(&self, port: u16, path: &str) -> Result<Reply, PortError> {
        let (_, version) = self
            .serving()
            .filter(|_| port == 9300)
            .ok_or_else(|| PortError::failed(PortKind::Instances, "connection refused"))?;
        let body = if path == "/readyz" {
            "ready read-write".to_owned()
        } else {
            assert_eq!(path, "/metricz");
            gauge(&version, COMMIT, None)
        };
        Ok(Reply { status: 200, body })
    }
}

impl Clock for Mac {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }

    fn pause(&self, span: Duration) {
        let span = u64::try_from(span.as_millis()).unwrap_or(u64::MAX);
        self.now.set(self.now.get() + span);
    }
}

/// The params hostkeeper writes for the update to `version`, from `previous` when there is one:
/// the keys of the manifest the release sends for a Mac, and the updater's.
fn params_text(version: &str, previous: Option<&str>) -> String {
    let mut text = format!(
        "config_file = \"{CONFIG_FILE}\"\nservice_manager = \"launchd\"\nlaunchd_label = \"{LABEL}\"\n"
    );
    text.push_str(&format!(
        "\"updater.staged_release\" = \"{}\"\n\"updater.target_version\" = \"{version}\"\n",
        release(version)
    ));
    if let Some(previous) = previous {
        text.push_str(&format!(
            "\"updater.previous_release\" = \"{}\"\n\"updater.previous_version\" = \"{previous}\"\n",
            release(previous)
        ));
    }
    text
}

/// [`params_text`], parsed.
pub(super) fn params(version: &str, previous: Option<&str>) -> Params {
    Params::parse(&params_text(version, previous)).expect("params")
}

/// The record the farm Macs hold: the update from 0.3.202 to 0.3.203, in flight, no marker.
fn stale_record() -> Record {
    Record {
        updating: true,
        from: Some(PathBuf::from(release(CURRENT))),
        to: Some(PathBuf::from(release(STALE))),
        version: Some(STALE.to_owned()),
        ..Record::idle(None)
    }
}

/// A Mac as the farm Macs stand: 0.3.202 staged, installed and served by the job, and the
/// hook's record of the update to 0.3.203 in flight.
fn farm_mac(name: &str) -> Mac {
    farm_mac_with(Mac::new(name))
}

/// [`farm_mac`] of `mac`.
pub(super) fn farm_mac_with(mac: Mac) -> Mac {
    mac.stage(CURRENT);
    mac.activate(CURRENT);
    let booted = mac.launchctl(&["bootstrap", "system", LAUNCHD_PLIST]);
    assert_eq!(booted.expect("bootstrap").code, Some(0));
    assert_eq!(mac.serving().map(|s| s.1).as_deref(), Some(CURRENT));
    mac.write_record(&stale_record());
    mac
}

/// The update of `mac` from 0.3.202 to 0.3.209, step by step as hostkeeper runs it, each step
/// checked: `prepare` plans the hand-over and stops nothing; hostkeeper's `activate` installs the
/// package; `cutover` sends SIGUSR2 and the same process serves 0.3.209; `verify` sees the volume;
/// `cleanup` commits. No step boots the job out or in, and nothing is left under the archive
/// package's copy directory.
fn updates_by_hand_over(mac: &Mac) {
    let (pid, _) = mac.serving().expect("the job serves");
    mac.stage(NEXT);
    let params = params(NEXT, Some(CURRENT));

    let done = mac.step(Step::Prepare, &params);
    assert_ok(&done);
    assert_eq!(
        done.report.detail.as_deref(),
        Some(
            format!(
                "prepared {} to follow {}: tierstore hands over to the next image of its process in cutover",
                release(NEXT),
                release(CURRENT)
            )
            .as_str()
        )
    );
    assert_eq!(
        mac.serving(),
        Some((pid, CURRENT.to_owned())),
        "prepare stops nothing"
    );

    mac.activate(NEXT);
    assert_ok(&mac.step(Step::Cutover, &params));
    assert_eq!(
        mac.serving(),
        Some((pid, NEXT.to_owned())),
        "the same process serves"
    );
    let verified = mac.step(Step::Verify, &params);
    assert_ok(&verified);
    // The node's provider, on the socket of the Mac's file, placed both trees of the check under
    // the file's mount point, they read back from the NFS front of the file (never the volume,
    // which macOS does not let the hook's process read), and it released them.
    let said = verified.report.detail.clone().unwrap_or_default();
    assert!(
        said.starts_with(&format!(
            "the mount provider at {SOCKET} placed, served and released a tree with its L2 \
             sidecar and one without an L2 sidecar, read from the NFS front at {FILE_FRONT}; "
        )),
        "{said}"
    );
    assert_eq!(mac.provider.placements(), 0);
    assert_ok(&mac.step(Step::Cleanup, &params));
    let record = mac.record();
    assert!(!record.updating, "{record:?}");
    assert!(record.committed(), "{record:?}");
    let changed: Vec<String> = mac
        .launchctl_verbs()
        .into_iter()
        .filter(|verb| verb != "print")
        .collect();
    assert_eq!(
        changed,
        ["kill"],
        "one SIGUSR2, and no bootout or bootstrap"
    );
    assert_eq!(mac.archive_copies(), Vec::<PathBuf>::new());
}

/// Asserts that a farm Mac (0.3.202 from the installer package, the stale record of 0.3.203)
/// takes the next release through every step: `prepare` finds both releases' binaries in the
/// trees hostkeeper staged and has the new one check the node's file as `_tierstore`, `cutover`
/// hands the job's process over to the binary `activate` installed, and `verify` and `cleanup`
/// commit. Catches a `prepare` that looks for the staged binary only at `bin/tierstore` and the
/// Debian package's path (`host_failed`, "... is not there, nor ..."), and a check of the file
/// that runs the staged binary inside the version directory, which hostkeeper keeps closed to
/// every user but root ("Permission denied").
#[test]
fn a_farm_mac_updates_from_its_package_by_handing_over_to_the_next_package() {
    let mac = farm_mac("mac-package-update");
    updates_by_hand_over(&mac);
}

/// Asserts that the farm Mac that also has the failed 0.3.208 attempt staged (its `prepare`
/// wrote nothing, so the hook's record still holds the update to 0.3.203) takes the next release
/// the same way, and that the 0.3.208 version directory is not read.
#[test]
fn a_farm_mac_with_a_failed_attempt_staged_updates_the_same_way() {
    let mac = farm_mac("mac-package-failed");
    mac.stage(FAILED);
    assert_eq!(mac.record(), stale_record());
    updates_by_hand_over(&mac);
    let failed = mac.at(&release(FAILED)).to_string_lossy().into_owned();
    let calls = mac.calls.borrow();
    let words: Vec<&String> = calls
        .iter()
        .flat_map(|call| std::iter::once(&call.program).chain(&call.args))
        .collect();
    assert!(
        words.iter().all(|word| !word.starts_with(&failed)),
        "{words:?}"
    );
}

/// Asserts that a farm Mac whose next release's mount provider refuses the tree without an L2
/// sidecar (its list is not in the store, as a release that refuses every such tree answers) fails
/// `verify` with `provider_refused` after the volume was proved, records nothing verified and
/// leaves no placement, and that the `rollback` hostkeeper runs next hands the same process back
/// to 0.3.202. Catches a Mac update reported healthy while its provider refuses every job's tree
/// of that form, on the package the release ships and the node file a Mac has.
#[test]
fn a_farm_mac_whose_provider_refuses_a_tree_fails_verify_and_rolls_back() {
    let mac = farm_mac_with(Mac::with_provider(
        "mac-package-refused",
        Arranged::NoPlainList,
    ));
    let (pid, _) = mac.serving().expect("the job serves");
    mac.stage(NEXT);
    let params = params(NEXT, Some(CURRENT));
    assert_ok(&mac.step(Step::Prepare, &params));
    mac.activate(NEXT);
    assert_ok(&mac.step(Step::Cutover, &params));
    let done = mac.step(Step::Verify, &params);
    assert_failed(&done, Reason::ProviderRefused);
    let said = done.report.detail.clone().unwrap_or_default();
    assert!(
        said.starts_with(&format!(
            "the mount provider at {SOCKET} refused Mount of the tree without an L2 sidecar"
        )),
        "{said}"
    );
    assert!(
        said.ends_with(&format!("no tree list at {}", WITHOUT_SIDECAR.key())),
        "{said}"
    );
    assert!(!mac.record().has(Marker::Verified));
    assert_eq!(mac.provider.placements(), 0);
    mac.activate(CURRENT);
    assert_ok(&mac.step(Step::Rollback, &params));
    assert_eq!(mac.serving(), Some((pid, CURRENT.to_owned())));
}

/// Asserts that a rollback after `cutover` (hostkeeper's `activate` of the kept 0.3.202 package
/// first) hands the same process back to 0.3.202 with SIGUSR2 and leaves the record at rest.
/// Catches a rollback that cannot ask the releases of the installer package what they list.
#[test]
fn a_rollback_after_cutover_hands_the_mac_back_to_its_previous_package() {
    let mac = farm_mac("mac-package-rollback");
    let (pid, _) = mac.serving().expect("the job serves");
    mac.stage(NEXT);
    let params = params(NEXT, Some(CURRENT));
    assert_ok(&mac.step(Step::Prepare, &params));
    mac.activate(NEXT);
    assert_ok(&mac.step(Step::Cutover, &params));
    mac.activate(CURRENT);
    assert_ok(&mac.step(Step::Rollback, &params));
    assert_eq!(mac.serving(), Some((pid, CURRENT.to_owned())));
    assert_eq!(mac.record(), Record::idle(None));
    assert_eq!(
        mac.launchctl_verbs()
            .iter()
            .filter(|v| *v == "kill")
            .count(),
        2
    );
    assert!(!mac.launchctl_verbs().iter().any(|v| v == "bootout"));
}

/// Asserts that the boot step of a Mac whose job is not loaded (after a reboot) loads the plist
/// the package installed, which runs the package's binary, and puts no copy anywhere.
#[test]
fn the_boot_step_loads_the_job_of_the_installed_package() {
    let mac = farm_mac("mac-package-restore");
    *mac.job.borrow_mut() = None;
    assert_ok(&mac.step(Step::Restore, &params(CURRENT, None)));
    assert_eq!(mac.serving().map(|s| s.1).as_deref(), Some(CURRENT));
    assert_eq!(mac.archive_copies(), Vec::<PathBuf>::new());
}

/// Asserts that a staged package without the binary at the package's path is refused in
/// `prepare` by name (`host_failed`, the two places a Mac's release may have it), that the job
/// still serves 0.3.202 in the same process, that no `launchctl` call changed anything, and that
/// the record is the one the Mac had.
#[test]
fn a_staged_package_without_its_binary_is_refused_by_name_with_nothing_stopped() {
    let mac = farm_mac("mac-package-missing");
    let (pid, _) = mac.serving().expect("the job serves");
    mac.stage(NEXT);
    let dir = mac.at(&release(NEXT));
    set_mode(&dir, 0o755);
    let binary = dir
        .join("tree")
        .join(package_binary().trim_start_matches('/'));
    fs::remove_file(&binary).expect("remove the binary");
    set_mode(&dir, 0o700);
    let done = mac.step(Step::Prepare, &params(NEXT, Some(CURRENT)));
    assert_failed(&done, Reason::HostFailed);
    assert_eq!(
        done.report.detail.as_deref(),
        Some(
            format!(
                "host: {} is not there, nor {}",
                dir.join("bin/tierstore").display(),
                binary.display()
            )
            .as_str()
        )
    );
    assert_eq!(mac.serving(), Some((pid, CURRENT.to_owned())));
    assert!(mac.launchctl_verbs().iter().all(|v| v == "print"));
    assert_eq!(mac.record(), stale_record());
}

/// Asserts that `uninstall` of a Mac of the installer package boots the job out, leaves the
/// package's files to hostkeeper (the binary and the plist stay where `installer` put them), puts
/// nothing under the archive package's copy directory, and leaves the record at rest.
#[test]
fn uninstall_boots_the_job_out_and_leaves_the_package_to_hostkeeper() {
    let mac = farm_mac("mac-package-uninstall");
    assert_ok(&mac.step(Step::Uninstall, &params(CURRENT, None)));
    assert_eq!(mac.serving(), None);
    assert!(mac.job.borrow().is_none());
    assert!(mac.at(&package_binary()).is_file());
    assert!(mac.at(LAUNCHD_PLIST).is_file());
    assert_eq!(mac.archive_copies(), Vec::<PathBuf>::new());
    assert_eq!(mac.record(), Record::idle(None));
}

/// Asserts that the stand-ins hold `_tierstore` to what it may reach as a Mac does: the binary
/// inside a staged version directory (0700, hostkeeper's) is out of its reach, so `sudo` refuses
/// to execute it, exit 1 and "Permission denied", and the binary `installer` put in place is
/// within it. The update tests rest on this: a check that runs the staged binary fails there.
#[test]
fn the_account_reaches_the_installed_binary_and_not_the_one_in_a_staged_version() {
    let mac = farm_mac("mac-package-reach");
    let staged = mac
        .at(&release(CURRENT))
        .join("tree")
        .join(package_binary().trim_start_matches('/'));
    assert!(staged.is_file());
    assert!(!mac.account_reaches(&staged));
    assert!(mac.account_reaches(&mac.at(&package_binary())));
    let program = staged.to_string_lossy().into_owned();
    let config = mac.at(CONFIG_FILE).to_string_lossy().into_owned();
    let args = [
        "-n",
        "-u",
        "#299",
        "--",
        program.as_str(),
        "--config",
        config.as_str(),
        "--check-config",
    ];
    let refused = mac.sudo(&args).expect("sudo ends");
    let said = format!("sudo: unable to execute {program}: Permission denied\n");
    assert_eq!(
        refused,
        Output {
            code: Some(1),
            stdout: Vec::new(),
            stderr: said.into_bytes(),
        }
    );
}

/// Runs the update hook's command line `tierstore update-hook <words>` on `mac`, as hostkeeper
/// runs it: the params file is read and checked against the arguments, and the step runs on
/// the Mac's own ports. Returns the exit status, the last line as JSON, and the words for a
/// person.
fn hook(mac: &Mac, words: &[&str]) -> (String, serde_json::Value, String) {
    let args: Vec<OsString> = ["tierstore", "update-hook"]
        .iter()
        .chain(words)
        .map(OsString::from)
        .collect();
    let step = |invocation: &Invocation, params: &Params| mac.step(invocation.step, params);
    let machine = Machine { step: &step };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let status = crate::server::update_hook::run(&args, &mut out, &mut err, &machine);
    let out = String::from_utf8(out).expect("UTF-8");
    let line = serde_json::from_str(out.lines().last().expect("a last line")).expect("JSON");
    (
        format!("{status:?}"),
        line,
        String::from_utf8(err).expect("UTF-8"),
    )
}

/// Asserts that the command hostkeeper runs on a farm Mac, `tierstore update-hook prepare
/// <release> --params <file>`, with the params file of the package's manifest, exits 0 with
/// the hand-over prepared in its last line, leaves the job serving 0.3.202 in the same process,
/// and leaves no copy of the binary behind; and that the same command for a staged package without
/// its binary exits 1 with `host_failed`, class `environment`, the two paths in its detail, and
/// the record as it was.
#[test]
fn the_update_hook_command_prepares_a_farm_mac_and_refuses_a_package_without_its_binary() {
    let mac = farm_mac("mac-package-verb");
    let (pid, _) = mac.serving().expect("the job serves");
    mac.stage(NEXT);
    let file = mac.work.join("params.toml");
    fs::write(&file, params_text(NEXT, Some(CURRENT))).expect("params file");
    let file = file.to_string_lossy().into_owned();
    let staged = release(NEXT);

    let (status, line, err) = hook(&mac, &["prepare", &staged, "--params", &file]);
    assert_eq!(status, format!("{:?}", ExitCode::from(0)));
    assert_eq!(
        line["detail"],
        format!(
            "prepared {staged} to follow {}: tierstore hands over to the next image of its process in cutover",
            release(CURRENT)
        )
    );
    assert_eq!(line.get("reason"), None);
    assert_eq!(err, "");
    assert_eq!(mac.serving(), Some((pid, CURRENT.to_owned())));
    assert_eq!(mac.archive_copies(), Vec::<PathBuf>::new());

    let mac = farm_mac("mac-package-verb-missing");
    mac.stage(NEXT);
    let dir = mac.at(&staged);
    set_mode(&dir, 0o755);
    let binary = dir
        .join("tree")
        .join(package_binary().trim_start_matches('/'));
    fs::remove_file(&binary).expect("remove the binary");
    set_mode(&dir, 0o700);
    let file = mac.work.join("params.toml");
    fs::write(&file, params_text(NEXT, Some(CURRENT))).expect("params file");
    let file = file.to_string_lossy().into_owned();
    let (status, line, _) = hook(&mac, &["prepare", &staged, "--params", &file]);
    assert_eq!(status, format!("{:?}", ExitCode::from(1)));
    assert_eq!(line["reason"], "host_failed");
    assert_eq!(line["class"], "environment");
    assert_eq!(
        line["detail"],
        format!(
            "host: {} is not there, nor {}",
            dir.join("bin/tierstore").display(),
            binary.display()
        )
    );
    assert_eq!(mac.record(), stale_record());
}
