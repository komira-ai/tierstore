//! Traffic of every kind across the hand-off of a node with a local cache.
//!
//! Two instances of a build node (with and without `write_through`) or of a storage node with
//! `[local_cache]` run in one process over one control socket and the same upstream. A client
//! holds requests at a known point (half a request written, half an answer read) while the
//! second instance takes the sockets, and finishes them after. Requests are written on raw
//! connections, so that which instance a request is on is known: a connection that was answered
//! before the take is the first instance's, and a connection that was answered `/readyz` with
//! something else than `draining` after it is the second's.

use std::any::Any;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tierstore_testkit::dir::TestDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::oneshot;

use super::cache_cli::serve_cache;
use super::cache_cli_tests::{Node as BuildNode, QUICK, probe};
use super::cli::{Addresses, Error};
use super::combined_cli::serve_combined;
use super::combined_cli_tests::Node as StorageNode;
use super::config::{PerRun, settings_from_text};
use super::config_cache::CacheSettings;
use super::config_combined::CombinedSettings;
use super::local_cache::testing::{digest, pattern};
use super::local_cache::{Disk, SystemDisk};
use super::ops_tests::completion;
use super::testing::texts;
use super::{Drained, ServerConfig};
use crate::server::auth::sign_as;
use crate::{Backend, MemoryBackend, SystemClock};

const KEY_ID: &str = "AKIDCACHE";
const SECRET: &str = "cache-secret-0123456789";
const BUCKET: &str = "tier-bucket";

/// What a client sends to ask `/healthz`, keeping the connection alive.
const HEALTHZ: &[u8] = b"GET /healthz HTTP/1.1\r\nhost: tier.test\r\n\r\n";

/// What a client sends to ask `/readyz`, keeping the connection alive.
const READYZ: &[u8] = b"GET /readyz HTTP/1.1\r\nhost: tier.test\r\n\r\n";

/// How many tries a wait for a state makes.
const TRIES: u32 = 1_500;

/// The receive buffer of a client that reads an answer slowly, so that the answer is held in the
/// server for the want of room.
const SMALL_WINDOW: u32 = 4096;

/// A request, signed, in two pieces: its head, and its body.
struct Wire {
    head: Vec<u8>,
    body: Vec<u8>,
}

/// `/bucket/key`, where `key` may carry a query.
fn obj(key: &str) -> String {
    format!("/{BUCKET}/{key}")
}

/// The request `method uri` signed with the node's key.
fn wire(method: &str, uri: &str, body: &[u8]) -> Wire {
    let config = ServerConfig::new(BUCKET, KEY_ID, SECRET, Arc::new(SystemClock));
    let signed = sign_as(&config, (KEY_ID, SECRET), method, uri, &[], body);
    let (parts, _) = signed.into_parts();
    let mut head = format!("{method} {uri} HTTP/1.1\r\n");
    for (name, value) in &parts.headers {
        head.push_str(&format!("{name}: {}\r\n", value.to_str().unwrap()));
    }
    head.push_str(&format!("content-length: {}\r\n\r\n", body.len()));
    Wire {
        head: head.into_bytes(),
        body: body.to_vec(),
    }
}

/// The head of an answer.
struct Head {
    status: u16,
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        let found = self.headers.iter().find(|(have, _)| have == name);
        found.map(|(_, value)| value.as_str())
    }

    fn length(&self) -> usize {
        self.header("content-length")
            .map_or(0, |text| text.parse().unwrap())
    }
}

/// An answer, whole.
struct Answer {
    head: Head,
    body: Vec<u8>,
}

impl Answer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A client connection that parses what the server answers.
struct Conn {
    stream: TcpStream,
    got: Vec<u8>,
}

impl Conn {
    async fn open(at: SocketAddr) -> Self {
        let stream = TcpStream::connect(at).await.unwrap();
        Self {
            stream,
            got: Vec::new(),
        }
    }

    /// A connection with a small receive buffer.
    async fn open_slow(at: SocketAddr) -> Self {
        let socket = TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(SMALL_WINDOW).unwrap();
        let stream = socket.connect(at).await.unwrap();
        Self {
            stream,
            got: Vec::new(),
        }
    }

