//! A build node and the hand-off of its listening sockets: two nodes over one cache directory
//! and one control socket, as an update runs them.

use std::future::Future;
use std::net::SocketAddr;
use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixListener};
use tokio::sync::oneshot;

use super::Drained;
use super::cache_cli::serve_cache;
use super::cache_cli_tests::{Node, QUICK, probe, serving};
use super::cli::{Addresses, Error, HandingError, exit_status};
use super::config::settings_from_text;
use super::config_cache::CacheSettings;
use super::config_testing::oci_interface;
use super::handoff::HandoffError;
use super::local_cache::{Disk, SystemDisk};
use crate::{Backend, MemoryBackend};

/// What a client sends to ask `/healthz`, keeping the connection alive.
const HEALTHZ: &[u8] = b"GET /healthz HTTP/1.1\r\nhost: tier.test\r\n\r\n";

/// A scratch directory for the control socket, and the socket's path in it. The directory is
/// under the system's temporary directory, whose path is short enough for a Unix socket.
fn control() -> (TestDir, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), "tscache-handoff");
    let socket = dir.path().join("control.sock");
    (dir, socket)
}

/// The settings of a build node over `node` that hands off over `socket`, waiting `wait` (a
/// key's text); `listen` replaces the address of its `s3` interface, and `extra` is more
/// interfaces.
fn handing(node: &Node, socket: &Path, wait: &str, listen: &str, extra: &str) -> CacheSettings {
    let server = format!(
        "handoff_socket = {:?}\nhandoff_wait = \"{wait}\"",
        socket.display().to_string()
    );
    let text = format!("{}{extra}", node.text(&server, "")).replace("127.0.0.1:0", listen);
    settings_from_text(&text, QUICK)
        .unwrap()
        .into_cache()
        .unwrap()
}

/// Asserts that `socket` is a Unix socket of mode 0600.
fn assert_control_socket(socket: &Path) {
    let meta = std::fs::symlink_metadata(socket).unwrap();
    assert!(meta.file_type().is_socket(), "{}", socket.display());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
}

/// The ends of two build nodes that run in one task: the first, which is serving when `work` is
/// called, and the second, which `go` starts.
struct Pair {
    /// What the first serves.
    old: Addresses,
    /// Starts the second.
    go: oneshot::Sender<()>,
    /// What the second serves, once it has taken the sockets.
    new_bound: oneshot::Receiver<Addresses>,
    /// Resolves when the first has ended.
    old_ended: oneshot::Receiver<()>,
    /// Stops the second.
    stop_new: oneshot::Sender<()>,
}

/// Runs a build node on `first` and, once `work` says go, one on `second`, over one in-memory
/// upstream and the real disk; `work` drives them. Gives how each ended.
async fn two_nodes<F, Fut>(
    first: &CacheSettings,
    second: &CacheSettings,
    work: F,
) -> (Result<Drained, Error>, Result<Drained, Error>)
where
    F: FnOnce(Pair) -> Fut,
    Fut: Future<Output = ()>,
{
    let upstream: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let (old_told, old_bound) = oneshot::channel::<Addresses>();
    let (new_told, new_bound) = oneshot::channel::<Addresses>();
    let (old_done, old_ended) = oneshot::channel::<()>();
    let (go, going) = oneshot::channel::<()>();
    let (stop_old, stopped_old) = oneshot::channel::<()>();
    let (stop_new, stopped_new) = oneshot::channel::<()>();
    let old = async {
        let stopped = async move {
            let _ = stopped_old.await;
        };
        let drained = serve_cache(
            first,
            Arc::clone(&upstream),
            Arc::clone(&disk),
            stopped,
            move |at| drop(old_told.send(at)),
        )
        .await;
        let _ = old_done.send(());
        drained
    };
    let new = async {
        going.await.unwrap();
        let stopped = async move {
            let _ = stopped_new.await;
        };
        serve_cache(
            second,
            Arc::clone(&upstream),
            Arc::clone(&disk),
            stopped,
            move |at| drop(new_told.send(at)),
        )
        .await
    };
    let driving = async {
        let old = old_bound.await.unwrap();
        work(Pair {
            old,
            go,
            new_bound,
            old_ended,
            stop_new,
        })
        .await;
        // Held until `work` is over: the first stops when a successor takes its sockets.
        drop(stop_old);
    };
    let (old, new, ()) = tokio::join!(old, new, driving);
    (old, new)
}

