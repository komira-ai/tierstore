//! The client of the mount provider's socket against the node's real provider, the mount check
//! over it, and the read of a tree through `std::fs`.

use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tierstore_testkit::dir::TestDir;
use tierstore_trees_api::provider::v1::MountResponse;
use tokio::sync::Notify;
use tonic::Code;

use super::super::mount_check::check;
use super::super::ports::{
    MountProvider, ProviderCode, ProviderFault, ReadFault, TreeAt, TreeNode,
};
use super::super::report::{Class, Done, Outcome, Reason};
use super::super::shape::ProviderShape;
use super::provider::{MAX_FILE_BYTES, MAX_PATHS, ProviderSocket, ProviderTimes};
use super::provider::{code_of, framed, unframed};
use crate::server::local_cache::trees::{TreeStore, TreesConfig};
use crate::server::provider::check::{TREES, WITH_SIDECAR, WITHOUT_SIDECAR, pack, pack_key};
use crate::server::provider::testing::Rig;
use crate::server::provider::{NOT_PREPARED_HERE, serve};

/// The mount the test provider answers directories under.
const MOUNT: &str = "/run/tierstore/fs";

/// A provider over the tree store's bench, served on a socket in a scratch directory.
struct Served {
    rig: Rig,
    _dir: TestDir,
    socket: PathBuf,
    halt: Arc<Notify>,
}

impl Served {
    /// The provider, with the check's objects in its upstream as a build service stores them: the
    /// pack, both lists and the one sidecar, the list of the tree without a sidecar left out when
    /// `refuse_plain`, so that the provider refuses that tree.
    async fn start(name: &str, refuse_plain: bool) -> Self {
        Self::over(Rig::new(name).await, name, refuse_plain).await
    }

    /// [`Served::start`] over `rig`.
    async fn over(rig: Rig, name: &str, refuse_plain: bool) -> Self {
        let up = &rig.bench.rig;
        up.put(&pack_key(), &pack()).await;
        up.put(&WITH_SIDECAR.key(), WITH_SIDECAR.list).await;
        up.put(&WITH_SIDECAR.sidecar_key(), WITH_SIDECAR.sidecar.unwrap())
            .await;
        if !refuse_plain {
            up.put(&WITHOUT_SIDECAR.key(), WITHOUT_SIDECAR.list).await;
        }
        let dir = TestDir::under(std::env::temp_dir(), name);
        let socket = dir.path().join("provider.sock");
        let halt = Arc::new(Notify::new());
        let (provider, cache) = (rig.provider.clone(), rig.bench.rig.cache.clone());
        let (path, notify) = (socket.clone(), Arc::clone(&halt));
        tokio::spawn(async move { serve(provider, &cache, &path, notify).await });
        let bound = socket.clone();
        crate::server::local_cache::testing::until("the socket is bound", || bound.exists()).await;
        Self {
            rig,
            _dir: dir,
            socket,
            halt,
        }
    }

    fn shape(&self) -> ProviderShape {
        ProviderShape {
            socket: self.socket.clone(),
            mountpoint: PathBuf::from(MOUNT),
            nfs: None,
        }
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        self.halt.notify_one();
    }
}

/// The real client, with a read of a tree that finds, instead of the mount (which no test has),
/// the tree the store has placed at the directory: the tree of the check whose id the placement
/// names.
struct ThroughStore {
    socket: ProviderSocket,
    store: TreeStore,
}

impl MountProvider for ThroughStore {
    fn prepare(&self, socket: &Path, index: u32, reference: &str) -> Result<(), ProviderFault> {
        self.socket.prepare(socket, index, reference)
    }

    fn mount(&self, socket: &Path, index: u32) -> Result<PathBuf, ProviderFault> {
        self.socket.mount(socket, index)
    }

    fn release(&self, socket: &Path, index: u32) -> Result<(), ProviderFault> {
        self.socket.release(socket, index)
    }

    fn read_tree(&self, at: TreeAt<'_>) -> Result<BTreeMap<String, TreeNode>, ReadFault> {
        let TreeAt::Mount(dir) = at else {
            panic!("the check reads this node's trees through the mount: {at:?}");
        };
        through_store(&self.store, dir)
    }
}