    /// A connection that the first instance has answered `/healthz` on, so that it is the first
    /// instance's.
    async fn served(at: SocketAddr, slow: bool) -> Self {
        let mut conn = if slow {
            Self::open_slow(at).await
        } else {
            Self::open(at).await
        };
        let answer = conn.ask(HEALTHZ).await;
        assert_eq!(answer.head.status, 200);
        conn
    }

    async fn send(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).await.unwrap();
    }

    /// Reads the head of the next answer.
    async fn head(&mut self) -> Head {
        let end = loop {
            if let Some(at) = self.got.windows(4).position(|w| w == b"\r\n\r\n") {
                break at;
            }
            let mut piece = [0_u8; 4096];
            let read = self.stream.read(&mut piece).await.unwrap();
            assert_ne!(read, 0, "{:?}", String::from_utf8_lossy(&self.got));
            self.got.extend_from_slice(&piece[..read]);
        };
        let text = String::from_utf8(self.got.drain(..end + 4).collect()).unwrap();
        let mut lines = text.trim_end().split("\r\n");
        let status = lines.next().unwrap();
        let status = status.split(' ').nth(1).unwrap().parse().unwrap();
        let headers = lines
            .map(|line| {
                let (name, value) = line.split_once(':').unwrap();
                (name.to_ascii_lowercase(), value.trim().to_owned())
            })
            .collect();
        Head { status, headers }
    }

    /// Takes the next `n` bytes of a body.
    async fn take(&mut self, n: usize) -> Vec<u8> {
        while self.got.len() < n {
            let mut piece = vec![0_u8; 64 * 1024];
            let read = self.stream.read(&mut piece).await.unwrap();
            assert_ne!(read, 0, "the connection ended inside a body");
            self.got.extend_from_slice(&piece[..read]);
        }
        self.got.drain(..n).collect()
    }

    /// Reads the answer after a head, whole.
    async fn answer(&mut self) -> Answer {
        let head = self.head().await;
        let body = self.take(head.length()).await;
        Answer { head, body }
    }

    /// Sends `bytes` and reads the answer.
    async fn ask(&mut self, bytes: &[u8]) -> Answer {
        self.send(bytes).await;
        self.answer().await
    }

    /// Sends a signed request and reads the answer.
    async fn call(&mut self, request: &Wire) -> Answer {
        self.send(&request.head).await;
        self.send(&request.body).await;
        self.answer().await
    }

    /// Sends the head of `request` and the first `n` bytes of its body.
    async fn begin(&mut self, request: &Wire, n: usize) {
        self.send(&request.head).await;
        self.send(&request.body[..n]).await;
    }

    /// Whether the peer has closed its end and nothing is left to read.
    async fn ended(&mut self) -> bool {
        let mut piece = [0_u8; 16];
        self.got.is_empty() && self.stream.read(&mut piece).await.unwrap() == 0
    }
}

