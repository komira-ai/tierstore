//! The single-instance steps over the machine's own ports, as a Mac runs them: a scratch root
//! that holds the declaration, the state directory, the release links and the releases where a
//! Mac has them, a stand-in for `launchctl` that keeps launchd's state apart from the process's
//! (is the job loaded, does its process run, and which release; a bootout unloads the job and its
//! process exits a little later), and an instance on a loopback port of its own that answers as
//! the release whose process runs. And the timeouts of the declaration against the bounds of the
//! hook's own work.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::launchd_system::{
    BOOTOUT_TIMEOUT, CONNECT_TIMEOUT, GONE_ATTEMPTS, LAUNCHCTL_TIMEOUT, LaunchdConfig, POLL,
    READ_TIMEOUT, launchd_config, run_on,
};
use super::machine::testing::me;
use super::machine::{Call, LAUNCHCTL_PROGRAM, Output, RunError, Runner, STATE_FILE};
use super::steps::{PROBE_ATTEMPTS, READY_ATTEMPTS};
use super::{
    Done, HostArgs, Invocation, LAUNCHD_DECLARATION_PATH, Outcome, ROOT_UID, Reason, SingleRecord,
    Step,
};

/// The releases, as the host names them; each is a directory under the scratch root with its
/// binary.
const OLD: &str = "/usr/local/tierstore/releases/old";
const NEW: &str = "/usr/local/tierstore/releases/new";
const STATE: &str = "var/db/updater/hooks/tierstore";
const LINKS: &str = "usr/local/tierstore";
/// How many `print`s after a `bootout` launchd still has the job: its process drains and exits.
const EXIT_PRINTS: u32 = 2;

/// launchd's state of the job: whether it is loaded, whether its process runs and on which
/// release, and how many more `print`s an unloading job stays.
#[derive(Clone, Debug, Default)]
struct Job {
    loaded: bool,
    running: Option<PathBuf>,
    exiting: u32,
}

/// The job, which the stand-in changes and the instance answers from.
type Shared = Arc<Mutex<Job>>;

/// `launchctl`, as far as the hook sees it. `print` answers whether the job is loaded; `bootout`
/// unloads it, and its process exits [`EXIT_PRINTS`] `print`s later; `bootstrap` of a job that
/// is not loaded loads it and starts the release `current` names (`RunAtLoad`), and of a loaded
/// one fails; `kickstart` of a loaded job starts its process if it does not run. Every call is
/// kept, and so is every moment the link `current` named another release than the one whose
/// process runs.
struct Launchd {
    links: PathBuf,
    root: PathBuf,
    job: Shared,
    calls: RefCell<Vec<Call>>,
    moved_under_a_process: RefCell<Vec<String>>,
}

