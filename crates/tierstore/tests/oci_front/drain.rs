//! The OCI front drains as the S3 front does (`tests/s3_front/drain.rs`): from the start of the
//! drain every answer carries `Connection: close`, an answer that is being sent when the settle
//! ends is sent in full, an idle connection gets a lingering close at the end of the settle (a
//! clean end of stream, then the front reads what the client still sends until it closes), and a
//! request that arrives after that end is counted as late. The first three run the server as the
//! binary does, through `run_serving` and its stop; the count is read off the listener's counters,
//! since the S3 listener that shows `/metricz` has stopped taking connections by then.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use clap::Parser;
use objstore_s3::{ObjectKey, ObjectStore};
use tierstore::server::Drained;
use tierstore::server::cli::{Addresses, Cli, run_serving};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::binary::{files, store};
use crate::client::{Conn, connector};
use crate::limits::{SAFETY, SHORT};
use crate::rig::{Rig, hex_of};

/// What a client sends to ask `/v2/`, keeping the connection alive.
const PING: &[u8] = b"GET /v2/ HTTP/1.1\r\nhost: oci.test\r\n\r\n";

/// How often a test looks again at whether the server has done what it waits for.
const POLL: Duration = Duration::from_millis(10);

/// A server run in this process as the binary runs it, with an OCI front: on a task of this
/// thread (the future of `run_serving` is not `Send`), so in [`local`].
struct Running {
    bound: Addresses,
    stop: Option<oneshot::Sender<()>>,
    run: JoinHandle<Drained>,
}

impl Running {
    /// Starts the server over `flags`, with `--drain-lb-wait` set to `lb_wait`.
    async fn start(mut flags: Vec<String>, lb_wait: &str) -> Self {
        let at = flags.iter().position(|flag| flag == "--drain-lb-wait");
        flags[at.expect("the flag") + 1] = lb_wait.to_owned();
        let cli = Cli::try_parse_from(flags).expect("flags");
        let (stop, stopped) = oneshot::channel::<()>();
        let (told, bound) = oneshot::channel::<Addresses>();
        let run = tokio::task::spawn_local(async move {
            let stopped = async move {
                let _ = stopped.await;
            };
            let ready = move |addresses| {
                let _ = told.send(addresses);
            };
            run_serving(&cli, stopped, ready).await.expect("a drain")
        });
        let bound = bound.await.expect("bound");
        Self {
            bound,
            stop: Some(stop),
            run,
        }
    }

    fn oci(&self) -> SocketAddr {
        self.bound.oci.expect("the OCI front is bound")
    }

    /// Stores `content` where its digest names it, through the S3 front; the path of its blob.
    async fn seed(&self, content: &[u8]) -> String {
        let hex = hex_of(content);
        let key = ObjectKey::new(format!("oci/blobs/sha256/{hex}")).expect("a key");
        let bytes = Bytes::copy_from_slice(content);
        let put = store(self.bound.s3).put_new(&key, bytes, None).await;
        put.expect("put");
        format!("/v2/library/app/blobs/sha256:{hex}")
    }

    /// Stops the server as SIGTERM does: the drain begins.
    fn begin_drain(&mut self) {
        let stop = self.stop.take().expect("not stopped yet");
        stop.send(()).expect("running");
    }

    /// Begins the drain and returns once `/readyz` says 503.
    async fn begin_drain_and_see_it(&mut self) {
        self.begin_drain();
        let url = format!("http://{}/readyz", self.bound.s3);
        let draining = async {
            while reqwest::get(&url).await.expect("readyz").status() == 200 {
                tokio::time::sleep(POLL).await;
            }
        };
        let seen = tokio::time::timeout(SAFETY, draining).await;
        seen.expect("the drain never began");
    }

    /// Returns once the OCI front takes no new connection: the settle is over.
    async fn settled(&self) {
        let oci = self.oci();
        let settling = async {
            while TcpStream::connect(oci).await.is_ok() {
                tokio::time::sleep(POLL).await;
            }
        };
        let over = tokio::time::timeout(SAFETY, settling).await;
        over.expect("the settle never ended");
    }

