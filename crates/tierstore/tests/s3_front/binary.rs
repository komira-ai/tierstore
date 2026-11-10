//! The `tierstore` binary as a process: its flags and file, its start line, its refusals, and its drain on
//! SIGTERM and SIGINT.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use objstore_s3::s3::fake::FakeS3;
use objstore_s3::s3::{Credentials, PART_BYTES, S3Config, S3Store, S3Tuning};
use objstore_s3::{BodyStream, ByteRange, Capabilities, ObjectKey, ObjectStore, ObjectStoreError};
use reqwest::Client;
use rustix::process::{Pid, Signal, kill_process};
use tierstore::server::build_info::{CAPABILITIES, COMMIT, VERSION};
use tierstore_testkit::reaped::{Reaped, first_line, spawn};

use crate::client::assert_refused;
use crate::files::{BIN_KEY_ID, BIN_SECRET, Files, Tables};

const BIN: &str = env!("CARGO_BIN_EXE_tierstore");

/// The longest a test waits for the binary to do what it was asked to. It only stops a test whose
/// process hangs, and then the test fails: no test asserts how long the binary takes.
const SAFETY: Duration = Duration::from_secs(60);

/// Runs the binary to its end: the exit status, stdout and stderr.
pub(crate) fn run(flags: &[String]) -> (i32, String, String) {
    let out = Command::new(BIN).args(flags).output().unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A running binary, and the address it said it listens on.
pub(crate) struct Running {
    /// None once `exit_code` has taken it to wait for its end.
    child: Option<Reaped>,
    pid: Pid,
    addr: String,
    /// The start line the binary printed.
    line: String,
    /// Keeps the pipe of the binary's stdout open.
    _stdout: std::io::BufReader<std::process::ChildStdout>,
}

pub(crate) fn start(flags: &[String]) -> Running {
    let mut command = Command::new(BIN);
    command
        .args(flags)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = spawn(&mut command, 3).unwrap();
    let stdout = child.stdout.take().unwrap();
    let (line, reader) = first_line(stdout, Duration::from_secs(30)).unwrap();
    let words: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(words[0], "tierstore", "{line}");
    assert_eq!(words[1], VERSION, "{line}");
    let listen = words[2].strip_prefix("listen=").expect("listen=");
    let pid = Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    Running {
        child: Some(child),
        pid,
        addr: listen.to_owned(),
        line,
        _stdout: reader,
    }
}

impl Running {
    /// The start line the binary printed.
    pub(crate) fn line(&self) -> &str {
        &self.line
    }

    /// The address the binary said it listens on.
    pub(crate) fn addr(&self) -> &str {
        &self.addr
    }

    /// The `S3Store` client with the key of the key file.
    pub(crate) fn store(&self) -> S3Store {
        self.store_as(BIN_KEY_ID, BIN_SECRET)
    }

    /// The `S3Store` client with this key.
    fn store_as(&self, key_id: &str, secret: &str) -> S3Store {
        let config = S3Config {
            endpoint: format!("http://{}", self.addr),
            region: "us-east-1".to_owned(),
            bucket: "tier-bucket".to_owned(),
            credentials: Credentials::new(key_id, secret),
            capabilities: Capabilities {
                conditional_put: true,
                object_lock: false,
            },
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            min_body_rate: None,
        };
        // The threshold of the client before it was a setting (see `support.rs`).
        let tuning = S3Tuning {
            single_put_max: PART_BYTES,
            ..S3Tuning::default()
        };
        S3Store::new_tuned(config, tuning).unwrap()
    }

    pub(crate) fn signal(&self, signal: Signal) {
        kill_process(self.pid, signal).unwrap();
    }

    /// The exit code, once the process has exited: a thread of the blocking pool waits for the
    /// process to end. A process that has not ended within `SAFETY` is killed, so that the wait
    /// ends, and the test fails.
    pub(crate) async fn exit_code(&mut self) -> Option<i32> {
        let mut child = self.child.take().expect("no process to wait for");
        let waiting = tokio::task::spawn_blocking(move || {
            let status = child.wait();
            (child, status)
        });
        let waited = tokio::time::timeout(SAFETY, waiting).await;
        if waited.is_err() {
            kill_process(self.pid, Signal::KILL).unwrap();
        }
        let (_child, status) = waited.expect("the process did not exit").unwrap();
        status.unwrap().code()
    }
}

/// Waits until the drain of the binary has begun: `/readyz` says 503, which it does through the
/// wait, or the connection is refused, which it is once the wait is over. The binary is asked
/// again as soon as it has answered, and nothing is asserted about how long it takes.
async fn until_draining(ready: &str) {
    let client = Client::new();
    let asking = async {
        loop {
            match client.get(ready).send().await {
                Ok(probe) if probe.status() != 503 => {}
                _ => return,
            }
        }
    };
    let waited = tokio::time::timeout(SAFETY, asking).await;
    assert!(waited.is_ok());
}

/// Asserts that the binary prints one start line with its version and the address it is bound
/// to, serves S3 calls with the key of its key file, answers `/readyz` 200, and on SIGTERM
/// stops answering `/readyz` 200 (it says 503, or takes no connection once the wait is over) and
/// exits with status 0. The test waits for the drain to begin and for the exit, and asserts nothing about
/// how long either takes; that the server goes on serving through the wait is asserted in the
/// crate (`src/server/drain_tests.rs`).
#[tokio::test]
async fn it_announces_serves_and_drains_on_sigterm() {
    let files = Files::new("bin-sigterm");
    let tables = Tables {
        hot: "kind = \"memory\"\nbudget = 1048576\nlease_ttl = \"1s\"".to_owned(),
        ..Tables::default()
    };
    let mut running = start(&files.flags_of(&tables, &["--drain-lb-wait", "3s"]));
    let store = running.store();
    let k = ObjectKey::new("cas/bin").unwrap();
    store
        .put_new(&k, Bytes::from_static(b"served by the binary"), None)
        .await
        .unwrap();
    let first = ByteRange::new(0, 6).unwrap();
    assert_eq!(store.get_range(&k, first).await.unwrap(), "served");
    let ready = format!("http://{}/readyz", running.addr);
    let probe = Client::new().get(&ready).send().await.unwrap();
    assert_eq!(probe.status(), 200);

    running.signal(Signal::TERM);
    until_draining(&ready).await;
    assert_eq!(running.exit_code().await, Some(0));
}

/// Asserts that SIGINT starts the same drain and ends the process with status 0.
#[tokio::test]
async fn it_drains_on_sigint_too() {
    let files = Files::new("bin-sigint");
    let mut running = start(&files.flags(&["--drain-lb-wait", "50ms"]));
    running.signal(Signal::INT);
    assert_eq!(running.exit_code().await, Some(0));
}

/// Asserts that `--help` and `--version` exit 0 and print `--config` and the version, and none of the
/// four flags that are for the host updater or kept for old units (they stay accepted), that
/// a start without `--config` exits 2 and says the settings are in a file, and that a flag that is
/// not known and a value that is not valid exit 2 with a usage text.
#[test]
fn the_flags_are_parsed_and_refused_as_the_usage_says() {
    let (code, out, _) = run(&["--help".to_owned()]);
    assert_eq!(code, 0);
    assert!(out.contains("--config"), "{out}");
    for hidden in [
        "--check-config",
        "--capabilities",
        "--drain-lb-wait",
        "--drain-deadline",
    ] {
        assert!(!out.contains(hidden), "{hidden}");
    }
    assert!(!out.contains("--listen"), "{out}");
    let (code, out, _) = run(&["--version".to_owned()]);
    assert_eq!(code, 0);
    // The form the release check reads: the name, the build's version (the crate's, or the one
    // the release workflow gave the build), `+`, the commit.
    let version = format!("tierstore {VERSION}+{COMMIT}");
    assert_eq!(out.trim_end(), version);
    let (code, out, err) = run(&[]);
    assert_eq!((code, out.as_str()), (2, ""));
    assert_eq!(
        err,
        "tierstore: --config is needed: the settings are in a file (docs/configuration.md), and tierstore takes no flag for them\n"
    );
    let files = Files::new("bin-flags");
    let mut unknown = files.flags(&[]);
    unknown.push("--no-such-flag".to_owned());
    assert_eq!(run(&unknown).0, 2);
    let bad_value = files.flags(&["--drain-lb-wait", "soon"]);
    assert_eq!(run(&bad_value).0, 2);
}

/// Asserts that a flag that used to hold a setting of the data path is refused with status 2, no
/// output on stdout and a message that is exactly the one naming the key that replaces it, with or
/// without a file, in either spelling of the flag, and that nothing is started.
#[test]
fn a_flag_that_went_is_refused_naming_the_key_that_replaces_it() {
    let files = Files::new("bin-removed");
    let config = files.flags(&[]);
    let cases = [
        (
            vec!["--listen", "127.0.0.1:0"],
            "tierstore: --listen was removed: the file given with --config holds it; declare an [[interface]] with type = \"s3\" and params port or addresses\n",
        ),
        (
            vec!["--hot-bytes=1000"],
            "tierstore: --hot-bytes was removed: the file given with --config holds it; set hot_store.budget\n",
        ),
        (
            vec!["--oci-listen", "127.0.0.1:0"],
            "tierstore: --oci-listen was removed: the file given with --config holds it; declare an [[interface]] with type = \"oci\" and params port or addresses\n",
        ),
        (
            vec!["--print-config"],
            "tierstore: --print-config was removed: there are no flags to print a file for; write the file (docs/configuration.md)\n",
        ),
    ];
    for (flags, want) in cases {
        let words: Vec<String> = flags.iter().map(|flag| (*flag).to_owned()).collect();
        let (code, out, err) = run(&words);
        assert_eq!(
            (code, out.as_str(), err.as_str()),
            (2, "", want),
            "{flags:?}"
        );
        let mut with_file = config.clone();
        with_file.extend(words);
        let (code, out, err) = run(&with_file);
        assert_eq!(
            (code, out.as_str(), err.as_str()),
            (2, "", want),
            "{flags:?}"
        );
    }
}

/// Asserts that `--capabilities`, alone, exits 0 and prints one line for each capability of the
/// build and nothing else, on stdout, and that it needs none of the flags a start requires.
/// Catches: a flag that is not there, one that still asks for `--listen`, a start that goes on to
/// serve, and output on stderr or text besides the names.
#[test]
fn capabilities_prints_the_names_one_to_a_line_and_exits_0() {
    let (code, out, err) = run(&["--capabilities".to_owned()]);
    assert_eq!((code, err.as_str()), (0, ""));
    let expected: String = CAPABILITIES
        .iter()
        .map(|name| format!("{name}\n"))
        .collect();
    assert_eq!(out, expected);
}

/// Asserts that `--capabilities` of the binary prints exactly `handover-v1`, `handoff-socket-v1`,
/// `handoff-cache-v1`, `handoff-state-dir-v1` and `handoff-fs-v1` in that order, each with a newline, and exits 0
/// with nothing on stderr: the first is the line the host updater's `prepare` step reads to hand
/// over to this release instead of restarting it, the second says the binary serves through
/// `server.handoff_socket`, the third that its build node and its storage node with `[local_cache]`
/// do too (a node whose configuration has `[local_cache]` is handed over only between releases that
/// list it), the fourth that the socket defaults to the state directory (a node whose configuration
/// leaves the key out is handed over only between releases that list it), the fifth that
/// `tierstore fs` hands its mount over (the second unit is handed over only between releases that
/// list it). Catches: any capability left out of the list (for `handover-v1`, every update is a
/// restart), a misspelled name the updater would not find, and a sixth name nothing promises.
#[test]
fn capabilities_lists_the_five_hand_over_words() {
    let (code, out, err) = run(&["--capabilities".to_owned()]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            0,
            "handover-v1\nhandoff-socket-v1\nhandoff-cache-v1\nhandoff-state-dir-v1\nhandoff-fs-v1\n",
            ""
        )
    );
}