/// Reads from `connection` until what was read ends with `end`, and gives it.
async fn read_until(connection: &mut TcpStream, end: &str) -> String {
    let mut got = Vec::new();
    let mut piece = [0_u8; 1024];
    while !got.ends_with(end.as_bytes()) {
        let read = connection.read(&mut piece).await.unwrap();
        assert_ne!(read, 0, "{:?}", String::from_utf8_lossy(&got));
        got.extend_from_slice(&piece[..read]);
    }
    String::from_utf8_lossy(&got).into_owned()
}

/// The `/readyz` answer of the instance that is not draining at `addr`. While the first drains,
/// both instances accept from one queue of the same socket, and the first answers every probe
/// 503 `draining`; a probe it takes is made again, in a bounded number of tries.
pub(super) async fn successor_readyz(addr: SocketAddr) -> (u16, String) {
    for _ in 0..1500 {
        let (status, text) = probe(addr, "/readyz").await;
        if (status, text.as_str()) != (503, "draining") {
            return (status, text);
        }
        tokio::task::yield_now().await;
    }
    panic!("every probe of /readyz at {addr} was answered by the draining instance");
}

/// Polls `/readyz` at `addr` until it says `want`, in a bounded number of tries.
async fn readyz_becomes(addr: SocketAddr, want: &str) {
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let mut last = String::new();
    for _ in 0..1500 {
        tick.tick().await;
        let (status, text) = probe(addr, "/readyz").await;
        if status == 200 && text == want {
            return;
        }
        last = text;
    }
    panic!("/readyz never said {want:?}; last: {last:?}");
}

/// Starts the second node while a client of the first has a request half written, and gives that
/// client's connection, what the second serves once it has taken the sockets, and the rest of
/// the client's request.
async fn take_with_request_in_flight(
    first: SocketAddr,
    go: oneshot::Sender<()>,
    new_bound: oneshot::Receiver<Addresses>,
) -> (TcpStream, Addresses, &'static [u8]) {
    let mut before = TcpStream::connect(first).await.unwrap();
    let (head, rest) = HEALTHZ.split_at(16);
    before.write_all(head).await.unwrap();
    go.send(()).unwrap();
    (before, new_bound.await.unwrap(), rest)
}

/// Asserts that a second build node started on the control socket of a running one takes its
/// sockets while a request is in flight: the client connected to the first with a request half
/// written gets its answer (`/healthz` 200); the second says the first's
/// address though its file names port 0; a new connection is answered by the second (`ready
/// read-only`, as the first still holds the cache directory); the control socket is a Unix socket
/// of mode 0600 before and after; the first ends with nothing cut; and the second stops on its
/// stop with nothing cut and removes the control socket. Catches: a second node that binds
/// beside the first, one that cuts the request in flight, and one that leaves no control socket
/// for the next update.
#[tokio::test]
async fn a_second_build_node_takes_the_sockets_with_a_request_in_flight() {
    let node = Node::new("tscache-handoff-take");
    let (_dir, socket) = control();
    let first = handing(&node, &socket, "1h", "127.0.0.1:0", "");
    let seen = socket.clone();
    let work = |mut pair: Pair| async move {
        assert_control_socket(&seen);
        let (mut before, taken, rest) =
            take_with_request_in_flight(pair.old.s3, pair.go, pair.new_bound).await;
        assert_eq!(taken.s3_all, pair.old.s3_all, "the very sockets");
        assert_eq!(taken.oci, None);
        assert_control_socket(&seen);
        let (status, text) = successor_readyz(taken.s3).await;
        assert_eq!((status, text.as_str()), (200, "ready read-only"));
        before.write_all(rest).await.unwrap();
        let late = read_until(&mut before, "\r\n\r\nok").await;
        assert!(late.starts_with("HTTP/1.1 200"), "{late}");
        // The first's drain holds a connection until the client closes its end.
        drop(before);
        (&mut pair.old_ended).await.unwrap();
        assert_control_socket(&seen);
        readyz_becomes(taken.s3, "ready read-write").await;
        pair.stop_new.send(()).unwrap();
    };
    let (old, new) = two_nodes(&first, &first, work).await;
    assert_eq!(old.unwrap(), Drained { abandoned: 0 });
    assert_eq!(new.unwrap(), Drained { abandoned: 0 });
    assert!(
        !socket.exists(),
        "a node that stops with no successor removes its control socket"
    );
}