/// The tree the store has placed at `dir`, below [`MOUNT`]: the tree of the check whose id the
/// placement names.
fn through_store(store: &TreeStore, dir: &Path) -> Result<BTreeMap<String, TreeNode>, ReadFault> {
    let not = |why: String| ReadFault::NotTheTree(why);
    let place = dir
        .strip_prefix(MOUNT)
        .map_err(|_| not("outside".to_owned()))?;
    let located = store
        .locate_fs(&place.to_string_lossy())
        .ok_or_else(|| not(format!("nothing is placed at {place:?}")))?;
    let tree = TREES
        .iter()
        .find(|tree| tree.key() == format!("cas/l/{}", located.tree))
        .ok_or_else(|| not("another tree is placed".to_owned()))?;
    Ok(tree.nodes())
}

/// The mount check of `served` with the real client.
async fn checked(served: &Served) -> Result<String, Done> {
    let through = ThroughStore {
        socket: ProviderSocket::new(PathBuf::from("/"), ProviderTimes::default()),
        store: served.rig.bench.store.clone(),
    };
    let shape = served.shape();
    tokio::task::spawn_blocking(move || check(&through, &shape, 5))
        .await
        .unwrap()
}

/// Asserts that the hook's client drives the node's real provider through the whole check over
/// its socket: both trees prepared from the objects as a build service stores them (one placed
/// from its L2 sidecar, one from its list alone), mounted at their sources' directories, found
/// placed there, released, and then not prepared here; and that no placement is left. Catches a
/// client whose requests the provider does not take, a provider that cannot place one of the two
/// forms, and a check that leaves a placement.
#[tokio::test(flavor = "multi_thread")]
async fn the_check_passes_against_the_real_provider_and_leaves_no_placement() {
    let served = Served::start("hook-prov-pass", false).await;
    let said = checked(&served).await.expect("the check passes");
    assert!(said.starts_with("the mount provider at "), "{said}");
    assert_eq!(served.rig.bench.store.sizes().0, 0);
}

/// Asserts that against a real provider that refuses the tree without an L2 sidecar (its list is
/// not in the store, so the provider answers its `Mount` `FAILED_PRECONDITION`), the check fails
/// with `provider_refused` and the provider's own reason, and leaves no placement of either tree.
/// Catches a verify that passes a node whose provider refuses that form of tree.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_provider_that_refuses_the_tree_without_a_sidecar_fails_the_check() {
    let served = Served::start("hook-prov-refuse", true).await;
    let done = checked(&served).await.expect_err("the check fails");
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::ProviderRefused));
    let detail = done.report.detail.unwrap_or_default();
    let reason = format!("no tree list at {}", WITHOUT_SIDECAR.key());
    assert!(
        detail.ends_with(&format!("with FailedPrecondition: {reason}")),
        "{detail}"
    );
    assert_eq!(served.rig.bench.store.sizes().0, 0);
}

/// Asserts that the client names each answer of the real provider by its status: a ref that is
/// no tree list's key `INVALID_ARGUMENT`, a source prepared again with another ref
/// `FAILED_PRECONDITION`, a `Mount` of a source never prepared `UNAVAILABLE` with exactly
/// [`NOT_PREPARED_HERE`] (what the check takes for a placement gone), and a `Release` of it a
/// success.
#[tokio::test(flavor = "multi_thread")]
async fn the_client_names_each_answer_by_its_status() {
    let served = Served::start("hook-prov-codes", false).await;
    let socket = served.socket.clone();
    let answers = tokio::task::spawn_blocking(move || {
        let client = ProviderSocket::new(PathBuf::from("/"), ProviderTimes::default());
        let bad = client.prepare(&socket, 0, "cas/l/short");
        client
            .prepare(&socket, 0, &WITH_SIDECAR.key())
            .expect("prepared");
        let again = client.prepare(&socket, 0, &WITHOUT_SIDECAR.key());
        let mounted = client.mount(&socket, 0);
        client.release(&socket, 0).expect("released");
        let unknown = client.mount(&socket, 7);
        let released = client.release(&socket, 7);
        (bad, again, mounted, unknown, released)
    })
    .await
    .unwrap();
    let (bad, again, mounted, unknown, released) = answers;
    assert!(matches!(
        bad,
        Err(ProviderFault::Answered {
            code: ProviderCode::InvalidArgument,
            ..
        })
    ));
    assert_eq!(
        again,
        Err(ProviderFault::Answered {
            code: ProviderCode::FailedPrecondition,
            reason: format!(
                "the source was prepared with another ref, {}",
                WITH_SIDECAR.key()
            ),
        })
    );
    let dir = mounted.expect("mounted");
    assert!(dir.starts_with(MOUNT), "{dir:?}");
    assert!(dir.ends_with("0"), "{dir:?}");
    assert_eq!(
        unknown,
        Err(ProviderFault::Answered {
            code: ProviderCode::Unavailable,
            reason: NOT_PREPARED_HERE.to_owned(),
        })
    );
    assert_eq!(released, Ok(()));
}