fn exited(code: i32) -> Result<Output, RunError> {
    Ok(Output {
        code: Some(code),
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

impl Launchd {
    fn current(&self) -> PathBuf {
        std::fs::read_link(self.links.join("current")).unwrap()
    }
}

impl Runner for Launchd {
    fn run(&self, call: &Call) -> Result<Output, RunError> {
        self.calls.borrow_mut().push(call.clone());
        assert_eq!(call.program, LAUNCHCTL_PROGRAM, "{call:?}");
        let mut job = self.job.lock().unwrap();
        if let Some(running) = &job.running
            && *running != self.current()
        {
            self.moved_under_a_process.borrow_mut().push(format!(
                "{} while {} runs",
                call.args.join(" "),
                running.display()
            ));
        }
        match call.args[0].as_str() {
            "print" => {
                if job.exiting > 0 {
                    job.exiting -= 1;
                    if job.exiting == 0 {
                        job.loaded = false;
                        job.running = None;
                    }
                }
                exited(if job.loaded { 0 } else { 113 })
            }
            "bootout" if job.loaded => {
                job.exiting = EXIT_PRINTS;
                exited(0)
            }
            "bootstrap" if !job.loaded => {
                // The program is `current/tierstore`: launchd follows the link at the start.
                let release = self.current();
                assert!(self.root.join(release.strip_prefix("/").unwrap()).is_dir());
                job.loaded = true;
                job.running = Some(release);
                exited(0)
            }
            "kickstart" if job.loaded => {
                if job.running.is_none() {
                    job.running = Some(self.current());
                }
                exited(0)
            }
            "kickstart" => exited(113),
            "bootout" => exited(3),
            "bootstrap" => exited(37),
            other => panic!("launchctl {other} is not a call the hook makes"),
        }
    }
}

/// Answers one request on `stream` as the release whose process runs.
fn answer(
    mut stream: std::net::TcpStream,
    job: &Shared,
    versions: &BTreeMap<PathBuf, String>,
    port: u16,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head);
    let path = text.split(' ').nth(1).unwrap_or("").to_owned();
    let release = job.lock().unwrap().running.clone();
    let (status, body) = match (release, path.as_str()) {
        (None, _) => ("503 Service Unavailable", "draining".to_owned()),
        (Some(_), "/readyz") => ("200 OK", "ready read-write".to_owned()),
        (Some(release), "/metricz") => (
            "200 OK",
            crate::server::build_info::gauge(&versions[&release], "abc", Some(port)),
        ),
        (Some(_), _) => ("404 Not Found", String::new()),
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// Answers `/readyz` and `/metricz` on a loopback port of its own, as the release whose process
/// runs: its version comes from `versions`, and the slot label is the port, as the shipped file
/// sets it. With no process running it answers 503.
fn instance(job: Shared, versions: BTreeMap<PathBuf, String>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let (job, versions) = (job.clone(), versions.clone());
            // A thread for each connection, so that a connection that never sends its request
            // (another test that took this port for a free one) holds up no other.
            std::thread::spawn(move || answer(stream, &job, &versions, port));
        }
    });
    port
}

/// A Mac under a scratch root: the declaration where the hook reads it, the state directory it
/// names (mode 0700) with an idle record, the links directory with `current` naming `OLD`, and
/// the releases, each in directories of the tests' own user (the release owner here) that no
/// group or others can write.
struct Mac {
    _dir: TestDir,
    root: PathBuf,
    launchd: Launchd,
}

fn dir_mode(path: &Path, bits: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits)).unwrap();
}

/// Makes `relative` under `root`, every directory of it mode 0755.
fn dirs(root: &Path, relative: &str) {
    let mut at = root.to_path_buf();
    for part in Path::new(relative) {
        at.push(part);
        if !at.exists() {
            std::fs::create_dir(&at).unwrap();
        }
        dir_mode(&at, 0o755);
    }
}

fn mac(name: &str, versions: &[(&str, &str)], job: Job, declaration: Option<String>) -> Mac {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let root = dir.path().to_path_buf();
    for (release, _) in versions {
        dirs(&root, release.trim_start_matches('/'));
        let binary = root.join(release.trim_start_matches('/')).join("tierstore");
        std::fs::write(&binary, b"").unwrap();
        dir_mode(&binary, 0o755);
    }
    let versions = versions
        .iter()
        .map(|(release, version)| (PathBuf::from(release), (*version).to_owned()))
        .collect();
    let job: Shared = Arc::new(Mutex::new(job));
    let port = instance(job.clone(), versions);
    dirs(&root, "usr/local/etc/updater.d");
    let declared = root.join(LAUNCHD_DECLARATION_PATH.trim_start_matches('/'));
    let text = declaration.unwrap_or_else(|| {
        format!(
            "uid = \"_tierstore\"\nstate_dir = \"/{STATE}\"\nhealth_ports = [{port}]\n\n[hook]\nservice = \"com.komira.tierstore\"\n"
        )
    });
    std::fs::write(&declared, text).unwrap();
    dir_mode(&declared, 0o644);
    let state = root.join(STATE);
    std::fs::create_dir_all(&state).unwrap();
    dir_mode(&state, 0o700);
    std::fs::write(state.join(STATE_FILE), SingleRecord::idle().encode()).unwrap();
    dir_mode(&state.join(STATE_FILE), 0o600);
    dirs(&root, LINKS);
    let links = root.join(LINKS);
    symlink(OLD, links.join("current")).unwrap();
    Mac {
        _dir: dir,
        launchd: Launchd {
            links,
            root: root.clone(),
            job,
            calls: RefCell::new(Vec::new()),
            moved_under_a_process: RefCell::new(Vec::new()),
        },
        root,
    }
}

