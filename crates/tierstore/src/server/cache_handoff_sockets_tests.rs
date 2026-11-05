//! A node with a local cache: the sockets a hand-off moves, and a hand-off that fails.
//!
//! The three shapes of node are run through the same cases: a build node, a storage node with
//! `[local_cache]`, and, as a control that must stay as it is, a storage node with none.

use std::fs::Permissions;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
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
use super::cache_cli_tests::{Node as BuildNode, QUICK, probe};
use super::cache_handoff_tests::successor_readyz;
use super::cli::{Addresses, Cli, Error, HandingError, exit_status, run_serving};
use super::combined_cli::serve_combined;
use super::combined_cli_tests::Node as CombinedNode;
use super::config::settings_from_text;
use super::config_cache::CacheSettings;
use super::config_combined::CombinedSettings;
use super::handoff::HandoffError;
use super::handoff::testing::{connect, send};
use super::local_cache::testing::{age, tree};
use super::local_cache::{Disk, SystemDisk};
use super::lock::try_take;
use crate::{Backend, MemoryBackend};

/// What a client sends to ask `/healthz`, keeping the connection alive.
const HEALTHZ: &[u8] = b"GET /healthz HTTP/1.1\r\nhost: tier.test\r\n\r\n";

/// The key of the reconciler's lease in the hot tier.
const LEASE: &str = "_tiering/lease";

/// How many looks a wait for a state that another task changes makes.
const TRIES: u32 = 1_500;

/// The pause between two such looks.
const PAUSE: Duration = Duration::from_millis(20);

/// The shape of a node.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// A storage node with no cache: the control.
    Plain,
    /// A build node.
    Build,
    /// A storage node with `[local_cache]`.
    Combined,
}

/// Every shape.
const SHAPES: [Shape; 3] = [Shape::Plain, Shape::Build, Shape::Combined];

/// The shapes that have a cache directory.
const CACHE_SHAPES: [Shape; 2] = [Shape::Build, Shape::Combined];

impl Shape {
    /// Whether the shape binds a loopback socket beside the configured addresses.
    fn adds_loopback(self) -> bool {
        !matches!(self, Self::Plain)
    }
}

/// The settings of a node of one of the shapes.
enum Config {
    Plain(Cli),
    Build(CacheSettings),
    Combined(Box<CombinedSettings>),
}

/// The scratch directories of a node of one shape, and its control socket.
struct Rig {
    /// The directory of a plain node, and its key file.
    plain: Option<(TestDir, PathBuf)>,
    build: Option<BuildNode>,
    combined: Option<CombinedNode>,
    /// The directory of the control socket.
    control: TestDir,
    /// The control socket.
    socket: PathBuf,
}

impl Rig {
    async fn new(shape: Shape) -> Self {
        let control = TestDir::under(std::env::temp_dir(), "tscache-sockets");
        let socket = control.path().join("control.sock");
        let mut rig = Self {
            plain: None,
            build: None,
            combined: None,
            control,
            socket,
        };
        match shape {
            Shape::Plain => {
                let dir = TestDir::under(std::env::temp_dir(), "tsplain-sockets");
                let key = dir.path().join("key");
                std::fs::write(&key, "AKIDCACHE\ncache-secret-0123456789\n").unwrap();
                std::fs::set_permissions(&key, Permissions::from_mode(0o600)).unwrap();
                rig.plain = Some((dir, key));
            }
            Shape::Build => rig.build = Some(BuildNode::new("tscache-sockets-build")),
            Shape::Combined => {
                let node = CombinedNode::new("tscache-sockets-combined");
                node.activate().await;
                rig.combined = Some(node);
            }
        }
        rig
    }

    /// The cache directory, if the shape has one.
    fn cache_dir(&self) -> Option<PathBuf> {
        let build = self.build.as_ref().map(BuildNode::cache_dir);
        build.or_else(|| self.combined.as_ref().map(CombinedNode::cache_dir))
    }

    /// The lock file of the cache directory.
    fn cache_lock(&self) -> PathBuf {
        self.cache_dir().expect("a cache directory").join(".lock")
    }

    /// The hot tier's lock file, for the shape that names one.
    fn hot_lock(&self) -> Option<PathBuf> {
        let node = self.combined.as_ref()?;
        Some(node.dir.path().join("instance.lock"))
    }

