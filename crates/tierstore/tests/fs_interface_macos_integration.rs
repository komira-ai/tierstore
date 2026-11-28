//! A build node of a Mac whose file has an `fs` interface, as a process: the one `tierstore`
//! serves the NFS front of the file system beside its S3 front, keeps it across SIGUSR2 (the
//! hand-over to the next image of its program, `docs/design/one-service.md` section 3.4), and
//! ends with status 0 at SIGTERM. Once with the mount left to the operator (`params.nfs.mount =
//! false`), where the NFS front is asked over the protocol; once with the mount made by the
//! process with `mount_nfs`, as the user of the test, on a scratch directory it owns.
//!
//! An integration target of real OS adapters (`mount_nfs`, `umount`, the kernel's NFS client): it
//! runs where macOS is tested beside a release (`beta.yml`), never in a pull request. Everything it
//! makes is in a scratch directory; the mount has a dead timeout of seconds, so that a server that
//! is gone ends it soon; no assertion is about time.

#![cfg(target_os = "macos")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};

use rustix::process::{Pid, Signal, kill_process};
use tierstore_testkit::dir::TestDir;

const BIN: &str = env!("CARGO_BIN_EXE_tierstore");

/// The key file's lines: an access key id and its secret.
const KEY: &str = "ID1\nsecret-0123456789abcdef\n";

/// The words before the address in the line the NFS front says when it serves.
const SERVING: &str = "tierstore fs: serving NFS on ";

/// The line the NFS front says when the mount is there already and is not made again.
const KEPT: &str = "is mounted already: the clients' mount goes on";

/// The program number of NFS, and its procedure NULL.
const NFS: u32 = 100_003;
const NULL: u32 = 0;

/// A node's scratch directory, the control socket of its file system in it, and its file.
struct Node {
    dir: TestDir,
    config: PathBuf,
}

impl Node {
    /// The node `name`, whose NFS front mounts its export when `mount`.
    fn new(name: &str, mount: bool) -> Self {
        let roots = [
            Path::new(env!("CARGO_TARGET_TMPDIR")),
            &std::env::temp_dir(),
        ];
        let (dir, control) = TestDir::for_socket(&roots, name, "ctl.sock");
        let key = dir.path().join("key");
        std::fs::write(&key, KEY).unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let (cache, runtime, mountpoint) = (
            dir.path().join("cache"),
            dir.path().join("run"),
            dir.path().join("mnt"),
        );
        for path in [&cache, &runtime, &mountpoint] {
            std::fs::create_dir_all(path).unwrap();
        }
        let (uid, gid) = (
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        );
        let text = format!(
            "[[interface]]\ntype = \"s3\"\nparams = {{ addresses = [\"127.0.0.1:0\"] }}\n\n\
             [[interface]]\ntype = \"fs\"\n[interface.params]\nmountpoint = {mountpoint:?}\n\
             runtime_dir = {runtime:?}\ncontrol_socket = {control:?}\nclient_group = \"{gid}\"\n\
             owner = \"{uid}:{gid}\"\nfetch_timeout = \"2s\"\n\
             [interface.params.nfs]\nlisten = \"127.0.0.1:0\"\nmount = {mount}\n\
             dead_timeout = \"5s\"\nunmount_wait = \"10s\"\n\n\
             [server]\nproxy_id = \"fs-interface\"\nbucket = \"b\"\naccess_key_file = {key:?}\n\
             drain_lb_wait = \"50ms\"\n\n\
             [local_cache]\npath = {cache:?}\nbudget = \"64MiB\"\n\n\
             [cold_store]\nkind = \"memory\"\n\n[[policy]]\nprefix = \"cas/\"\ncache = true\n",
        );
        let config = dir.path().join("node.toml");
        std::fs::write(&config, text).unwrap();
        Self { dir, config }
    }

    fn mountpoint(&self) -> PathBuf {
        self.dir.path().join("mnt")
    }

    /// Whether a file system is mounted at the mount point: its device is not its parent's.
    fn mounted(&self) -> bool {
        let device = |path: &Path| std::fs::metadata(path).unwrap().dev();
        device(&self.mountpoint()) != device(self.dir.path())
    }
}

/// A running node: the process, its standard output (the start lines), and the lines of its
/// standard error, read on a thread of their own.
struct Running {
    child: Child,
    stdout: BufReader<ChildStdout>,
    stderr: Receiver<String>,
    said: Vec<String>,
}

impl Running {
    fn start(node: &Node) -> Self {
        let mut child = Command::new(BIN)
            .arg("--config")
            .arg(&node.config)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let stderr = BufReader::new(child.stderr.take().unwrap());
        let (lines, received) = channel();
        drop(std::thread::spawn(move || {
            for line in stderr.lines().map_while(Result::ok) {
                if lines.send(line).is_err() {
                    return;
                }
            }
        }));
        Self {
            child,
            stdout,
            stderr: received,
            said: Vec::new(),
        }
    }

