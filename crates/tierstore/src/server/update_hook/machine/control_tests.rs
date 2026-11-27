//! The control socket client against a listener the test runs itself, in a scratch directory
//! that stands for the host's root.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::thread::JoinHandle;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::super::ports::{Control, PortError, PortKind};
use super::ControlSocket;

/// The path of the socket as the node's file names it, under the test's root.
const SOCKET: &str = "/run/tierstore-fs/control.sock";

/// A listener at [`SOCKET`] under `root` for one connection: it reads the request line, answers
/// `answer` as it is (no newline added), closes, and gives the request back.
fn serve(root: &Path, answer: Vec<u8>) -> JoinHandle<String> {
    let path = root.join(SOCKET.trim_start_matches('/'));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut request = String::new();
        let _ = BufReader::new(&stream).read_line(&mut request).unwrap();
        (&stream).write_all(&answer).unwrap();
        request
    })
}

fn client(root: &Path) -> ControlSocket {
    ControlSocket::new(root.to_path_buf(), Duration::from_secs(5))
}

fn message(error: PortError) -> String {
    match error {
        PortError::Failed {
            kind: PortKind::Instances,
            message,
        } => message,
        other => panic!("not an instances failure: {other:?}"),
    }
}

/// Asserts that a request goes out as one line on the socket under the root, and that the line
/// it is answered with comes back without its newline; what follows the line is not read.
#[test]
fn a_request_is_one_line_out_and_its_answer_one_line_back() {
    let dir = TestDir::under(std::env::temp_dir(), "uh-control-ok");
    let server = serve(dir.path(), b"{\"ok\":true}\nmore\n".to_vec());
    let answer = client(dir.path()).ask(Path::new(SOCKET), "set tree_list_ram_bytes 4096");
    assert_eq!(answer, Ok("{\"ok\":true}".to_owned()));
    assert_eq!(server.join().unwrap(), "set tree_list_ram_bytes 4096\n");
}

/// Asserts that an answer that ends before its newline (the server closed), an empty one, and one
/// longer than the most that is read are no answer, and each says what was read.
#[test]
fn an_answer_that_is_not_a_whole_line_is_an_error() {
    for (name, answer, read) in [
        ("uh-control-cut", b"{\"ok\":tr".to_vec(), 8),
        ("uh-control-empty", Vec::new(), 0),
        ("uh-control-long", vec![b'x'; 64 * 1024 + 1], 64 * 1024),
    ] {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let server = serve(dir.path(), answer);
        let error = client(dir.path()).ask(Path::new(SOCKET), "set tree_list_ram_bytes 4096");
        let at = dir.path().join(SOCKET.trim_start_matches('/'));
        assert_eq!(
            message(error.unwrap_err()),
            format!(
                "\"set tree_list_ram_bytes 4096\" to {}: no whole line answered ({read} bytes)",
                at.display()
            )
        );
        let _ = server.join();
    }
}

/// Asserts that a socket that is not there is an error of the instances port that names the
/// request and the path under the root.
#[test]
fn a_socket_that_is_not_there_is_an_error() {
    let dir = TestDir::under(std::env::temp_dir(), "uh-control-none");
    let error = client(dir.path())
        .ask(Path::new(SOCKET), "set tree_list_ram_bytes 4096")
        .unwrap_err();
    let at = dir.path().join(SOCKET.trim_start_matches('/'));
    let text = message(error);
    assert!(
        text.starts_with(&format!(
            "\"set tree_list_ram_bytes 4096\" to {}: ",
            at.display()
        )),
        "{text}"
    );
}
