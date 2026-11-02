//! The single-instance steps over the machine's own ports, as a Mac runs them: a scratch root
//! that holds the declaration, the state directory and the release links where a Mac has them,
//! a stand-in for `launchctl` that keeps launchd's state (is the job loaded, which release did it
//! start), and an instance on a loopback port of its own that answers as the release that runs.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::launchd_system::{LaunchdConfig, launchd_config, run_on};
use super::machine::testing::me;
use super::machine::{Call, LAUNCHCTL_PROGRAM, Output, RunError, Runner, STATE_FILE};
use super::{
    Done, HostArgs, Invocation, LAUNCHD_DECLARATION_PATH, Outcome, ROOT_UID, Reason, SingleRecord,
    Step,
};

/// The releases, by name: directories under the links directory of the scratch root, each with
/// its binary.
const OLD: &str = "old";
const NEW: &str = "new";
const STATE: &str = "var/db/updater/hooks/tierstore";
const LINKS: &str = "usr/local/tierstore";

/// The release launchd runs, which the instance answers as; `None` while the job is not loaded.
type Running = Arc<Mutex<Option<PathBuf>>>;

/// `launchctl`, as far as the hook sees it: `print` answers whether the job is loaded,
/// `bootstrap` loads it (and so starts the release `current` names at that moment), `bootout`
/// unloads it, `kickstart` of a loaded job does nothing more. The binary of a staged release, asked
/// its capabilities, lists `handover-v1`. Every call is kept.
struct Launchd {
    links: PathBuf,
    running: Running,
    calls: RefCell<Vec<Call>>,
}

fn exited(code: i32) -> Result<Output, RunError> {
    Ok(Output {
        code: Some(code),
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

impl Runner for Launchd {
    fn run(&self, call: &Call) -> Result<Output, RunError> {
        self.calls.borrow_mut().push(call.clone());
        if call.program != LAUNCHCTL_PROGRAM {
            assert_eq!(call.args, ["--capabilities"], "{call:?}");
            return Ok(Output {
                code: Some(0),
                stdout: b"handover-v1\n".to_vec(),
                stderr: Vec::new(),
            });
        }
        let mut running = self.running.lock().unwrap();
        match call.args[0].as_str() {
            "print" | "kickstart" => exited(if running.is_some() { 0 } else { 113 }),
            "bootout" => {
                *running = None;
                exited(0)
            }
            "bootstrap" => {
                *running = Some(std::fs::read_link(self.links.join("current")).unwrap());
                exited(0)
            }
            other => panic!("launchctl {other} is not a call the hook makes"),
        }
    }
}

/// Answers one request on `stream` as the release that runs.
fn answer(
    mut stream: std::net::TcpStream,
    running: &Running,
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
    let release = running.lock().unwrap().clone();
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

/// Answers `/readyz` and `/metricz` on a loopback port of its own, as the release that runs:
/// its version comes from `versions`, and the slot label is the port, as the shipped file sets
/// it. With no release running it answers 503.
fn instance(running: Running, versions: BTreeMap<PathBuf, String>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let (running, versions) = (running.clone(), versions.clone());
            // A thread for each connection, so that a connection that never sends its request
            // (another test that took this port for a free one) holds up no other.
            std::thread::spawn(move || answer(stream, &running, &versions, port));
        }
    });
    port
}

/// A Mac under a scratch root: the declaration where the hook reads it (in directories of the
/// tests' own user that no group or others can write), the state directory it names (mode 0700)
/// with an idle record, and the links directory with `current` naming `OLD`.
struct Mac {
    _dir: TestDir,
    root: PathBuf,
    launchd: Launchd,
}

fn dir_mode(path: &Path, bits: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits)).unwrap();
}

/// The directory of the release `name` under `root`.
fn release_of(root: &Path, name: &str) -> PathBuf {
    root.join(LINKS).join("releases").join(name)
}