    fn pid(&self) -> Pid {
        Pid::from_raw(i32::try_from(self.child.id()).unwrap()).unwrap()
    }

    /// The next start line on standard output: `tierstore <version> listen=<address> ...`.
    fn start_line(&mut self) -> String {
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).unwrap();
        assert!(read > 0, "the process ended: {:?}", self.said);
        line
    }

    /// The next line of standard error that contains `words`, the lines before it kept.
    fn said_with(&mut self, words: &str) -> String {
        loop {
            let Ok(line) = self.stderr.recv() else {
                panic!(
                    "the process ended before it said {words:?}: {:?}",
                    self.said
                );
            };
            self.said.push(line.clone());
            if line.contains(words) {
                return line;
            }
        }
    }

    /// The address of the NFS front, from the next line that says it serves.
    fn nfs_address(&mut self) -> SocketAddr {
        let line = self.said_with(SERVING);
        let rest = &line[line.find(SERVING).unwrap() + SERVING.len()..];
        rest[..rest.find(',').unwrap()].parse().unwrap()
    }

    /// Sends `signal` to the process.
    fn signal(&self, signal: Signal) {
        kill_process(self.pid(), signal).unwrap();
    }

    /// Ends the process with SIGTERM and asserts that it ends with status 0.
    fn stop(mut self) {
        self.signal(Signal::TERM);
        let status = self.child.wait().unwrap();
        while let Ok(line) = self.stderr.recv() {
            self.said.push(line);
        }
        assert_eq!(status.code(), Some(0), "{status}: {:?}", self.said);
    }
}

impl Drop for Running {
    /// A test that failed before its stop ends the process the same way, so that it unmounts and
    /// leaves no process behind.
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = kill_process(self.pid(), Signal::TERM);
            let _ = self.child.wait();
        }
    }
}

/// The address of the start line `line`.
fn listen_of(line: &str) -> SocketAddr {
    let listen = line
        .split_whitespace()
        .find_map(|word| word.strip_prefix("listen="))
        .unwrap_or_else(|| panic!("a start line with listen=: {line}"));
    listen.parse().unwrap()
}

/// Whether the NFS front at `address` answers the procedure NULL, called as one record of the RPC
/// framing with no credential: a reply to the call's xid (7) that is accepted (`MSG_ACCEPTED`) and
/// succeeded (`SUCCESS`).
fn answers_null(address: SocketAddr) -> bool {
    let words = [7u32, 0, 2, NFS, 3, NULL, 0, 0, 0, 0];
    let body: Vec<u8> = words.iter().flat_map(|word| word.to_be_bytes()).collect();
    let header = (1u32 << 31) | u32::try_from(body.len()).unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    stream.write_all(&header.to_be_bytes()).unwrap();
    stream.write_all(&body).unwrap();
    let mut answer = [0u8; 28];
    if stream.read_exact(&mut answer).is_err() {
        return false;
    }
    // The record mark, then the xid, REPLY, MSG_ACCEPTED, the verifier (flavor and length) and
    // the accept status.
    let word = |at: usize| u32::from_be_bytes(answer[at..at + 4].try_into().unwrap());
    word(4) == 7 && word(8) == 1 && word(12) == 0 && word(24) == 0
}

/// Asserts that the one process with an `fs` interface serves its NFS front (the procedure NULL is
/// answered on the address it says), and that SIGUSR2 hands the front over to the next image of
/// the program in the same process: the process does not exit, the new image says its start line
/// on the same S3 address, serves NFS on the same address, which answers; SIGTERM then ends it
/// with status 0.
#[test]
fn the_nfs_front_of_the_one_process_is_served_and_handed_to_the_next_image() {
    let node = Node::new("fs-interface-exec", false);
    let mut running = Running::start(&node);
    let first = listen_of(&running.start_line());
    let nfs = running.nfs_address();
    assert!(answers_null(nfs));

    running.signal(Signal::USR2);
    let second = listen_of(&running.start_line());
    assert_eq!(second, first);
    assert!(
        running.child.try_wait().unwrap().is_none(),
        "the process did not exit"
    );
    assert_eq!(running.nfs_address(), nfs);
    assert!(answers_null(nfs));
    running.stop();
}