/// A job loaded and running `OLD`.
fn running_old() -> Job {
    Job {
        loaded: true,
        running: Some(PathBuf::from(OLD)),
        exiting: 0,
    }
}

impl Mac {
    fn config(&self) -> LaunchdConfig<'_> {
        LaunchdConfig {
            root: self.root.clone(),
            owner: me(),
            declaration_owner: me(),
            release_owner: me(),
            launchctl: PathBuf::from(LAUNCHCTL_PROGRAM),
            poll: Some(Duration::from_millis(1)),
            runner: &self.launchd,
        }
    }

    /// Runs `restore`, which waits for nothing, with the steps' own poll.
    fn restore_with_the_hosts_poll(&self) -> Done {
        let invocation = Invocation::parse([std::ffi::OsString::from("restore")]).unwrap();
        let config = LaunchdConfig {
            poll: None,
            ..self.config()
        };
        run_on(&config, &invocation, None)
    }

    /// Runs `step` as the updater runs it, with the params of an update to `staged` (0.4.0).
    fn run_to(&self, step: Step, staged: &str) -> Done {
        let file = self.root.join("params.toml");
        std::fs::write(
            &file,
            format!(
                "promotion_bound_secs = 30\nmode = \"restart\"\n\"updater.staged_release\" = \"{staged}\"\n\"updater.target_version\" = \"0.4.0\"\n"
            ),
        )
        .unwrap();
        let mut words = vec![step.to_string()];
        if step == Step::Prepare {
            words.push(staged.to_owned());
        }
        if step != Step::Restore {
            words.push("--params".to_owned());
            words.push(file.to_string_lossy().into_owned());
        }
        let invocation =
            Invocation::parse(words.into_iter().map(std::ffi::OsString::from)).unwrap();
        let params = invocation
            .params
            .as_deref()
            .map(|path| super::Params::read(path).unwrap());
        run_on(&self.config(), &invocation, params.as_ref())
    }

    /// Runs `step` with the params of an update to `NEW`.
    fn run(&self, step: Step) -> Done {
        self.run_to(step, NEW)
    }

    /// The `launchctl` calls since the last look, as their arguments.
    fn take_calls(&self) -> Vec<String> {
        self.launchd
            .calls
            .borrow_mut()
            .drain(..)
            .map(|call| call.args.join(" "))
            .collect()
    }

    fn link(&self, name: &str) -> Option<PathBuf> {
        std::fs::read_link(self.root.join(LINKS).join(name)).ok()
    }

    fn running(&self) -> Option<PathBuf> {
        self.launchd.job.lock().unwrap().running.clone()
    }

    fn record(&self) -> SingleRecord {
        SingleRecord::decode(&std::fs::read(self.root.join(STATE).join(STATE_FILE)).unwrap())
            .unwrap()
    }

    /// Every moment `current` named another release than the running process's.
    fn moved_under_a_process(&self) -> Vec<String> {
        self.launchd.moved_under_a_process.borrow().clone()
    }
}

/// A stop and a start: `print`, `bootout`, `print` until the job is gone (its process exits
/// [`EXIT_PRINTS`] looks later), `print`, `bootstrap`.
const STOP_START: [&str; 6] = [
    "print system/com.komira.tierstore",
    "bootout system/com.komira.tierstore",
    "print system/com.komira.tierstore",
    "print system/com.komira.tierstore",
    "print system/com.komira.tierstore",
    "bootstrap system /Library/LaunchDaemons/com.komira.tierstore.plist",
];

