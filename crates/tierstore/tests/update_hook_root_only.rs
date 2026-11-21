//! The update hook's check of the configuration (`--check-config`) run by the real binary through
//! the real runner, from a staged release that only root may enter, as hostkeeper keeps every
//! staged package: the service account cannot search the directories above the binary, and the
//! check runs all the same, and still refuses a key file the account cannot read, naming it.
//!
//! The tests that need a second user run only when the test process is root, and the others only
//! when it is not (each says so on standard error and passes otherwise). In the first, the hook
//! is root and the check runs as the account of `etc/passwd` under the test's root, mapped to the
//! user and group `nobody` (65534). In the others the runner closes the release's directory
//! (mode 0) between the hook's open of the binary and the child's start, so the child cannot
//! search it, as the account cannot search a root-only tree.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, chown};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tierstore::server::update_hook::Host;
use tierstore::server::update_hook::machine::{
    Call, Output, RunError, Runner, SystemHost, SystemRunner,
};
use tierstore_testkit::dir::TestDir;

/// The staged release, as hostkeeper names it, under the test's root.
const RELEASE: &str = "/packages/tierstore/0.3.156";
/// The binary of a release of the Debian or RPM package, under the release.
const BINARY: &str = "tree/usr/libexec/tierstore/tierstore";
/// The node's configuration file, under the test's root.
const CONFIG: &str = "/etc/tierstore/config.toml";
/// The user and group ids the root tests give the service account: `nobody`.
const NOBODY: u32 = 65534;
/// The time a check may take.
const LIMIT: Duration = Duration::from_secs(60);

fn mode(path: &Path, bits: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(bits)).unwrap();
}

/// A node under a scratch root: the staged release with the tierstore binary of this build at
/// [`BINARY`], and a configuration at [`CONFIG`] that names one key file, `access.key`, mode 0600.
struct Node {
    _dir: TestDir,
    root: PathBuf,
}

impl Node {
    /// The binary is a hard link of the build's when `link` (the scratch root is then on the
    /// file system of the build's target directory) and it can be, else a copy, owned by the user
    /// that makes it.
    fn new(base: &Path, name: &str, link: bool) -> Node {
        let dir = TestDir::under(base, name);
        let root = dir.path().to_path_buf();
        let binary = root.join(RELEASE.trim_start_matches('/')).join(BINARY);
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        // The release's directories as hostkeeper leaves them, whatever the umask.
        let mut above = binary.clone();
        while above.pop() && above != root {
            mode(&above, 0o755);
        }
        let built = Path::new(env!("CARGO_BIN_EXE_tierstore"));
        // A link only of a binary no group or others can write: the hook refuses any other.
        let linkable = link && fs::metadata(built).unwrap().mode() & 0o022 == 0;
        if !linkable || fs::hard_link(built, &binary).is_err() {
            fs::copy(built, &binary).unwrap();
            mode(&binary, 0o755);
        }
        let etc = root.join("etc/tierstore");
        fs::create_dir_all(&etc).unwrap();
        let key = etc.join("access.key");
        fs::write(&key, "ID1\nsecret-0123456789abcdef\n").unwrap();
        mode(&key, 0o600);
        let config = format!(
            "[[interface]]\ntype = \"s3\"\nparams = {{ addresses = [\"127.0.0.1:0\"] }}\n\
             [server]\nproxy_id = \"t-1\"\nbucket = \"b\"\naccess_key_file = \"{}\"\n\
             [cold_store]\nkind = \"dir\"\npath = \"{}\"\n",
            key.display(),
            root.join("cold").display()
        );
        fs::write(etc.join("config.toml"), config).unwrap();
        Node { _dir: dir, root }
    }

    fn at(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    fn key(&self) -> PathBuf {
        self.at("/etc/tierstore/access.key")
    }

    /// The host the hook sees, as the user `hook_uid`, with `owner` as the release owner.
    fn host<'a>(&self, runner: &'a dyn Runner, hook_uid: u32, owner: u32) -> SystemHost<'a> {
        SystemHost::new(self.root.clone(), runner, LIMIT)
            .with_hook_user(hook_uid)
            .with_release_owner(owner)
    }
}

/// The real runner, with the directory `closed` at mode 0 while a program runs: only between the
/// hook's open of the binary and the program's end can no user but root search it.
struct Closing {
    closed: PathBuf,
}

impl Runner for Closing {
    fn run(&self, call: &Call) -> Result<Output, RunError> {
        mode(&self.closed, 0o000);
        let ran = SystemRunner.run(call);
        mode(&self.closed, 0o755);
        ran
    }
}

/// Says why a test does not run here, on standard error.
fn say(line: &str) {
    let _ = writeln!(io::stderr(), "{line}");
}

