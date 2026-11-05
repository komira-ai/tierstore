//! The cache directory under two instances across a hand-off: the successor reads it and writes
//! nothing while the old instance holds its lock, and takes the lock once, when the old one has
//! exited.
//!
//! Both instances accept from one queue of the same sockets, so a test that drives one of them
//! goes through [`pin`]: a relay on a port of its own that connects to the sockets until the
//! instance that answers is the one whose `server.slot` it was given (the slot label of
//! `/metricz`), and then passes the connection's bytes on.

use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use objstore_s3::{ByteRange, ObjectKey, ObjectStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use super::Drained;
use super::build_info::{COMMIT, VERSION, gauge};
use super::cache_cli::serve_cache;
use super::cache_cli_tests::{Node, QUICK, client, probe};
use super::cache_handoff_tests::successor_readyz;
use super::cli::{Addresses, Error};
use super::combined_cli::serve_combined;
use super::combined_cli_tests::Node as CombinedNode;
use super::config::settings_from_text;
use super::config_cache::CacheSettings;
use super::config_combined::CombinedSettings;
use super::local_cache::testing::{Rig, age, digest, pattern, tree, wait_until};
use super::local_cache::{Disk, SystemDisk};
use super::lock::try_take;
use crate::{Backend, Op};

/// The size of every object of these tests.
const SIZE: u64 = 1000;

/// How many tries a wait for a state makes.
const TRIES: u32 = 1_500;

/// What a client sends to ask `/metricz`, keeping the connection alive.
const METRICZ: &[u8] = b"GET /metricz HTTP/1.1\r\nhost: tier.test\r\n\r\n";

/// The text of the answer to [`METRICZ`] on `connection`, or none if the connection broke.
async fn scrape(connection: &mut TcpStream) -> Option<String> {
    connection.write_all(METRICZ).await.ok()?;
    let mut got = Vec::new();
    let mut piece = [0_u8; 4096];
    let head_end = loop {
        if let Some(at) = got.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        let read = connection.read(&mut piece).await.ok()?;
        if read == 0 {
            return None;
        }
        got.extend_from_slice(&piece[..read]);
    };
    let head = String::from_utf8_lossy(&got[..head_end]).to_ascii_lowercase();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))?
        .trim()
        .parse()
        .ok()?;
    while got.len() < head_end + length {
        let read = connection.read(&mut piece).await.ok()?;
        if read == 0 {
            return None;
        }
        got.extend_from_slice(&piece[..read]);
    }
    Some(String::from_utf8_lossy(&got[head_end..]).into_owned())
}

/// A connection to the instance with `slot` behind the sockets `shared`, found by trying
/// connections until one is answered by it, in a bounded number of tries.
async fn connect_to_slot(shared: SocketAddr, slot: u16) -> Option<TcpStream> {
    let want = gauge(VERSION, COMMIT, Some(slot));
    for _ in 0..TRIES {
        if let Ok(mut connection) = TcpStream::connect(shared).await
            && scrape(&mut connection)
                .await
                .is_some_and(|text| text.contains(&want))
        {
            return Some(connection);
        }
        tokio::task::yield_now().await;
    }
    None
}

/// A relay to one instance: every connection to `at` is passed on to a connection of the
/// instance with the slot it was made for. Dropping it closes every connection it passes on.
struct Pin {
    at: SocketAddr,
    task: JoinHandle<()>,
}

impl Drop for Pin {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Where a relay holds back a request: it passes on the first bytes of the first request of a
/// client, tells `arrived`, and passes on the rest when `release` has a permit.
#[derive(Clone)]
struct Hold {
    arrived: Arc<Semaphore>,
    release: Arc<Semaphore>,
}

impl Hold {
    fn new() -> Self {
        Self {
            arrived: Arc::new(Semaphore::new(0)),
            release: Arc::new(Semaphore::new(0)),
        }
    }
}

/// A relay to the instance with `slot` behind the sockets `shared`.
async fn pin(shared: SocketAddr, slot: u16) -> Pin {
    pin_holding(shared, slot, None).await
}

/// A relay as [`pin`] makes, that holds back the first request of each client as `hold` says: the
/// instance has a request half written, which keeps its drain going, until the test lets it go.
async fn pin_holding(shared: SocketAddr, slot: u16, hold: Option<Hold>) -> Pin {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut relays = JoinSet::new();
        while let Ok((mut client, _)) = listener.accept().await {
            let hold = hold.clone();
            relays.spawn(async move {
                let Some(mut instance) = connect_to_slot(shared, slot).await else {
                    return;
                };
                if let Some(hold) = hold {
                    let mut head = [0_u8; 4096];
                    let Ok(read) = client.read(&mut head).await else {
                        return;
                    };
                    let first = read.min(16);
                    let _ = instance.write_all(&head[..first]).await;
                    hold.arrived.add_permits(1);
                    let Ok(permit) = hold.release.acquire().await else {
                        return;
                    };
                    permit.forget();
                    let _ = instance.write_all(&head[first..read]).await;
                }
                let _ = tokio::io::copy_bidirectional(&mut client, &mut instance).await;
            });
        }
    });
    Pin { at, task }
}