/// A connection that the second instance answered `/readyz` on with something else than
/// `draining` (the first answers that, and closes), and what it said.
async fn successor(at: SocketAddr) -> (Conn, String) {
    for _ in 0..TRIES {
        let mut conn = Conn::open(at).await;
        let answer = conn.ask(READYZ).await;
        let text = answer.text();
        if (answer.head.status, text.as_str()) != (503, "draining") {
            return (conn, text);
        }
        // While the first instance's door is open it takes most of the connections; it closes
        // it when its settle is over.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("every probe of /readyz at {at} was answered by the draining instance");
}

/// Polls `/readyz` at `at` until it says `ready read-write`, in a bounded number of tries.
async fn read_write(at: SocketAddr) {
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let mut last = String::new();
    for _ in 0..TRIES {
        tick.tick().await;
        let (status, text) = probe(at, "/readyz").await;
        if status == 200 && text == "ready read-write" {
            return;
        }
        last = text;
    }
    panic!("/readyz never said ready read-write; last: {last:?}");
}

/// Polls `/readyz` at `at` until an answer says `draining`, in a bounded number of tries.
async fn draining(at: SocketAddr) {
    for _ in 0..TRIES {
        if probe(at, "/readyz").await == (503, "draining".to_owned()) {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("no answer at {at} said draining");
}

/// The upstream of a node, and what it takes to read it back.
enum Kind {
    /// A build node over a memory upstream.
    Build(Box<CacheSettings>, Arc<MemoryBackend>),
    /// A storage node over a hot and a cold directory.
    Storage(Box<CombinedSettings>, PathBuf, PathBuf),
}

/// One node shape: the settings both instances start from, and the upstream they share.
struct Shape {
    kind: Kind,
    /// Whether the node takes writes.
    writes: bool,
    /// What keeps the scratch directories.
    _scratch: Vec<Box<dyn Any>>,
}

/// A scratch directory for the control socket, and the socket's path in it.
fn control() -> (TestDir, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), "tscache-traffic");
    let socket = dir.path().join("control.sock");
    (dir, socket)
}

/// The line of `[server]` that names the control socket.
fn socket_line(socket: &Path) -> String {
    format!("handoff_socket = {:?}", socket.display().to_string())
}

/// The run knobs with a settle of `settle`.
fn settle(settle: Duration) -> PerRun {
    PerRun {
        drain_lb_wait: settle,
        drain_deadline: Duration::from_secs(60),
    }
}

impl Shape {
    /// A build node, with `write_through` as given.
    fn build(name: &str, writes: bool, per_run: PerRun) -> Self {
        let node = BuildNode::new(name);
        let (dir, socket) = control();
        let server = format!("{}\nhandoff_wait = \"1h\"", socket_line(&socket));
        let local = if writes { "write_through = true" } else { "" };
        let text = node.text(&server, local);
        let settings = settings_from_text(&text, per_run).unwrap();
        Self {
            kind: Kind::Build(
                Box::new(settings.into_cache().unwrap()),
                Arc::new(MemoryBackend::new()),
            ),
            writes,
            _scratch: vec![Box::new(node), Box::new(dir)],
        }
    }

    /// A storage node with a local cache, which writes through.
    async fn storage(name: &str, per_run: PerRun) -> Self {
        let node = StorageNode::new(name);
        node.activate().await;
        let (dir, socket) = control();
        let text = node.text_with(&socket_line(&socket), "", "");
        let settings = settings_from_text(&text, per_run).unwrap();
        let kind = Kind::Storage(
            Box::new(settings.into_combined().unwrap()),
            node.hot(),
            node.cold(),
        );
        Self {
            kind,
            writes: true,
            _scratch: vec![Box::new(node), Box::new(dir)],
        }
    }

    /// Puts `body` in the upstream, to be read through the node.
    async fn seed(&self, key: &str, body: &[u8]) {
        let body = Bytes::copy_from_slice(body);
        match &self.kind {
            Kind::Build(_, upstream) => upstream.put(key, body).await.unwrap(),
            Kind::Storage(_, hot, _) => {
                StorageNode::tier(hot.clone()).put(key, body).await.unwrap()
            }
        }
    }

    /// The object `key` in the upstream, if it is there: in a storage node's hot directory or,
    /// if it is not, in its cold one (the hot one is read first, since a copy to the cold tier
    /// comes before the removal from the hot one).
    async fn stored(&self, key: &str) -> Option<Vec<u8>> {
        let found = match &self.kind {
            Kind::Build(_, upstream) => upstream.get(key, None).await.ok(),
            Kind::Storage(_, hot, cold) => {
                let hot = StorageNode::tier(hot.clone());
                match hot.get(key, None).await {
                    Ok(body) => Some(body),
                    Err(_) => StorageNode::tier(cold.clone()).get(key, None).await.ok(),
                }
            }
        };
        found.map(|body| body.to_vec())
    }

    /// Runs one instance until `stopped` resolves.
    async fn run(
        &self,
        told: oneshot::Sender<Addresses>,
        stopped: oneshot::Receiver<()>,
        ended: Option<oneshot::Sender<()>>,
    ) -> Result<Drained, Error> {
        let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
        let stop = async move {
            let _ = stopped.await;
        };
        let ready = move |at| drop(told.send(at));
        let drained = match &self.kind {
            Kind::Build(settings, upstream) => {
                let upstream: Arc<dyn Backend> = upstream.clone();
                serve_cache(settings, upstream, disk, stop, ready).await
            }
            Kind::Storage(settings, ..) => serve_combined(settings, disk, stop, ready).await,
        };
        if let Some(ended) = ended {
            let _ = ended.send(());
        }
        drained
    }
}

/// The ends of two instances that run in one task, as the work sees them.
struct Pair {
    /// What both serve.
    at: SocketAddr,
    go: Option<oneshot::Sender<()>>,
    new_bound: Option<oneshot::Receiver<Addresses>>,
    old_ended: oneshot::Receiver<()>,
    stop_new: oneshot::Sender<()>,
}

impl Pair {
    /// Starts the second instance and waits until it has taken the sockets.
    async fn take(&mut self) {
        self.go.take().unwrap().send(()).unwrap();
        let taken = self.new_bound.take().unwrap().await.unwrap();
        assert_eq!(taken.s3, self.at, "the very sockets");
    }

    /// Waits for the first instance to end (the client's connections to it are closed by now) and
    /// for the second to have the cache directory.
    async fn first_ends(&mut self) {
        (&mut self.old_ended).await.unwrap();
        read_write(self.at).await;
    }

    /// Stops the second instance.
    fn stop(self) {
        self.stop_new.send(()).unwrap();
    }
}

/// Runs the node twice over one control socket, and `work` beside them from when the first
/// serves. Asserts that both end with nothing cut.
async fn overlap<F, Fut>(shape: &Shape, work: F)
where
    F: FnOnce(Pair) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (old_told, old_bound) = oneshot::channel::<Addresses>();
    let (new_told, new_bound) = oneshot::channel::<Addresses>();
    let (old_done, old_ended) = oneshot::channel::<()>();
    let (go, going) = oneshot::channel::<()>();
    let (stop_old, stopped_old) = oneshot::channel::<()>();
    let (stop_new, stopped_new) = oneshot::channel::<()>();
    let old = shape.run(old_told, stopped_old, Some(old_done));
    let new = async {
        going.await.unwrap();
        shape.run(new_told, stopped_new, None).await
    };
    let driving = async {
        let at = old_bound.await.unwrap().s3;
        work(Pair {
            at,
            go: Some(go),
            new_bound: Some(new_bound),
            old_ended,
            stop_new,
        })
        .await;
        // Held until `work` is over: the first stops when a successor takes its sockets.
        drop(stop_old);
    };
    let (old, new, ()) = tokio::join!(old, new, driving);
    assert_eq!(old.unwrap(), Drained { abandoned: 0 });
    assert_eq!(new.unwrap(), Drained { abandoned: 0 });
}