    /// The settings of a node of this shape that listens on `listen` and hands off over `socket`,
    /// waiting `wait` (a key's text).
    fn config(&self, socket: &Path, wait: &str, listen: &[&str]) -> Config {
        let quoted: Vec<String> = listen.iter().map(|addr| format!("\"{addr}\"")).collect();
        let quoted = quoted.join(", ");
        let server = format!(
            "handoff_socket = {:?}\nhandoff_wait = \"{wait}\"",
            socket.display().to_string()
        );
        if let Some((_, key)) = &self.plain {
            let text = format!(
                r#"
[[interface]]
type   = "s3"
params = {{ addresses = [{quoted}] }}

[server]
proxy_id        = "p1"
bucket          = "tier-bucket"
access_key_file = "{key}"
{server}

[hot_store]
kind   = "memory"
budget = 1000

[cold_store]
kind = "memory"
"#,
                key = key.display()
            );
            let settings = settings_from_text(&text, QUICK).unwrap();
            return Config::Plain(settings.into_storage().unwrap());
        }
        let any = "\"127.0.0.1:0\"";
        if let Some(node) = &self.build {
            let text = node.text(&server, "").replace(any, &quoted);
            let settings = settings_from_text(&text, QUICK).unwrap();
            return Config::Build(settings.into_cache().unwrap());
        }
        let node = self.combined.as_ref().expect("a node");
        let lock = self.hot_lock().expect("a lock file");
        let line = format!("lock_file = {:?}", lock.display().to_string());
        let text = node.text_with(&server, &line, "").replace(any, &quoted);
        let settings = settings_from_text(&text, QUICK).unwrap();
        Config::Combined(Box::new(settings.into_combined().unwrap()))
    }
}

/// Serves a node with `config`, over a fresh in-memory upstream where it has one and the real
/// disk, until `stop` resolves (or a successor takes its sockets).
async fn start(
    config: &Config,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(Addresses),
) -> Result<Drained, Error> {
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    match config {
        Config::Plain(cli) => run_serving(cli, stop, ready).await,
        Config::Build(settings) => {
            let upstream: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
            serve_cache(settings, upstream, disk, stop, ready).await
        }
        Config::Combined(settings) => serve_combined(&settings, disk, stop, ready).await,
    }
}

/// The ends of two nodes that run in one task: the first, which is serving when `work` is
/// called, and the second, which `go` starts. The first is not stopped by anything but the
/// second's taking its sockets, until `work` is over.
struct Pair {
    /// What the first serves.
    old: Addresses,
    /// Starts the second.
    go: oneshot::Sender<()>,
    /// What the second serves, once it has taken the sockets; closed if it fails.
    new_bound: oneshot::Receiver<Addresses>,
    /// Resolves when the first has ended.
    old_ended: oneshot::Receiver<()>,
    /// Stops the second.
    stop_new: oneshot::Sender<()>,
}