/// Asserts that a socket nobody listens on, and one whose listener closes the connection without
/// an answer, are silence (no answer of the protocol), each naming the call. Catches a provider
/// that is not there taken for one that refused.
#[test]
fn a_socket_with_no_answer_is_silence() {
    let dir = TestDir::under(std::env::temp_dir(), "hook-prov-silent");
    let client = ProviderSocket::new(PathBuf::from("/"), ProviderTimes::default());
    let missing = dir.path().join("missing.sock");
    match client.release(&missing, 0) {
        Err(ProviderFault::Silent(why)) => {
            assert!(why.starts_with("Release: "), "{why}");
            assert!(why.contains("missing.sock"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    let closing = dir.path().join("closing.sock");
    let listener = UnixListener::bind(&closing).unwrap();
    let accepting = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        drop(stream);
    });
    match client.mount(&closing, 0) {
        Err(ProviderFault::Silent(why)) => assert!(why.starts_with("Mount: "), "{why}"),
        other => panic!("{other:?}"),
    }
    accepting.join().unwrap();
}

/// Asserts the port's name of every gRPC status: the three the provider answers by their names,
/// any other by its number.
#[test]
fn every_status_has_its_name() {
    assert_eq!(
        code_of(Code::InvalidArgument),
        ProviderCode::InvalidArgument
    );
    assert_eq!(
        code_of(Code::FailedPrecondition),
        ProviderCode::FailedPrecondition
    );
    assert_eq!(code_of(Code::Unavailable), ProviderCode::Unavailable);
    assert_eq!(code_of(Code::Internal), ProviderCode::Other(13));
    assert_eq!(code_of(Code::Unimplemented), ProviderCode::Other(12));
}

/// Asserts that an answer's frame is read back to its message, and that data shorter than a
/// frame's head, a compressed frame, a frame of another length than it says, and bytes that are
/// not the message are refused by name.
#[test]
fn an_answer_is_one_frame_of_its_message() {
    let message = MountResponse {
        path: "/run/tierstore/fs/x".to_owned(),
    };
    let frame = framed(&message).unwrap();
    assert_eq!(frame[..5], [0, 0, 0, 0, 21]);
    assert_eq!(unframed::<MountResponse>(&frame), Ok(message));
    assert_eq!(
        unframed::<MountResponse>(&[0, 0, 0]),
        Err("an answer of 3 bytes, not a frame".to_owned())
    );
    let mut flagged = frame.to_vec();
    flagged[0] = 1;
    assert_eq!(
        unframed::<MountResponse>(&flagged),
        Err("an answer framed as 21 bytes (flag 1) with 21 bytes".to_owned())
    );
    assert_eq!(
        unframed::<MountResponse>(&frame[..10]),
        Err("an answer framed as 21 bytes (flag 0) with 5 bytes".to_owned())
    );
    let junk = [0, 0, 0, 0, 2, 0xff, 0xff];
    let refused = unframed::<MountResponse>(&junk).unwrap_err();
    assert!(
        refused.starts_with("an answer that is not the message: "),
        "{refused}"
    );
}

/// The read of the tree at `dir` with the real client.
fn read(dir: &Path) -> Result<BTreeMap<String, TreeNode>, ReadFault> {
    ProviderSocket::new(PathBuf::from("/"), ProviderTimes::default()).read_tree(TreeAt::Mount(dir))
}

/// Asserts that the read of a tree gives every path below the directory by its path, with the
/// bytes of every file (an empty one included), every directory (an empty one included), and the
/// target of a link, which is not followed. Catches a read that follows a link out of the tree,
/// or leaves out an empty file or directory that the tree has.
#[test]
fn a_tree_is_read_whole_without_following_a_link() {
    let dir = TestDir::under(std::env::temp_dir(), "hook-prov-read");
    let root = dir.path().join("tree");
    std::fs::create_dir_all(root.join("data/dir")).unwrap();
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::write(root.join("bin/tool"), b"#!/bin/sh\n").unwrap();
    std::fs::write(root.join("data/empty"), b"").unwrap();
    std::os::unix::fs::symlink("/etc", root.join("tool")).unwrap();
    let read = read(&root).expect("a tree");
    let want = BTreeMap::from([
        ("bin".to_owned(), TreeNode::Dir),
        (
            "bin/tool".to_owned(),
            TreeNode::File(b"#!/bin/sh\n".to_vec()),
        ),
        ("data".to_owned(), TreeNode::Dir),
        ("data/dir".to_owned(), TreeNode::Dir),
        ("data/empty".to_owned(), TreeNode::File(Vec::new())),
        ("tool".to_owned(), TreeNode::Link("/etc".to_owned())),
    ]);
    assert_eq!(read, want);
}

/// Asserts that the read refuses, by name: a directory that is not there (what the system said,
/// as a failure: no slow fill), a file over
/// [`MAX_FILE_BYTES`], more than [`MAX_PATHS`] paths, a name or a link target that is not UTF-8,
/// and a path that is no file, directory or link (a socket). Catches a read that loads a large
/// file whole, walks a huge directory, or takes a socket for a file.
#[test]
fn a_tree_that_is_no_tree_of_the_check_is_refused_by_name() {
    let dir = TestDir::under(std::env::temp_dir(), "hook-prov-refuse-read");
    let missing = dir.path().join("missing");
    assert_eq!(
        read(&missing),
        Err(ReadFault::Failed(
            "the directory could not be listed: No such file or directory (os error 2)".to_owned()
        ))
    );

    let large = dir.path().join("large");
    std::fs::create_dir_all(&large).unwrap();
    std::fs::write(large.join("big"), vec![0u8; MAX_FILE_BYTES as usize + 1]).unwrap();
    assert_eq!(
        read(&large),
        Err(ReadFault::NotTheTree(format!(
            "big is larger than {MAX_FILE_BYTES} bytes"
        )))
    );

    let many = dir.path().join("many");
    std::fs::create_dir_all(&many).unwrap();
    for n in 0..=MAX_PATHS {
        std::fs::write(many.join(format!("f{n}")), b"").unwrap();
    }
    assert_eq!(
        read(&many),
        Err(ReadFault::NotTheTree(format!(
            "more than {MAX_PATHS} paths"
        )))
    );

    let odd = dir.path().join("odd");
    std::fs::create_dir_all(&odd).unwrap();
    let name = std::ffi::OsStr::from_bytes(b"bad\xff");
    std::fs::write(odd.join(name), b"").unwrap();
    assert_eq!(
        read(&odd),
        Err(ReadFault::NotTheTree(
            "\"\" holds a name that is not UTF-8".to_owned()
        ))
    );

    let link = dir.path().join("link");
    std::fs::create_dir_all(&link).unwrap();
    std::os::unix::fs::symlink(name, link.join("l")).unwrap();
    assert_eq!(
        read(&link),
        Err(ReadFault::NotTheTree(
            "l links to a target that is not UTF-8".to_owned()
        ))
    );

    let special = dir.path().join("special");
    std::fs::create_dir_all(&special).unwrap();
    let _listener = UnixListener::bind(special.join("s")).unwrap();
    assert_eq!(
        read(&special),
        Err(ReadFault::NotTheTree(
            "s is neither a file, a directory nor a link".to_owned()
        ))
    );
}

/// The bytes of a frame, for the length in [`an_answer_is_one_frame_of_its_message`].
#[test]
fn a_request_frame_is_its_message_after_five_bytes() {
    let frame: Bytes = framed(&MountResponse::default()).unwrap();
    assert_eq!(frame.as_ref(), [0, 0, 0, 0, 0]);
}

/// A listener on a socket in `dir` that answers every connection with `answer` (none: it keeps
/// the connection open and never answers), as long as the test runs.
fn answering(dir: &TestDir, name: &str, answer: Option<&'static [u8]>) -> PathBuf {
    let path = dir.path().join(name);
    let listener = UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            if let Some(answer) = answer {
                use std::io::Write as _;
                let _ = stream.write_all(answer);
            }
            held.push(stream);
        }
    });
    path
}