/// Asserts that the second node is read-only until the first has exited and read-write after:
/// while the first holds the cache directory (a client's request in flight keeps it draining)
/// the second's `/readyz` says `ready read-only`; once the first has ended it says `ready
/// read-write`, and the directory's lock is held by the second. Catches: a second node that
/// writes to a directory the first still writes to, and one that stays read-only for good.
#[tokio::test]
async fn the_second_build_node_is_read_only_until_the_first_has_exited() {
    let node = Node::new("tscache-handoff-mode");
    let (_dir, socket) = control();
    let first = handing(&node, &socket, "1h", "127.0.0.1:0", "");
    let lock = node.cache_dir().join(".lock");
    let work = |mut pair: Pair| async move {
        let (before, taken, _) =
            take_with_request_in_flight(pair.old.s3, pair.go, pair.new_bound).await;
        let (_, text) = successor_readyz(taken.s3).await;
        assert_eq!(text, "ready read-only");
        assert!(super::lock::try_take(&lock).unwrap().is_none());
        drop(before);
        (&mut pair.old_ended).await.unwrap();
        readyz_becomes(taken.s3, "ready read-write").await;
        assert!(super::lock::try_take(&lock).unwrap().is_none());
        pair.stop_new.send(()).unwrap();
    };
    let (old, new) = two_nodes(&first, &first, work).await;
    assert_eq!(old.unwrap(), Drained { abandoned: 0 });
    assert_eq!(new.unwrap(), Drained { abandoned: 0 });
}

/// Asserts that the registry front is handed over with the S3 front: the second names the same
/// OCI address as the first, though its file names port 0, and `/v2/` answers 200 there while the
/// first still serves and after it has ended. Catches: an OCI front the second binds anew beside
/// the first, or one that is lost in the hand-over.
#[tokio::test]
async fn the_oci_front_is_handed_over_with_the_s3_front() {
    let node = Node::new("tscache-handoff-oci");
    let (_dir, socket) = control();
    let oci = oci_interface("127.0.0.1:0", ", prefix = \"cas/\"");
    let first = handing(&node, &socket, "1h", "127.0.0.1:0", &oci);
    let work = |mut pair: Pair| async move {
        let at = pair.old.oci.unwrap();
        assert_eq!(probe(at, "/v2/").await.0, 200);
        let (before, taken, _) =
            take_with_request_in_flight(pair.old.s3, pair.go, pair.new_bound).await;
        assert_eq!(taken.oci, Some(at));
        assert_eq!(taken.oci_all, pair.old.oci_all);
        assert_eq!(probe(at, "/v2/").await.0, 200);
        drop(before);
        (&mut pair.old_ended).await.unwrap();
        assert_eq!(probe(at, "/v2/").await.0, 200);
        pair.stop_new.send(()).unwrap();
    };
    let (old, new) = two_nodes(&first, &first, work).await;
    assert_eq!(old.unwrap(), Drained { abandoned: 0 });
    assert_eq!(new.unwrap(), Drained { abandoned: 0 });
}

/// The hand-off error `error` holds, if it is one.
fn handoff_error(error: &Error) -> Option<&HandoffError> {
    match error.downcast_ref::<HandingError>()? {
        HandingError::Handoff(why) => Some(why),
        _ => None,
    }
}