/// Runs a node on `first` and, once `work` says go, one on `second`; `work` drives them. Gives
/// how each ended.
async fn swap<F, Fut>(
    first: &Config,
    second: &Config,
    work: F,
) -> (Result<Drained, Error>, Result<Drained, Error>)
where
    F: FnOnce(Pair) -> Fut,
    Fut: Future<Output = ()>,
{
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
        let drained = start(first, stopped, move |at| drop(old_told.send(at))).await;
        let _ = old_done.send(());
        drained
    };
    let new = async {
        going.await.expect("the test starts the second");
        let stopped = async move {
            let _ = stopped_new.await;
        };
        start(second, stopped, move |at| drop(new_told.send(at))).await
    };
    let driving = async {
        let old = old_bound.await.expect("the first binds");
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

/// Asserts that `socket` is a Unix socket of mode 0600.
fn assert_control_socket(socket: &Path) {
    let meta = std::fs::symlink_metadata(socket).unwrap();
    assert!(meta.file_type().is_socket(), "{}", socket.display());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
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

/// A connection to `at` with the head of a `/healthz` request written: a client in flight, which
/// keeps the drain of the instance that took it going.
async fn half_written(at: SocketAddr) -> TcpStream {
    let mut connection = TcpStream::connect(at).await.unwrap();
    connection.write_all(&HEALTHZ[..16]).await.unwrap();
    connection
}

/// Sends the rest of the request [`half_written`] began and gives the answer.
async fn finish_request(connection: &mut TcpStream) -> String {
    connection.write_all(&HEALTHZ[16..]).await.unwrap();
    read_until(connection, "\r\n\r\nok").await
}

/// Asks `/healthz` at `at`, asserts the answer is 200 `ok`, and counts the request in `sent`.
async fn healthy(at: SocketAddr, sent: &mut u32) {
    let (status, text) = probe(at, "/healthz").await;
    assert_eq!((status, text.as_str()), (200, "ok"), "{at}");
    *sent += 1;
}

/// Polls `/readyz` at `addr` until it says `want`, in a bounded number of tries.
async fn readyz_becomes(addr: SocketAddr, want: &str) {
    let mut last = String::new();
    for _ in 0..TRIES {
        let (status, text) = probe(addr, "/readyz").await;
        if status == 200 && text == want {
            return;
        }
        last = text;
        tokio::time::sleep(PAUSE).await;
    }
    panic!("/readyz never said {want:?}; last: {last:?}");
}

/// The lines of the lease object in the hot directory `hot` (holder, nonce, generation), or none.
async fn lease_lines(hot: &Path) -> Vec<String> {
    use crate::Backend as _;
    let tier = CombinedNode::tier(hot.to_owned());
    match tier.get(LEASE, None).await {
        Ok(body) => String::from_utf8(body.to_vec())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// The nonce of the lease in `hot`, once there is a lease whose nonce is not `not`: a new holder
/// has taken it. Looks a bounded number of times.
async fn nonce_other_than(hot: &Path, not: Option<&str>) -> String {
    for _ in 0..TRIES {
        if let Some(nonce) = lease_lines(hot).await.get(1)
            && Some(nonce.as_str()) != not
        {
            return nonce.clone();
        }
        tokio::time::sleep(PAUSE).await;
    }
    panic!("no lease other than {not:?} was taken");
}

/// The hand-off error `error` holds, if it is one.
fn handoff_error(error: &Error) -> Option<&HandoffError> {
    match error.downcast_ref::<HandingError>()? {
        HandingError::Handoff(why) => Some(why),
        _ => None,
    }
}

/// The addresses, as their IPs, that a node of `shape` listening on `listen` binds, in order: the
/// configured ones, then 127.0.0.1 when a cache adds it (it is not there, and no wildcard covers
/// it).
fn expected_ips(shape: Shape, listen: &[&str]) -> Vec<IpAddr> {
    let mut ips: Vec<IpAddr> = listen
        .iter()
        .map(|addr| addr.parse::<SocketAddr>().unwrap().ip())
        .collect();
    let loopback = IpAddr::from([127, 0, 0, 1]);
    let wildcard = IpAddr::from([0, 0, 0, 0]);
    if shape.adds_loopback() && !ips.contains(&loopback) && !ips.contains(&wildcard) {
        ips.push(loopback);
    }
    ips
}

/// Asserts that a second node of `shape` listening on `listen` (port 0 for each) takes the very
/// sockets of the first: the first binds the configured addresses and, for a cache, 127.0.0.1 on
/// a port of its own, in that order, each on a port the system chose; the second says the same
/// addresses to `ready`; each of them is answered by the second while the first drains (a client
/// left open keeps it so) and after it has ended, and the first ends with nothing cut.
async fn takes_the_same_sockets(shape: Shape, listen: &[&str]) {
    let rig = Rig::new(shape).await;
    let config = rig.config(&rig.socket, "1h", listen);
    let work = |mut pair: Pair| async move {
        let old = pair.old.clone();
        let ips: Vec<IpAddr> = old.s3_all.iter().map(SocketAddr::ip).collect();
        assert_eq!(ips, expected_ips(shape, listen), "{shape:?} {old:?}");
        let mut ports: Vec<u16> = old.s3_all.iter().map(SocketAddr::port).collect();
        assert!(ports.iter().all(|port| *port != 0), "{ports:?}");
        ports.sort_unstable();
        ports.dedup();
        assert_eq!(ports.len(), old.s3_all.len(), "a port of its own each");
        let before = half_written(old.s3).await;
        pair.go.send(()).unwrap();
        let taken = pair.new_bound.await.unwrap();
        assert_eq!(taken.s3_all, old.s3_all, "{shape:?}: no socket of its own");
        assert_eq!(taken.s3, old.s3);
        for at in &taken.s3_all {
            let (status, _) = successor_readyz(*at).await;
            assert_eq!(status, 200, "{shape:?} {at}");
        }
        drop(before);
        (&mut pair.old_ended).await.unwrap();
        let mut sent = 0;
        for at in &taken.s3_all {
            healthy(*at, &mut sent).await;
        }
        assert_eq!(usize::try_from(sent).unwrap(), old.s3_all.len());
        pair.stop_new.send(()).unwrap();
    };
    let (old, new) = swap(&config, &config, work).await;
    assert_eq!(old.unwrap(), Drained { abandoned: 0 }, "{shape:?}");
    assert_eq!(new.unwrap(), Drained { abandoned: 0 }, "{shape:?}");
}

/// Asserts (T02) that an address that is not loopback gets the loopback socket of a cache:
/// a node listening on `127.0.0.2:0` offers that socket and 127.0.0.1 on a port of its own, the
/// second node takes both and binds nothing, and a client on the 127.0.0.1 socket is answered
/// after the first has ended; a storage node with no cache offers the one socket. Catches: a
/// length mismatch on every update of a cache node, a successor that binds 127.0.0.1 beside the
/// first, and a loopback listener lost in the hand-over.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_cache_node_hands_over_its_loopback_socket_beside_a_non_loopback_address() {
    for shape in SHAPES {
        takes_the_same_sockets(shape, &["127.0.0.2:0"]).await;
    }
}

/// Asserts (T02) that a wildcard address adds no socket: a node on `0.0.0.0:0` offers that one
/// socket, which the second node takes and a client on 127.0.0.1 reaches through. Catches: a
/// second loopback bind that the wildcard already covers.
#[tokio::test]
async fn a_wildcard_address_hands_over_one_socket() {
    for shape in SHAPES {
        takes_the_same_sockets(shape, &["0.0.0.0:0"]).await;
    }
}

/// Asserts (T02) that two non-loopback addresses and the added loopback socket are handed over
/// in the order of the list and then the loopback one, each on a port of its own: three sockets
/// for a cache and two for a storage node with none, the same ones in the second node.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn two_addresses_are_handed_over_in_order_with_the_loopback_socket_last() {
    for shape in SHAPES {
        takes_the_same_sockets(shape, &["127.0.0.2:0", "127.0.0.3:0"]).await;
    }
}

/// Asserts (T02) that a loopback address at port 0 is handed over as it was bound, with no
/// socket added beside it: the second node says the very port the first chose.
#[tokio::test]
async fn a_loopback_address_at_port_zero_hands_over_the_port_the_first_chose() {
    for shape in SHAPES {
        takes_the_same_sockets(shape, &["127.0.0.1:0"]).await;
    }
}

/// Asserts (T02) that a successor whose settings list the addresses in another order is refused
/// with the exact `Mismatch` text naming both lists (what the first offers, in its order, and
/// what the settings name with the loopback socket of a cache), with exit status 1 and nothing
/// bound, and that the first goes on answering on each of its addresses and keeps its control
/// socket. Catches: an order-blind comparison that hands the sockets over to a node that expects
/// them elsewhere.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_reordered_list_is_a_mismatch_and_the_first_goes_on() {
    for shape in SHAPES {
        let rig = Rig::new(shape).await;
        let listen = ["127.0.0.2:0", "127.0.0.3:0"];
        let reordered = ["127.0.0.3:0", "127.0.0.2:0"];
        let first = rig.config(&rig.socket, "1h", &listen);
        let second = rig.config(&rig.socket, "1h", &reordered);
        let work = |pair: Pair| {
            let socket = rig.socket.clone();
            async move {
                pair.go.send(()).unwrap();
                assert!(
                    pair.new_bound.await.is_err(),
                    "a failed start binds nothing"
                );
                let mut sent = 0;
                for at in &pair.old.s3_all {
                    healthy(*at, &mut sent).await;
                }
                assert_eq!(usize::try_from(sent).unwrap(), pair.old.s3_all.len());
                assert_control_socket(&socket);
            }
        };
        let (old, new) = swap(&first, &second, work).await;
        assert_eq!(old.unwrap(), Drained { abandoned: 0 }, "{shape:?}");
        let error = new.unwrap_err();
        assert_eq!(exit_status(&error), 1, "{shape:?}");
        assert!(
            matches!(handoff_error(&error), Some(HandoffError::Mismatch { .. })),
            "{error}"
        );
        // The first's addresses are not known here, so the offered list is read out of the text.
        let text = error.to_string();
        let mut wanted: Vec<String> = reordered.iter().map(|a| format!("s3 {a}")).collect();
        if shape.adds_loopback() {
            wanted.push("s3 127.0.0.1:0".to_owned());
        }
        let head = "hand-off: the running instance listens on [";
        let tail = format!("], these settings name {wanted:?}");
        assert!(text.starts_with(head), "{shape:?} {text}");
        assert!(text.ends_with(&tail), "{shape:?} {text}");
        let offered = &text[head.len()..text.len() - tail.len()];
        let ips: Vec<&str> = offered
            .split(", ")
            .map(|item| item.trim_matches('"'))
            .map(|item| item.rsplit_once(':').unwrap().0)
            .collect();
        let mut want_ips = vec!["s3 127.0.0.2", "s3 127.0.0.3"];
        if shape.adds_loopback() {
            want_ips.push("s3 127.0.0.1");
        }
        assert_eq!(ips, want_ips, "{shape:?} {text}");
    }
}

/// Asserts (T03) that a second node takes the sockets of the first and the first returns on its
/// own, for each shape: the first is never told to stop, yet returns `Drained` with nothing cut
/// once the client that held its drain has closed; a counted set of requests, before, during
/// and after the swap, and one request in flight across it, all answer 200; the second's control
/// socket is a socket of mode 0600 from the moment it serves until its stop, and gone after it.
/// Catches: a start path that does not stop its instance when the successor has taken the
/// sockets (it would serve for ever), and a request cut by the swap.
async fn swap_answers_every_request(shape: Shape) {
    const EACH: u32 = 4;
    let rig = Rig::new(shape).await;
    let config = rig.config(&rig.socket, "1h", &["127.0.0.1:0"]);
    let work = |mut pair: Pair| {
        let rig = &rig;
        async move {
            let at = pair.old.s3;
            let hot = rig.combined.as_ref().map(CombinedNode::hot);
            let first_nonce = match &hot {
                Some(hot) => Some(nonce_other_than(hot, None).await),
                None => None,
            };
            if shape.adds_loopback() {
                assert!(try_take(&rig.cache_lock()).unwrap().is_none());
            }
            let mut sent = 0;
            for _ in 0..EACH {
                healthy(at, &mut sent).await;
            }
            let mut before = half_written(at).await;
            pair.go.send(()).unwrap();
            let taken = pair.new_bound.await.unwrap();
            assert_eq!(taken, pair.old);
            assert_control_socket(&rig.socket);
            for _ in 0..EACH {
                healthy(at, &mut sent).await;
            }
            let late = finish_request(&mut before).await;
            assert!(late.starts_with("HTTP/1.1 200"), "{late}");
            sent += 1;
            // The first's drain holds a connection until the client closes its end.
            drop(before);
            (&mut pair.old_ended).await.unwrap();
            for _ in 0..EACH {
                healthy(at, &mut sent).await;
            }
            assert_eq!(sent, 3 * EACH + 1);
            assert_control_socket(&rig.socket);
            if shape.adds_loopback() {
                readyz_becomes(at, "ready read-write").await;
                assert!(try_take(&rig.cache_lock()).unwrap().is_none());
            }
            if let (Some(hot), Some(first)) = (&hot, &first_nonce) {
                nonce_other_than(hot, Some(first)).await;
                let lock = rig.hot_lock().unwrap();
                assert!(try_take(&lock).unwrap().is_none());
            }
            assert_control_socket(&rig.socket);
            pair.stop_new.send(()).unwrap();
        }
    };
    let (old, new) = swap(&config, &config, work).await;
    assert_eq!(old.unwrap(), Drained { abandoned: 0 }, "{shape:?}");
    assert_eq!(new.unwrap(), Drained { abandoned: 0 }, "{shape:?}");
    assert!(!rig.socket.exists(), "{shape:?}: removed by the final stop");
    if shape.adds_loopback() {
        assert!(try_take(&rig.cache_lock()).unwrap().is_some());
    }
    if let Some(lock) = rig.hot_lock() {
        assert!(try_take(&lock).unwrap().is_some());
    }
}

/// Asserts (T03) the swap of a storage node with no cache (the control): see
/// [`swap_answers_every_request`].
#[tokio::test]
async fn a_plain_storage_node_returns_after_the_swap_and_answers_every_request() {
    swap_answers_every_request(Shape::Plain).await;
}

/// Asserts (T03) the swap of a build node: see [`swap_answers_every_request`]. Besides, the cache
/// directory's lock is held by the first before, by the second once it is read-write (`ready
/// read-write` after the first returned), and free after the second has stopped.
#[tokio::test]
async fn a_build_node_returns_after_the_swap_and_answers_every_request() {
    swap_answers_every_request(Shape::Build).await;
}

/// Asserts (T03) the swap of a storage node with a cache: see [`swap_answers_every_request`].
/// Besides, both locks are the second's once the first has returned (the cache reads-write and the
/// reconciler's lease has another holder), they are held while it runs, and free after its stop.
#[tokio::test]
async fn a_combined_node_returns_after_the_swap_and_answers_every_request() {
    swap_answers_every_request(Shape::Combined).await;
}

/// How a start fails before the running instance's `done`.
#[derive(Clone, Copy, Debug)]
enum Failure {
    /// The settings name other addresses.
    Mismatch,
    /// Another taker is at the running instance.
    Refused,
    /// The control socket is a listener that never answers.
    Timeout,
}

/// Reads from `stream` until its `offer` frame has come in, and gives the text read.
async fn offer_read(stream: &tokio::net::UnixStream) -> String {
    let mut got = Vec::new();
    while !got.ends_with(b"\n\n") {
        stream.readable().await.unwrap();
        let mut piece = [0_u8; 1024];
        match stream.try_read(&mut piece) {
            Ok(0) => break,
            Ok(read) => got.extend_from_slice(&piece[..read]),
            Err(why) if why.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(why) => panic!("{why}"),
        }
    }
    String::from_utf8(got).unwrap()
}

/// Sends `nak` on `stream` and reads until the running instance has closed its end, which it does
/// after it has let the offer go: a taker that comes after finds it free.
async fn let_go(stream: &tokio::net::UnixStream) {
    send(stream, &["nak"]).await;
    loop {
        stream.readable().await.unwrap();
        let mut piece = [0_u8; 64];
        match stream.try_read(&mut piece) {
            Ok(0) => return,
            Ok(_) => {}
            Err(why) if why.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(why) => panic!("{why}"),
        }
    }
}

/// Asserts that a start of `shape` that fails as `failure` says, hand-off error, exit status 1
/// and the exact words; binds nothing (`ready` is never called); leaves the running instance
/// answering, holding the cache directory's lock, the hot tier's lock file (where there is one)
/// and its control socket; leaves the cache directory's tree as it was (every entry's kind, size,
/// mode, inode and time of change, taken after the tree was aged); and that a correct start then
/// takes the sockets and serves read-write once the first has ended.
async fn failed_start_changes_nothing(shape: Shape, failure: Failure) {
    let rig = Rig::new(shape).await;
    let first = rig.config(&rig.socket, "1h", &["127.0.0.1:0"]);
    let root = rig.cache_dir().expect("a cache directory");
    let work = |mut pair: Pair| {
        let (rig, root) = (&rig, &root);
        async move {
            let at = pair.old.s3;
            age(root);
            let before = tree(root);
            let silent_path = rig.control.path().join("silent.sock");
            let mut silent = None;
            let mut held = None;
            let (bad, want) = match failure {
                Failure::Mismatch => {
                    let bad = rig.config(&rig.socket, "1h", &["0.0.0.0:0"]);
                    let want = format!(
                        "hand-off: the running instance listens on [\"s3 {at}\"], these settings name [\"s3 0.0.0.0:0\"]"
                    );
                    (bad, want)
                }
                Failure::Refused => {
                    let taker = connect(&rig.socket).await;
                    send(&taker, &["take 1"]).await;
                    let offer = offer_read(&taker).await;
                    assert_eq!(offer, format!("offer\ns3 {at}\n\n"));
                    held = Some(taker);
                    let bad = rig.config(&rig.socket, "1h", &["127.0.0.1:0"]);
                    let want = "hand-off: the running instance refused: another taker is taking the sockets";
                    (bad, want.to_owned())
                }
                Failure::Timeout => {
                    silent = Some(UnixListener::bind(&silent_path).unwrap());
                    let bad = rig.config(&silent_path, "50ms", &["127.0.0.1:0"]);
                    let want = format!(
                        "hand-off: no end of the hand-off at {} in time",
                        silent_path.display()
                    );
                    (bad, want)
                }
            };
            let error = start(&bad, async {}, |bound| panic!("bound {bound:?}"))
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), want, "{shape:?} {failure:?}");
            assert_eq!(exit_status(&error), 1, "{shape:?} {failure:?}");
            let kind = handoff_error(&error);
            let right = match failure {
                Failure::Mismatch => matches!(kind, Some(HandoffError::Mismatch { .. })),
                Failure::Refused => matches!(kind, Some(HandoffError::Refused(_))),
                Failure::Timeout => matches!(kind, Some(HandoffError::Timeout(_))),
            };
            assert!(right, "{shape:?} {failure:?}: {error}");

            let (status, text) = probe(at, "/healthz").await;
            assert_eq!((status, text.as_str()), (200, "ok"));
            let (status, text) = probe(at, "/readyz").await;
            assert_eq!((status, text.as_str()), (200, "ready read-write"));
            assert_eq!(tree(root), before, "{shape:?} {failure:?}: the tree");
            assert!(try_take(&rig.cache_lock()).unwrap().is_none());
            if let Some(lock) = rig.hot_lock() {
                assert!(try_take(&lock).unwrap().is_none());
            }
            assert_control_socket(&rig.socket);

            if let Some(taker) = &held {
                let_go(taker).await;
            }
            drop(silent);
            pair.go.send(()).unwrap();
            let taken = pair.new_bound.await.unwrap();
            assert_eq!(taken, pair.old, "a correct start takes the sockets");
            (&mut pair.old_ended).await.unwrap();
            readyz_becomes(at, "ready read-write").await;
            pair.stop_new.send(()).unwrap();
        }
    };
    let (old, new) = swap(&first, &first, work).await;
    assert_eq!(
        old.unwrap(),
        Drained { abandoned: 0 },
        "{shape:?} {failure:?}"
    );
    assert_eq!(
        new.unwrap(),
        Drained { abandoned: 0 },
        "{shape:?} {failure:?}"
    );
}

