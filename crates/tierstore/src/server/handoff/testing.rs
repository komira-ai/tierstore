//! Fixtures of the tests of the hand-off: a scratch directory for the control socket, listening
//! sockets, and a fake running instance that says what a test wants it to.

use std::fmt::Display;
use std::future::Future;
use std::net::{SocketAddr, TcpListener};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::PathBuf;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;
use tokio::net::{UnixListener, UnixStream};

use super::wire::{self, Frame};
use super::{Front, Handoff, Socket};

/// The longest a test of the hand-off waits for one step: twice the longest step (the lingering
/// close of a connection whose client keeps its end open, which waits the server's
/// `header_timeout` of 60s), so it only turns a step that never ends into a failure that names
/// it. No test passes or fails by how long a step took.
const BOUND: Duration = Duration::from_secs(120);

/// What `waiting` gives, or a failure of the test that names `step` if it has not given it within
/// [`BOUND`]: a test that would hang fails instead, and says where.
pub(crate) async fn within<F: Future>(step: impl Display, waiting: F) -> F::Output {
    let waited = tokio::time::timeout(BOUND, waiting).await;
    let hung = |_| format!("{step}: not done within {BOUND:?}, so the test stops here");
    waited.map_err(hung).expect("every step of a test ends")
}

/// A scratch directory and the path of a control socket in it. Keep the directory while the
/// path is used.
pub(crate) fn scratch() -> (TestDir, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), "handoff");
    let path = dir.path().join("control.sock");
    (dir, path)
}

/// The hand-off of `path`, with a wait that only a hung test reaches.
pub(crate) fn handoff(path: PathBuf) -> Handoff {
    Handoff {
        socket: path,
        wait: Duration::from_secs(3600),
    }
}

/// A listening socket on a loopback port the system picks, and its address.
pub(crate) fn listening() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a socket");
    let addr = listener.local_addr().expect("an address");
    (listener, addr)
}

/// The socket of `front` that is a copy of `listener`.
pub(crate) fn socket(front: Front, listener: &TcpListener) -> Socket {
    Socket::new(front, listener.try_clone().expect("a copy"))
}

/// A client's connection to the control socket at `path`.
pub(crate) async fn connect(path: &std::path::Path) -> UnixStream {
    UnixStream::connect(path).await.expect("a connection")
}

/// Sends the request `lines` on `stream` and reads the answer.
pub(super) async fn ask(stream: &UnixStream, lines: &[&str]) -> Frame {
    send(stream, lines).await;
    wire::recv(stream).await.expect("an answer")
}

/// Sends a frame of `lines` with no descriptor.
pub(crate) async fn send(stream: &UnixStream, lines: &[&str]) {
    let lines: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
    wire::send(stream, &lines, &[]).await.expect("sent");
}

/// A fake running instance at `path`: it reads one request, answers with `lines` and `fds`, and,
/// if `then` is given, reads the taker's answer and replies with `then`. Returns when it is done.
pub(crate) fn pretending(
    path: &std::path::Path,
    lines: Vec<String>,
    fds: Vec<TcpListener>,
    then: Option<Vec<String>>,
) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(path).expect("a control socket");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("a taker");
        wire::recv(&stream).await.expect("a request");
        let borrowed: Vec<BorrowedFd<'_>> = fds.iter().map(AsFd::as_fd).collect();
        wire::send(&stream, &lines, &borrowed).await.expect("sent");
        if let Some(then) = then {
            wire::recv(&stream).await.expect("an answer");
            wire::send(&stream, &then, &[]).await.expect("sent");
        }
    })
}

/// [`pretending`], with `before_then` run after the taker's answer is read and before `then` is
/// sent: a test changes the world at the very point between the taker's `ack` and its `done`.
pub(crate) fn pretending_with(
    path: &std::path::Path,
    lines: Vec<String>,
    fds: Vec<TcpListener>,
    then: Vec<String>,
    before_then: impl FnOnce() + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(path).expect("a control socket");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("a taker");
        wire::recv(&stream).await.expect("a request");
        let borrowed: Vec<BorrowedFd<'_>> = fds.iter().map(AsFd::as_fd).collect();
        wire::send(&stream, &lines, &borrowed).await.expect("sent");
        wire::recv(&stream).await.expect("an answer");
        before_then();
        wire::send(&stream, &then, &[]).await.expect("sent");
    })
}

/// Asserts that a step that never ends fails the test with the step's name, on the paused clock
/// (no real time passes).
#[tokio::test(start_paused = true)]
#[should_panic(expected = "a step that never ends: not done within 120s, so the test stops here")]
async fn a_step_that_never_ends_fails_with_its_name() {
    within("a step that never ends", std::future::pending::<()>()).await;
}