/// Asserts that an update of a Mac, step by step through the machine's own ports, runs no
/// program but `/bin/launchctl` (no `nft`, no `systemctl`, and no release: `prepare` checks the
/// staged release on disk without running it), and only in `cutover`, exactly: `print`,
/// `bootout`, `print` until launchd no longer has the job (its process exits two looks after the
/// bootout), `print`, `bootstrap system <plist>`. `current` never names another release than the
/// one whose process runs: the link moves only after the process is gone. Then `current` names
/// the new release, `previous` is gone, the new release runs, the record is committed with
/// nothing in flight, and the state directory holds the record and the lock alone (no slot
/// links). It would catch a link swapped under a draining process, a restart through any other
/// sequence, and a link or a record left in the wrong state.
#[test]
fn an_update_of_a_mac_runs_launchctl_alone_and_only_in_cutover() {
    let host = mac(
        "tierstore-launchd-update",
        &[(OLD, "0.3.0"), (NEW, "0.4.0")],
        running_old(),
        None,
    );
    for (step, expected) in [
        (Step::Prepare, &[][..]),
        (Step::Cutover, &STOP_START[..]),
        (Step::Verify, &[][..]),
        (Step::Cleanup, &[][..]),
    ] {
        let done = host.run(step);
        assert_eq!(done.outcome, Outcome::Success, "{step}: {done:?}");
        assert_eq!(host.take_calls(), expected, "{step}");
    }
    assert_eq!(host.moved_under_a_process(), Vec::<String>::new());
    assert_eq!(host.link("current"), Some(PathBuf::from(NEW)));
    assert_eq!(host.link("previous"), None);
    assert_eq!(host.running(), Some(PathBuf::from(NEW)));
    let record = host.record();
    assert!(record.committed() && !record.updating, "{record:?}");
    let mut kept: Vec<String> = std::fs::read_dir(host.root.join(STATE))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    kept.sort();
    assert_eq!(kept, ["lock", "state.json"]);
}

/// Asserts that when the new release answers as another version, `cutover` through the
/// machine's ports fails with `probe_failed`, says `rolled back` in its detail, boots the job out
/// and bootstraps it again on the previous release (the same calls twice), never moving
/// `current` under a running process, so that `current` names the old release and its process
/// runs. It would catch a failed probe that leaves the bad release running on a Mac.
#[test]
fn a_failed_probe_on_a_mac_restarts_the_previous_release() {
    let host = mac(
        "tierstore-launchd-probe",
        &[(OLD, "0.3.0"), (NEW, "9.9.9")],
        running_old(),
        None,
    );
    assert_eq!(host.run(Step::Prepare).outcome, Outcome::Success);
    let done = host.run(Step::Cutover);
    assert_eq!(done.report.reason, Some(Reason::ProbeFailed), "{done:?}");
    let detail = done.report.detail.unwrap();
    assert!(
        detail.ends_with(&format!("; rolled back: the service runs {OLD} again")),
        "{detail}"
    );
    assert_eq!(host.take_calls(), [STOP_START, STOP_START].concat());
    assert_eq!(host.moved_under_a_process(), Vec::<String>::new());
    assert_eq!(host.link("current"), Some(PathBuf::from(OLD)));
    assert_eq!(host.running(), Some(PathBuf::from(OLD)));
    let back = host.run(Step::Rollback);
    assert_eq!(back.outcome, Outcome::Success, "{back:?}");
    assert_eq!(host.link("previous"), None);
    assert_eq!(host.record(), SingleRecord::idle());
}

/// Asserts that `restore` of a Mac whose job is loaded but whose process does not run is `print`
/// and `kickstart`, which starts the process on the release `current` names, and that the record
/// and the links are not changed; and that a `restore` of a job that runs is the same two calls
/// and changes nothing. Both run with the poll a release has, since they wait for nothing. It
/// would catch a boot entry that bootstraps a loaded job, which launchd refuses, or one that
/// leaves a loaded job stopped.
#[test]
fn restore_of_a_mac_starts_a_loaded_job_that_does_not_run() {
    let stopped = Job {
        loaded: true,
        running: None,
        exiting: 0,
    };
    for (job, name) in [(stopped, "stopped"), (running_old(), "running")] {
        let host = mac(
            &format!("tierstore-launchd-restore-{name}"),
            &[(OLD, "0.3.0")],
            job,
            None,
        );
        let done = host.restore_with_the_hosts_poll();
        assert_eq!(done.outcome, Outcome::Success, "{name}: {done:?}");
        assert_eq!(
            host.take_calls(),
            [
                "print system/com.komira.tierstore",
                "kickstart system/com.komira.tierstore"
            ],
            "{name}"
        );
        assert_eq!(host.running(), Some(PathBuf::from(OLD)), "{name}");
        assert_eq!(host.record(), SingleRecord::idle());
        assert_eq!(host.link("current"), Some(PathBuf::from(OLD)));
    }
}