/// Asserts that a provider that takes the connection and never answers is silence once the
/// call's bound has passed, naming the call and its bound, and that one that answers with
/// something that is not HTTP/2 is silence naming the connection. The bound ends a call the other
/// side never ends, so the outcome is the same however long the bound is. Catches a call that
/// waits for ever on a provider that hangs, holding `verify` until hostkeeper kills it.
#[test]
fn a_provider_that_never_answers_or_answers_no_protocol_is_silence() {
    let dir = TestDir::under(std::env::temp_dir(), "hook-prov-hang");
    let times = ProviderTimes {
        mount: std::time::Duration::from_millis(200),
        ..ProviderTimes::default()
    };
    let client = ProviderSocket::new(PathBuf::from("/"), times);
    let hanging = answering(&dir, "hang.sock", None);
    assert_eq!(
        client.mount(&hanging, 0),
        Err(ProviderFault::Silent(
            "Mount: no answer within 200 ms".to_owned()
        ))
    );
    let garbage = answering(&dir, "junk.sock", Some(b"HTTP/1.1 200 OK\r\n\r\n"));
    match client.mount(&garbage, 0) {
        Err(ProviderFault::Silent(why)) => {
            assert!(why.starts_with("Mount: no "), "{why}");
        }
        other => panic!("{other:?}"),
    }
}

