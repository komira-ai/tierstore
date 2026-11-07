//! The accept loop of the OCI listener over a source of connections that fails, on the paused
//! clock: no socket.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, watch};

use super::listener::{OciLimits, Shared, accept_loop, drain_of};
use super::test_pki::{ca, ip, leaf};
use super::{
    OciError, OciFront, OciListener, OciMetrics, OciMode, OciPrefix, acceptor, plain_warning,
};
use crate::MemoryBackend;
use crate::server::drain::Drain;
use crate::server::listen::bind;
use crate::server::testing::established_unaccepted;
use crate::server::{ACCEPT_PAUSE, Incoming};
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// A source whose first accept fails and whose others never complete; it counts the asks.
struct Failing {
    asked: Arc<AtomicUsize>,
}

impl Incoming for Failing {
    type Stream = TcpStream;

    fn poll_incoming(
        &mut self,
        _cx: &mut Context<'_>,
    ) -> Poll<io::Result<(TcpStream, SocketAddr)>> {
        match self.asked.fetch_add(1, Ordering::SeqCst) {
            0 => Poll::Ready(Err(io::Error::other("an accept that failed"))),
            _ => Poll::Pending,
        }
    }
}

/// Asserts that a failed accept does not end the loop or take a connection: after the pause the
/// loop asks again, and a stop then ends it with nothing cut.
#[tokio::test(start_paused = true)]
async fn a_failed_accept_is_followed_by_another_ask() {
    let dir = TestDir::under(std::env::temp_dir(), "oci-listener-accept");
    let files = leaf(dir.path(), "front", &ca(), vec![ip("127.0.0.1")]);
    let listen: SocketAddr = "127.0.0.1:0".parse().expect("an address");
    let tls = acceptor(&files.cert, &files.key, listen).expect("an acceptor");
    let prefix = OciPrefix::new("oci/").expect("a prefix");
    let metrics = Arc::new(OciMetrics::default());
    let front = Arc::new(OciFront::new(
        Arc::new(MemoryBackend::new()),
        prefix,
        metrics,
    ));
    let second = Duration::from_secs(1);
    let limits = OciLimits {
        max_connections: 1,
        handshake_timeout: second,
        header_timeout: second,
        write_idle: second,
        lifetime: second,
        drain_deadline: second,
        listen: crate::server::ListenTuning::default(),
    };
    let asked = Arc::new(AtomicUsize::new(0));
    let incoming = Failing {
        asked: Arc::clone(&asked),
    };
    let (stop, stopped) = watch::channel(false);
    let shared = Shared::new(drain_of(&front, limits), limits.max_connections);
    let looping = tokio::spawn(accept_loop(
        incoming,
        Some(tls),
        front,
        limits,
        stopped,
        shared,
    ));
    tokio::time::sleep(ACCEPT_PAUSE * 2).await;
    assert_eq!(asked.load(Ordering::SeqCst), 2);
    stop.send(true).expect("a loop");
    assert_eq!(looping.await.expect("the loop ended"), 0);
}

fn plain_front() -> (Arc<OciFront>, OciLimits) {
    let prefix = OciPrefix::new("oci/").expect("a prefix");
    let metrics = Arc::new(OciMetrics::default());
    let backend = Arc::new(MemoryBackend::new());
    let front = Arc::new(OciFront::new(backend, prefix, metrics));
    let second = Duration::from_secs(1);
    let limits = OciLimits {
        max_connections: 1,
        handshake_timeout: second,
        header_timeout: second,
        write_idle: second,
        lifetime: second,
        drain_deadline: second,
        listen: crate::server::ListenTuning::default(),
    };
    (front, limits)
}

