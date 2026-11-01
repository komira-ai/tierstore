//! The drain of the OCI listener over loopback sockets, watched from inside: each test waits for
//! the events of the drain it depends on (the listener settled, a connection lingering, a late
//! request read, a connection slot given back), as the listener's drain and slots show them,
//! instead of waiting for a time. The same behaviour through the binary is in
//! `tests/oci_front/drain.rs`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::listener::{OciLimits, Shared};
use super::{OciFront, OciListener, OciMetrics, OciPrefix};
use crate::server::drain::SETTLED;
use crate::{Backend as _, MemoryBackend};

/// The longest a test waits for the listener to do what it waits for. It only stops a test that
/// hangs, and then the test fails.
const SAFETY: Duration = Duration::from_secs(60);

/// What a client sends to ask `/v2/`, keeping the connection alive.
const PING: &[u8] = b"GET /v2/ HTTP/1.1\r\nhost: oci.test\r\n\r\n";

/// The bytes of a blob a client leaves unread until the listener has written the whole answer:
/// more than the client's small receive buffer holds, few enough for the server's send buffer.
const TAIL: usize = 64 << 10;

/// A plain listener on loopback, serving on a task, watched through its drain and slots.
struct Front {
    addr: SocketAddr,
    backend: Arc<MemoryBackend>,
    metrics: Arc<OciMetrics>,
    shared: Shared,
    max_connections: usize,
    stop: watch::Sender<bool>,
    serving: JoinHandle<usize>,
}

impl Front {
    /// A listener whose limits are long ones (no test meets them unless it sets one), changed by
    /// `change`.
    async fn start(change: impl FnOnce(&mut OciLimits)) -> Self {
        let long = Duration::from_secs(3600);
        let mut limits = OciLimits {
            max_connections: 16,
            handshake_timeout: long,
            header_timeout: long,
            write_idle: long,
            lifetime: long,
            drain_deadline: long,
        };
        change(&mut limits);
        let backend = Arc::new(MemoryBackend::new());
        let metrics = Arc::new(OciMetrics::default());
        let prefix = OciPrefix::new("oci/").expect("a prefix");
        let front = Arc::new(OciFront::new(backend.clone(), prefix, metrics.clone()));
        let addr: SocketAddr = "127.0.0.1:0".parse().expect("an address");
        let listener = OciListener::bind_plain(addr, front, limits).await;
        let listener = listener.expect("bound");
        let (addr, shared) = (
            listener.local_addr().expect("an address"),
            listener.shared(),
        );
        let (stop, stopped) = watch::channel(false);
        let serving = tokio::spawn(listener.serve(stopped));
        Self {
            addr,
            backend,
            metrics,
            shared,
            max_connections: limits.max_connections,
            stop,
            serving,
        }
    }

    /// Stores `content` under the digest `hex`, whatever it hashes to; the path of its blob.
    async fn put(&self, hex: &str, content: &[u8]) -> String {
        let key = format!("oci/blobs/sha256/{hex}");
        let put = self
            .backend
            .put(&key, Bytes::copy_from_slice(content))
            .await;
        put.expect("put");
        format!("/v2/a/blobs/sha256:{hex}")
    }

    /// Resolves once `holds` is true of the listener; `what` names it in the failure.
    async fn until(&self, what: &str, holds: impl Fn(&Shared) -> bool) {
        let waiting = async {
            while !holds(&self.shared) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        let seen = tokio::time::timeout(SAFETY, waiting).await;
        seen.unwrap_or_else(|_| panic!("never seen: {what}"));
    }

    /// Ends the settle, the drain begun with it, and returns once the listener has settled.
    async fn settle(&self) {
        self.stop.send(true).expect("serving");
        self.until("the settle over", |s| s.drain.phase() == SETTLED)
            .await;
    }

    /// Resolves once every connection slot is free: no connection is open or lingering, and the
    /// accept loop, which has ended, holds none.
    async fn slots_free(&self) {
        let max = self.max_connections;
        self.until("every slot free", |s| s.slots.available_permits() == max)
            .await;
    }

    /// The connections the listener cut at the drain deadline.
    async fn cut(self) -> usize {
        let ended = tokio::time::timeout(SAFETY, self.serving).await;
        ended.expect("the listener never ended").expect("joined")
    }
}

/// Reads the head of an answer: the head in lower case, and the bytes of the body that came with
/// it.
async fn read_head(tcp: &mut TcpStream) -> (String, Vec<u8>) {
    let mut taken = Vec::new();
    let mut piece = [0_u8; 16 * 1024];
    loop {
        if let Some(at) = taken.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&taken[..at]).to_ascii_lowercase();
            return (head, taken[at + 4..].to_vec());
        }
        let read = tcp.read(&mut piece).await.expect("a read");
        assert!(read > 0, "the connection ended inside the head");
        taken.extend_from_slice(&piece[..read]);
    }
}