/// The user this test runs as.
fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// Asserts that the check runs the staged binary, and accepts the configuration, when the user
/// it runs as cannot search the directory of the release: the hook opens the binary and the
/// child executes that open file. Catches a check that starts the binary by its path, which
/// fails with "Permission denied" on every host whose staged packages only root may enter, so
/// that `prepare` fails `host_failed` and no release can be installed.
#[test]
fn a_check_runs_the_staged_binary_when_its_directories_cannot_be_searched() {
    let node = Node::new(
        Path::new(env!("CARGO_TARGET_TMPDIR")),
        "hook-root-only-runs",
        true,
    );
    if me() == 0 {
        say("skipped: root searches a directory of mode 0; the root tests run this case");
        return;
    }
    let runner = Closing {
        closed: node.at(RELEASE),
    };
    let host = node.host(&runner, me(), me());
    let got = host.check_config(Path::new(RELEASE), Path::new(CONFIG));
    assert_eq!(got, Ok(None));
}

/// Asserts that the check of a binary in a release nobody but root may search still refuses a
/// key file the user it runs as cannot read, with the binary's own line, which names the file.
/// Catches a check that passes by not reading the key files, or that fails before it reads them.
#[test]
fn a_check_from_a_closed_release_names_the_key_file_it_cannot_read() {
    let node = Node::new(
        Path::new(env!("CARGO_TARGET_TMPDIR")),
        "hook-root-only-key",
        true,
    );
    if me() == 0 {
        say("skipped: root reads a key file of mode 0, so this runs as another user only");
        return;
    }
    mode(&node.key(), 0o000);
    let runner = Closing {
        closed: node.at(RELEASE),
    };
    let host = node.host(&runner, me(), me());
    let got = host.check_config(Path::new(RELEASE), Path::new(CONFIG));
    let line = format!(
        "tierstore: server.access_key_file {}: Permission denied (os error 13)",
        node.key().display()
    );
    assert_eq!(got, Ok(Some(line)));
}

/// A node made by root for the root tests: under the system's temporary directory (which every
/// user may search), the release's directories mode 0700 and root's, the binary a copy of root's,
/// `etc/passwd` naming `tierstore` as `nobody`, and the configuration and its key file readable
/// by `nobody` (the key file `nobody`'s, mode 0600). `None`, with the reason on standard error,
/// when the test process is not root.
fn root_node(name: &str) -> Option<Node> {
    if me() != 0 {
        say(
            "skipped: a check run as another user than the hook's needs the hook to be root; \
             run the tests as root to run this one",
        );
        return None;
    }
    let node = Node::new(&std::env::temp_dir(), name, false);
    mode(&node.root, 0o755);
    let mut dir = node.at(RELEASE).join(BINARY);
    while dir.pop() && dir != node.root {
        mode(&dir, 0o700);
    }
    fs::write(
        node.at("/etc/passwd"),
        format!("tierstore:x:{NOBODY}:{NOBODY}::/:/bin/false\n"),
    )
    .unwrap();
    mode(&node.at("/etc"), 0o755);
    mode(&node.at("/etc/tierstore"), 0o755);
    mode(&node.at(CONFIG), 0o644);
    chown(node.key(), Some(NOBODY), Some(NOBODY)).unwrap();
    Some(node)
}

/// Asserts that a root hook runs the check of a release whose directories are mode 0700 and
/// root's as the service account, and that the account's check accepts the configuration and
/// the key file it owns. Catches a check that starts the staged binary by its path as the
/// account, which cannot search the release (`Permission denied`, `prepare` fails `host_failed`).
#[test]
fn a_root_hook_checks_as_the_account_from_a_release_only_root_may_enter() {
    let Some(node) = root_node("hook-root-only-as-root") else {
        return;
    };
    assert_eq!(
        fs::metadata(node.at(RELEASE)).unwrap().mode() & 0o777,
        0o700
    );
    let host = node.host(&SystemRunner, 0, 0);
    let got = host.check_config(Path::new(RELEASE), Path::new(CONFIG));
    assert_eq!(got, Ok(None));
}

/// Asserts that the account's check from a root-only release still refuses a key file the
/// account cannot read (root's, mode 0600), with the binary's line naming the file. Catches a
/// check that runs as root, which would read the file and accept what the service cannot start
/// with.
#[test]
fn a_root_hook_check_names_the_key_file_the_account_cannot_read() {
    let Some(node) = root_node("hook-root-only-as-root-key") else {
        return;
    };
    chown(node.key(), Some(0), Some(0)).unwrap();
    let host = node.host(&SystemRunner, 0, 0);
    let got = host.check_config(Path::new(RELEASE), Path::new(CONFIG));
    let line = format!(
        "tierstore: server.access_key_file {}: is not owned by the user the server runs as",
        node.key().display()
    );
    assert_eq!(got, Ok(Some(line)));
}