/// Asserts that a Mac is refused before any `launchctl` call, with the job and the links as they
/// were, when its declaration is absent (`host_failed`), when it is the two-slot hook's
/// (`state_invalid`), when the state directory is not the hook's alone (mode 0750: `state_io`,
/// naming the mode), and when the staged release is not one to trust on disk (`host_failed`): a
/// path through the link `current`, and a binary a group can write. It would catch a restart of
/// the service by a hook that cannot trust what it read.
#[test]
fn a_mac_that_cannot_be_trusted_is_refused_before_any_call() {
    let versions = [(OLD, "0.3.0"), (NEW, "0.4.0")];
    let absent = mac("tierstore-launchd-absent", &versions, running_old(), None);
    std::fs::remove_file(
        absent
            .root
            .join(LAUNCHD_DECLARATION_PATH.trim_start_matches('/')),
    )
    .unwrap();
    let done = absent.run(Step::Prepare);
    assert_eq!(done.report.reason, Some(Reason::HostFailed), "{done:?}");
    let slots = include_str!("../../../../../packaging/updater.d/tierstore.toml").to_owned();
    let linux = mac(
        "tierstore-launchd-linux",
        &versions,
        running_old(),
        Some(slots),
    );
    let done = linux.run(Step::Cutover);
    assert_eq!(done.report.reason, Some(Reason::StateInvalid), "{done:?}");
    let open = mac("tierstore-launchd-open", &versions, running_old(), None);
    dir_mode(&open.root.join(STATE), 0o750);
    let done = open.run(Step::Cutover);
    assert_eq!(done.report.reason, Some(Reason::StateIo), "{done:?}");
    let detail = done.report.detail.unwrap();
    assert!(detail.ends_with("has mode 0750, not 0700"), "{detail}");
    let through = mac("tierstore-launchd-through", &versions, running_old(), None);
    let done = through.run_to(Step::Prepare, "/usr/local/tierstore/current");
    assert_eq!(done.report.reason, Some(Reason::HostFailed), "{done:?}");
    let detail = done.report.detail.unwrap();
    assert!(
        detail.ends_with("/usr/local/tierstore/current is a symbolic link"),
        "{detail}"
    );
    let writable = mac("tierstore-launchd-writable", &versions, running_old(), None);
    let binary = writable
        .root
        .join(NEW.trim_start_matches('/'))
        .join("tierstore");
    dir_mode(&binary, 0o775);
    let done = writable.run(Step::Prepare);
    assert_eq!(done.report.reason, Some(Reason::HostFailed), "{done:?}");
    for host in [absent, linux, open, through, writable] {
        assert_eq!(host.take_calls(), Vec::<String>::new());
        assert_eq!(host.link("current"), Some(PathBuf::from(OLD)));
        assert_eq!(host.running(), Some(PathBuf::from(OLD)));
    }
}

/// Asserts that the machine a Mac's hook builds from the command line is the host's own: the
/// root `/`, the hook's own user as the owner of its files, root as the owner of the
/// declaration and of a staged release, `/bin/launchctl`, and the steps' own poll; and that a
/// `--root`, a `--root-uid` and a `--poll-interval-ms` of a conformance build reach it. It would
/// catch a release that reads a Mac's files under another root, or runs another `launchctl`.
#[test]
fn the_launchd_config_is_the_hosts_own() {
    let runner = Launchd {
        links: PathBuf::new(),
        root: PathBuf::new(),
        job: Arc::new(Mutex::new(Job::default())),
        calls: RefCell::new(Vec::new()),
        moved_under_a_process: RefCell::new(Vec::new()),
    };
    let config = launchd_config(&runner, &HostArgs::default());
    assert_eq!(config.root, PathBuf::from("/"));
    assert_eq!(config.owner, me());
    assert_eq!(config.declaration_owner, ROOT_UID);
    assert_eq!(config.release_owner, ROOT_UID);
    assert_eq!(config.launchctl, PathBuf::from("/bin/launchctl"));
    assert_eq!(config.poll, None);
    let args = HostArgs {
        root: PathBuf::from("/scratch"),
        root_uid: 7,
        poll: Some(Duration::from_millis(3)),
        ..HostArgs::default()
    };
    let config = launchd_config(&runner, &args);
    assert_eq!(config.root, PathBuf::from("/scratch"));
    assert_eq!(config.declaration_owner, 7);
    assert_eq!(config.release_owner, 7);
    assert_eq!(config.poll, Some(Duration::from_millis(3)));
}