/// Reads one answer whole, head and body (its `content-length`); the head in lower case.
async fn read_answer(tcp: &mut TcpStream) -> String {
    let (head, mut body) = read_head(tcp).await;
    let length = content_length(&head);
    let mut piece = [0_u8; 1024];
    while body.len() < length {
        let read = tcp.read(&mut piece).await.expect("a read");
        assert!(read > 0, "the connection ended inside the answer");
        body.extend_from_slice(&piece[..read]);
    }
    head
}

fn content_length(head: &str) -> usize {
    let line = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"));
    line.expect("a content-length")
        .trim()
        .parse()
        .expect("a number")
}

/// A client socket with a small receive buffer, which the kernel does not grow, connected to
/// `addr`: the client decides how much of an answer the server can have sent.
async fn narrow(addr: SocketAddr) -> TcpStream {
    let socket = TcpSocket::new_v4().expect("a socket");
    socket.set_recv_buffer_size(4096).expect("a receive buffer");
    socket.connect(addr).await.expect("connected")
}

/// Content with no two chunks alike, so that a lost piece shows.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|at| (at % 251) as u8).collect()
}

/// Asserts the order that a plain close gets wrong: the settle ends while a blob is being sent,
/// the listener writes the whole answer (seen as the connection lingering: the server has shut its
/// end, after the last byte) while the end of it is still unread, and only then does the client
/// send its next request. The listener reads that request (seen as the late count) and does not
/// reset the connection, so the client reads the whole blob and a clean end of stream; the
/// listener ends with nothing cut once the client has closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_request_after_the_whole_blob_is_written_does_not_cut_it() {
    let front = Front::start(|_| {}).await;
    let content = pattern(4 << 20);
    let hex = hex::encode(Sha256::digest(&content));
    let path = front.put(&hex, &content).await;
    let mut tcp = narrow(front.addr).await;
    let ask = format!("GET {path} HTTP/1.1\r\nhost: oci.test\r\n\r\n");
    tcp.write_all(ask.as_bytes()).await.expect("sent");
    let (head, mut body) = read_head(&mut tcp).await;
    assert!(head.starts_with("http/1.1 200 "), "{head}");
    let length = content_length(&head);
    assert_eq!(length, content.len());

    front.shared.drain.begin();
    front.settle().await;
    let mut piece = vec![0_u8; 16 << 10];
    while body.len() < length - TAIL {
        let want = piece.len().min(length - TAIL - body.len());
        let read = tcp.read(&mut piece[..want]).await.expect("a read");
        assert!(read > 0, "the connection ended at {} bytes", body.len());
        body.extend_from_slice(&piece[..read]);
    }
    front
        .until("the whole answer written", |s| s.drain.lingering() == 1)
        .await;
    tcp.write_all(PING)
        .await
        .expect("the next request is taken");
    front
        .until("the late request read", |s| s.drain.late() == 1)
        .await;
    let mut rest = Vec::new();
    let read = tcp.read_to_end(&mut rest).await;
    assert!(read.is_ok(), "the connection was reset: {read:?}");
    body.extend_from_slice(&rest);
    assert_eq!(
        body.len(),
        length,
        "the blob is whole, and nothing follows it"
    );
    assert!(body == content, "the bytes of the blob differ");
    assert_eq!(front.metrics.late_requests(), 1);
    drop(tcp);
    assert_eq!(front.cut().await, 0);
}

/// Asserts that an idle connection gets a lingering close at the end of the settle: a clean end of
/// stream, then a request the client writes is read (seen as the late count) and does not reset the
/// connection (no error on the socket once the listener has read it, and the next write is
/// taken), and the listener holds the connection until the client closes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_connection_lingers_and_reads_what_comes_late() {
    let front = Front::start(|_| {}).await;
    let mut tcp = TcpStream::connect(front.addr).await.expect("connected");
    tcp.write_all(PING).await.expect("sent");
    let head = read_answer(&mut tcp).await;
    assert!(!head.contains("connection: close"), "{head}");

    front.shared.drain.begin();
    front.settle().await;
    let mut piece = [0_u8; 64];
    assert_eq!(tcp.read(&mut piece).await.expect("a clean end"), 0);
    front.until("lingering", |s| s.drain.lingering() == 1).await;
    tcp.write_all(PING)
        .await
        .expect("the late request is taken");
    front
        .until("the late request read", |s| s.drain.late() == 1)
        .await;
    let error = tcp.take_error().expect("the socket's error");
    assert!(error.is_none(), "the connection was reset: {error:?}");
    tcp.write_all(PING).await.expect("still taken");
    // The listener cannot end while the connection lingers, and it still does.
    assert_eq!(front.shared.drain.lingering(), 1);
    assert!(!front.serving.is_finished());

    tcp.shutdown().await.expect("closed");
    front
        .until("the linger over", |s| s.drain.lingering() == 0)
        .await;
    assert_eq!(front.metrics.late_requests(), 1, "once for the connection");
    assert_eq!(front.cut().await, 0);
}