fn mac(name: &str, versions: &[(&str, &str)], declaration: Option<String>) -> Mac {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let root = dir.path().to_path_buf();
    let running: Running = Arc::new(Mutex::new(Some(release_of(&root, OLD))));
    for (release, _) in versions {
        // The release as the updater places it: a directory with the binary, of the tests' own
        // user (the release owner here), that no group or others can write.
        let at = release_of(&root, release);
        std::fs::create_dir_all(&at).unwrap();
        std::fs::write(at.join("tierstore"), b"").unwrap();
        dir_mode(&at.join("tierstore"), 0o755);
    }
    let versions = versions
        .iter()
        .map(|(release, version)| (release_of(&root, release), (*version).to_owned()))
        .collect();
    let port = instance(running.clone(), versions);
    let declared = root.join(LAUNCHD_DECLARATION_PATH.trim_start_matches('/'));
    std::fs::create_dir_all(declared.parent().unwrap()).unwrap();
    for up in [
        "usr",
        "usr/local",
        "usr/local/etc",
        "usr/local/etc/updater.d",
    ] {
        dir_mode(&root.join(up), 0o755);
    }
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
    let links = root.join(LINKS);
    std::fs::create_dir_all(&links).unwrap();
    dir_mode(&links, 0o755);
    symlink(release_of(&root, OLD), links.join("current")).unwrap();
    Mac {
        _dir: dir,
        root,
        launchd: Launchd {
            links,
            running,
            calls: RefCell::new(Vec::new()),
        },
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

    /// The directory of the release `name`.
    fn release(&self, name: &str) -> PathBuf {
        release_of(&self.root, name)
    }

    /// Runs `step` as the updater runs it, with the params of an update to `NEW` (0.4.0).
    fn run(&self, step: Step, params_dir: &Path) -> Done {
        let staged = self.release(NEW).display().to_string();
        let file = params_dir.join("params.toml");
        std::fs::write(
            &file,
            format!(
                "promotion_bound_secs = 30\nmode = \"restart\"\n\"updater.staged_release\" = \"{staged}\"\n\"updater.target_version\" = \"0.4.0\"\n"
            ),
        )
        .unwrap();
        let mut words = vec![step.to_string()];
        if step == Step::Prepare {
            words.push(staged.clone());
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

    /// The programs run since the last look, as words: a `launchctl` call as its arguments, and
    /// a release's binary as `<release>/tierstore` and its arguments.
    fn take_calls(&self) -> Vec<String> {
        let releases = self.root.join(LINKS).join("releases");
        self.launchd
            .calls
            .borrow_mut()
            .drain(..)
            .map(|call| {
                if call.program == LAUNCHCTL_PROGRAM {
                    return call.args.join(" ");
                }
                let program = Path::new(&call.program);
                let release = program.strip_prefix(&releases).expect("a release's binary");
                format!("{} {}", release.display(), call.args.join(" "))
            })
            .collect()
    }

    fn link(&self, name: &str) -> Option<PathBuf> {
        std::fs::read_link(self.root.join(LINKS).join(name)).ok()
    }

    fn record(&self) -> SingleRecord {
        SingleRecord::decode(&std::fs::read(self.root.join(STATE).join(STATE_FILE)).unwrap())
            .unwrap()
    }
}

const STOP_START: [&str; 5] = [
    "print system/com.komira.tierstore",
    "bootout system/com.komira.tierstore",
    "print system/com.komira.tierstore",
    "print system/com.komira.tierstore",
    "bootstrap system /Library/LaunchDaemons/com.komira.tierstore.plist",
];

/// Asserts that an update of a Mac, step by step through the machine's own ports, runs no
/// program but `/bin/launchctl` and the staged binary's check in `prepare` (its capabilities,
/// asked once it is seen to be the release owner's), and that only `cutover` calls `launchctl`,
/// exactly: `print`, `bootout`, `print` until the job is gone, `print`, `bootstrap system
/// <plist>`. Then `current` names the
/// new release, `previous` is gone, the instance answers as 0.4.0, the record is committed with
/// nothing in flight, and the state directory holds the record and the lock alone (no slot
/// links). It would catch an `nft` or `systemctl` call on a Mac, a restart through any other
/// sequence, and a link or a record left in the wrong state.
#[test]
fn an_update_of_a_mac_runs_no_nft_or_systemctl_and_launchctl_only_in_cutover() {
    let host = mac(
        "tierstore-launchd-update",
        &[(OLD, "0.3.0"), (NEW, "0.4.0")],
        None,
    );
    let scratch = host.root.clone();
    for (step, expected) in [
        (Step::Prepare, &["new/tierstore --capabilities"][..]),
        (Step::Cutover, &STOP_START[..]),
        (Step::Verify, &[][..]),
        (Step::Cleanup, &[][..]),
    ] {
        let done = host.run(step, &scratch);
        assert_eq!(done.outcome, Outcome::Success, "{step}: {done:?}");
        assert_eq!(host.take_calls(), expected, "{step}");
    }
    assert_eq!(host.link("current"), Some(host.release(NEW)));
    assert_eq!(host.link("previous"), None);
    assert_eq!(
        *host.launchd.running.lock().unwrap(),
        Some(host.release(NEW))
    );
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
/// and bootstraps it again on the previous release (the same five calls twice), so that
/// `current` names the old release and the instance answers as it. It would catch a failed probe
/// that leaves the bad release running on a Mac.
#[test]
fn a_failed_probe_on_a_mac_restarts_the_previous_release() {
    let host = mac(
        "tierstore-launchd-probe",
        &[(OLD, "0.3.0"), (NEW, "9.9.9")],
        None,
    );
    let scratch = host.root.clone();
    assert_eq!(host.run(Step::Prepare, &scratch).outcome, Outcome::Success);
    host.take_calls();
    let done = host.run(Step::Cutover, &scratch);
    assert_eq!(done.report.reason, Some(Reason::ProbeFailed), "{done:?}");
    let detail = done.report.detail.unwrap();
    assert!(
        detail.ends_with(&format!(
            "; rolled back: the service runs {} again",
            host.release(OLD).display()
        )),
        "{detail}"
    );
    assert_eq!(host.take_calls(), [STOP_START, STOP_START].concat());
    assert_eq!(host.link("current"), Some(host.release(OLD)));
    assert_eq!(
        *host.launchd.running.lock().unwrap(),
        Some(host.release(OLD))
    );
    let back = host.run(Step::Rollback, &scratch);
    assert_eq!(back.outcome, Outcome::Success, "{back:?}");
    assert_eq!(host.link("previous"), None);
    assert_eq!(host.record(), SingleRecord::idle());
}

/// Asserts that `restore` of a Mac whose job is loaded is `print` and `kickstart`, which starts
/// the job if it is not running and does nothing if it runs, and that the record and the links
/// are not changed; it is run with the poll a release has, since it waits for nothing. It would
/// catch a boot entry that bootstraps a loaded job, which launchd refuses.
#[test]
fn restore_of_a_mac_kickstarts_the_loaded_job() {
    let host = mac("tierstore-launchd-restore", &[(OLD, "0.3.0")], None);
    let done = host.restore_with_the_hosts_poll();
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        host.take_calls(),
        [
            "print system/com.komira.tierstore",
            "kickstart system/com.komira.tierstore"
        ]
    );
    assert_eq!(host.record(), SingleRecord::idle());
    assert_eq!(host.link("current"), Some(host.release(OLD)));
}

/// Asserts that a Mac is refused before any `launchctl` call when its declaration is absent
/// (`host_failed`), when it is the two-slot hook's (`state_invalid`), and when the state
/// directory is not the hook's alone (mode 0750: `state_io`, naming the mode). It would catch a
/// restart of the service by a hook that cannot trust what it read.
#[test]
fn a_mac_that_cannot_be_trusted_is_refused_before_any_call() {
    let absent = mac("tierstore-launchd-absent", &[(OLD, "0.3.0")], None);
    std::fs::remove_file(
        absent
            .root
            .join(LAUNCHD_DECLARATION_PATH.trim_start_matches('/')),
    )
    .unwrap();
    let scratch = absent.root.clone();
    let done = absent.run(Step::Prepare, &scratch);
    assert_eq!(done.report.reason, Some(Reason::HostFailed), "{done:?}");
    let slots = include_str!("../../../../../packaging/updater.d/tierstore.toml").to_owned();
    let linux = mac("tierstore-launchd-linux", &[(OLD, "0.3.0")], Some(slots));
    let scratch = linux.root.clone();
    let done = linux.run(Step::Cutover, &scratch);
    assert_eq!(done.report.reason, Some(Reason::StateInvalid), "{done:?}");
    let open = mac("tierstore-launchd-open", &[(OLD, "0.3.0")], None);
    dir_mode(&open.root.join(STATE), 0o750);
    let scratch = open.root.clone();
    let done = open.run(Step::Cutover, &scratch);
    assert_eq!(done.report.reason, Some(Reason::StateIo), "{done:?}");
    let detail = done.report.detail.unwrap();
    assert!(detail.ends_with("has mode 0750, not 0700"), "{detail}");
    for host in [absent, linux, open] {
        assert_eq!(host.take_calls(), Vec::<String>::new());
    }
}

/// Asserts that the machine a Mac's hook builds from the command line is the host's own: the
/// root `/`, the hook's own user as the owner of its files, root as the owner of the
/// declaration and of a staged release's binary, `/bin/launchctl`, and the steps' own poll; and that a `--root` and a
/// `--poll-interval-ms` of a conformance build reach it. It would catch a release that reads a
/// Mac's files under another root, or runs another `launchctl`.
#[test]
fn the_launchd_config_is_the_hosts_own() {
    let runner = Launchd {
        links: PathBuf::new(),
        running: Arc::new(Mutex::new(None)),
        calls: RefCell::new(Vec::new()),
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
