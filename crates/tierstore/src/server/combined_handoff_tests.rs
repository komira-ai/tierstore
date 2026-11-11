//! A storage node with `[local_cache]` and the hand-off of its listening sockets: two instances
//! in one process over the same tiers, cache directory, hot-tier lock file and control socket,
//! as an update runs them.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

use super::Drained;
use super::cache_cli_tests::probe;
use super::cache_handoff_tests::successor_readyz;
use super::cli::{Addresses, Error, HandingError, exit_status};
use super::combined_cli::serve_combined;
use super::combined_cli_tests::{Node, QUICK};
use super::config::settings_from_text;
use super::config_combined::CombinedSettings;
use super::config_testing::oci_interface;
use super::handoff::HandoffError;
use super::local_cache::{Disk, SystemDisk};
use super::lock::try_take;

use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};

/// What a client sends to ask `/healthz`, keeping the connection alive.
const HEALTHZ: &[u8] = b"GET /healthz HTTP/1.1\r\nhost: tier.test\r\n\r\n";

/// The key of the reconciler's lease in the hot tier.
const LEASE: &str = "_tiering/lease";

/// How many tries a wait for a state makes, with a short pause between two.
const TRIES: u32 = 1_000;

/// A short pause between two looks at a state that another task changes.
const PAUSE: Duration = Duration::from_millis(20);

/// A scratch directory for the control socket (under the system's temporary directory, whose
/// path is short enough for a Unix socket) and the socket's path in it.
fn control() -> (TestDir, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), "tscombined-handoff");
    let socket = dir.path().join("control.sock");
    (dir, socket)
}

/// The settings of the node with the hand-off over `socket`, with `interfaces` and `hot_extra`.
fn handing(node: &Node, socket: &Path, hot_extra: &str, interfaces: &str) -> CombinedSettings {
    let server = format!("handoff_socket = {:?}", socket.display().to_string());
    let text = node.text_with(&server, hot_extra, interfaces);
    settings_from_text(&text, QUICK)
        .unwrap()
        .into_combined()
        .unwrap()
}

/// The `[hot_store]` line that names the lock file `instance.lock` of `node`.
fn lock_line(node: &Node) -> (PathBuf, String) {
    let lock = node.dir.path().join("instance.lock");
    let line = format!("lock_file = {:?}", lock.display().to_string());
    (lock, line)
}

/// Asserts that `socket` is a Unix socket of mode 0600.
fn assert_control_socket(socket: &Path) {
    let meta = std::fs::symlink_metadata(socket).unwrap();
    assert!(meta.file_type().is_socket(), "{}", socket.display());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
}

/// Reads one answer to [`HEALTHZ`] from `connection`: its head and its body `ok`.
async fn answer_of(connection: &mut TcpStream) -> String {
    let mut answer = Vec::new();
    let mut piece = [0_u8; 1024];
    while !answer.ends_with(b"\r\n\r\nok") {
        let read = connection.read(&mut piece).await.unwrap();
        assert_ne!(read, 0, "{:?}", String::from_utf8_lossy(&answer));
        answer.extend_from_slice(&piece[..read]);
    }
    String::from_utf8_lossy(&answer).into_owned()
}

/// A connection to `at` whose request is half written after one answered request: the head of a
/// second `/healthz` is sent, and [`finish_request`] sends the rest.
async fn half_written(at: SocketAddr) -> TcpStream {
    let mut connection = TcpStream::connect(at).await.unwrap();
    connection.write_all(HEALTHZ).await.unwrap();
    let first = answer_of(&mut connection).await;
    assert!(first.starts_with("HTTP/1.1 200"), "{first}");
    connection.write_all(&HEALTHZ[..16]).await.unwrap();
    connection
}

/// Sends the rest of the request [`half_written`] began and gives the answer.
async fn finish_request(connection: &mut TcpStream) -> String {
    connection.write_all(&HEALTHZ[16..]).await.unwrap();
    answer_of(connection).await
}