/// Asserts that the start warning for a plain front is for the addresses off loopback and for no
/// other: it names the unspecified addresses, documentation addresses and an IPv4-mapped address
/// that is not loopback, and is absent for anywhere in the IPv4 loopback block, `::1` and the
/// mapped loopback address.
#[test]
fn the_plain_warning_is_for_non_loopback_addresses_only() {
    for quiet in ["127.0.0.1:0", "[::1]:9500", "[::ffff:127.0.0.1]:9500"] {
        let addr: SocketAddr = quiet.parse().expect("an address");
        assert_eq!(plain_warning(addr), None, "{quiet}");
    }
    // The far end of the loopback block, built from its octets: the public-file lint allows no
    // dotted loopback address but 127.0.0.1.
    let far = SocketAddr::from((Ipv4Addr::new(127, 255, 0, 9), 9500));
    assert_eq!(plain_warning(far), None, "{far}");
    for warned in [
        "0.0.0.0:9500",
        "[::]:9500",
        "192.0.2.1:9500",
        "198.51.100.7:9500",
        "[2001:db8::1]:9500",
        "[::ffff:192.0.2.1]:9500",
    ] {
        let addr: SocketAddr = warned.parse().expect("an address");
        let text = plain_warning(addr).expect(warned);
        assert!(!text.contains('\n'), "{text}");
        assert!(text.contains(&addr.to_string()), "{text}");
        assert!(
            text.contains("serves plain HTTP on a non-loopback address"),
            "{text}"
        );
        assert!(
            text.contains("params tls_cert_file and tls_key_file"),
            "{text}"
        );
    }
}

/// One plain request of the front at `port` on loopback, answered and closed: the whole reply.
async fn get_v2(port: u16) -> String {
    let mut conn = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connected");
    let request = "GET /v2/ HTTP/1.1\r\nHost: front\r\nConnection: close\r\n\r\n";
    conn.write_all(request.as_bytes()).await.expect("written");
    let mut reply = Vec::new();
    conn.read_to_end(&mut reply).await.expect("read");
    String::from_utf8_lossy(&reply).into_owned()
}

/// The address a client dials to reach a socket bound at `at`: a wildcard by way of loopback.
fn reach(at: SocketAddr) -> SocketAddr {
    let ip = match at.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, at.port())
}

/// Asserts that one listener binds every address it is given and answers a request at each, that
/// the first is the one `local_addr` says, and that a stop closes every socket and ends the one
/// loop with nothing cut.
#[tokio::test]
async fn one_listener_serves_every_address_it_is_given() {
    let (front, limits) = plain_front();
    let mut wanted: Vec<SocketAddr> = ["127.0.0.1:0", "0.0.0.0:0"]
        .iter()
        .map(|text| text.parse().expect("an address"))
        .collect();
    if std::net::TcpListener::bind("[::1]:0").is_ok() {
        wanted.push("[::1]:0".parse().expect("an address"));
    }
    let listener = OciListener::bind_all(&wanted, None, front, limits)
        .await
        .expect("bound");
    let bound = listener.local_addrs().expect("addresses");
    assert_eq!(bound.len(), wanted.len(), "{bound:?}");
    assert_eq!(listener.local_addr().expect("the first"), bound[0]);
    let sockets = listening(&bound);
    assert_eq!(sockets.len(), bound.len(), "{sockets:?}");
    let (stop, stopped) = watch::channel(false);
    let serving = tokio::spawn(listener.serve(stopped));
    for at in &bound {
        let mut conn = TcpStream::connect(reach(*at)).await.expect("connected");
        let request = "GET /v2/ HTTP/1.1\r\nHost: front\r\nConnection: close\r\n\r\n";
        conn.write_all(request.as_bytes()).await.expect("written");
        let mut reply = Vec::new();
        conn.read_to_end(&mut reply).await.expect("read");
        let reply = String::from_utf8_lossy(&reply);
        assert!(reply.starts_with("HTTP/1.1 200 "), "{at}: {reply}");
    }
    stop.send(true).expect("a listener");
    assert_eq!(serving.await.expect("served"), 0);
    let left: Vec<_> = listening(&bound)
        .into_iter()
        .filter(|socket| sockets.contains(socket))
        .collect();
    assert!(
        left.is_empty(),
        "the listener's sockets still listen: {left:?}"
    );
}

