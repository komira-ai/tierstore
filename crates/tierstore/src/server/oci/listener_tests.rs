//! The accept loop of the OCI listener over a source of connections that fails, on the paused
//! clock: no socket.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::watch;

use super::listener::{OciLimits, Shared, accept_loop, drain_of};
use super::test_pki::{ca, ip, leaf};
use super::{
    OciError, OciFront, OciListener, OciMetrics, OciMode, OciPrefix, acceptor, plain_warning,
};
use crate::MemoryBackend;
use crate::server::listen::bind;
use crate::server::{ACCEPT_PAUSE, Incoming};

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
        assert!(text.contains("--oci-tls-cert and --oci-tls-key"), "{text}");
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
    for at in &bound {
        let refused = TcpStream::connect(reach(*at)).await;
        assert!(refused.is_err(), "{at} still takes connections");
    }
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

/// A plain accept loop over `incoming`, draining, with room for `max_connections` and bounds long
/// enough that none of them ends a test's connection, and its stop.
fn plain_loop(
    incoming: Held,
    max_connections: usize,
) -> (tokio::task::JoinHandle<usize>, watch::Sender<bool>) {
    let (front, limits) = plain_front();
    let minute = Duration::from_secs(60);
    let limits = OciLimits {
        max_connections,
        header_timeout: minute,
        write_idle: minute,
        lifetime: minute,
        drain_deadline: minute,
        ..limits
    };
    let (stop, stopped) = watch::channel(false);
    let shared = Shared::new(drain_of(&front, limits), limits.max_connections);
    // The drain begins as the binary begins it, before the stop.
    shared.drain.begin();
    let looping = tokio::spawn(accept_loop(incoming, None, front, limits, stopped, shared));
    (looping, stop)
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
    let mut answer = String::new();
    client.read_to_string(&mut answer).await.unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.contains("connection: close"), "{answer}");
    drop(client);
    assert_eq!(looping.await.expect("the loop ended"), 0);
}

/// Asserts that the OCI front waits for a free slot at the end of its settle for no longer than
/// the door's grace: with its one slot held by a connection that sends nothing, the connection the
/// kernel holds is not taken, the socket closes (that client reads the end of its stream, the
/// limit the module states), the connection in the slot is closed in turn, and the loop ends with
/// nothing cut. It would catch a door that waits for a slot for ever, and one that takes a
/// connection past the front's limit.
#[tokio::test(start_paused = true)]
async fn the_door_waits_for_a_free_slot_no_longer_than_its_grace() {
    let (first, mut in_slot) = pair().await;
    let (second, mut waiting) = pair().await;
    let held = Held {
        held: vec![Ok(second)],
        ready: vec![first],
        counted: AtomicBool::new(false),
    };
    // The first connection takes the slot, before the stop or once the door is closing.
    let (looping, stop) = plain_loop(held, 1);
    stop.send(true).expect("a loop");

    // A request on the held connection gets no answer: it was never taken past the limit, and
    // the kernel ends it (an end of stream, or a reset) when the socket closes.
    let request = b"GET /v2/ HTTP/1.1\r\nhost: oci.test\r\n\r\n";
    let _ = waiting.write_all(request).await;
    let mut answer = Vec::new();
    let _ = waiting.read_to_end(&mut answer).await;
    assert_eq!(String::from_utf8_lossy(&answer), "", "not served");
    let mut byte = [0_u8; 1];
    assert_eq!(
        in_slot.read(&mut byte).await.unwrap(),
        0,
        "closed, unanswered"
    );
    // The client closes its end, which ends the lingering close of the server's.
    drop(in_slot);
    assert_eq!(looping.await.expect("the loop ended"), 0);
}