/// The two shapes that take writes.
async fn writing_shapes(name: &str) -> [Shape; 2] {
    [
        Shape::build(&format!("{name}-b"), true, QUICK),
        Shape::storage(&format!("{name}-s"), QUICK).await,
    ]
}

/// Asserts (T11, bytes) that a PUT of which the first instance has read half when the second
/// takes the sockets is finished on the same connection after the take, for a build node that
/// writes through and for a storage node: the answer is 200 and the body is in the upstream
/// byte for byte after both have ended. Catches: a take that cuts a request in flight or
/// stores a torn body.
#[tokio::test]
async fn a_put_half_sent_before_the_take_is_stored_whole_when_it_completes_after_it() {
    for shape in writing_shapes("tscache-traffic-put").await {
        let body = pattern(300 * 1024);
        let put = wire("PUT", &obj("cas/half"), &body);
        let work = |mut pair: Pair| async move {
            let mut conn = Conn::served(pair.at, false).await;
            conn.begin(&put, put.body.len() / 2).await;
            pair.take().await;
            conn.send(&put.body[put.body.len() / 2..]).await;
            let answer = conn.answer().await;
            assert_eq!(answer.head.status, 200, "{}", answer.text());
            drop(conn);
            pair.first_ends().await;
            pair.stop();
        };
        overlap(&shape, work).await;
        let stored = shape.stored("cas/half").await.unwrap();
        assert_eq!(digest(&stored), digest(&body));
    }
}

