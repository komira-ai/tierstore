//! `tierstore fs` on macOS as a process: the command line is parsed, the file is loaded (a group
//! named by name is looked up through the system), the NFS frontend serves on a loopback port, and
//! SIGTERM ends it with status 0. Nothing is mounted
//! (`fs.nfs.mount = false`), so the test runs without a mount command, and no assertion is about
//! time.

#![cfg(target_os = "macos")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::process::{Command, Stdio};

use rustix::process::{Pid, Signal, kill_process};
use tierstore_testkit::dir::TestDir;

/// The program number of NFS, and its procedure NULL.
const NFS: u32 = 100_003;
const NULL: u32 = 0;

/// The words before the address in the line the server says when it serves.
const SERVING: &str = "tierstore fs: serving NFS on ";

/// The call of procedure `procedure` of `program` (version 3) with no credential and no
/// arguments, as one record of the RPC framing.
fn call(program: u32, procedure: u32) -> Vec<u8> {
    let words = [7, 0, 2, program, 3, procedure, 0, 0, 0, 0];
    let body: Vec<u8> = words
        .iter()
        .flat_map(|word: &u32| word.to_be_bytes())
        .collect();
    let header = (1u32 << 31) | u32::try_from(body.len()).unwrap();
    [header.to_be_bytes().as_slice(), &body].concat()
}

/// A started `tierstore fs`: the process, what it said up to the line that it serves, the address
/// it serves on, and its scratch directory.
struct Started {
    child: std::process::Child,
    said: String,
    address: String,
    dir: TestDir,
}

/// Starts `tierstore fs --config <file>` with a valid file whose `fs.client_group` is `group` (the
/// group id of the scratch directory when none), and `fs.nfs.mount = false`, and reads its stderr
/// up to the line that it serves.
fn start(name: &str, group: Option<&str>) -> Started {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let (cache, runtime, mountpoint) = (
        dir.path().join("cache"),
        dir.path().join("run"),
        dir.path().join("mnt"),
    );
    for path in [&cache, &runtime, &mountpoint] {
        std::fs::create_dir_all(path).unwrap();
    }
    let gid = std::fs::metadata(&runtime).unwrap().gid();
    let uid = rustix::process::geteuid().as_raw();
    let client_group = group.map_or_else(|| gid.to_string(), str::to_owned);
    let text = format!(
        "[fs]\nmountpoint = {mountpoint:?}\nruntime_dir = {runtime:?}\n\
         control_socket = {:?}\nclient_group = \"{client_group}\"\nowner = \"{uid}:{gid}\"\n\
         fill_socket = {:?}\ncache_path = {cache:?}\n\
         [fs.nfs]\nlisten = \"127.0.0.1:0\"\nmount = false\n",
        dir.path().join("control.sock"),
        dir.path().join("fill.sock"),
    );
    let file = dir.path().join("fs.toml");
    std::fs::write(&file, text).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_tierstore"))
        .arg("fs")
        .arg("--config")
        .arg(&file)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs");
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut said = String::new();
    let address = loop {
        let mut line = String::new();
        if stderr.read_line(&mut line).unwrap() == 0 {
            let status = child.wait().unwrap();
            panic!("the command ended before it served: {status}: {said}");
        }
        said.push_str(&line);
        if let Some(rest) = line.strip_prefix(SERVING) {
            let end = rest.find(',').expect("the address ends at a comma");
            break rest[..end].to_owned();
        }
    };
    Started {
        child,
        said,
        address,
        dir,
    }
}

/// Ends `started` with SIGTERM, and asserts it ends with status 0.
fn stop(mut started: Started) {
    let pid = Pid::from_raw(i32::try_from(started.child.id()).unwrap()).unwrap();
    kill_process(pid, Signal::TERM).unwrap();
    let status = started.child.wait().unwrap();
    assert_eq!(status.code(), Some(0), "{status}: {}", started.said);
}

/// What `/usr/bin/id` says with `flag` of the user of this test, without the newline.
fn id(flag: &str) -> String {
    let out = Command::new("/usr/bin/id").arg(flag).output().unwrap();
    assert!(out.status.success(), "id {flag}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Asserts that `fs.client_group` given by name is looked up through the system (Open Directory,
/// by `dscacheutil`), not read from `/etc/group`: the process starts, and its control socket has
/// the group id the system gives the name (the primary group of the user of the test).
#[test]
fn fs_takes_its_client_group_by_name_from_the_system() {
    let name = id("-gn");
    let gid: u32 = id("-g").parse().unwrap();
    let started = start("fs-macos-group", Some(&name));
    let socket = started.dir.path().join("control.sock");
    assert_eq!(std::fs::metadata(&socket).unwrap().gid(), gid, "{name}");
    stop(started);
}

/// Asserts the command end to end: `tierstore fs --config <file>` with a valid file and
/// `fs.nfs.mount = false` says on which loopback port it serves, answers a call of the NFS
/// protocol on it (accepted, with the xid of the call), and on SIGTERM ends with status 0.
#[test]
fn fs_serves_nfs_and_ends_with_success_on_sigterm() {
    let started = start("fs-macos-cli", None);
    let address = started.address.clone();
    let mut stream = std::net::TcpStream::connect(&address).unwrap();
    stream.write_all(&call(NFS, NULL)).unwrap();
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).unwrap();
    let length = (u32::from_be_bytes(header) & !(1 << 31)) as usize;
    let mut reply = vec![0u8; length];
    stream.read_exact(&mut reply).unwrap();
    let words: Vec<u32> = reply
        .chunks_exact(4)
        .map(|word| u32::from_be_bytes(word.try_into().unwrap()))
        .collect();
    // xid, REPLY, MSG_ACCEPTED, the verifier (flavor, length), SUCCESS.
    assert_eq!(words[..6], [7, 1, 0, 0, 0, 0], "{words:?}");
    drop(stream);
    stop(started);
}