/// The sockets of this network namespace that listen on the port of one of `addrs`, by inode, from
/// the tables of `/proc`: an inode names a socket, so another test's listener that takes a port
/// once it is free is not taken for this one's.
#[cfg(target_os = "linux")]
fn listening(addrs: &[SocketAddr]) -> Vec<String> {
    let ports: Vec<u16> = addrs.iter().map(SocketAddr::port).collect();
    let mut sockets = Vec::new();
    for table in ["tcp", "tcp6"] {
        let text = std::fs::read_to_string(format!("/proc/self/net/{table}")).expect("a table");
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let port = fields[1].rsplit_once(':').map(|(_, port)| port);
            let port = port.and_then(|port| u16::from_str_radix(port, 16).ok());
            if fields[3] == "0A" && port.is_some_and(|port| ports.contains(&port)) {
                sockets.push(fields[9].to_owned());
            }
        }
    }
    sockets
}

/// Outside Linux there is no table to read: the addresses that take a connect stand for the
/// sockets, which another test's listener on a port freed a moment ago could also answer.
#[cfg(not(target_os = "linux"))]
fn listening(addrs: &[SocketAddr]) -> Vec<String> {
    let taking = addrs
        .iter()
        .filter(|addr| std::net::TcpStream::connect(reach(**addr)).is_ok());
    taking.map(ToString::to_string).collect()
}

/// Asserts that a listener given no address binds nothing and has no address to report.
#[tokio::test]
async fn a_listener_with_no_address_has_none_to_report() {
    let (front, limits) = plain_front();
    let listener = OciListener::bind_all(&[], None, front, limits)
        .await
        .expect("bound");
    assert!(listener.local_addrs().expect("none").is_empty());
    assert!(listener.local_addr().is_err());
}

/// Asserts that the listener binds a plain address that is not loopback (the unspecified
/// address), says its mode is plain HTTP, and answers a request there; and that it serves plain
/// on loopback and TLS where a certificate is given.
#[tokio::test]
async fn a_plain_listener_binds_off_loopback_and_serves() {
    let (front, limits) = plain_front();
    let off: SocketAddr = "0.0.0.0:0".parse().expect("an address");
    let wide = OciListener::bind_plain(off, Arc::clone(&front), limits)
        .await
        .expect("bound");
    assert_eq!(wide.mode(), OciMode::Http);
    let bound = wide.local_addr().expect("an address");
    assert!(bound.ip().is_unspecified() && bound.port() != 0, "{bound}");
    let (stop, stopped) = watch::channel(false);
    let serving = tokio::spawn(wide.serve(stopped));
    let reply = get_v2(bound.port()).await;
    assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
    stop.send(true).expect("a listener");
    assert_eq!(serving.await.expect("served"), 0);

    let on: SocketAddr = "127.0.0.1:0".parse().expect("an address");
    let plain = OciListener::bind_plain(on, front, limits)
        .await
        .expect("bound");
    assert_eq!(plain.mode(), OciMode::Http);
    assert!(plain.local_addr().expect("an address").ip().is_loopback());

    let dir = TestDir::under(std::env::temp_dir(), "oci-listener-mode");
    let files = leaf(dir.path(), "front", &ca(), vec![ip("127.0.0.1")]);
    let tls = acceptor(&files.cert, &files.key, on).expect("an acceptor");
    let (front, limits) = plain_front();
    let secure = OciListener::bind(on, tls, front, limits)
        .await
        .expect("bound");
    assert_eq!(secure.mode(), OciMode::Https);
    assert_eq!(OciMode::Http.as_str(), "http");
    assert_eq!(OciMode::Https.as_str(), "https");
}

/// The address the bind of `addrs` was refused for; the test fails if it was not refused, or for
/// another reason than a bind.
async fn refused_at(addrs: &[SocketAddr]) -> SocketAddr {
    let (front, limits) = plain_front();
    let bound = OciListener::bind_all(addrs, None, front, limits).await;
    let Err(OciError::Bind { addr, source }) = bound else {
        panic!("the bind of {addrs:?} was not refused for a bind");
    };
    assert_eq!(source.kind(), io::ErrorKind::AddrInUse, "{addr}");
    addr
}