    /// How the drain ended.
    async fn drained(self) -> Drained {
        let ended = tokio::time::timeout(SAFETY, self.run).await;
        ended.expect("the drain never ended").expect("joined")
    }
}

/// Runs `test` where [`Running::start`] can put the server on a task of its own.
async fn local(test: impl std::future::Future<Output = ()>) {
    tokio::task::LocalSet::new().run_until(test).await;
}

/// Reads the head of an answer from `tcp`: the head, and the bytes of the body that came with it.
async fn read_head(tcp: &mut TcpStream) -> (String, Vec<u8>) {
    let mut taken = Vec::new();
    let mut piece = [0_u8; 16 * 1024];
    loop {
        if let Some(at) = taken.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&taken[..at]).to_ascii_lowercase();
            return (head, taken[at + 4..].to_vec());
        }
        let read = tcp.read(&mut piece).await.expect("a read");
        assert!(
            read > 0,
            "the connection ended inside the head of the answer"
        );
        taken.extend_from_slice(&piece[..read]);
    }
}

/// The `content-length` of a head that [`read_head`] read.
fn content_length(head: &str) -> usize {
    let line = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"));
    line.expect("a content-length")
        .trim()
        .parse()
        .expect("a number")
}

/// Asserts, over plain HTTP and over HTTPS, that once the drain has begun (`/readyz` says 503)
/// every answer of the OCI front carries `Connection: close`: on a kept-alive connection that was
/// served before the drain without it, and on new connections in the settle, for the ping, a blob
/// (whole, and its head), a blob that is not there, a tag and a write; and that each of those
/// connections then ends with a clean end of stream and the drain with nothing cut. It would catch
/// a front that says nothing of the drain until the settle is over, so that its clients send their
/// next request on a connection that is about to close.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn from_the_start_of_the_drain_every_answer_says_connection_close() {
    local(async {
        tokio::join!(
            every_answer_says_close(false),
            every_answer_says_close(true)
        );
    })
    .await;
}

async fn every_answer_says_close(tls: bool) {
    let files = files(if tls {
        "oci-drain-close-tls"
    } else {
        "oci-drain-close-plain"
    });
    let flags = if tls {
        files.flags(true)
    } else {
        files.plain("127.0.0.1:0")
    };
    // Long enough for every request below to be made within it.
    let mut running = Running::start(flags, "5s").await;
    let (oci, connector) = (running.oci(), connector(&files.ca_pem));
    let open = || async {
        let opened = match tls {
            true => Conn::open(oci, &connector).await,
            false => Conn::open_plain(oci).await,
        };
        opened.expect("a connection")
    };
    let content = b"a layer pulled during a drain".repeat(100);
    let blob = running.seed(&content).await;
    let missing = format!("/v2/library/app/blobs/sha256:{}", hex_of(b"not stored"));
    let mut kept = open().await;
    let before = kept.request("GET", "/v2/", &[]).await.expect("an answer");
    assert_eq!((before.status, before.header("connection")), (200, None));

    running.begin_drain_and_see_it().await;
    let after = kept.request("GET", "/v2/", &[]).await.expect("an answer");
    assert_eq!(after.status, 200);
    assert_eq!(after.header("connection"), Some("close"), "{after:?}");
    assert!(kept.read_to_end().await.expect("a clean end").is_empty());
    let asked = [
        ("GET", "/v2/", 200),
        ("GET", blob.as_str(), 200),
        ("HEAD", blob.as_str(), 200),
        ("GET", missing.as_str(), 404),
        ("GET", "/v2/library/app/manifests/latest", 404),
        ("PUT", blob.as_str(), 405),
    ];
    for (method, path, status) in asked {
        let mut conn = open().await;
        let reply = conn.request(method, path, &[]).await.expect("an answer");
        assert_eq!(reply.status, status, "{method} {path}: {reply:?}");
        let close = reply.header("connection");
        assert_eq!(close, Some("close"), "{method} {path}: {reply:?}");
        if (method, status) == ("GET", 200) && path == blob {
            assert!(reply.body == content, "{method} {path}: the bytes differ");
        }
        let end = conn.read_to_end().await;
        assert!(end.expect("a clean end").is_empty(), "{method} {path}");
    }
    drop(kept);
    assert_eq!(running.drained().await, Drained { abandoned: 0 });
}