/// Starts the binary with `flags` and the variable `STATE_DIRECTORY` set to `state` as systemd
/// sets it for a unit with `StateDirectory=`; the rest as [`start`].
fn start_in_state_dir(flags: &[String], state: &std::path::Path) -> Running {
    let mut command = Command::new(BIN);
    command
        .args(flags)
        .env("STATE_DIRECTORY", state)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = spawn(&mut command, 3).unwrap();
    let stdout = child.stdout.take().unwrap();
    let (line, reader) = first_line(stdout, Duration::from_secs(30)).unwrap();
    let words: Vec<&str> = line.split_whitespace().collect();
    let listen = words[2].strip_prefix("listen=").expect("listen=");
    let pid = Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    Running {
        child: Some(child),
        pid,
        addr: listen.to_owned(),
        line,
        _stdout: reader,
    }
}

/// Asserts that a start whose file leaves out `server.handoff_socket`, run with the state
/// directory systemd made for the unit, makes the control socket `handoff.sock` in it (a socket
/// file), that the drain keys of the file are read (a `drain_lb_wait` of 50 ms and a
/// `drain_deadline`), and that SIGTERM ends it with status 0. Catches a default socket that is not
/// made. (A start with no state directory makes none: `a_state_directory_gives_the_socket_its_default`
/// and `the_state_directory_is_the_first_absolute_one_systemd_names` in the crate.)
#[tokio::test]
async fn the_state_directory_holds_the_default_hand_over_socket() {
    use std::os::unix::fs::FileTypeExt as _;
    let files = Files::new("bin-state-dir");
    let tables = Tables {
        server: "drain_lb_wait = \"50ms\"\ndrain_deadline = \"1m\"".to_owned(),
        ..Tables::default()
    };
    let flags = files.flags_of(&tables, &[]);
    // Short, as a Unix socket's path is at most 103 bytes.
    let state = tierstore_testkit::dir::TestDir::under(std::env::temp_dir(), "tssd");
    let mut running = start_in_state_dir(&flags, state.path());
    let socket = state.path().join("handoff.sock");
    let made = std::fs::symlink_metadata(&socket).unwrap();
    assert!(made.file_type().is_socket(), "{made:?}");
    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
}