/// Asserts that a front asked for the wildcard of both families on one port, as a hand-over
/// needs, is refused when one of the two is taken, names the one that is, and keeps neither
/// socket: with `[::]:P` taken (by a socket that is IPv6 only, so `0.0.0.0:P` is free), and with
/// `0.0.0.0:P` taken and `[::]:P` asked first. In the second case the IPv6 socket of the front is
/// bound before the refusal only because it is `IPV6_V6ONLY`: without it the refusal would name
/// `[::]:P`. It would catch a front that serves on one family when the other cannot be bound
/// (the redirected connections of the other refused), and an IPv6 socket that holds the IPv4
/// port too.
#[tokio::test]
#[expect(
    clippy::disallowed_macros,
    reason = "a skipped test says why on standard error (AGENTS.md T12)"
)]
async fn a_listener_that_cannot_bind_both_wildcards_is_refused_and_keeps_neither() {
    // A host without IPv6 cannot have the two wildcards of a port: the test says so on stderr,
    // with the reason the system gave, and asserts nothing.
    if let Err(why) = std::net::TcpListener::bind("[::1]:0") {
        eprintln!("skipped: this host has no IPv6 loopback ({why}); there is no [::]:P to bind");
        return;
    }
    let at = |text: String| -> SocketAddr { text.parse().expect("an address") };

    let taken = bind(at("[::]:0".to_owned())).expect("the v6 wildcard");
    let port = taken.local_addr().expect("an address").port();
    let (v4, v6) = (at(format!("0.0.0.0:{port}")), at(format!("[::]:{port}")));
    assert_eq!(refused_at(&[v4, v6]).await, v6);
    drop(bind(v4).expect("the v4 wildcard is free again"));
    drop(taken);

    let taken = bind(at("0.0.0.0:0".to_owned())).expect("the v4 wildcard");
    let port = taken.local_addr().expect("an address").port();
    let (v4, v6) = (at(format!("0.0.0.0:{port}")), at(format!("[::]:{port}")));
    assert_eq!(refused_at(&[v6, v4]).await, v4);
    drop(bind(v6).expect("the v6 wildcard is free again"));
}

/// A source whose connections the kernel holds as handshakes in flight until the door that ends
/// the settle has counted them: then their handshakes end and they wait to be taken, in order.
/// Each is a real loopback connection, and an entry may be an accept that fails.
struct Held {
    /// What the source gives once the handshakes have been counted.
    held: Vec<io::Result<(TcpStream, SocketAddr)>>,
    /// What it gives at once, before the end of the settle.
    ready: Vec<(TcpStream, SocketAddr)>,
    counted: AtomicBool,
}

impl Incoming for Held {
    type Stream = TcpStream;

    fn poll_incoming(
        &mut self,
        _cx: &mut Context<'_>,
    ) -> Poll<io::Result<(TcpStream, SocketAddr)>> {
        if let Some(ready) = self.ready.pop() {
            return Poll::Ready(Ok(ready));
        }
        match self.held.pop() {
            Some(accepted) if self.counted.load(Ordering::SeqCst) => Poll::Ready(accepted),
            Some(accepted) => {
                self.held.push(accepted);
                Poll::Pending
            }
            None => Poll::Pending,
        }
    }

    fn handshakes(&self) -> io::Result<usize> {
        let counted = self.counted.swap(true, Ordering::SeqCst);
        Ok(if counted { 0 } else { self.held.len() })
    }
}

/// A loopback connection: the server's end, with its peer, and the client's.
async fn pair() -> ((TcpStream, SocketAddr), TcpStream) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    (listener.accept().await.unwrap(), client)
}

/// An accept loop over `incoming` (TLS when `acceptor` is given), with room for
/// `max_connections` and bounds long enough that none of them ends a test's connection, its stop,
/// and its drain, which the test begins.
fn closing_loop<I>(
    incoming: I,
    max_connections: usize,
    acceptor: Option<TlsAcceptor>,
) -> (
    tokio::task::JoinHandle<usize>,
    watch::Sender<bool>,
    Arc<Drain>,
)
where
    I: Incoming<Stream = TcpStream> + Send + Sync + 'static,
{
    let (front, _) = plain_front();
    let minute = Duration::from_secs(60);
    let limits = OciLimits {
        max_connections,
        handshake_timeout: minute,
        header_timeout: minute,
        write_idle: minute,
        lifetime: minute,
        drain_deadline: minute,
        listen: crate::server::ListenTuning::default(),
    };
    let (stop, stopped) = watch::channel(false);
    let shared = Shared::new(drain_of(&front, limits), limits.max_connections);
    let drain = Arc::clone(&shared.drain);
    let looping = tokio::spawn(accept_loop(
        incoming, acceptor, front, limits, stopped, shared,
    ));
    (looping, stop, drain)
}