/// Asserts (T11, bytes) that a streamed GET of several hundred KiB, of an object the cache holds
/// (a hit) and of one it does not (a miss, which fills), read in part before the take and to the
/// end after it, gives the object byte for byte (compared by hash) with no reset, for a build
/// node and for a storage node; and that the second instance, on a connection known to be its
/// own, gives the same bytes for both objects. Catches: a take that cuts an answer being written
/// or tears the file a hit is read from.
#[tokio::test]
async fn a_streamed_get_of_a_hit_and_of_a_miss_is_whole_across_the_take() {
    let shapes = [
        Shape::build("tscache-traffic-get-b", false, QUICK),
        Shape::build("tscache-traffic-get-w", true, QUICK),
        Shape::storage("tscache-traffic-get-s", QUICK).await,
    ];
    for shape in shapes {
        let hit = pattern(300 * 1024);
        let miss = pattern(340 * 1024 + 11);
        shape.seed("cas/hit", &hit).await;
        shape.seed("cas/miss", &miss).await;
        let (hit, miss) = (&hit, &miss);
        let work = |mut pair: Pair| async move {
            let at = pair.at;
            let mut plain = Conn::served(at, false).await;
            let ask = wire("GET", &obj("cas/hit"), b"");
            for _ in 0..2 {
                let answer = plain.call(&ask).await;
                assert_eq!(digest(&answer.body), digest(hit));
            }
            let (status, text) = probe(at, "/metricz").await;
            assert_eq!(status, 200);
            assert!(text.contains("tierstore_cache_hits_total 1\n"), "{text}");
            drop(plain);

            let first = 64 * 1024;
            let mut held = Vec::new();
            for (key, whole) in [("cas/hit", hit), ("cas/miss", miss)] {
                let mut conn = Conn::served(at, true).await;
                conn.send(&wire("GET", &obj(key), b"").head).await;
                let head = conn.head().await;
                assert_eq!(head.status, 200);
                assert_eq!(head.length(), whole.len());
                let before = conn.take(first).await;
                held.push((conn, before, whole));
            }
            pair.take().await;
            for (mut conn, mut got, whole) in held {
                got.extend(conn.take(whole.len() - first).await);
                assert_eq!(digest(&got), digest(whole));
            }

            let (mut second, said) = successor(at).await;
            assert_eq!(said, "ready read-only");
            for (key, whole) in [("cas/hit", hit), ("cas/miss", miss)] {
                let answer = second.call(&wire("GET", &obj(key), b"")).await;
                assert_eq!(answer.head.status, 200);
                assert_eq!(digest(&answer.body), digest(whole));
            }
            drop(second);
            pair.first_ends().await;
            pair.stop();
        };
        overlap(&shape, work).await;
    }
}

/// The upload id that a create of `key` on `conn` gives.
async fn created(conn: &mut Conn, key: &str) -> String {
    let answer = conn
        .call(&wire("POST", &obj(&format!("{key}?uploads")), b""))
        .await;
    assert_eq!(answer.head.status, 200, "{}", answer.text());
    texts(&answer.text(), "UploadId").concat()
}

/// The uri of part `number` of upload `id` of `key`.
fn part_uri(key: &str, id: &str, number: u32) -> String {
    obj(&format!("{key}?partNumber={number}&uploadId={id}"))
}

/// Uploads a part on `conn` and gives its ETag.
async fn uploaded(conn: &mut Conn, key: &str, id: &str, number: u32, body: &[u8]) -> String {
    let answer = conn
        .call(&wire("PUT", &part_uri(key, id, number), body))
        .await;
    assert_eq!(answer.head.status, 200, "{}", answer.text());
    answer.head.header("etag").unwrap().to_owned()
}