/// Asserts (T04) that a build node and a storage node with a cache whose settings name other
/// addresses than the running instance (a wildcard for its loopback address) fail the start with
/// the exact `Mismatch` words after the cache is open, and change nothing: see
/// [`failed_start_changes_nothing`]. Catches: a read-only starter that cleans up, scans with
/// delete or probes in a directory the running instance writes.
#[tokio::test]
async fn a_start_with_other_addresses_changes_nothing_of_the_running_cache_node() {
    for shape in CACHE_SHAPES {
        failed_start_changes_nothing(shape, Failure::Mismatch).await;
    }
}

/// Asserts (T04) that a second taker, while a first is held at the offer by a raw client of the
/// control socket, is refused with the exact words (`another taker is taking the sockets`), and
/// changes nothing: see [`failed_start_changes_nothing`]. The raw client reads the offer
/// (`offer` and the one address) and then answers `nak`, after which a correct start takes.
#[tokio::test]
async fn a_second_taker_is_refused_and_changes_nothing_of_the_running_cache_node() {
    for shape in CACHE_SHAPES {
        failed_start_changes_nothing(shape, Failure::Refused).await;
    }
}

/// Asserts (T04) that a start whose control socket is a listener that never answers fails with
/// the exact `Timeout` words once its short `server.handoff_wait` is over, and changes nothing of
/// the running instance (which has a control socket of its own): see
/// [`failed_start_changes_nothing`].
#[tokio::test]
async fn a_silent_listener_fails_the_start_and_changes_nothing_of_the_running_cache_node() {
    for shape in CACHE_SHAPES {
        failed_start_changes_nothing(shape, Failure::Timeout).await;
    }
}