/// [`closing_loop`], plain, whose drain has begun, as the binary begins it before the stop.
fn plain_loop(
    incoming: Held,
    max_connections: usize,
) -> (tokio::task::JoinHandle<usize>, watch::Sender<bool>) {
    let (looping, stop, drain) = closing_loop(incoming, max_connections, None);
    drain.begin();
    (looping, stop)
}

/// The request the OCI tests send: the API's base, which a plain front answers 200.
const BASE: &[u8] = b"GET /v2/ HTTP/1.1\r\nhost: oci.test\r\n\r\n";

/// Reads one whole answer from `client`, which may leave the connection open: the head, and the
/// body its `content-length` gives. A read that fails, or the end of the stream, before the
/// answer is whole is an error: an answer cut after its head is not taken for one served.
async fn read_one<R: AsyncRead + Unpin>(client: &mut R) -> io::Result<String> {
    let mut bytes = Vec::new();
    let mut piece = [0_u8; 4096];
    loop {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length = head.lines().find_map(|line| {
                let line = line.to_ascii_lowercase();
                line.strip_prefix("content-length: ")?
                    .trim()
                    .parse::<usize>()
                    .ok()
            });
            let Some(length) = length else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("no content-length: {text}"),
                ));
            };
            if body.len() >= length {
                return Ok(text);
            }
        }
        let read = client.read(&mut piece).await?;
        if read == 0 {
            let cut = format!("the connection ended inside an answer: {text}");
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, cut));
        }
        bytes.extend_from_slice(&piece[..read]);
    }
}

/// Asserts that `answer` is the whole answer of the API base: 200, `Connection: close` when
/// `closed`, and the body `{}` of exactly the length its head gives.
#[track_caller]
fn assert_ping(answer: &str, closed: bool) {
    let (head, body) = answer.split_once("\r\n\r\n").expect("a head");
    assert!(head.starts_with("HTTP/1.1 200 "), "{answer}");
    assert_eq!(head.contains("connection: close"), closed, "{answer}");
    assert!(head.contains("content-length: 2"), "{answer}");
    assert_eq!(body, "{}", "{answer}");
}

/// Asserts that the reader the OCI tests read answers with refuses an answer cut after its head
/// (its body short of the length the head gives, then the end of the stream), and one whose
/// transport fails there, and takes a whole one: a reset or a truncation after the headers is not
/// taken for a served request. It would catch a reader that lets the ping test pass on a
/// connection the server cut.
#[tokio::test]
async fn an_answer_cut_after_its_head_is_not_taken_for_a_served_one() {
    let whole = "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 2\r\n\r\n{}";
    let (mut client, mut server) = tokio::io::duplex(1024);
    server.write_all(whole.as_bytes()).await.unwrap();
    drop(server);
    let answer = read_one(&mut client).await.expect("a whole answer");
    assert_ping(&answer, true);

    let (mut client, mut server) = tokio::io::duplex(1024);
    server
        .write_all(&whole.as_bytes()[..whole.len() - 1])
        .await
        .unwrap();
    drop(server);
    let cut = read_one(&mut client).await.unwrap_err();
    assert_eq!(cut.kind(), io::ErrorKind::UnexpectedEof);

    let mut failing = cut_by_reset(whole.len() - 1, whole.as_bytes());
    let reset = read_one(&mut failing).await.unwrap_err();
    assert_eq!(reset.kind(), io::ErrorKind::ConnectionReset);
}

/// A stream that gives the first `upto` bytes of `bytes` and then fails as a reset connection.
fn cut_by_reset(upto: usize, bytes: &[u8]) -> impl AsyncRead + Unpin {
    let given = io::Cursor::new(bytes[..upto].to_vec());
    given.chain(Resetting)
}

/// A reader that fails as a reset connection.
struct Resetting;

impl AsyncRead for Resetting {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()))
    }
}