/// The bytes of the blob that the client of [`a_blob_get_begun_before_the_drain_is_sent_in_full`]
/// leaves unread when it sends its next request: more than its receive buffer holds, so that the
/// server still has some of them to send, and few enough for the server's send buffer (on
/// loopback it starts at a megabyte or more) to take them, so that the server has written the
/// whole answer and is closing the connection.
const TAIL: usize = 256 << 10;

/// Asserts that a blob whose GET began before the drain, and whose answer is still being sent when
/// the settle ends, is sent in full, and that its connection then ends with a clean end of stream:
/// also when the client, told nothing of the drain (the head of the answer came before it), sends
/// its next request on the connection once the server has written the whole answer and while the
/// end of it is still in the server's send buffer. A server that closes the connection then, with
/// that request unread, resets it, and the reset throws the end of the blob away; with a
/// lingering close the request is read and dropped, and the blob arrives whole. The drain ends
/// with nothing cut once the client has closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blob_get_begun_before_the_drain_is_sent_in_full() {
    local(a_blob_get_begun_before_the_drain_is_sent_in_full_here()).await;
}

async fn a_blob_get_begun_before_the_drain_is_sent_in_full_here() {
    let files = files("oci-drain-blob");
    let mut running = Running::start(files.plain("127.0.0.1:0"), "200ms").await;
    // Larger than the server's send buffer and the client's receive buffer, so that the answer is
    // still being sent when the settle ends; and no two chunks alike, so that a lost piece shows.
    let content: Vec<u8> = (0..16_u32 << 20).map(|at| (at % 251) as u8).collect();
    let blob = running.seed(&content).await;
    // A small receive buffer, which the kernel does not grow: the client decides how much of the
    // answer the server can have sent.
    let socket = TcpSocket::new_v4().expect("a socket");
    socket.set_recv_buffer_size(4096).expect("a receive buffer");
    let mut tcp = socket.connect(running.oci()).await.expect("connected");
    let ask = format!("GET {blob} HTTP/1.1\r\nhost: oci.test\r\n\r\n");
    tcp.write_all(ask.as_bytes()).await.expect("sent");
    let (head, mut body) = read_head(&mut tcp).await;
    assert!(head.starts_with("http/1.1 200 "), "{head}");
    assert!(!head.contains("connection: close"), "{head}");
    let length = content_length(&head);
    assert_eq!(length, content.len());

    running.begin_drain();
    running.settled().await;
    let mut piece = vec![0_u8; 64 << 10];
    while body.len() < length - TAIL {
        let want = piece.len().min(length - TAIL - body.len());
        let read = tcp.read(&mut piece[..want]).await.expect("a read");
        assert!(read > 0, "the connection ended at {} bytes", body.len());
        body.extend_from_slice(&piece[..read]);
    }
    // The server takes the rest into its send buffer, and is done with the answer.
    tokio::time::sleep(SHORT).await;
    tcp.write_all(PING)
        .await
        .expect("the next request is taken");
    let mut rest = Vec::new();
    let read = tcp.read_to_end(&mut rest).await;
    body.extend_from_slice(&rest);
    let got = body.len().min(length);
    assert!(read.is_ok(), "reset at {got} of {length} bytes: {read:?}");
    assert!(got == length, "the blob was cut at {got} of {length} bytes");
    assert!(
        body[..length] == content[..],
        "the bytes of the blob differ"
    );
    let after = String::from_utf8_lossy(&body[length..]).to_ascii_lowercase();
    let answered = after.starts_with("http/1.1 200 ") && after.contains("connection: close");
    assert!(after.is_empty() || answered, "{after}");
    drop(tcp);
    assert_eq!(running.drained().await, Drained { abandoned: 0 });
}