/// The ids of the uploads that a listing on `conn` gives.
async fn open_uploads(conn: &mut Conn) -> Vec<String> {
    let answer = conn
        .call(&wire("GET", &format!("/{BUCKET}?uploads"), b""))
        .await;
    assert_eq!(answer.head.status, 200, "{}", answer.text());
    texts(&answer.text(), "UploadId")
}

/// Asserts (T11, multipart) that an upload created and given its first part on the first
/// instance goes on across the take, for a build node that writes through and for a storage
/// node: a third part of which half was written before the take is finished after it, the second
/// part and the completion on a connection of the second instance (which is read-only) are
/// answered 200 with no `NoSuchUpload`, the object read back is the three parts joined, and
/// after both end the upstream holds it byte for byte; and an upload created before the take
/// and aborted on the second instance is gone (a listing no longer names it, a part and a
/// completion of it are `NoSuchUpload`) and leaves no object. Catches: parts that the second
/// instance cannot see, and an abort that does not remove the upload.
#[tokio::test]
async fn a_multipart_upload_goes_on_across_the_take_and_an_abort_removes_it() {
    for shape in writing_shapes("tscache-traffic-multi").await {
        let one = pattern(120 * 1024);
        let two = pattern(90 * 1024 + 5);
        let three = pattern(60 * 1024 + 9);
        let joined = [&one[..], &two[..], &three[..]].concat();
        let work = |mut pair: Pair| {
            let (one, two, three, joined) =
                (one.clone(), two.clone(), three.clone(), joined.clone());
            async move {
                let at = pair.at;
                let mut first = Conn::served(at, false).await;
                let id = created(&mut first, "cas/multi").await;
                let etag_one = uploaded(&mut first, "cas/multi", &id, 1, &one).await;
                let gone = created(&mut first, "cas/aborted").await;
                uploaded(&mut first, "cas/aborted", &gone, 1, &one).await;
                let mut in_flight = Conn::served(at, false).await;
                let third = wire("PUT", &part_uri("cas/multi", &id, 3), &three);
                in_flight.begin(&third, three.len() / 2).await;
                pair.take().await;

                let (mut second, said) = successor(at).await;
                assert_eq!(said, "ready read-only");
                let etag_two = uploaded(&mut second, "cas/multi", &id, 2, &two).await;
                in_flight.send(&third.body[three.len() / 2..]).await;
                let sent = in_flight.answer().await;
                assert_eq!(sent.head.status, 200, "{}", sent.text());
                let etag_three = sent.head.header("etag").unwrap().to_owned();

                let delete = wire("DELETE", &obj(&format!("cas/aborted?uploadId={gone}")), b"");
                assert_eq!(second.call(&delete).await.head.status, 204);
                let mut listed = open_uploads(&mut second).await;
                listed.sort();
                assert_eq!(listed, vec![id.clone()]);
                let stray = second
                    .call(&wire("PUT", &part_uri("cas/aborted", &gone, 2), &two))
                    .await;
                assert_eq!(stray.head.status, 404, "{}", stray.text());
                assert_eq!(texts(&stray.text(), "Code"), ["NoSuchUpload"]);
                let parts = completion(&[(1, &etag_one), (2, &etag_two), (3, &etag_three)]);
                let finish = wire(
                    "POST",
                    &obj(&format!("cas/aborted?uploadId={gone}")),
                    completion(&[(1, &etag_one)]).as_bytes(),
                );
                let refused = second.call(&finish).await;
                assert_eq!(refused.head.status, 404, "{}", refused.text());
                assert_eq!(texts(&refused.text(), "Code"), ["NoSuchUpload"]);

                let finish = wire(
                    "POST",
                    &obj(&format!("cas/multi?uploadId={id}")),
                    parts.as_bytes(),
                );
                let done = second.call(&finish).await;
                assert_eq!(done.head.status, 200, "{}", done.text());
                assert_eq!(texts(&done.text(), "Key"), ["cas/multi"]);
                assert!(open_uploads(&mut second).await.is_empty());
                let read = second.call(&wire("GET", &obj("cas/multi"), b"")).await;
                assert_eq!(digest(&read.body), digest(&joined));
                drop((first, in_flight, second));
                pair.first_ends().await;
                pair.stop();
            }
        };
        overlap(&shape, work).await;
        let stored = shape.stored("cas/multi").await.unwrap();
        assert_eq!(digest(&stored), digest(&joined));
        assert_eq!(shape.stored("cas/aborted").await, None);
    }
}