/// Asserts that the OCI front, at the end of its settle, takes the connections the kernel held
/// for it as handshakes in flight, after an accept that fails, and serves their request with
/// `Connection: close`, and that the loop then ends with nothing cut. It would catch an OCI front
/// that closes its socket at once at the end of the settle, which resets those connections.
#[tokio::test]
async fn the_connections_held_at_the_end_of_the_settle_are_taken_and_served() {
    let (server, mut client) = pair().await;
    let held = Held {
        held: vec![Ok(server), Err(io::Error::other("an accept that failed"))],
        ready: Vec::new(),
        counted: AtomicBool::new(false),
    };
    let (looping, stop) = plain_loop(held, 4);
    stop.send(true).expect("a loop");

    client
        .write_all(b"GET /v2/ HTTP/1.1\r\nhost: oci.test\r\n\r\n")
        .await
        .unwrap();
    let answer = read_one(&mut client).await.expect("a whole answer");
    assert_ping(&answer, true);
    drop(client);
    assert_eq!(looping.await.expect("the loop ended"), 0);
}

/// Asserts that the OCI front waits for a free slot at the end of its settle for no longer than
/// the door's grace: with its one slot held by a connection that sends nothing, the connection the
/// kernel holds is not taken, the socket closes (that client reads the end of its stream, the
/// limit the module states), the connection in the slot is closed in turn, and the loop ends with
/// nothing cut. It would catch a door that waits for a slot for ever, and one that takes a
/// connection past the front's limit.
///
/// The clock is paused. It moves past the door's grace by auto-advance while nothing is on its way
/// through the kernel, and the test waits for what the grace does by events: the door drops its
/// source ([`Telling`]) and the connection in the slot shuts its end and lingers. From then on the
/// clock is held ([`ClockHold`]): the closes that follow are on their way through the kernel, and
/// a loopback close on macOS may reach its peer after the runtime has parked, which on a clock
/// left to auto-advance would move it to the drain deadline and cut the lingering connection. The
/// client in the slot closes its end only once the runtime has parked waiting for it ([`Parks`]),
/// so that the outcome is shown not to depend on when that close arrives.
#[test]
fn the_door_waits_for_a_free_slot_no_longer_than_its_grace() {
    let parks = Arc::new(Parks::default());
    let counted = Arc::clone(&parks);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .on_thread_park(move || counted.parked())
        .build()
        .expect("a runtime");
    runtime.block_on(async {
        let (first, mut in_slot) = pair().await;
        let (second, mut waiting) = pair().await;
        let (gone, door_closed) = oneshot::channel();
        // The second connection is in the accept queue from the start, ready to be taken.
        let held = Telling {
            incoming: Held {
                held: vec![Ok(second)],
                ready: vec![first],
                counted: AtomicBool::new(true),
            },
            _gone: gone,
        };
        // The first connection takes the slot, before the stop or once the door is closing.
        let (looping, stop, drain) = closing_loop(held, 1, None);
        drain.begin();
        stop.send(true).expect("a loop");
        // A request on the held connection gets no answer: it was never taken past the limit, and
        // the kernel ends it (an end of stream, or a reset) when the socket closes.
        let request = b"GET /v2/ HTTP/1.1\r\nhost: oci.test\r\n\r\n";
        let _ = waiting.write_all(request).await;

        // The door's grace is the first timer: at its end the connection in the slot, which sent
        // nothing in it, shuts its end and lingers, and the door closes.
        lingering(&drain).await;
        let _ = door_closed.await;
        let _hold = ClockHold::new();

        let mut answer = Vec::new();
        let _ = waiting.read_to_end(&mut answer).await;
        assert_eq!(String::from_utf8_lossy(&answer), "", "not served");
        let mut byte = [0_u8; 1];
        assert_eq!(
            in_slot.read(&mut byte).await.unwrap(),
            0,
            "closed, unanswered"
        );
        // The client closes its end, which ends the lingering close of the server's, once the
        // runtime has parked with the loop waiting for it.
        let in_slot = in_slot.into_std().expect("a std socket");
        let (seen, parks) = (parks.count(), Arc::clone(&parks));
        let client = std::thread::spawn(move || {
            parks.wait_past(seen);
            drop(in_slot);
        });
        assert_eq!(looping.await.expect("the loop ended"), 0);
        client.join().expect("the client closed");
    });
}