/// Asserts that a part of the drain given in the file and by its flag is refused with status 2 and
/// the exact message, and that the flag alone is still taken and the key alone is.
#[test]
fn a_drain_part_given_twice_is_refused_by_the_binary() {
    let files = Files::new("bin-drain-twice");
    let tables = Tables {
        server: "drain_deadline = \"1m\"".to_owned(),
        ..Tables::default()
    };
    let flags = files.flags_of(&tables, &["--drain-deadline", "2m", "--check-config"]);
    let (code, out, err) = run(&flags);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(
        err.contains("server.drain_deadline: is set in the file and by --drain-deadline too; give it once, in the file"),
        "{err}"
    );
    let once = files.flags_of(&tables, &["--check-config"]);
    assert_eq!(run(&once), (0, String::new(), String::new()));
    let flag_only = files.flags(&["--drain-deadline", "2m", "--check-config"]);
    assert_eq!(run(&flag_only), (0, String::new(), String::new()));
}

/// The file of a build node and of a storage node with a cache, as `--check-config` reads them,
/// each with `server_extra` lines in `[server]`: the S3 tiers are named, never called.
fn handoff_files(files: &Files, server_extra: &str) -> [(&'static str, String); 2] {
    let dir = files.key.parent().unwrap().to_path_buf();
    let upstream = files.secret("upstream.key", "UPKEY\nup-secret\n");
    let build = format!(
        r#"
[[interface]]
type   = "s3"
params = {{ addresses = ["127.0.0.1:0"] }}

[server]
proxy_id        = "bin-build"
bucket          = "tier-bucket"
access_key_file = "{key}"
{server_extra}

[local_cache]
path   = "{cache}"
budget = "1MiB"

[cold_store]
kind             = "s3"
bucket           = "tierstore-fake"
endpoint         = "http://192.0.2.1:9300"
credentials_file = "{upstream}"

[[policy]]
prefix = "cas/"
cache  = true
"#,
        key = files.key.display(),
        cache = dir.join("cache").display(),
        upstream = upstream.display(),
    );
    let combined = format!(
        r#"
[[interface]]
type   = "s3"
params = {{ addresses = ["127.0.0.1:0"] }}

[server]
proxy_id        = "bin-combined"
bucket          = "tier-bucket"
access_key_file = "{key}"
{server_extra}

[hot_store]
kind   = "dir"
path   = "{hot}"
budget = "1MiB"

[cold_store]
kind = "dir"
path = "{cold}"

[local_cache]
path   = "{cache}"
budget = "1MiB"

[[policy]]
prefix = "cas/"
cache  = true
"#,
        key = files.key.display(),
        hot = dir.join("hot").display(),
        cold = dir.join("cold").display(),
        cache = dir.join("cache").display(),
    );
    [("build.toml", build), ("combined.toml", combined)]
}

/// Asserts (T29) that `--config <file> --check-config` of this build exits 0 with nothing printed
/// for a build node and for a storage node with `[local_cache]`, each with an absolute
/// `server.handoff_socket` and a `server.handoff_wait`, and that it creates neither the socket nor
/// the cache. This is the answer the update hook's `prepare` reads from the staged release; the
/// previous release that refuses the same file is modelled by the hook's fake, as no older
/// binary is at hand offline.
#[test]
fn check_config_accepts_the_handoff_keys_of_a_build_node_and_a_combined_node() {
    let files = Files::new("bin-check-handoff-good");
    // The path of a Unix socket is short (103 bytes at most on macOS, 107 on Linux): its
    // directory is the one `socket_dir` chooses, where the path is short enough to bind.
    let (_state, socket) = crate::files::socket_dir("bin-handoff", "handoff.sock");
    let extra = format!(
        "handoff_socket = {:?}\nhandoff_wait = \"5s\"",
        socket.display().to_string()
    );
    for (name, text) in handoff_files(&files, &extra) {
        let config = files.secret(name, &text).display().to_string();
        let flags = ["--config", &config, "--check-config"].map(str::to_owned);
        assert_eq!(run(&flags), (0, String::new(), String::new()), "{name}");
    }
    assert!(!socket.exists());
    assert!(!files.key.with_file_name("cache").exists());
}

/// Asserts (T29) that `--check-config` of a build node and of a storage node with `[local_cache]`
/// whose `server.handoff_socket` is not an absolute path exits 2, prints nothing on stdout and
/// names `server.handoff_socket` on stderr. Catches a check that passes a file whose start would
/// refuse.
#[test]
fn check_config_refuses_a_bad_handoff_socket_naming_the_key() {
    let files = Files::new("bin-check-handoff-bad");
    for (name, text) in handoff_files(&files, "handoff_socket = \"handoff.sock\"") {
        let config = files.secret(name, &text).display().to_string();
        let flags = ["--config", &config, "--check-config"].map(str::to_owned);
        let (code, out, err) = run(&flags);
        assert_eq!((code, out.as_str()), (2, ""), "{name}");
        assert!(err.contains("server.handoff_socket"), "{name}: {err}");
    }
}

/// Asserts that `--capabilities` with any other flag is refused with status 2, naming it, and
/// prints nothing on stdout, so that a script that reads the list never takes a refusal for one.
/// Catches: the other flags ignored (a start's flags given by mistake read as a list), and a
/// refusal that does not say which flag.
#[test]
fn capabilities_with_another_flag_is_refused_naming_it() {
    for other in [
        vec!["--listen", "127.0.0.1:0"],
        vec!["--version"],
        vec!["--check-config"],
    ] {
        let mut flags = vec!["--capabilities".to_owned()];
        flags.extend(other.iter().map(|flag| (*flag).to_owned()));
        let (code, out, err) = run(&flags);
        assert_eq!((code, out.as_str()), (2, ""), "{other:?}");
        assert!(
            err.starts_with("tierstore: --capabilities takes no other flag, got "),
            "{err}"
        );
        assert!(err.contains(other[0]), "{other:?}: {err}");
    }
}

/// Asserts that the refusal does not depend on where `--capabilities` stands, that a repeated
/// `--capabilities` is the one flag given twice and not another flag, and that
/// `--capabilities=x` is not `--capabilities`: the parser refuses it, as a flag that takes no
/// value. Catches: a refusal that looks only at the argument after the flag, a repeat taken for
/// another flag, and a prefix match that takes `--capabilities=x` for the flag.
#[test]
fn capabilities_is_found_wherever_it_stands() {
    let flags = |list: &[&str]| -> Vec<String> { list.iter().map(|s| (*s).to_owned()).collect() };
    let (code, out, err) = run(&flags(&["--listen", "127.0.0.1:0", "--capabilities"]));
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(
        err.starts_with("tierstore: --capabilities takes no other flag, got --listen"),
        "{err}"
    );
    let (once, once_out, _) = run(&flags(&["--capabilities"]));
    let (twice, twice_out, twice_err) = run(&flags(&["--capabilities", "--capabilities"]));
    assert_eq!((twice, twice_err.as_str()), (once, ""));
    assert_eq!(twice_out, once_out);
    let (code, out, err) = run(&flags(&["--capabilities=x"]));
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(!err.contains("takes no other flag"), "{err}");
    assert!(err.contains("--capabilities"), "{err}");
}

/// Asserts that the start line ends with `commit=` and the commit the build embeds, and that the
/// words before it are as they were, and that `/metricz` of the process names the same commit in
/// its build gauge. Catches: a start line that leaves the commit out or puts it where a reader
/// of the address words would take it for one, and a gauge and a start line that disagree.
#[tokio::test]
async fn the_start_line_and_the_gauge_name_the_commit() {
    let files = Files::new("bin-commit");
    let mut running = start(&files.flags(&["--drain-lb-wait", "50ms"]));
    let line = running.line().to_owned();
    let words: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(words.len(), 4, "{line}");
    assert_eq!(words[3], format!("commit={COMMIT}"), "{line}");
    let url = format!("http://{}/metricz", running.addr());
    let body = Client::new()
        .get(&url)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let gauge = format!("tierstore_build_info{{version=\"{VERSION}\",commit=\"{COMMIT}\"}} 1\n");
    assert!(body.contains(&gauge), "{body}");
    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
}

/// Asserts that a start that cannot go on exits with status 1 and a reason that names the key:
/// a tier that is a directory that cannot be made, water marks out of order, an id that is not an
/// id, an address that is in use, and a key file that others can read.
#[test]
fn a_start_that_cannot_go_on_says_why() {
    let files = Files::new("bin-refused");
    let good = files.flags(&[]);
    // A directory under a file cannot be made.
    let under_a_file = format!("{}/sub", files.key.display());
    let tier = |name: &str| format!("kind = \"dir\"\npath = {under_a_file:?}\n{name}");
    let hot = Tables {
        hot: tier("budget = 1048576"),
        ..Tables::default()
    };
    let (code, out, err) = run(&files.flags_of(&hot, &[]));
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(err.starts_with("tierstore: "), "{err}");
    assert!(err.contains("hot_store.kind"), "{err}");
    let cold = Tables {
        cold: tier(""),
        ..Tables::default()
    };
    let (code, _, err) = run(&files.flags_of(&cold, &[]));
    assert_eq!(code, 1);
    assert!(err.contains("cold_store.kind"), "{err}");
    let marks = Tables {
        hot: "kind = \"memory\"\nbudget = 1048576\nlow_water = 90".to_owned(),
        ..Tables::default()
    };
    let (code, _, err) = run(&files.flags_of(&marks, &[]));
    assert_eq!(code, 1);
    assert!(err.contains("hot_store.low_water"), "{err}");
    let id = Tables {
        proxy_id: "a/b".to_owned(),
        ..Tables::default()
    };
    let (code, _, err) = run(&files.flags_of(&id, &[]));
    assert_eq!(code, 1);
    assert!(err.contains("opening the proxy"), "{err}");
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap().to_string();
    let busy = Tables {
        listen: vec![addr],
        ..Tables::default()
    };
    let (code, _, err) = run(&files.flags_of(&busy, &[]));
    assert_eq!(code, 1);
    assert!(err.contains("binding"), "{err}");
    drop(taken);
    std::fs::set_permissions(&files.key, Permissions::from_mode(0o644)).unwrap();
    let (code, _, err) = run(&good);
    assert_eq!(code, 1);
    assert!(err.contains("others can read it"), "{err}");
}

/// Asserts that the binary serves a read-only key given as a JSON file beside the full key of a
/// file of two lines: the read-only key reads a ranged get of what the full key put, and is
/// refused a put and a delete with 403 `AccessDenied`.
#[tokio::test]
async fn it_serves_a_read_only_key_from_a_json_file_beside_the_full_key() {
    let files = Files::new("bin-read-only");
    let json =
        r#"{"access_key_id": "AKIDREADER", "secret_access_key": "reader-secret-0123456789"}"#;
    let reader_file = files.secret("reader.json", json);
    let tables = Tables {
        server: format!(
            "read_only_key_files = [{:?}]",
            reader_file.display().to_string()
        ),
        ..Tables::default()
    };
    let mut running = start(&files.flags_of(&tables, &["--drain-lb-wait", "50ms"]));
    let full = running.store();
    let reader = running.store_as("AKIDREADER", "reader-secret-0123456789");
    let k = ObjectKey::new("cas/ro").unwrap();
    let body = Bytes::from_static(b"read by the daemons");
    full.put_new(&k, body, None).await.unwrap();
    let first = ByteRange::new(0, 4).unwrap();
    assert_eq!(reader.get_range(&k, first).await.unwrap(), "read");
    let other = ObjectKey::new("cas/other").unwrap();
    let put = reader.put_new(&other, Bytes::from_static(b"x"), None).await;
    assert_refused(put, 403, "AccessDenied");
    let delete = reader.delete(&k).await;
    assert_refused(delete, 403, "AccessDenied");
    assert_eq!(full.get_range(&k, first).await.unwrap(), "read");
    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
}

/// Asserts that two keys with one id, the full key's and a read-only key's, are refused at start
/// with status 2, as flags that cannot be mended by a restart are, with a message that names the
/// id and the flags and not the secret; that a read-only key file that is not a key (a JSON
/// object with a field besides the two) or that others can read exits 1 and names the flag, and
/// not the secret.
#[test]
fn a_read_only_key_file_that_cannot_be_used_is_a_start_that_says_why() {
    let files = Files::new("bin-read-only-refused");
    let same = files.secret("same", &format!("{BIN_KEY_ID}\nanother-secret\n"));
    let flag = |path: &std::path::Path| {
        let tables = Tables {
            server: format!("read_only_key_files = [{:?}]", path.display().to_string()),
            ..Tables::default()
        };
        files.flags_of(&tables, &[])
    };
    let (code, out, err) = run(&flag(same.as_path()));
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.starts_with("tierstore: "), "{err}");
    assert!(err.contains(BIN_KEY_ID), "{err}");
    assert!(err.contains("server.read_only_key_files"), "{err}");
    assert!(!err.contains("another-secret"), "{err}");
    let extra = r#"{"access_key_id": "A", "secret_access_key": "topsecret", "note": 1}"#;
    let typo = files.secret("typo", extra);
    let (code, _, err) = run(&flag(typo.as_path()));
    assert_eq!(code, 1);
    assert!(err.contains("server.read_only_key_files"), "{err}");
    assert!(!err.contains("topsecret"), "{err}");
    let open = files.secret("open", "READER\nreader-secret-0123456789\n");
    std::fs::set_permissions(&open, Permissions::from_mode(0o644)).unwrap();
    let (code, _, err) = run(&flag(open.as_path()));
    assert_eq!(code, 1);
    assert!(err.contains("others can read it"), "{err}");
    assert!(!err.contains("reader-secret-0123456789"), "{err}");
}