/// The value of the sample `name` (no labels) in the text of `/metricz`.
fn metric(text: &str, name: &str) -> u64 {
    let start = format!("{name} ");
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix(&start))
        .unwrap_or_else(|| panic!("{name} in {text}"));
    line.trim().parse().unwrap()
}

/// The hits, misses and fills the instance behind `at` counts, from its `/metricz`.
async fn counts(at: SocketAddr) -> (u64, u64, u64) {
    let (status, text) = probe(at, "/metricz").await;
    assert_eq!(status, 200);
    (
        metric(&text, "tierstore_cache_hits_total"),
        metric(&text, "tierstore_cache_misses_total"),
        metric(&text, "tierstore_cache_fills_total"),
    )
}

/// Reads all of `key` through the node at `at`.
async fn read(at: SocketAddr, key: &str) -> Bytes {
    let key = ObjectKey::new(key).unwrap();
    let all = ByteRange::new(0, SIZE).unwrap();
    client(at).get_range(&key, all).await.unwrap()
}

/// The data file of `key` under the cache directory `root`.
fn data_file(root: &Path, key: &str) -> PathBuf {
    let name = digest(key.as_bytes());
    root.join("obj")
        .join(&name[..2])
        .join(&name[2..4])
        .join(name)
}

/// The names of the files in the stage directory of `root`.
fn staged(root: &Path) -> Vec<String> {
    std::fs::read_dir(root.join(".stage"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

/// Asserts that every sidecar under `root/obj` has its data file and every data file its
/// sidecar, and gives how many objects there are.
fn assert_no_stray_sidecars(root: &Path) -> usize {
    let mut pending = vec![root.join("obj")];
    let (mut data, mut sidecars) = (Vec::new(), Vec::new());
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "meta") {
                sidecars.push(path.with_extension(""));
            } else {
                data.push(path);
            }
        }
    }
    data.sort();
    sidecars.sort();
    assert_eq!(data, sidecars, "a data file for each sidecar and back");
    data.len()
}

/// Waits, in a bounded number of tries, until `/readyz` at `addr` says `ready read-write`.
async fn becomes_read_write(addr: SocketAddr) {
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let mut last = String::new();
    for _ in 0..TRIES {
        tick.tick().await;
        let (status, text) = probe(addr, "/readyz").await;
        if status == 200 && text == "ready read-write" {
            return;
        }
        last = text;
    }
    panic!("/readyz never said read-write; last: {last:?}");
}

/// A scratch directory for the control socket, and the socket's path in it.
fn control() -> (tierstore_testkit::dir::TestDir, PathBuf) {
    let dir = tierstore_testkit::dir::TestDir::under(std::env::temp_dir(), "tslock-handoff");
    let socket = dir.path().join("control.sock");
    (dir, socket)
}

/// The ends of two nodes that run in one task: the first, which is serving when `work` is
/// called, and the second, which `go` starts.
struct Pair {
    old: Addresses,
    go: oneshot::Sender<()>,
    new_bound: oneshot::Receiver<Addresses>,
    old_ended: oneshot::Receiver<()>,
    stop_new: oneshot::Sender<()>,
}