/// A source of connections that says when it is gone: the door drops its source when it closes,
/// and with it this sender, which ends the wait of its receiver. It counts no handshakes in flight,
/// so what `incoming` gives is taken only once it is in the accept queue.
struct Telling<I> {
    incoming: I,
    _gone: oneshot::Sender<()>,
}

impl<I: Incoming> Incoming for Telling<I> {
    type Stream = I::Stream;

    fn poll_incoming(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<(I::Stream, SocketAddr)>> {
        self.incoming.poll_incoming(cx)
    }
}

/// Returns once a connection of `drain` lingers: it has shut its end and waits for its client's.
async fn lingering(drain: &Drain) {
    loop {
        let entered = drain.idle.notified();
        tokio::pin!(entered);
        let _ = entered.as_mut().enable();
        if drain.lingering() > 0 {
            return;
        }
        entered.await;
    }
}

/// Keeps a paused clock from auto-advancing while it lives: tokio does not auto-advance a paused
/// clock while a `spawn_blocking` task runs (`tokio::time::pause`, "Preventing auto-advance"), and
/// this one runs until the hold is dropped.
struct ClockHold {
    _dropped_ends_it: std::sync::mpsc::Sender<()>,
}

impl ClockHold {
    fn new() -> Self {
        let (hold, dropped) = std::sync::mpsc::channel::<()>();
        drop(tokio::task::spawn_blocking(move || {
            let _ = dropped.recv();
        }));
        Self {
            _dropped_ends_it: hold,
        }
    }
}

/// The idle parks of a runtime, counted as they come.
#[derive(Default)]
struct Parks {
    count: Mutex<u64>,
    moved: Condvar,
}

impl Parks {
    fn parked(&self) {
        *self.count.lock().unwrap() += 1;
        self.moved.notify_all();
    }

    fn count(&self) -> u64 {
        *self.count.lock().unwrap()
    }

    /// Blocks until there were more parks than `seen`.
    fn wait_past(&self, seen: u64) {
        let count = self.count.lock().unwrap();
        drop(
            self.moved
                .wait_while(count, |count| *count <= seen)
                .unwrap(),
        );
    }
}

/// Asserts that an OCI front whose every slot is held by an idle keep-alive connection (it has
/// carried a request, and its client waits for the end of the stream) closes that connection when
/// the door begins, so that its client leaves and frees the slot, and that the connection the
/// kernel holds for the listener is then taken in that slot and its request answered in full,
/// with `Connection: close`; the loop ends with nothing cut. It would catch a door that waits for a
/// slot only the end of the drain would free, and so closes the socket on a connected client.
#[tokio::test]
async fn an_idle_connection_in_every_slot_closes_and_frees_one_for_the_held() {
    let (first, mut idle) = pair().await;
    let (second, mut queued) = pair().await;
    let held = Held {
        held: vec![Ok(second)],
        ready: vec![first],
        counted: AtomicBool::new(false),
    };
    let (looping, stop, drain) = closing_loop(held, 1, None);
    idle.write_all(BASE).await.unwrap();
    let kept = read_one(&mut idle).await.expect("a whole answer");
    assert_ping(&kept, false);

    drain.begin();
    stop.send(true).expect("a loop");
    queued.write_all(BASE).await.unwrap();
    let mut rest = Vec::new();
    idle.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty(), "closed with nothing more sent");
    drop(idle);
    let answer = read_one(&mut queued).await.expect("a whole answer");
    assert_ping(&answer, true);
    drop(queued);
    assert_eq!(looping.await.expect("the loop ended"), 0);
}