/// Asserts, as the binary runs the front, that a kept-alive connection that is idle when the
/// settle ends gets a lingering close: a clean end of stream, after which the front still reads
/// the connection, so that a request the client writes then is taken and does not reset the
/// connection (a reset would show as an error on the socket and on the next write); and that the
/// drain waits for the client to close before it ends, with nothing cut. It would catch a front
/// that closes an idle connection with a plain close, and a drain that ends without waiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_connection_gets_a_lingering_close_at_the_end_of_the_settle() {
    local(an_idle_connection_gets_a_lingering_close_at_the_end_of_the_settle_here()).await;
}

async fn an_idle_connection_gets_a_lingering_close_at_the_end_of_the_settle_here() {
    let files = files("oci-drain-idle");
    let mut running = Running::start(files.plain("127.0.0.1:0"), "200ms").await;
    let mut tcp = TcpStream::connect(running.oci()).await.expect("connected");
    tcp.write_all(PING).await.expect("sent");
    let (head, mut body) = read_head(&mut tcp).await;
    assert!(head.starts_with("http/1.1 200 "), "{head}");
    let mut piece = [0_u8; 1024];
    while body.len() < content_length(&head) {
        let read = tcp.read(&mut piece).await.expect("a read");
        assert!(read > 0, "the connection ended inside the answer");
        body.extend_from_slice(&piece[..read]);
    }

    running.begin_drain();
    let first = tokio::time::timeout(SAFETY, tcp.read(&mut piece)).await;
    assert_eq!(first.expect("no end of stream").expect("a clean end"), 0);
    tcp.write_all(PING)
        .await
        .expect("the late request is taken");
    tokio::time::sleep(SHORT).await;
    let error = tcp.take_error().expect("the socket's error");
    assert!(error.is_none(), "the connection was reset: {error:?}");
    tcp.write_all(PING).await.expect("still taken: not reset");
    assert!(
        !running.run.is_finished(),
        "the drain did not wait for the client"
    );

    tcp.shutdown().await.expect("closed");
    assert_eq!(running.drained().await, Drained { abandoned: 0 });
}

/// Asserts, over plain HTTP and over TLS (where the end of stream is the server's TLS close), that
/// a client that writes on a connection after the end of stream of its lingering close is counted
/// in `tierstore_oci_drain_late_requests_total`, once for the connection however many requests it
/// writes, that its connection is not reset, and that the stop waits for it to close.
#[tokio::test]
async fn a_request_after_the_settle_is_counted_as_late() {
    for tls in [false, true] {
        let rig = if tls {
            Rig::start().await
        } else {
            Rig::start_plain().await
        };
        let metrics = Arc::clone(&rig.metrics);
        let mut conn = rig.conn().await;
        let ping = conn.request("GET", "/v2/", &[]).await.expect("an answer");
        assert_eq!(ping.status, 200);
        let stopping = tokio::spawn(rig.stop());
        let end = tokio::time::timeout(SAFETY, conn.read_to_end()).await;
        let end = end.expect("no end of stream");
        assert!(end.expect("a clean end").is_empty(), "tls {tls}");
        for _ in 0..2 {
            conn.write_raw(PING)
                .await
                .expect("the late request is taken");
            tokio::time::sleep(SHORT).await;
        }
        assert!(!stopping.is_finished(), "tls {tls}: the stop did not wait");
        conn.shutdown().await.expect("closed");
        let cut = tokio::time::timeout(SAFETY, stopping).await;
        assert_eq!(cut.expect("the stop never ended").expect("joined"), 0);
        let text = metrics.render();
        let late = "tierstore_oci_drain_late_requests_total 1\n";
        assert!(text.contains(late), "tls {tls}: {text}");
    }
}