/// The longest each piece of a step's work may take, from the hook's own bounds: every call
/// running to its time limit and every look at the instance taking its connect timeout, its
/// whole-request deadline and its pause.
struct Bounds {
    look: Duration,
    stop: Duration,
    start: Duration,
    ready: Duration,
    probe: Duration,
}

impl Bounds {
    fn of_the_hook() -> Bounds {
        let look = CONNECT_TIMEOUT + READ_TIMEOUT + POLL;
        Bounds {
            look,
            stop: LAUNCHCTL_TIMEOUT + BOOTOUT_TIMEOUT + (LAUNCHCTL_TIMEOUT + POLL) * GONE_ATTEMPTS,
            start: LAUNCHCTL_TIMEOUT * 2,
            ready: look * READY_ATTEMPTS,
            probe: look * PROBE_ATTEMPTS,
        }
    }

    /// A rollback: a stop and a start on the previous release, and its readiness.
    fn rollback(&self) -> Duration {
        self.stop + self.start + self.ready
    }
}

/// Asserts that each timeout of the declaration packaging ships for a Mac covers the longest
/// the hook's step may take, computed from the hook's own bounds (each `launchctl` call to its
/// time limit, each look at the instance to its connect timeout, its whole-request deadline
/// (`LoopbackInstances` bounds the write and every read together) and its pause): a
/// `cutover` that looks again at a swap a re-run finds, swaps, probes and rolls back; a `verify`
/// that probes and waits for the promotion for `promotion_bound_secs`; a `cleanup` that confirms;
/// a `rollback` that restarts the previous release and waits for it; a `prepare`, which only
/// reads and writes files; and that a `bootout` may run longer than the plist's `ExitTimeOut`, so
/// launchd ends a drain before the hook gives up on it. Nothing is timed: the bounds are added
/// up. It would catch an updater that kills a step still within the bounds the hook gives itself.
#[test]
fn the_declarations_timeouts_cover_the_hooks_bounds() {
    let shipped: toml::Table =
        include_str!("../../../../../packaging/macos/updater.d/tierstore.toml")
            .parse()
            .unwrap();
    let hook = shipped["hook"].as_table().unwrap();
    let timeout = |step: &str| {
        Duration::from_secs(
            hook["timeout_secs"][step]
                .as_integer()
                .unwrap()
                .try_into()
                .unwrap(),
        )
    };
    let bound = Duration::from_secs(
        hook["params"]["promotion_bound_secs"]
            .as_integer()
            .unwrap()
            .try_into()
            .unwrap(),
    );
    let plist = include_str!("../../../../../packaging/macos/com.komira.tierstore.plist");
    let exit_timeout: u64 = plist
        .split("<key>ExitTimeOut</key>")
        .nth(1)
        .and_then(|rest| rest.split("<integer>").nth(1))
        .and_then(|rest| rest.split('<').next())
        .and_then(|n| n.trim().parse().ok())
        .expect("ExitTimeOut in the plist");
    assert!(
        BOOTOUT_TIMEOUT > Duration::from_secs(exit_timeout),
        "a bootout is killed before launchd ends the drain"
    );
    let b = Bounds::of_the_hook();
    let again = b.start + b.ready + b.probe;
    let swap = b.stop + b.start + b.ready + b.probe;
    let needs = [
        ("cutover", again + swap + b.rollback()),
        ("verify", b.look + b.probe + bound + b.look),
        ("cleanup", b.look + b.probe),
        ("rollback", b.rollback()),
        ("prepare", Duration::from_secs(30)),
    ];
    for (step, needed) in needs {
        assert!(
            timeout(step) >= needed,
            "{step}: {:?} is less than {needed:?}",
            timeout(step)
        );
    }
}