/// A TLS acceptor for 127.0.0.1 and a connector that trusts its CA, from files in `dir`.
fn tls_pair(dir: &TestDir) -> (TlsAcceptor, TlsConnector) {
    let authority = ca();
    let files = leaf(dir.path(), "front", &authority, vec![ip("127.0.0.1")]);
    let listen: SocketAddr = "127.0.0.1:0".parse().expect("an address");
    let tls = acceptor(&files.cert, &files.key, listen).expect("an acceptor");
    use rustls::pki_types::pem::PemObject as _;
    let mut roots = rustls::RootCertStore::empty();
    let pem = authority.pem();
    for cert in rustls::pki_types::CertificateDer::pem_slice_iter(pem.as_bytes()) {
        roots.add(cert.expect("a certificate")).expect("a root");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    (tls, TlsConnector::from(Arc::new(config)))
}

/// A client over TLS on `tcp`: its handshake, its request, and everything it reads.
async fn tls_client(connector: TlsConnector, tcp: TcpStream) -> io::Result<String> {
    let name = rustls::pki_types::ServerName::from(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let mut stream = connector.connect(name, tcp).await?;
    stream.write_all(BASE).await?;
    read_one(&mut stream).await
}

/// Asserts, on a TLS front with one slot, that a connection the door takes whose client then fails
/// its TLS handshake (it sends bytes that are not TLS) gives its slot back, and that the next
/// connection the kernel held, whose TLS handshake ends after the settle, is taken in that slot
/// and its request answered with `Connection: close`; the loop ends with nothing cut. It would
/// catch a failed handshake that keeps its slot, and a door that closes on a connection whose
/// handshake is still under way.
#[tokio::test]
async fn a_failed_tls_handshake_frees_its_slot_and_a_later_one_is_served() {
    let dir = TestDir::under(std::env::temp_dir(), "oci-door-tls");
    let (tls, connector) = tls_pair(&dir);
    let (bad_server, mut bad) = pair().await;
    let (good_server, good) = pair().await;
    let held = Held {
        held: vec![Ok(good_server), Ok(bad_server)],
        ready: Vec::new(),
        counted: AtomicBool::new(false),
    };
    let (looping, stop, drain) = closing_loop(held, 1, Some(tls));
    drain.begin();
    stop.send(true).expect("a loop");
    bad.write_all(b"not a TLS record\r\n\r\n").await.unwrap();
    let mut nothing = Vec::new();
    let _ = bad.read_to_end(&mut nothing).await;
    let answer = tls_client(connector, good)
        .await
        .expect("a whole answer over TLS");
    assert_ping(&answer, true);
    assert_eq!(looping.await.expect("the loop ended"), 0);
}

/// Asserts that a connection the door takes whose client resets it before sending anything (a
/// read that fails, during the wait for its first request) ends and gives its slot back, and that
/// the next connection the kernel held is taken in that slot and answered; the loop ends with
/// nothing cut. It would catch a wait for a first request that outlives a connection whose read
/// failed, and keeps its slot.
#[tokio::test]
async fn a_connection_reset_before_its_first_request_frees_its_slot() {
    let (reset_server, reset) = pair().await;
    let (next_server, mut next) = pair().await;
    let held = Held {
        held: vec![Ok(next_server), Ok(reset_server)],
        ready: Vec::new(),
        counted: AtomicBool::new(false),
    };
    let (looping, stop) = plain_loop(held, 1);
    stop.send(true).expect("a loop");
    reset.set_zero_linger().unwrap();
    drop(reset);
    next.write_all(BASE).await.unwrap();
    let answer = read_one(&mut next).await.expect("a whole answer");
    assert_ping(&answer, true);
    drop(next);
    assert_eq!(looping.await.expect("the loop ended"), 0);
}

/// Asserts that the backlog in the limits of the OCI listener is the one its sockets listen with:
/// with a backlog of 1, of six connections no more than the queue and the one the kernel lets
/// over it are established while nothing accepts; with the default backlog more than that are.
#[tokio::test]
async fn the_oci_bind_listens_with_the_backlog_in_its_limits() {
    let (front, limits) = plain_front();
    let wanted: SocketAddr = "127.0.0.1:0".parse().expect("an address");
    let mut small = limits;
    small.listen.backlog = 1;
    let listener = OciListener::bind_plain(wanted, Arc::clone(&front), small)
        .await
        .expect("bound");
    let at = listener.local_addr().expect("an address");
    let held = established_unaccepted(at, 6).await;
    assert!((1..=3).contains(&held), "{held} established on {at}");
    let listener = OciListener::bind_plain(wanted, front, limits)
        .await
        .expect("bound");
    let at = listener.local_addr().expect("an address");
    assert!(established_unaccepted(at, 6).await > 3, "{at}");
}
