//! The `fake-s3` program, from outside: its flags, its start line, its control socket, and the
//! S3 service it serves as the control socket says.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

use tierstore_testkit::dir::TestDir;

fn scratch(name: &str) -> (TestDir, PathBuf) {
    let temp = std::env::temp_dir();
    TestDir::for_socket(
        &[temp.as_path(), Path::new(support::SCRATCH)],
        name,
        "c.sock",
    )
}

/// The program started on `control`, and its start line.
fn started(control: &Path) -> (Child, String) {
    let mut child = Command::new(support::FAKE_S3)
        .arg("--control")
        .arg(control)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout: ChildStdout = child.stdout.take().unwrap();
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    (child, line)
}

/// Sends each of `lines` on one connection to `control`: the answers.
fn ask(control: &Path, lines: &[&str]) -> Vec<String> {
    let stream = UnixStream::connect(control).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    lines
        .iter()
        .map(|line| {
            writer.write_all(format!("{line}\n").as_bytes()).unwrap();
            let mut answer = String::new();
            reader.read_line(&mut answer).unwrap();
            answer.trim_end().to_owned()
        })
        .collect()
}

/// One request to the service at `address`: its status line.
fn status_of(address: &str, method: &str, key: &str) -> String {
    let mut sock = TcpStream::connect(address).unwrap();
    let head =
        format!("{method} /cas/{key} HTTP/1.1\r\nhost: {address}\r\ncontent-length: 1\r\n\r\nx");
    sock.write_all(head.as_bytes()).unwrap();
    let mut line = String::new();
    BufReader::new(&mut sock).read_line(&mut line).unwrap();
    line.trim_end().to_owned()
}

/// Asserts the start line; that each command is answered on the control socket, a stall and a
/// failure as the service then serves, a line that is no command refused with its reason, and
/// the counts of `stats`; that a connection closed without a command leaves the socket serving;
/// and that `stop` ends the program with status 0.
#[test]
fn the_control_socket_switches_how_the_service_serves() {
    let (_dir, control) = scratch("fake-s3-control");
    let (mut child, line) = started(&control);
    let words: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(words[0], "fake-s3");
    let address = words[1].strip_prefix("listen=").unwrap().to_owned();
    assert_eq!(words[2], format!("control={}", control.display()));

    assert_eq!(status_of(&address, "PUT", "p/one"), "HTTP/1.1 200 OK");
    assert_eq!(
        ask(&control, &["fail 503 p/", "stats", "halt", "serve"]),
        [
            "ok",
            "stalled=0 begun=1",
            "refused: unknown command \"halt\"",
            "ok"
        ]
    );
    assert_eq!(ask(&control, &["fail 500"]), ["ok"]);
    assert_eq!(
        status_of(&address, "GET", "p/one"),
        "HTTP/1.1 500 Internal Server Error"
    );
    drop(UnixStream::connect(&control).unwrap());
    assert_eq!(ask(&control, &["stall p/"]), ["ok"]);
    let mut held = TcpStream::connect(&address).unwrap();
    held.write_all(format!("GET /cas/p/one HTTP/1.1\r\nhost: {address}\r\n\r\n").as_bytes())
        .unwrap();
    let mut counted = ask(&control, &["stats"]);
    while counted != ["stalled=1 begun=3"] {
        std::thread::sleep(std::time::Duration::from_millis(10));
        counted = ask(&control, &["stats"]);
    }
    assert_eq!(ask(&control, &["serve"]), ["ok"]);
    let mut answer = [0u8; 15];
    held.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"HTTP/1.1 200 OK");

    assert_eq!(ask(&control, &["stop"]), ["ok"]);
    assert_eq!(child.wait().unwrap().code(), Some(0));
}

/// Asserts the exit statuses of a program that cannot run: 1, naming the socket, for a control
/// socket that is there already; 2 for a missing flag; and 0 for `--help`, which prints the
/// flags.
#[test]
fn a_program_that_cannot_run_says_why_with_its_status() {
    let (dir, control) = scratch("fake-s3-refused");
    std::fs::write(&control, b"in the way").unwrap();
    let taken = Command::new(support::FAKE_S3)
        .arg("--control")
        .arg(&control)
        .output()
        .unwrap();
    assert_eq!(taken.status.code(), Some(1));
    let said = String::from_utf8(taken.stderr).unwrap();
    assert!(
        said.starts_with(&format!(
            "fake-s3: the control socket {}: ",
            control.display()
        )),
        "{said}"
    );

    let missing = Command::new(support::FAKE_S3).output().unwrap();
    assert_eq!(missing.status.code(), Some(2));
    let help = Command::new(support::FAKE_S3)
        .arg("--help")
        .output()
        .unwrap();
    assert_eq!(help.status.code(), Some(0));
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("--control <CONTROL>")
    );
    drop(dir);
}