/// Runs two nodes, the second once `work` says go, and gives how each ended. `start` runs a node
/// of the settings it is given with the address it tells and the stop it waits for.
async fn two_nodes<S, F, Fut, Run, RunFut>(
    first: S,
    second: S,
    run: Run,
    work: F,
) -> (Result<Drained, Error>, Result<Drained, Error>)
where
    F: FnOnce(Pair) -> Fut,
    Fut: Future<Output = ()>,
    Run: Fn(S, Box<dyn FnOnce(Addresses) + Send>, oneshot::Receiver<()>) -> RunFut,
    RunFut: Future<Output = Result<Drained, Error>>,
{
    let (old_told, old_bound) = oneshot::channel::<Addresses>();
    let (new_told, new_bound) = oneshot::channel::<Addresses>();
    let (old_done, old_ended) = oneshot::channel::<()>();
    let (go, going) = oneshot::channel::<()>();
    let (stop_old, stopped_old) = oneshot::channel::<()>();
    let (stop_new, stopped_new) = oneshot::channel::<()>();
    let old = async {
        let told: Box<dyn FnOnce(Addresses) + Send> = Box::new(move |at| drop(old_told.send(at)));
        let drained = run(first, told, stopped_old).await;
        let _ = old_done.send(());
        drained
    };
    let new = async {
        going.await.unwrap();
        let told: Box<dyn FnOnce(Addresses) + Send> = Box::new(move |at| drop(new_told.send(at)));
        run(second, told, stopped_new).await
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

/// What a build node over `upstream` and the real disk is run with.
async fn run_cache_node(
    settings: &CacheSettings,
    upstream: &Arc<dyn Backend>,
    told: Box<dyn FnOnce(Addresses) + Send>,
    stopped: oneshot::Receiver<()>,
) -> Result<Drained, Error> {
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let stopped = async move {
        let _ = stopped.await;
    };
    serve_cache(settings, Arc::clone(upstream), disk, stopped, told).await
}

/// The settings of a build node over `node` with `slot`, that hands off over `socket`.
fn cache_settings(node: &Node, socket: &Path, slot: u16) -> CacheSettings {
    let server = format!(
        "handoff_socket = {:?}\nhandoff_wait = \"1h\"\nslot = {slot}",
        socket.display().to_string()
    );
    settings_from_text(&node.text(&server, ""), QUICK)
        .unwrap()
        .into_cache()
        .unwrap()
}

/// Fills the cache of the directory of `node` with `keys`, by reading them through a lone build
/// node, and waits until that node has let the lock go.
async fn warm_build_node(node: &Node, upstream: &Arc<dyn Backend>, keys: &[&str]) {
    let settings = node.settings("", "");
    let (told, bound) = oneshot::channel::<Addresses>();
    let (stop, stopped) = oneshot::channel::<()>();
    let told: Box<dyn FnOnce(Addresses) + Send> = Box::new(move |at| drop(told.send(at)));
    let running = run_cache_node(&settings, upstream, told, stopped);
    let driving = async {
        let at = bound.await.unwrap().s3;
        for key in keys {
            let _ = read(at, key).await;
        }
        drop(stop);
    };
    let (drained, ()) = tokio::join!(running, driving);
    assert_eq!(drained.unwrap(), Drained { abandoned: 0 });
    free_lock(&node.cache_dir().join(".lock")).await;
}

/// Waits, in a bounded number of turns, until nothing holds the lock `lock`.
async fn free_lock(lock: &Path) {
    let mut freed = false;
    for _ in 0..100_000 {
        freed = try_take(lock).unwrap().is_some();
        if freed {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(freed, "the lock was still held after the node exited");
}

/// Asserts (T12), for a build node over a warm cache directory, what the successor does to the
/// directory while the old instance holds its lock and has a fill in flight, and when it takes
/// the lock. The old instance (slot 9411) has a fill of `cas/late` held at the upstream; the
/// successor (slot 9412) takes the sockets and says `ready read-only`. Driven through the
/// successor alone: a hit of an object the scan found, a miss, and a refused PUT, with the
/// listing of the whole directory (names, sizes, modes, inodes and times, after the tree was
/// stamped with an old time) the same after them, the old one's staged fill still there, and the upstream asked once for the miss and not
/// for the hit and not at all to write. After the old one's fill is released and published, the
/// listing is again the same across more reads through the successor, and the object the old one
/// published after the scan is a miss for the successor (upstream asked, no hit counted). When
/// the old instance has ended, `/readyz` says `ready read-write`, `.stage` is empty, the object
/// the old one published is a hit and the upstream is not asked, the counters are the successor's
/// own, and every sidecar has its data file and back. Catches: a successor that clears `.stage`,
/// scans with deletes or stamps hits while the old one fills, one that never takes the lock, and
/// one that keeps an index that cannot see what the old one published.
#[tokio::test]
async fn a_build_node_successor_over_a_warm_directory_writes_nothing_until_the_old_one_exits() {
    let node = Node::new("tslock-build");
    let (_dir, socket) = control();
    let rig = Rig::new("tslock-build-upstream");
    let body = pattern(SIZE as usize);
    let keys = ["cas/warm0", "cas/warm1", "cas/miss", "cas/late"];
    for key in keys {
        rig.put(key, &body).await;
    }
    let upstream = Arc::clone(&rig.upstream);
    let for_nodes: Arc<dyn Backend> = upstream.clone();
    warm_build_node(&node, &for_nodes, &["cas/warm0", "cas/warm1"]).await;
    let root = node.cache_dir();
    assert_eq!(assert_no_stray_sidecars(&root), 2);

    let first = cache_settings(&node, &socket, 9411);
    let second = cache_settings(&node, &socket, 9412);
    let tamper = Arc::clone(&rig.tamper);
    let work = |mut pair: Pair| {
        let (upstream, tamper, root, body) = (
            Arc::clone(&upstream),
            Arc::clone(&tamper),
            root.clone(),
            body.clone(),
        );
        async move {
            let shared = pair.old.s3;
            let (status, text) = probe(shared, "/readyz").await;
            assert_eq!((status, text.as_str()), (200, "ready read-write"));

            // The old instance starts a fill and the upstream holds it after reading the object.
            let gate = tamper.hold_key_after_read("cas/late");
            let to_old = pin(shared, 9411).await;
            let filling = {
                let at = to_old.at;
                tokio::spawn(async move { read(at, "cas/late").await })
            };
            wait_until("the fill of the old instance reached the upstream", || {
                tamper.parked() == 1
            })
            .await;
            assert_eq!(upstream.calls_on(Op::Get, "cas/late"), 1);

            pair.go.send(()).unwrap();
            let taken = (&mut pair.new_bound).await.unwrap();
            assert_eq!(taken.s3, shared, "the very sockets");
            let (status, text) = successor_readyz(shared).await;
            assert_eq!((status, text.as_str()), (200, "ready read-only"));
            let to_new = pin(shared, 9412).await;
            let new = to_new.at;
            assert_eq!(counts(new).await, (0, 0, 0));

            age(&root);
            let before = tree(&root);
            let held = staged(&root);
            assert_eq!(held.len(), 1, "the fill of the old instance: {held:?}");
            let gets = upstream.calls(Op::Get);

            assert_eq!(read(new, "cas/warm0").await, body);
            assert_eq!(upstream.calls(Op::Get), gets, "a hit is not read upstream");
            assert_eq!(read(new, "cas/miss").await, body);
            assert_eq!(upstream.calls_on(Op::Get, "cas/miss"), 1);
            let store = client(new);
            let key = ObjectKey::new("cas/put").unwrap();
            let put = store.put_new(&key, Bytes::from_static(b"x"), None).await;
            assert!(put.is_err(), "a build node takes no PUT");
            assert_eq!(upstream.calls(Op::Put), 0);
            assert_eq!(tree(&root), before, "the successor wrote to the directory");
            assert_eq!(staged(&root), held);
            assert_eq!(counts(new).await, (1, 1, 0));
            assert_eq!(tree(&root), before);

            // The old instance's fill is released and published.
            gate.add_permits(100);
            assert_eq!(filling.await.unwrap(), body);
            let (data, sidecar) = {
                let data = data_file(&root, "cas/late");
                let mut sidecar = data.clone().into_os_string();
                sidecar.push(".meta");
                (data, PathBuf::from(sidecar))
            };
            wait_until("the old instance published its fill", || {
                data.exists() && sidecar.exists() && staged(&root).is_empty()
            })
            .await;
            age(&root);
            let published = tree(&root);

            // The successor's index was scanned before, so what the old one published is a miss.
            let gets = upstream.calls_on(Op::Get, "cas/late");
            assert_eq!(read(new, "cas/late").await, body);
            assert_eq!(upstream.calls_on(Op::Get, "cas/late"), gets + 1);
            assert_eq!(read(new, "cas/warm1").await, body);
            assert_eq!(counts(new).await, (2, 2, 0));
            assert_eq!(
                tree(&root),
                published,
                "the successor wrote after the publish"
            );
            let (status, text) = probe(new, "/readyz").await;
            assert_eq!((status, text.as_str()), (200, "ready read-only"));

            // The old instance ends when the connections it drains are closed.
            drop(to_old);
            (&mut pair.old_ended).await.unwrap();
            becomes_read_write(shared).await;
            drop(to_new);
            assert!(staged(&root).is_empty(), "{:?}", staged(&root));
            let gets = upstream.calls(Op::Get);
            assert_eq!(read(shared, "cas/late").await, body);
            assert_eq!(upstream.calls(Op::Get), gets, "now a hit");
            assert_eq!(counts(shared).await, (3, 2, 0));
            assert_eq!(assert_no_stray_sidecars(&root), 3);
            assert_eq!(upstream.calls(Op::Put), 0);
            pair.stop_new.send(()).unwrap();
        }
    };
    let run = |settings: CacheSettings,
               told: Box<dyn FnOnce(Addresses) + Send>,
               stopped: oneshot::Receiver<()>| {
        let upstream = Arc::clone(&for_nodes);
        async move { run_cache_node(&settings, &upstream, told, stopped).await }
    };
    let (old, new) = two_nodes(first, second, run, work).await;
    assert_eq!(old.unwrap(), Drained { abandoned: 0 });
    assert_eq!(new.unwrap(), Drained { abandoned: 0 });
}

/// What a storage node with a local cache over the real disk is run with.
async fn run_combined_node(
    settings: CombinedSettings,
    told: Box<dyn FnOnce(Addresses) + Send>,
    stopped: oneshot::Receiver<()>,
) -> Result<Drained, Error> {
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let stopped = async move {
        let _ = stopped.await;
    };
    serve_combined(&settings, disk, stopped, told).await
}

/// The settings of a storage node over `node` with `slot`, that hands off over `socket`.
fn combined_settings(node: &CombinedNode, socket: &Path, slot: u16) -> CombinedSettings {
    let server = format!(
        "handoff_socket = {:?}\nslot = {slot}",
        socket.display().to_string()
    );
    settings_from_text(&node.text_with(&server, "", ""), QUICK)
        .unwrap()
        .into_combined()
        .unwrap()
}

/// Fills the cache of the directory of `node` with `keys`, by reading them through a lone
/// storage node, and waits until that node has let the lock go.
async fn warm_storage_node(node: &CombinedNode, socket: &Path, keys: &[&str]) {
    let settings = combined_settings(node, socket, 9420);
    let (told, bound) = oneshot::channel::<Addresses>();
    let (stop, stopped) = oneshot::channel::<()>();
    let told: Box<dyn FnOnce(Addresses) + Send> = Box::new(move |at| drop(told.send(at)));
    let running = run_combined_node(settings, told, stopped);
    let driving = async {
        let at = bound.await.unwrap().s3;
        for key in keys {
            let _ = read(at, key).await;
        }
        drop(stop);
    };
    let (drained, ()) = tokio::join!(running, driving);
    assert_eq!(drained.unwrap(), Drained { abandoned: 0 });
    free_lock(&node.cache_dir().join(".lock")).await;
}

/// Asserts (T12), for a storage node with a local cache over a warm cache directory and dir
/// tiers, what the successor does to the directory while the old instance holds its lock. The
/// old instance (slot 9421) has a fill of the kind its drain leaves in `.stage/`; the successor
/// (slot 9422) takes the sockets and says `ready read-only`. Driven through the successor alone:
/// a hit, a miss and a PUT, with the listing of the whole directory the same after them and the
/// staged file still there, and the PUT in the hot tier. After the old instance has filled and
/// published an object (read through it on a connection of its own), the listing is the same
/// across more reads through the successor, and that object is a miss for it (no hit counted).
/// When the old instance has ended, `/readyz` says `ready read-write`, `.stage` is empty, the
/// object the old one published is a hit, and every sidecar has its data file and back. Catches:
/// a successor that clears `.stage`, scans with deletes or stamps hits while the old one fills, a
/// write-through that reaches the old one's directory, and one that never takes the lock.
#[tokio::test]
async fn a_storage_node_successor_over_a_warm_directory_writes_nothing_until_the_old_one_exits() {
    let node = CombinedNode::new("tslock-combined");
    node.activate().await;
    let (_dir, socket) = control();
    let body = pattern(SIZE as usize);
    let hot = CombinedNode::tier(node.hot());
    for key in ["cas/warm0", "cas/warm1", "cas/miss", "cas/late"] {
        hot.put(key, Bytes::from(body.clone())).await.unwrap();
    }
    warm_storage_node(&node, &socket, &["cas/warm0", "cas/warm1"]).await;
    let root = node.cache_dir();
    assert_eq!(assert_no_stray_sidecars(&root), 2);

    let first = combined_settings(&node, &socket, 9421);
    let second = combined_settings(&node, &socket, 9422);
    let work = |mut pair: Pair| {
        let (root, body, hot) = (root.clone(), body.clone(), node.hot());
        async move {
            let shared = pair.old.s3;
            let (status, text) = probe(shared, "/readyz").await;
            assert_eq!((status, text.as_str()), (200, "ready read-write"));
            // A read of the old instance, half written, keeps its drain going and is finished
            // after the successor's scan: it fills an object the scan did not see.
            let hold = Hold::new();
            let to_old = pin_holding(shared, 9421, Some(hold.clone())).await;
            let filling = {
                let at = to_old.at;
                tokio::spawn(async move { read(at, "cas/late").await })
            };
            let _ = hold.arrived.acquire().await.unwrap();
            // What an unfinished fill of the old instance leaves in the directory.
            std::fs::write(root.join(".stage").join("0000000000000009"), b"a fill").unwrap();

            pair.go.send(()).unwrap();
            let taken = (&mut pair.new_bound).await.unwrap();
            assert_eq!(taken.s3, shared, "the very sockets");
            let (status, text) = successor_readyz(shared).await;
            assert_eq!((status, text.as_str()), (200, "ready read-only"));
            let to_new = pin(shared, 9422).await;
            let new = to_new.at;
            assert_eq!(counts(new).await, (0, 0, 0));

            age(&root);
            let before = tree(&root);
            assert_eq!(read(new, "cas/warm0").await, body);
            assert_eq!(read(new, "cas/miss").await, body);
            let put = ObjectKey::new("cas/put").unwrap();
            client(new)
                .put_new(&put, Bytes::from(body.clone()), None)
                .await
                .unwrap();
            let hot = CombinedNode::tier(hot);
            assert_eq!(hot.get("cas/put", None).await.unwrap(), body);
            assert_eq!(tree(&root), before, "the successor wrote to the directory");
            assert_eq!(staged(&root), ["0000000000000009"]);
            assert_eq!(counts(new).await, (1, 1, 0));

            // The old instance fills and publishes an object the successor's scan did not see.
            hold.release.add_permits(1);
            assert_eq!(filling.await.unwrap(), body);
            let data = data_file(&root, "cas/late");
            wait_until("the old instance published its fill", || data.exists()).await;
            age(&root);
            let published = tree(&root);
            assert_eq!(read(new, "cas/late").await, body);
            assert_eq!(read(new, "cas/warm1").await, body);
            assert_eq!(counts(new).await, (2, 2, 0));
            assert_eq!(
                tree(&root),
                published,
                "the successor wrote after the publish"
            );
            let (status, text) = probe(new, "/readyz").await;
            assert_eq!((status, text.as_str()), (200, "ready read-only"));

            drop(to_old);
            (&mut pair.old_ended).await.unwrap();
            becomes_read_write(shared).await;
            drop(to_new);
            assert!(staged(&root).is_empty(), "{:?}", staged(&root));
            assert_eq!(read(shared, "cas/late").await, body);
            assert_eq!(counts(shared).await, (3, 2, 0));
            assert!(assert_no_stray_sidecars(&root) >= 3);
            pair.stop_new.send(()).unwrap();
        }
    };
    let (old, new) = two_nodes(first, second, run_combined_node, work).await;
    assert_eq!(old.unwrap(), Drained { abandoned: 0 });
    assert_eq!(new.unwrap(), Drained { abandoned: 0 });
}