/// Asserts that an HTTP/2 answer with no `grpc-status` is silence that says so, with the HTTP
/// status. Catches a server that is not the provider (or a broken one) taken for one that
/// answered success.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_with_no_grpc_status_is_silence() {
    use http_body_util::Full;
    use hyper::server::conn::http2 as server;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    let dir = TestDir::under(std::env::temp_dir(), "hook-prov-nostatus");
    let path = dir.path().join("plain.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let service = hyper::service::service_fn(|_request| async {
                Ok::<_, std::convert::Infallible>(hyper::Response::new(Full::new(Bytes::new())))
            });
            tokio::spawn(
                server::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service),
            );
        }
    });
    let answered = tokio::task::spawn_blocking(move || {
        ProviderSocket::new(PathBuf::from("/"), ProviderTimes::default()).release(&path, 0)
    })
    .await
    .unwrap();
    assert_eq!(
        answered,
        Err(ProviderFault::Silent(
            "Release: an answer with no grpc-status (HTTP 200 OK)".to_owned()
        ))
    );
}

/// The reads every preparation may queue together, as the provider's default.
const QUEUE: u64 = 256 << 20;

/// The check's failure or deferral against `served`: its outcome, reason, class and detail.
async fn deferred(served: &Served) -> (Outcome, Option<Reason>, Class, String) {
    let done = checked(served).await.expect_err("the check does not pass");
    let detail = done.report.detail.clone().unwrap_or_default();
    (done.outcome, done.report.reason, done.report.class, detail)
}

/// Asserts that on a node whose placements are at `trees.max_placements` (a job's tree holds the
/// one place), the check is not failed: the provider takes the check's `Prepare`, cannot make the
/// tree, and answers its `Mount` `UNAVAILABLE` with `the tree was not made: too many placements`,
/// so the check defers with `provider_busy`, of the environment's class, and leaves the job's
/// placement alone and none of its own. Catches an update failed, and rolled back on every node
/// at once, because the node was full when `verify` ran.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_at_its_placement_limit_defers_the_check() {
    let config = TreesConfig {
        max_placements: 1,
        ..TreesConfig::default()
    };
    let rig = Rig::bounded(
        "hook-load-placements",
        config,
        Duration::from_secs(3600),
        QUEUE,
    )
    .await;
    let _ = rig.mounted("js.job", 0).await;
    let served = Served::over(rig, "hook-load-placements", false).await;
    let (outcome, reason, class, detail) = deferred(&served).await;
    assert_eq!(
        (outcome, reason, class),
        (
            Outcome::Deferred,
            Some(Reason::ProviderBusy),
            Class::Environment
        )
    );
    assert!(
        detail
            .contains("with UNAVAILABLE: the tree was not made: too many placements: 1 are alive"),
        "{detail}"
    );
    assert_eq!(served.rig.bench.store.sizes().0, 1);
}