/// The lease object in the hot directory `hot`: its lines (holder, nonce, generation), or none.
async fn lease_lines(hot: &Path) -> Vec<String> {
    use crate::Backend as _;
    let tier = Node::tier(hot.to_owned());
    match tier.get(LEASE, None).await {
        Ok(body) => String::from_utf8(body.to_vec())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// The nonce of the lease in `hot`, which tells one holder of the lease from another (renewals
/// keep it), once there is a lease whose nonce is not `not`. It looks until there is one: a lease
/// that never comes is stopped by nextest's hang guard, not by a bound of this test.
async fn nonce_other_than(hot: &Path, not: Option<&str>) -> String {
    loop {
        if let Some(nonce) = lease_lines(hot).await.get(1)
            && Some(nonce.as_str()) != not
        {
            return nonce.clone();
        }
        tokio::time::sleep(PAUSE).await;
    }
}

/// The two instances of a hand-over and the channels that drive them. See [`handover`].
struct Handover {
    /// The addresses the first instance bound.
    old_bound: oneshot::Receiver<Addresses>,
    /// The addresses the second instance bound.
    new_bound: oneshot::Receiver<Addresses>,
    /// Resolves when the first instance has returned.
    old_ended: oneshot::Receiver<()>,
    /// Starts the second instance.
    go: oneshot::Sender<()>,
    /// Stops the first instance (it ends on its own when the second takes its sockets).
    stop_old: oneshot::Sender<()>,
    /// Stops the second instance.
    stop_new: oneshot::Sender<()>,
}

/// Runs `serve_combined` with `settings` on the real disk, tells `told` the addresses and
/// `ended` when it returns, and stops when `stopped` resolves or is dropped.
async fn instance(
    settings: &CombinedSettings,
    told: oneshot::Sender<Addresses>,
    stopped: oneshot::Receiver<()>,
    ended: Option<oneshot::Sender<()>>,
) -> Result<Drained, Error> {
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let drained = serve_combined(
        settings,
        disk,
        async move {
            let _ = stopped.await;
        },
        move |at| drop(told.send(at)),
    )
    .await;
    if let Some(ended) = ended {
        let _ = ended.send(());
    }
    drained
}

/// Starts a first instance with `old`, and, when `drive` sends on `go`, a second with `new`, and
/// runs `drive` beside them. The result of each instance is returned.
async fn handover<F, Fut>(
    old: &CombinedSettings,
    new: &CombinedSettings,
    drive: F,
) -> (Result<Drained, Error>, Result<Drained, Error>)
where
    F: FnOnce(Handover) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (old_told, old_bound) = oneshot::channel();
    let (new_told, new_bound) = oneshot::channel();
    let (old_done, old_ended) = oneshot::channel();
    let (go, going) = oneshot::channel::<()>();
    let (stop_old, stopped_old) = oneshot::channel();
    let (stop_new, stopped_new) = oneshot::channel();
    let first = instance(old, old_told, stopped_old, Some(old_done));
    let second = async {
        going.await.unwrap();
        instance(new, new_told, stopped_new, None).await
    };
    let driving = drive(Handover {
        old_bound,
        new_bound,
        old_ended,
        go,
        stop_old,
        stop_new,
    });
    let (first, second, ()) = tokio::join!(first, second, driving);
    (first, second)
}

/// Asserts the switch of a node with a cache over one control socket: the second start takes the
/// first's sockets while a request of the first is half written, and says the very addresses to
/// `ready`; the half-written request is answered; a connection made after the switch is answered
/// by the second; the control socket (mode 0600) is the second's after the first has ended; the
/// first ends with nothing cut and the second, stopped, with nothing cut and its control socket
/// removed. Catches: a cache node that binds beside the running instance, drops a request in
/// flight, or leaves no control socket for the next update.
#[tokio::test]
async fn a_second_combined_start_takes_the_sockets_with_a_request_in_flight() {
    let node = Node::new("tscombined-handoff-switch");
    node.activate().await;
    let (_dir, socket) = control();
    let settings = handing(&node, &socket, "", "");
    let drive = |h: Handover| {
        let socket = socket.clone();
        async move {
            let at = h.old_bound.await.unwrap();
            assert_control_socket(&socket);
            let mut before = half_written(at.s3).await;
            h.go.send(()).unwrap();
            let taken = h.new_bound.await.unwrap();
            assert_eq!(taken, at, "the second serves the very sockets");

            let after = probe(at.s3, "/healthz").await;
            assert_eq!((after.0, after.1.as_str()), (200, "ok"));
            let late = finish_request(&mut before).await;
            assert!(late.starts_with("HTTP/1.1 200"), "{late}");
            // The first instance's drain holds a connection until the client closes its end.
            drop(before);
            let _ = h.old_ended.await;
            for _ in 0..3 {
                let alone = probe(at.s3, "/healthz").await;
                assert_eq!((alone.0, alone.1.as_str()), (200, "ok"));
            }
            assert_control_socket(&socket);
            let _ = h.stop_new.send(());
            drop(h.stop_old);
        }
    };
    let (old, new) = handover(&settings, &settings, drive).await;
    assert_eq!(old.unwrap(), Drained { abandoned: 0 });
    assert_eq!(new.unwrap(), Drained { abandoned: 0 });
    assert!(
        !socket.exists(),
        "a stop with no successor removes the socket"
    );
}

/// Asserts that the cache of the second instance is `ready read-only` while the first instance
/// runs (it holds the cache directory's lock through its drain, which a connection left open
/// prolongs), and `ready read-write` after the first has ended; and that a put through the second
/// reads back. Catches: a successor that serves read-write beside a holder of the directory, and
/// one that stays read-only for good.
#[tokio::test]
async fn the_second_cache_is_read_only_until_the_first_exits_and_then_read_write() {
    let node = Node::new("tscombined-handoff-modes");
    node.activate().await;
    let (_dir, socket) = control();
    let settings = handing(&node, &socket, "", "");
    let drive = |h: Handover| async move {
        let at = h.old_bound.await.unwrap();
        let (status, text) = probe(at.s3, "/readyz").await;
        assert_eq!((status, text.as_str()), (200, "ready read-write"));
        let before = half_written(at.s3).await;
        h.go.send(()).unwrap();
        let taken = h.new_bound.await.unwrap();
        assert_eq!(taken, at);
        let (status, text) = successor_readyz(at.s3).await;
        assert_eq!((status, text.as_str()), (200, "ready read-only"));
        // The first instance goes on holding the directory while its connection is open.
        drop(before);
        let _ = h.old_ended.await;
        let mut said = String::new();
        for _ in 0..TRIES {
            said = probe(at.s3, "/readyz").await.1;
            if said == "ready read-write" {
                break;
            }
            tokio::time::sleep(PAUSE).await;
        }
        assert_eq!(said, "ready read-write");
        let _ = h.stop_new.send(());
        drop(h.stop_old);
    };
    let (old, new) = handover(&settings, &settings, drive).await;
    assert_eq!(old.unwrap(), Drained { abandoned: 0 });
    assert_eq!(new.unwrap(), Drained { abandoned: 0 });
}

/// Asserts that the reconciler of the second instance waits for the first: while the first
/// instance runs (a connection left open keeps its drain going) the hot tier's lock file is held,
/// seen by a try of it from outside, and the lease in the hot tier keeps the nonce the first
/// instance's reconciler took, so the second wrote none (the second's own try of the lock came in
/// its start, before it bound, while the first held it); once the connection is closed and the
/// first has ended, the lease is taken anew (another nonce) and the lock is held again, by the
/// second; and the lease is gone when the second has stopped, however far its reconciler was in
/// a write of the lease when the stop came. Catches: a successor whose reconciler works beside the
/// first's, one that never takes the lock and the lease the first gave up, and one that leaves its
/// lease behind when it is stopped.
#[tokio::test]
async fn the_second_reconciler_waits_for_the_lock_file_and_the_lease_of_the_first() {
    let node = Node::new("tscombined-handoff-lease");
    node.activate().await;
    let (_dir, socket) = control();
    let (lock, line) = lock_line(&node);
    let settings = handing(&node, &socket, &line, "");
    let hot = node.hot();
    let (hot_in, lock_in) = (hot.clone(), lock.clone());
    let drive = |h: Handover| async move {
        let (hot, lock) = (hot_in, lock_in);
        let at = h.old_bound.await.unwrap();
        let first_nonce = nonce_other_than(&hot, None).await;
        let before = half_written(at.s3).await;
        h.go.send(()).unwrap();
        let taken = h.new_bound.await.unwrap();
        assert_eq!(taken, at);
        // The second took the lock file's try in its start, before it bound, and the first held
        // the lock then, so the second is outside the lock and the lease while the first runs. It
        // serves from the proxy: its answer is the event that it is up, not a wait.
        let (status, _) = probe(at.s3, "/healthz").await;
        assert_eq!(status, 200);
        assert!(
            try_take(&lock).unwrap().is_none(),
            "the first holds the lock"
        );
        let lines = lease_lines(&hot).await;
        assert_eq!(lines.get(1), Some(&first_nonce), "{lines:?}");
        drop(before);
        let _ = h.old_ended.await;
        let second_nonce = nonce_other_than(&hot, Some(&first_nonce)).await;
        assert_ne!(second_nonce, first_nonce);
        // The lock is the second's now: nobody else takes it.
        assert!(try_take(&lock).unwrap().is_none());
        let _ = h.stop_new.send(());
        drop(h.stop_old);
    };
    let (old, new) = handover(&settings, &settings, drive).await;
    assert_eq!(old.unwrap(), Drained { abandoned: 0 });
    assert_eq!(new.unwrap(), Drained { abandoned: 0 });
    assert!(
        lease_lines(&hot).await.is_empty(),
        "the second gave the lease up when it stopped"
    );
    assert!(
        try_take(&lock).unwrap().is_some(),
        "the lock is free after both"
    );
}

/// Asserts that the OCI front is handed over with the S3 front when the node configures one: the
/// second instance says the first's S3 and OCI addresses, a connection to the OCI address is
/// answered by the second after the first has ended, and both instances end with nothing cut.
/// Catches: an OCI listener that is bound anew (a port the first still holds) or dropped in the
/// hand-over of a cache node.
#[tokio::test]
async fn the_oci_front_of_a_combined_node_is_handed_over_with_the_s3_front() {
    let node = Node::new("tscombined-handoff-oci");
    node.activate().await;
    let (_dir, socket) = control();
    let oci = oci_interface("127.0.0.1:0", ", prefix = \"cas/\"");
    let settings = handing(&node, &socket, "", &oci);
    let drive = |h: Handover| async move {
        let at = h.old_bound.await.unwrap();
        let oci_at = at.oci.expect("an OCI front");
        let before = half_written(at.s3).await;
        h.go.send(()).unwrap();
        let taken = h.new_bound.await.unwrap();
        assert_eq!(taken, at, "the second serves the very sockets");
        assert_eq!(taken.oci, Some(oci_at));
        assert_eq!(taken.oci_all, at.oci_all);
        drop(before);
        let _ = h.old_ended.await;
        let (status, _) = probe(oci_at, "/v2/").await;
        assert_eq!(status, 200);
        let (status, text) = probe(at.s3, "/healthz").await;
        assert_eq!((status, text.as_str()), (200, "ok"));
        let _ = h.stop_new.send(());
        drop(h.stop_old);
    };
    let (old, new) = handover(&settings, &settings, drive).await;
    assert_eq!(old.unwrap(), Drained { abandoned: 0 });
    assert_eq!(new.unwrap(), Drained { abandoned: 0 });
}

/// Asserts that a start whose settings name other addresses than the running instance listens on
/// is refused with the exact `Mismatch` text and exit status 1, binds nothing (`ready` is never
/// called), and leaves the running instance serving with its control socket, which then stops
/// with nothing cut. Catches: a mismatched start that binds anyway or takes the sockets, and one
/// that stops the running instance.
#[tokio::test]
async fn a_start_with_other_addresses_is_refused_and_the_running_instance_goes_on() {
    let node = Node::new("tscombined-handoff-mismatch");
    node.activate().await;
    let (_dir, socket) = control();
    let old = handing(&node, &socket, "", "");
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let other = free.local_addr().unwrap();
    drop(free);
    let text = node.text_with(
        &format!("handoff_socket = {:?}", socket.display().to_string()),
        "",
        "",
    );
    let text = text.replace("127.0.0.1:0", &other.to_string());
    let new = settings_from_text(&text, QUICK)
        .unwrap()
        .into_combined()
        .unwrap();
    let running = std::cell::Cell::new(None);
    let seen = &running;
    let drive = |h: Handover| {
        let socket = socket.clone();
        async move {
            let at = h.old_bound.await.unwrap();
            seen.set(Some(at.s3));
            h.go.send(()).unwrap();
            // A start that fails never tells its addresses.
            assert!(h.new_bound.await.is_err());
            let (status, text) = probe(at.s3, "/healthz").await;
            assert_eq!((status, text.as_str()), (200, "ok"));
            let (status, text) = probe(at.s3, "/readyz").await;
            assert_eq!((status, text.as_str()), (200, "ready read-write"));
            assert_control_socket(&socket);
            let _ = h.stop_old.send(());
            let _ = h.old_ended.await;
            let _ = h.stop_new.send(());
        }
    };
    let (old, new) = handover(&old, &new, drive).await;
    assert_eq!(old.unwrap(), Drained { abandoned: 0 });
    let why = new.unwrap_err();
    assert_eq!(exit_status(&why), 1, "{why}");
    let at = running.get().unwrap();
    let want = format!(
        "hand-off: the running instance listens on [\"s3 {at}\"], these settings name \
         [\"s3 {other}\"]"
    );
    assert_eq!(why.to_string(), want);
    assert!(matches!(
        why.downcast_ref::<HandingError>(),
        Some(HandingError::Handoff(HandoffError::Mismatch { .. }))
    ));
}