/// Asserts the mount of the one process on a real Mac: the start mounts the export at the mount
/// point (a file system is mounted there, and the `trees` directory is looked up in it), the
/// hand-over to the next image at SIGUSR2 leaves the mount up and the next image finds it and makes
/// none (it says so, and `trees` is still looked up through the mount), and SIGTERM unmounts it
/// before the process ends with status 0.
#[test]
fn the_one_process_mounts_keeps_the_mount_across_sigusr2_and_unmounts_at_sigterm() {
    let node = Node::new("fs-interface-mount", true);
    let mut running = Running::start(&node);
    let _ = running.start_line();
    let _ = running.said_with("mounted at");
    assert!(node.mounted(), "{:?}", running.said);
    // The root of the export is not listable (nobody lists the namespace), but `trees` in it is
    // looked up.
    let trees =
        |node: &Node| std::fs::metadata(node.mountpoint().join("trees")).map(|meta| meta.is_dir());
    assert!(trees(&node).unwrap(), "{:?}", running.said);

    running.signal(Signal::USR2);
    let _ = running.start_line();
    let _ = running.said_with(KEPT);
    assert!(
        running.child.try_wait().unwrap().is_none(),
        "the process did not exit"
    );
    assert!(node.mounted());
    assert!(trees(&node).unwrap(), "{:?}", running.said);

    running.stop();
    assert!(!node.mounted(), "the stop unmounts");
}

/// What `/usr/bin/id` says with `flag` of the user of this test, without the newline.
fn id(flag: &str) -> String {
    let out = Command::new("/usr/bin/id").arg(flag).output().unwrap();
    assert!(out.status.success(), "id {flag}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Runs `tierstore --check-config` over a node's file whose `fs` interface has `client_group` as
/// `group` and `owner` as `owner` and whose other keys are valid, and gives the process's output.
fn check(name: &str, group: &str, owner: &str) -> (std::process::Output, TestDir) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let (cache, runtime, mountpoint) = (
        dir.path().join("cache"),
        dir.path().join("run"),
        dir.path().join("mnt"),
    );
    let key = dir.path().join("key");
    std::fs::write(&key, KEY).unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let text = format!(
        "[[interface]]\ntype = \"s3\"\nparams = {{ port = 9300 }}\n\n\
         [[interface]]\ntype = \"fs\"\n[interface.params]\nmountpoint = {mountpoint:?}\n\
         runtime_dir = {runtime:?}\nclient_group = \"{group}\"\nowner = \"{owner}\"\n\n\
         [server]\nproxy_id = \"fs-interface\"\nbucket = \"b\"\naccess_key_file = {key:?}\n\n\
         [local_cache]\npath = {cache:?}\nbudget = \"64MiB\"\n\n\
         [cold_store]\nkind = \"memory\"\n\n[[policy]]\nprefix = \"cas/\"\ncache = true\n",
    );
    let file = dir.path().join("node.toml");
    std::fs::write(&file, text).unwrap();
    let output = Command::new(BIN)
        .arg("--config")
        .arg(&file)
        .arg("--check-config")
        .output()
        .expect("the binary runs");
    (output, dir)
}

/// Asserts that `client_group` of the `fs` interface given by name is looked up through the system
/// (Open Directory, by `dscacheutil`), not read from `/etc/group`: `--check-config` of a file that
/// names the primary group of the user of the test exits 0 and says nothing.
#[test]
fn the_fs_interface_takes_its_client_group_by_name_from_the_system() {
    let name = id("-gn");
    let gid = std::fs::metadata(std::env::temp_dir()).unwrap().gid();
    let owner = format!("{}:{gid}", id("-u"));
    let (output, _dir) = check("fs-interface-group", &name, &owner);
    assert_eq!(output.status.code(), Some(0), "{name}: {output:?}");
    assert!(
        output.stdout.is_empty() && output.stderr.is_empty(),
        "{output:?}"
    );
}

/// Asserts that `owner` of the `fs` interface given as `<user>:<group>` by the names of the user
/// of this test (their accounts of Open Directory, looked up by `dscacheutil`) is taken:
/// `--check-config` exits 0 and says nothing.
#[test]
fn the_fs_interface_takes_its_owner_by_names_from_the_system() {
    let owner = format!("{}:{}", id("-un"), id("-gn"));
    let (output, _dir) = check("fs-interface-owner", &id("-g"), &owner);
    assert_eq!(output.status.code(), Some(0), "{owner}: {output:?}");
    assert!(
        output.stdout.is_empty() && output.stderr.is_empty(),
        "{output:?}"
    );
}

/// Asserts that an `owner` whose user is a name the system does not have is refused: status 2 and
/// a line that names the `owner` param and the name.
#[test]
fn the_fs_interface_refuses_an_owner_whose_user_the_system_does_not_have() {
    let owner = format!("no-such-user-tierstore:{}", id("-gn"));
    let (output, _dir) = check("fs-interface-owner-absent", &id("-g"), &owner);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let said = String::from_utf8(output.stderr).unwrap();
    assert!(
        said.contains("params.owner: \"no-such-user-tierstore\" is neither a user id"),
        "{said}"
    );
}