/// Asserts (T11, `write_through`) that every PUT a node that writes through acknowledges with
/// 200 is in the upstream after both instances have ended, for a build node with `write_through`
/// and for a storage node: one acknowledged by the first instance before the take, one
/// acknowledged by the second while it is read-only (`ready read-only`), and one acknowledged by
/// the second once it has the cache directory (`ready read-write`); each holds exactly the
/// bytes sent. Catches: an acknowledged write that is lost across the swap, in the overlap, or
/// at the flip from read-only to read-write.
#[tokio::test]
async fn every_acknowledged_put_of_a_node_that_writes_through_is_in_the_upstream_after_both_end() {
    for shape in writing_shapes("tscache-traffic-wthru").await {
        let bodies = [pattern(70_000), pattern(70_001), pattern(70_002)];
        let work = |mut pair: Pair| {
            let bodies = bodies.clone();
            async move {
                let at = pair.at;
                let mut first = Conn::served(at, false).await;
                let put = |n: usize| wire("PUT", &obj(&format!("cas/ack-{n}")), &bodies[n]);
                assert_eq!(first.call(&put(0)).await.head.status, 200);
                drop(first);
                pair.take().await;
                let (mut second, said) = successor(at).await;
                assert_eq!(said, "ready read-only");
                let answer = second.call(&put(1)).await;
                assert_eq!(answer.head.status, 200, "{}", answer.text());
                pair.first_ends().await;
                let (mut second_rw, said) = successor(at).await;
                assert_eq!(said, "ready read-write");
                let answer = second_rw.call(&put(2)).await;
                assert_eq!(answer.head.status, 200, "{}", answer.text());
                drop((second, second_rw));
                pair.stop();
            }
        };
        overlap(&shape, work).await;
        for (n, body) in bodies.iter().enumerate() {
            let stored = shape.stored(&format!("cas/ack-{n}")).await;
            let stored = stored.unwrap_or_else(|| panic!("ack-{n} is not in the upstream"));
            assert_eq!(digest(&stored), digest(body), "ack-{n}");
        }
    }
}

/// Asserts (T11, `write_through` off) that a build node that is a read cache refuses a PUT and
/// the create of a multipart upload with 403 `AccessDenied` and the words "This tierstore is a
/// read cache: it does not accept writes; send them to the storage endpoint it reads from.", on
/// the first instance before the take and on both instances after it (a connection of the first
/// that was answered before the take, and one of the second, which is read-only), and nothing is
/// stored in the upstream; reads go on being served. Catches: a successor that takes writes
/// the first refused, and a write that reaches the upstream.
#[tokio::test]
async fn a_read_cache_refuses_writes_on_both_instances_and_stores_nothing() {
    let shape = Shape::build("tscache-traffic-readonly", false, QUICK);
    assert!(!shape.writes);
    shape.seed("cas/read", b"readable").await;
    let words = "This tierstore is a read cache: it does not accept writes; send them to the \
                 storage endpoint it reads from.";
    let work = |mut pair: Pair| async move {
        let at = pair.at;
        let put = wire("PUT", &obj("cas/refused"), b"refused");
        let create = wire("POST", &obj("cas/refused-upload?uploads"), b"");
        let mut refusals = Vec::new();
        for request in [&put, &create] {
            let mut conn = Conn::served(at, false).await;
            refusals.push(conn.call(request).await);
        }
        // A PUT of which the first instance has the head of when the second takes the sockets.
        let mut held = Conn::served(at, false).await;
        held.send(&put.head[..16]).await;
        pair.take().await;
        held.send(&put.head[16..]).await;
        held.send(&put.body).await;
        refusals.push(held.answer().await);
        for request in [&put, &create] {
            let (mut second, said) = successor(at).await;
            assert_eq!(said, "ready read-only");
            refusals.push(second.call(request).await);
        }
        for refusal in &refusals {
            assert_eq!(refusal.head.status, 403, "{}", refusal.text());
            assert_eq!(texts(&refusal.text(), "Code"), ["AccessDenied"]);
            assert_eq!(texts(&refusal.text(), "Message"), [words]);
        }
        let (mut reader, _) = successor(at).await;
        let read = reader.call(&wire("GET", &obj("cas/read"), b"")).await;
        assert_eq!(read.body, b"readable");
        drop((held, reader));
        pair.first_ends().await;
        pair.stop();
    };
    overlap(&shape, work).await;
    assert_eq!(shape.stored("cas/refused").await, None);
    assert_eq!(
        shape.stored("cas/read").await.as_deref(),
        Some(&b"readable"[..])
    );
    let Kind::Build(_, upstream) = &shape.kind else {
        unreachable!("a build node");
    };
    assert_eq!(upstream.list("").await.unwrap().len(), 1);
}