/// Asserts that settings naming other addresses than the running node serves are refused: the
/// start fails with exit status 1 and the exact words of the mismatch (the want list of a build
/// node holds the added loopback socket for an address that is not loopback), binds nothing, and
/// the running node goes on serving and keeps its control socket. Catches: a start that binds
/// beside a node that listens elsewhere, and one that takes sockets it was not asked for.
#[tokio::test]
async fn settings_naming_other_addresses_than_the_running_node_are_refused() {
    let node = Node::new("tscache-handoff-mismatch");
    let (_dir, socket) = control();
    let first = handing(&node, &socket, "1h", "127.0.0.1:0", "");
    let other = handing(&node, &socket, "1h", "127.0.0.2:0", "");
    let upstream: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let seen = socket.clone();
    let work = |at: SocketAddr| {
        let (upstream, disk) = (Arc::clone(&upstream), Arc::clone(&disk));
        async move {
            let refused = serve_cache(&other, upstream, disk, async {}, |bound| {
                panic!("bound {bound:?}")
            })
            .await
            .unwrap_err();
            assert!(
                matches!(handoff_error(&refused), Some(HandoffError::Mismatch { .. })),
                "{refused}"
            );
            assert_eq!(exit_status(&refused), 1);
            let offered = vec![format!("s3 {at}")];
            let wanted = vec!["s3 127.0.0.2:0".to_owned(), "s3 127.0.0.1:0".to_owned()];
            assert_eq!(
                refused.to_string(),
                format!(
                    "hand-off: the running instance listens on {offered:?}, these settings name {wanted:?}"
                )
            );
            assert_eq!(probe(at, "/readyz").await.0, 200);
            assert_control_socket(&seen);
        }
    };
    let drained = serving(&first, Arc::clone(&upstream), Arc::clone(&disk), work)
        .await
        .unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
}

/// Asserts that with no instance on the control socket (no file, and a file no process listens
/// on, as a killed node leaves) the start binds its own sockets, serves read-write, makes the
/// control socket (mode 0600) and removes it on its stop. Catches: a start that fails or waits
/// out `server.handoff_wait` where nothing answers, and one that leaves the stale file.
#[tokio::test]
async fn with_no_node_on_the_socket_the_build_node_binds_its_own() {
    for stale in [false, true] {
        let node = Node::new("tscache-handoff-cold");
        let (_dir, socket) = control();
        if stale {
            drop(UnixListener::bind(&socket).unwrap());
            assert!(socket.exists(), "a file no one listens on");
        }
        let settings = handing(&node, &socket, "1h", "127.0.0.1:0", "");
        let upstream: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
        let seen = socket.clone();
        let work = |at: SocketAddr| async move {
            let (status, text) = probe(at, "/readyz").await;
            assert_eq!((status, text.as_str()), (200, "ready read-write"));
            assert_control_socket(&seen);
        };
        let drained = serving(&settings, upstream, disk, work).await.unwrap();
        assert_eq!(drained, Drained { abandoned: 0 }, "stale: {stale}");
        assert!(!socket.exists(), "stale: {stale}");
    }
}

/// Asserts that a running instance that accepts the control connection and never answers fails
/// the start once `server.handoff_wait` is over, with exit status 1 and the exact words naming
/// the socket, and that nothing is bound. Catches: a silent cold start where an instance is
/// there, and a start that waits for ever.
#[tokio::test]
async fn an_instance_that_never_answers_fails_the_build_node_start() {
    let node = Node::new("tscache-handoff-silent");
    let (_dir, socket) = control();
    let silent = UnixListener::bind(&socket).unwrap();
    let settings = handing(&node, &socket, "50ms", "127.0.0.1:0", "");
    let upstream: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let error = serve_cache(&settings, upstream, disk, async {}, |at| {
        panic!("bound {at:?}")
    })
    .await
    .unwrap_err();
    assert!(
        matches!(handoff_error(&error), Some(HandoffError::Timeout(path)) if path == &socket),
        "{error}"
    );
    assert_eq!(exit_status(&error), 1);
    assert_eq!(
        error.to_string(),
        format!(
            "hand-off: no end of the hand-off at {} in time",
            socket.display()
        )
    );
    drop(silent);
}