/// Asserts that on a node whose provider holds `trees.max_sources` sources (one, a job's), the
/// check's `Prepare` is answered `UNAVAILABLE` with `the node holds 1 sources
/// (trees.max_sources): release one, or retry`, and the check defers with `provider_busy` and
/// leaves the job's source and placement alone. Catches an update failed for a burst of jobs.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_at_its_sources_bound_defers_the_check() {
    let config = TreesConfig {
        max_sources: 1,
        ..TreesConfig::default()
    };
    let rig = Rig::bounded(
        "hook-load-sources",
        config,
        Duration::from_secs(3600),
        QUEUE,
    )
    .await;
    let _ = rig.mounted("js.job", 0).await;
    let served = Served::over(rig, "hook-load-sources", false).await;
    let (outcome, reason, class, detail) = deferred(&served).await;
    assert_eq!(
        (outcome, reason, class),
        (
            Outcome::Deferred,
            Some(Reason::ProviderBusy),
            Class::Environment
        )
    );
    assert!(
        detail.ends_with(
            "answered Prepare of the tree with its L2 sidecar with UNAVAILABLE: the node holds 1 \
             sources (trees.max_sources): release one, or retry"
        ),
        "{detail}"
    );
    assert_eq!(served.rig.bench.store.sizes().0, 1);
}

/// Asserts that on a node whose preparations are all taken (`trees.prepare_concurrency` of one,
/// held by a job's preparation whose list read the store does not answer), the check's `Prepare`
/// is taken and waits for a turn, its `Mount` is answered `UNAVAILABLE` `not ready` once the
/// provider's wait for the tree has passed, and the check defers with `provider_busy`, leaving no
/// placement. The job's read never ends while the check runs, so the answer does not depend on how
/// long the wait is. Catches an update failed while the node's preparations are busy.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_whose_preparations_are_all_taken_defers_the_check() {
    use crate::server::provider::testing::sidecar_key_of;

    let config = TreesConfig {
        prepare_concurrency: 1,
        ..TreesConfig::default()
    };
    let rig = Rig::bounded("hook-load-turns", config, Duration::from_millis(200), QUEUE).await;
    let gate = rig
        .bench
        .rig
        .tamper
        .hold_key_after_read(&sidecar_key_of(&rig.key));
    rig.prepared("js.job", 0);
    let tamper = Arc::clone(&rig.bench.rig.tamper);
    crate::server::local_cache::testing::until("the job's read is held", || tamper.parked() > 0)
        .await;
    let served = Served::over(rig, "hook-load-turns", false).await;
    let (outcome, reason, class, detail) = deferred(&served).await;
    assert_eq!(
        (outcome, reason, class),
        (
            Outcome::Deferred,
            Some(Reason::ProviderBusy),
            Class::Environment
        )
    );
    assert!(
        detail.ends_with(
            "answered Mount of the tree with its L2 sidecar with UNAVAILABLE: not ready"
        ),
        "{detail}"
    );
    assert_eq!(served.rig.bench.store.sizes().0, 0);
    gate.add_permits(1);
}

/// Asserts that on a node whose read-ahead queue has no room (its bound is 0 bytes here, as a
/// queue already full of other sources' reads leaves none), the check's trees are placed without
/// their reads queued, as with `trees.prepare_prefetch = "none"`, and the check passes: a full
/// queue is no refusal at all, and the files are read as the mount opens them. Catches a check
/// that fails, or waits, on a node whose read-ahead is busy.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_whose_read_ahead_queue_is_full_passes_the_check() {
    let rig = Rig::bounded(
        "hook-load-queue",
        TreesConfig::default(),
        Duration::from_secs(3600),
        0,
    )
    .await;
    let served = Served::over(rig, "hook-load-queue", false).await;
    let said = checked(&served).await.expect("the check passes");
    assert!(said.starts_with("the mount provider at "), "{said}");
    assert_eq!(served.rig.bench.store.sizes().0, 0);
}