/// Asserts (T11, keep-alive) that an answer on a keep-alive connection of the first instance
/// after the take says `Connection: close` and is the last on it, for a build node and a storage
/// node: a connection that was answered `/healthz` without `Connection: close` while the first
/// served, and has the head of its next request written when the second takes the sockets, is
/// answered (200) after the first is seen to drain, with `connection: close`, and then ends.
/// The settle is long here, since a connection with no request on the wire is closed by the
/// first instance at the end of the settle; the assertion does not depend on how long it lasts.
/// Catches: a draining instance that keeps a connection alive for the next request.
#[tokio::test]
async fn a_keep_alive_connection_of_the_draining_instance_is_answered_with_connection_close() {
    let long = settle(Duration::from_secs(2));
    let shapes = [
        Shape::build("tscache-traffic-ka-b", false, long),
        Shape::storage("tscache-traffic-ka-s", long).await,
    ];
    for shape in shapes {
        let work = |mut pair: Pair| async move {
            let at = pair.at;
            let mut conn = Conn::open(at).await;
            let before = conn.ask(HEALTHZ).await;
            assert_eq!(before.head.status, 200);
            assert_eq!(before.head.header("connection"), None);
            conn.send(&HEALTHZ[..16]).await;
            pair.take().await;
            draining(at).await;
            conn.send(&HEALTHZ[16..]).await;
            let after = conn.answer().await;
            assert_eq!(after.head.status, 200);
            assert_eq!(after.head.header("connection"), Some("close"));
            assert!(conn.ended().await, "the connection ends after the answer");
            drop(conn);
            pair.first_ends().await;
            pair.stop();
        };
        overlap(&shape, work).await;
    }
}

/// Asserts that the first instance's `/readyz` is `draining` while a client's request is held
/// and the second's is `ready read-only`: the overlap the other tests hold open is real. Catches:
/// tests that pass because the second instance never overlapped the first.
#[tokio::test]
async fn the_overlap_the_traffic_tests_hold_is_a_real_one() {
    let long = settle(Duration::from_secs(2));
    let shapes = [
        Shape::build("tscache-traffic-overlap-b", true, long),
        Shape::storage("tscache-traffic-overlap-s", long).await,
    ];
    for shape in shapes {
        let work = |mut pair: Pair| async move {
            let at = pair.at;
            let mut held = Conn::served(at, false).await;
            held.send(&HEALTHZ[..16]).await;
            pair.take().await;
            draining(at).await;
            assert_eq!(successor(at).await.1, "ready read-only");
            held.send(&HEALTHZ[16..]).await;
            assert_eq!(held.answer().await.head.status, 200);
            drop(held);
            pair.first_ends().await;
            pair.stop();
        };
        overlap(&shape, work).await;
    }
}