/// Asserts that a connection that lingers holds its connection slot: with room for one connection,
/// once the first lingers the slot is taken (no permit free while the accept loop waits for one),
/// so a second connection, made then, is not served; when the first client closes, the slot is
/// given back and the second is served, with `Connection: close` (the drain has begun).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lingering_connection_holds_its_slot() {
    let front = Front::start(|limits| limits.max_connections = 1).await;
    let mut first = TcpStream::connect(front.addr).await.expect("connected");
    first.write_all(PING).await.expect("sent");
    read_answer(&mut first).await;

    front.shared.drain.begin();
    first.write_all(PING).await.expect("sent");
    let head = read_answer(&mut first).await;
    assert!(head.contains("connection: close"), "{head}");
    let mut piece = [0_u8; 64];
    assert_eq!(first.read(&mut piece).await.expect("a clean end"), 0);
    front.until("lingering", |s| s.drain.lingering() == 1).await;
    // The only slot is the lingering connection's: the accept loop waits for it.
    assert_eq!(front.shared.slots.available_permits(), 0);

    let mut second = TcpStream::connect(front.addr).await.expect("connected");
    second.write_all(PING).await.expect("sent");
    // Not accepted, so not answered, while the first holds the slot.
    let early = second.try_read(&mut piece);
    let blocked = matches!(&early, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock);
    assert!(
        blocked,
        "the second was answered while the first lingered: {early:?}"
    );
    assert_eq!(front.shared.drain.lingering(), 1);

    first.shutdown().await.expect("closed");
    drop(first);
    let head = read_answer(&mut second).await;
    assert!(head.starts_with("http/1.1 200 "), "{head}");
    assert!(head.contains("connection: close"), "{head}");
    drop(second);
    front.settle().await;
    front.slots_free().await;
    assert_eq!(front.cut().await, 0);
}

/// Asserts that a blob whose body fails while the drain is on (here an object that is not its
/// digest, which ends the body in an error before its last bytes) is cut short for the client, and
/// that its connection lets go of everything: its slot is given back, nothing lingers, and the
/// listener ends with nothing cut, long before its drain deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blob_that_fails_in_the_drain_frees_its_connection() {
    let front = Front::start(|_| {}).await;
    // Several chunks, so that the head and the first ones go out before the check at the last.
    let content = pattern(16 << 20);
    let hex = hex::encode(Sha256::digest(b"not these bytes"));
    let path = front.put(&hex, &content).await;
    let mut tcp = narrow(front.addr).await;
    let ask = format!("GET {path} HTTP/1.1\r\nhost: oci.test\r\n\r\n");
    tcp.write_all(ask.as_bytes()).await.expect("sent");
    let (head, mut body) = read_head(&mut tcp).await;
    assert!(head.starts_with("http/1.1 200 "), "{head}");

    front.shared.drain.begin();
    front.settle().await;
    let mut rest = Vec::new();
    let _ = tcp.read_to_end(&mut rest).await;
    body.extend_from_slice(&rest);
    assert!(body.len() < content.len(), "a failed blob was sent whole");
    front.slots_free().await;
    assert_eq!(front.shared.drain.lingering(), 0);
    assert_eq!(front.metrics.mismatches(), 1);
    drop(tcp);
    assert_eq!(front.cut().await, 0);
}

/// Asserts that a client that goes away in the middle of a blob while the drain is on frees its
/// connection: the slot is given back, nothing lingers, and the listener ends with nothing cut,
/// long before its drain deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_leaves_in_the_drain_frees_its_connection() {
    let front = Front::start(|_| {}).await;
    let content = pattern(4 << 20);
    let hex = hex::encode(Sha256::digest(&content));
    let path = front.put(&hex, &content).await;
    let mut tcp = narrow(front.addr).await;
    let ask = format!("GET {path} HTTP/1.1\r\nhost: oci.test\r\n\r\n");
    tcp.write_all(ask.as_bytes()).await.expect("sent");
    let (head, _) = read_head(&mut tcp).await;
    assert!(head.starts_with("http/1.1 200 "), "{head}");

    front.shared.drain.begin();
    front.settle().await;
    // Most of the blob unread: the close resets the connection under the server's writes.
    drop(tcp);
    front.slots_free().await;
    assert_eq!(front.shared.drain.lingering(), 0);
    assert_eq!(front.cut().await, 0);
}

/// Asserts that a lingering close ends at its bound, the header timeout, when that is shorter
/// than the drain deadline and the client keeps its connection open: the connection is closed then
/// (its slot given back, nothing lingering), and the listener ends with nothing cut instead of
/// waiting for the deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lingering_close_ends_at_its_bound_before_the_deadline() {
    let front = Front::start(|limits| limits.header_timeout = Duration::from_millis(200)).await;
    let mut tcp = TcpStream::connect(front.addr).await.expect("connected");
    tcp.write_all(PING).await.expect("sent");
    read_answer(&mut tcp).await;

    front.settle().await;
    let mut piece = [0_u8; 64];
    assert_eq!(tcp.read(&mut piece).await.expect("a clean end"), 0);
    // The client keeps its end open: the bound, not the client, ends the linger.
    front.slots_free().await;
    assert_eq!(front.shared.drain.lingering(), 0);
    assert_eq!(front.cut().await, 0);
    drop(tcp);
}