/// Asserts that the binary serves over two S3 tiers, each a bucket of a store of its own that it
/// reaches with the key file and the region of its tier: an object put is read back, is in the
/// hot store and then, copied by the reconciler, in the cold store; a multipart upload lands in
/// the hot store, as one object made of the parts, and is then copied to the cold store, with no
/// upload left open in either; and the
/// requests of each tier were signed with the key and the region of that tier (each store
/// refuses another key); and the `ETag` a head answers is the tag in quotes once, for an
/// object put whole and for one made of parts, and an `If-Match` with it names the object's
/// tag (the answer is that objects cannot be replaced) where another tag is a failed
/// precondition.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn it_serves_over_two_s3_tiers_and_lands_a_multipart_upload_in_the_hot_one() {
    let hot = FakeS3::start().await;
    let cold = FakeS3::start().await;
    // A hot tier that keeps its format between starts is activated by the operator
    // (`tierstore format-epoch --activate`): the object is put in it before the node starts.
    let active = Bytes::from_static(br#"{"epoch":2,"phase":"active"}"#);
    let capabilities = Capabilities {
        conditional_put: true,
        object_lock: false,
    };
    let format = ObjectKey::new("_tiering/format").unwrap();
    hot.store(capabilities)
        .put_new(&format, active, None)
        .await
        .unwrap();
    hot.require_credentials("HOTKEY", "hot-secret");
    cold.require_credentials("COLDKEY", "cold-secret");
    let files = Files::new("bin-s3-tiers");
    let hot_key = files.secret("hot-key", "HOTKEY\nhot-secret\n");
    let cold_key = files.secret("cold-key", "COLDKEY\ncold-secret\n");
    let hot_key = hot_key.display().to_string();
    let cold_key = cold_key.display().to_string();
    let tables = Tables {
        hot: format!(
            "kind = \"s3\"\nbucket = \"hot-bucket\"\nendpoint = \"{}\"\ncredentials_file = {hot_key:?}\nbudget = 1048576\nlease_ttl = \"1s\"",
            hot.endpoint()
        ),
        cold: format!(
            "kind = \"s3\"\nbucket = \"cold-bucket\"\nendpoint = \"{}\"\nregion = \"eu-west-1\"\ncredentials_file = {cold_key:?}",
            cold.endpoint()
        ),
        ..Tables::default()
    };
    let flags = files.flags_of(&tables, &["--drain-lb-wait", "50ms"]);
    let mut running = start(&flags);
    let store = running.store();

    let small = ObjectKey::new("cas/small").unwrap();
    let text = Bytes::from_static(b"small object");
    store.put_new(&small, text, None).await.unwrap();
    let all = ByteRange::new(0, 12).unwrap();
    let got = store.get_range(&small, all).await.unwrap();
    assert_eq!(got, "small object");
    let name = "cas/small".to_owned();
    assert!(hot.keys().contains(&name));
    let copying = async {
        while !cold.keys().contains(&name) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let copied = tokio::time::timeout(SAFETY, copying).await;
    assert!(copied.is_ok(), "not copied");

    let big = ObjectKey::new("cas/big").unwrap();
    let len = PART_BYTES + 1;
    let bytes = Bytes::from(vec![9_u8; usize::try_from(len).unwrap()]);
    let body: BodyStream = stream::iter(vec![Ok(bytes)]).boxed();
    store.put_stream(&big, len, body).await.unwrap();
    assert_eq!(hot.object_len(&big), Some(len));
    assert_eq!(hot.stats().completed, 1);
    assert_eq!(hot.incomplete_uploads(), 0);
    let name = "cas/big".to_owned();
    let copying = async {
        while !cold.keys().contains(&name) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let copied = tokio::time::timeout(SAFETY, copying).await;
    assert!(copied.is_ok(), "not copied");
    assert_eq!(cold.object_len(&big), Some(len));
    assert_eq!(cold.incomplete_uploads(), 0);
    assert_eq!(store.head(&big).await.unwrap().size, len);
    let head = store.head(&big).await.unwrap();
    let inner = head.etag.trim_matches('"');
    assert_eq!(head.etag, format!("\"{inner}\""));
    let head = store.head(&small).await.unwrap();
    let inner = head.etag.trim_matches('"');
    assert_eq!(head.etag, format!("\"{inner}\""));
    let swap = store.put_if_match(&small, Bytes::new(), &head.etag);
    let same = swap.await.unwrap_err();
    assert!(
        matches!(same, ObjectStoreError::Service { status: 501, .. }),
        "{same:?}"
    );
    let swap = store.put_if_match(&small, Bytes::new(), "\"stale\"");
    let stale = swap.await.unwrap_err();
    assert!(
        matches!(stale, ObjectStoreError::Service { status: 412, .. }),
        "{stale:?}"
    );
    assert_eq!(hot.regions(), ["us-east-1"]);
    assert_eq!(cold.regions(), ["eu-west-1"]);

    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
}

/// A loopback listener that takes every connection and closes it at once, so that a client of it
/// meets a transport failure and not a refusal, and that counts the connections it took. It stays
/// bound until it is dropped, so its port is never another process's meanwhile.
struct Closer {
    endpoint: String,
    accepted: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Closer {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (count, ended) = (Arc::clone(&accepted), Arc::clone(&stop));
        let thread = std::thread::spawn(move || {
            for connection in listener.incoming() {
                if ended.load(Ordering::SeqCst) {
                    return;
                }
                if connection.is_ok() {
                    count.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        Self {
            endpoint: format!("http://{addr}"),
            accepted,
            stop,
            thread: Some(thread),
        }
    }

    /// The connections taken so far.
    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for Closer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // A connection wakes the thread that waits in `accept`, which then sees the flag.
        let at = self.endpoint.trim_start_matches("http://").to_owned();
        let _ = std::net::TcpStream::connect(at);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Asserts that a start does not ask its cold tier anything: the cold tier is an S3 store at a
/// loopback listener that counts the connections it takes, the address is taken so that the start
/// ends after the settings are built, and the start fails at the address (status 1, nothing on
/// stdout) and not at the tier: no line says a tier was tested or is unreachable, and the cold
/// store was never connected to. The two keys of the removed start-up self-test are accepted and
/// each is one warning, word for word.
#[test]
fn a_start_does_not_ask_its_tiers_and_the_keys_of_the_removed_self_test_warn() {
    let files = Files::new("bin-no-self-test");
    let secret = files.secret("cold-key", "ID\nsecret\n");
    let key = secret.display().to_string();
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = taken.local_addr().unwrap().to_string();
    let cold = Closer::start();
    let tables = Tables {
        listen: vec![address.clone()],
        server: "backend_check_wait = \"2s\"".to_owned(),
        cold: format!(
            "kind = \"s3\"\nbucket = \"cold-bucket\"\nendpoint = {:?}\ncredentials_file = {key:?}\nskip_checks = [\"multipart-conditional=because\"]",
            cold.endpoint
        ),
        ..Tables::default()
    };
    let (code, out, err) = run(&files.flags_of(&tables, &[]));
    assert_eq!((code, out.as_str()), (1, ""));
    let ignored =
        " is deprecated and ignored: tierstore no longer tests its stores at start-up; remove it";
    let warned: Vec<&str> = err
        .lines()
        .filter(|line| line.contains("deprecated"))
        .collect();
    let skip = format!("tierstore: cold_store.skip_checks{ignored}");
    let wait = format!("tierstore: server.backend_check_wait{ignored}");
    assert_eq!(warned, [skip, wait], "{err}");
    let last = err.lines().last().unwrap();
    assert!(
        last.starts_with(&format!("tierstore: binding {address}")),
        "{err}"
    );
    assert!(!err.contains("unreachable"), "{err}");
    assert!(!err.contains("tested the"), "{err}");
    assert_eq!(cold.accepted(), 0);
}

/// Asserts what a start still asks of the hot tier: opening the proxy reads and writes the hot
/// tier's own state (the node's registration and its journal), so a hot tier that cannot be
/// reached (a loopback listener that closes every connection) fails the start with status 1 (the
/// unit tries again), nothing on stdout, and a last line that begins
/// `tierstore: opening the proxy: `. No line says a tier was tested.
#[test]
fn a_hot_tier_that_cannot_be_reached_fails_the_start_at_the_proxy_with_status_1() {
    let files = Files::new("bin-hot-unreachable");
    let secret = files.secret("hot-key", "ID\nsecret\n");
    let key = secret.display().to_string();
    let hot = Closer::start();
    let tables = Tables {
        hot: format!(
            "kind = \"s3\"\nbucket = \"hot-bucket\"\nendpoint = {:?}\ncredentials_file = {key:?}\nbudget = 1048576",
            hot.endpoint
        ),
        ..Tables::default()
    };
    let flags = files.flags_of(&tables, &[]);
    let (code, out, err) = run(&flags);
    assert_eq!((code, out.as_str()), (1, ""));
    let last = err.lines().last().unwrap();
    assert!(last.starts_with("tierstore: opening the proxy: "), "{err}");
    assert!(!err.contains("tested the"), "{err}");
    assert!(hot.accepted() >= 1);
}

/// Asserts that `conditional_delete = false` for a `memory` or a `dir` tier is a refusal that a
/// restart cannot mend, status 2, with nothing on stdout, and that the message names the key, in
/// full, for either tier.
#[test]
fn conditional_delete_false_for_a_memory_or_dir_tier_is_status_2() {
    let files = Files::new("bin-delete-off");
    let tier = files.key.with_file_name("cold");
    let dir = format!("kind = \"dir\"\npath = {:?}", tier.display().to_string());
    let hot = Tables {
        hot: "kind = \"memory\"\nbudget = 1000\nconditional_delete = false".to_owned(),
        ..Tables::default()
    };
    let cold = Tables {
        cold: format!("{dir}\nconditional_delete = false"),
        ..Tables::default()
    };
    for (tables, key) in [(hot, "hot_store"), (cold, "cold_store")] {
        let (code, out, err) = run(&files.flags_of(&tables, &[]));
        assert_eq!((code, out.as_str()), (2, ""), "{key}: {err}");
        let want = format!(
            "{key}.conditional_delete: false is only for kind \"s3\": a memory or dir tier deletes conditionally in one atomic step"
        );
        assert!(err.contains(&want), "{err}");
    }
}

/// Asserts that a tier that its keys do not describe is refused: an S3 tier with no endpoint, and
/// the keys of an S3 tier given for a tier that is not one, are a file that does not load (status
/// 2, naming the key); a credentials file that is not there and a bucket name that is not a
/// bucket name are a start that fails (status 1).
#[test]
fn an_s3_tier_that_its_keys_do_not_describe_is_refused_saying_why() {
    let files = Files::new("bin-s3-refused");
    let secret = files.secret("hot-key", "ID\nsecret\n");
    let key = secret.display().to_string();
    let hot = |lines: &str| Tables {
        hot: format!("budget = 1048576\n{lines}"),
        ..Tables::default()
    };
    let no_endpoint = hot(&format!(
        "kind = \"s3\"\nbucket = \"hot-bucket\"\ncredentials_file = {key:?}"
    ));
    let (code, out, err) = run(&files.flags_of(&no_endpoint, &[]));
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.starts_with("tierstore: "), "{err}");
    assert!(err.contains("hot_store.endpoint: it is needed"), "{err}");

    let described = |bucket: &str, credentials: &str| {
        hot(&format!(
            "kind = \"s3\"\nbucket = \"{bucket}\"\nendpoint = \"http://127.0.0.1:1\"\ncredentials_file = \"{credentials}\""
        ))
    };
    let missing = described("hot-bucket", "/no/such/file");
    let (code, _, err) = run(&files.flags_of(&missing, &[]));
    assert_eq!(code, 1);
    assert!(err.contains("hot_store.credentials_file"), "{err}");
    let named = described("Not_A_Bucket", &key);
    let (code, _, err) = run(&files.flags_of(&named, &[]));
    assert_eq!(code, 1);
    assert!(err.contains("bucket name"), "{err}");
    let memory = hot(&format!(
        "kind = \"memory\"\nendpoint = \"http://127.0.0.1:1\"\ncredentials_file = {key:?}"
    ));
    let (code, _, err) = run(&files.flags_of(&memory, &[]));
    assert_eq!(code, 2);
    assert!(
        err.contains("hot_store.endpoint: not for kind \"memory\""),
        "{err}"
    );
}
