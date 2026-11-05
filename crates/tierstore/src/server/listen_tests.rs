//! The sockets of a front that listens on more than one address, and the `addresses` param that
//! names them.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use super::{
    Addrs, ListenTuning, Listeners, LoopbackBind, bind, bind_all, bind_all_with, bind_set,
    bind_with, joined, local_addrs, note_nodelay,
};
use crate::server::Drained;
use crate::server::Incoming as _;
use crate::server::cli_serve::bind_s3;
use crate::server::testing::{Rig, established_unaccepted};

fn addr(text: &str) -> SocketAddr {
    text.parse().expect("an address")
}

/// Whether this host has IPv6 loopback, decided by trying to bind it.
fn has_v6() -> bool {
    std::net::TcpListener::bind(addr("[::1]:0")).is_ok()
}

/// The addresses a test listens on: IPv4 loopback and the IPv4 wildcard, which are two sockets
/// on every host, and IPv6 loopback where the host has it.
fn wanted() -> Vec<SocketAddr> {
    let mut wanted = vec![addr("127.0.0.1:0"), addr("0.0.0.0:0")];
    if has_v6() {
        wanted.push(addr("[::1]:0"));
    }
    wanted
}

/// The address a client dials to reach a socket bound at `at`: a wildcard is reached by way of
/// loopback.
fn reach(at: SocketAddr) -> SocketAddr {
    let ip = match at.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, at.port())
}

/// One request of `/healthz` at `at`, answered and closed: the whole reply.
async fn healthz(at: SocketAddr) -> String {
    let mut conn = TcpStream::connect(reach(at)).await.expect("connected");
    let request = "GET /healthz HTTP/1.1\r\nHost: tierstore\r\nConnection: close\r\n\r\n";
    conn.write_all(request.as_bytes()).await.expect("written");
    let mut reply = Vec::new();
    conn.read_to_end(&mut reply).await.expect("read");
    String::from_utf8_lossy(&reply).into_owned()
}

/// A server over `wanted`, serving all of them, with the addresses they are bound to and the
/// task that serves.
async fn started(wanted: &[SocketAddr]) -> (Rig, Vec<SocketAddr>, JoinHandle<Drained>) {
    let rig = Rig::with(1000, |config| config.drain_lb_wait = Duration::ZERO).await;
    let listeners = bind_all(wanted).expect("bound");
    let bound = local_addrs(&listeners).expect("addresses");
    let server = Arc::clone(&rig.server);
    let served = tokio::spawn(async move { server.serve_all(listeners).await });
    (rig, bound, served)
}

/// Asserts that one server takes the connections of every listener it is given: each address
/// answers `/healthz`, and the sockets are as many as were asked for, on ports of their own.
/// A server that served only the first would leave the others unanswered.
#[tokio::test]
async fn every_listener_serves() {
    let wanted = wanted();
    let (rig, bound, served) = started(&wanted).await;
    assert_eq!(bound.len(), wanted.len(), "{bound:?}");
    let mut ports: Vec<u16> = bound.iter().map(SocketAddr::port).collect();
    ports.sort_unstable();
    ports.dedup();
    assert_eq!(ports.len(), wanted.len(), "{bound:?}");
    for at in &bound {
        let reply = healthz(*at).await;
        assert!(reply.starts_with("HTTP/1.1 200 "), "{at}: {reply}");
    }
    rig.server.begin_drain();
    assert_eq!(served.await.expect("served"), Drained { abandoned: 0 });
}

/// Asserts that the drain of the one server stops every listener: once it has ended, no address
/// takes a connection, the first or any other, and the server has none open.
#[tokio::test]
async fn the_drain_stops_every_listener() {
    let wanted = wanted();
    let (rig, bound, served) = started(&wanted).await;
    assert_eq!(bound.len(), wanted.len(), "{bound:?}");
    for at in &bound {
        let reply = healthz(*at).await;
        assert!(reply.starts_with("HTTP/1.1 200 "), "{at}: {reply}");
    }
    rig.server.begin_drain();
    assert_eq!(served.await.expect("served"), Drained { abandoned: 0 });
    assert_eq!(rig.server.open_connections(), 0);
    for at in &bound {
        let refused = TcpStream::connect(reach(*at)).await;
        assert!(refused.is_err(), "{at} still takes connections");
    }
}

/// Asserts that the wildcard of each family can be bound on one port: `0.0.0.0:P` and `[::]:P`,
/// as a redirect of a remote packet needs, and that each takes the connections of its own family.
/// Without `IPV6_V6ONLY` the second bind is refused, as the IPv6 socket would hold the IPv4 port
/// too.
#[tokio::test]
async fn a_v6_wildcard_and_a_v4_wildcard_bind_on_one_port() {
    if !has_v6() {
        return;
    }
    let v4 = bind(addr("0.0.0.0:0")).expect("the v4 wildcard");
    let port = v4.local_addr().expect("an address").port();
    let v6 = bind(addr(&format!("[::]:{port}"))).expect("the v6 wildcard, on the same port");
    let (dialed, accepted) = tokio::join!(
        TcpStream::connect(reach(addr(&format!("0.0.0.0:{port}")))),
        v4.accept()
    );
    dialed.expect("dialed the v4 socket");
    assert!(accepted.expect("accepted").1.is_ipv4());
    let (dialed, accepted) = tokio::join!(
        TcpStream::connect(reach(addr(&format!("[::]:{port}")))),
        v6.accept()
    );
    dialed.expect("dialed the v6 socket");
    assert!(accepted.expect("accepted").1.is_ipv6());
}

/// Asserts that the sockets bound before a failed bind of the same set are released: the set
/// `[free, busy]` fails on `busy`, and then the `free` address (a port found by binding one and
/// giving it up) can be bound again.
#[tokio::test]
async fn the_sockets_bound_before_a_failed_bind_are_released() {
    let held = bind(addr("127.0.0.1:0")).expect("a first listener");
    let busy = held.local_addr().expect("an address");
    let probe = bind(addr("127.0.0.1:0")).expect("a probe");
    let free = probe.local_addr().expect("an address");
    drop(probe);
    let failed = bind_all(&[free, busy]).err();
    assert_eq!(failed.expect("the set is refused").0, busy);
    let again = bind(free).expect("the first address of the failed set is free again");
    assert_eq!(again.local_addr().expect("an address"), free);
}

/// Asserts that an address that cannot be bound fails the whole set and is the one the error
/// names, and that the reuse of an address (`SO_REUSEADDR`) does not let a second listener take
/// the address of a first.
#[tokio::test]
async fn an_address_that_cannot_be_bound_is_named_and_fails_the_set() {
    let held = bind(addr("127.0.0.1:0")).expect("a first listener");
    let busy = held.local_addr().expect("an address");
    let failed = bind_all(&[addr("0.0.0.0:0"), busy]).err();
    let (at, why) = failed.expect("the set is refused");
    assert_eq!(at, busy);
    assert_eq!(why.kind(), std::io::ErrorKind::AddrInUse);
}

/// The addresses as the start line shows them.
#[test]
fn the_start_line_joins_the_addresses_with_commas() {
    let two = [addr("0.0.0.0:9400"), addr("[::]:9400")];
    assert_eq!(joined(&two), "0.0.0.0:9400,[::]:9400");
    assert_eq!(joined(&two[..1]), "0.0.0.0:9400");
}

/// A table of one `addresses` key, as the params of an interface have it.
#[derive(Debug, Deserialize)]
struct Table {
    addresses: Addrs,
}

/// Asserts that an `addresses` param reads one address as a string and several as a list, and a
/// list of one as the string.
#[test]
fn an_addresses_param_is_a_string_or_a_list() {
    let one: Table = toml::from_str("addresses = \"192.0.2.1:9400\"").expect("a string");
    assert_eq!(one.addresses.0, [addr("192.0.2.1:9400")]);
    let listed: Table = toml::from_str("addresses = [\"192.0.2.1:9400\"]").expect("a list of one");
    assert_eq!(listed.addresses, one.addresses);
    let two: Table =
        toml::from_str("addresses = [\"0.0.0.0:9400\", \"[::]:9400\"]").expect("a list");
    assert_eq!(two.addresses.0, [addr("0.0.0.0:9400"), addr("[::]:9400")]);
}

/// Asserts that a value that is no address is refused without showing it, in a list as alone.
#[test]
fn an_addresses_value_that_is_no_address_is_refused_without_its_text() {
    for text in [
        "addresses = \"secret-word\"",
        "addresses = [\"192.0.2.1:9400\", \"secret-word\"]",
    ] {
        let why = toml::from_str::<Table>(text).expect_err("refused");
        assert!(!why.message().contains("secret-word"), "{}", why.message());
    }
}

fn addrs(texts: &[&str]) -> Vec<SocketAddr> {
    texts.iter().map(|text| addr(text)).collect()
}

/// Asserts that with a local cache the bind set is the configured addresses and then 127.0.0.1 on
/// each distinct port, once: an address of another family or host names only its port.
#[test]
fn a_local_cache_adds_loopback_on_each_distinct_port_after_the_configured_addresses() {
    let one = addrs(&["192.0.2.10:9300"]);
    assert_eq!(
        bind_set(&one, true),
        addrs(&["192.0.2.10:9300", "127.0.0.1:9300"])
    );
    let several = addrs(&["192.0.2.10:9300", "192.0.2.11:9300", "192.0.2.10:9301"]);
    assert_eq!(
        bind_set(&several, true),
        addrs(&[
            "192.0.2.10:9300",
            "192.0.2.11:9300",
            "192.0.2.10:9301",
            "127.0.0.1:9300",
            "127.0.0.1:9301"
        ])
    );
    let v6 = addrs(&["[2001:db8::1]:9300", "[::]:9301"]);
    assert_eq!(
        bind_set(&v6, true),
        addrs(&[
            "[2001:db8::1]:9300",
            "[::]:9301",
            "127.0.0.1:9300",
            "127.0.0.1:9301"
        ])
    );
}

/// Asserts that without a local cache the bind set is exactly the configured addresses.
#[test]
fn without_a_local_cache_loopback_is_not_added() {
    let configured = addrs(&["192.0.2.10:9300", "[2001:db8::1]:9301"]);
    assert_eq!(bind_set(&configured, false), configured);
}

/// Asserts that a port that already has 127.0.0.1 or the IPv4 wildcard gets nothing added, so no
/// address is bound twice; the other ports of the same list still get loopback.
#[test]
fn a_port_with_loopback_or_the_wildcard_gets_nothing_added() {
    let listed = addrs(&["127.0.0.1:9300", "192.0.2.10:9300"]);
    assert_eq!(bind_set(&listed, true), listed);
    let wildcard = addrs(&["0.0.0.0:9300", "[::]:9300"]);
    assert_eq!(bind_set(&wildcard, true), wildcard);
    let mixed = addrs(&["0.0.0.0:9300", "192.0.2.10:9301"]);
    assert_eq!(
        bind_set(&mixed, true),
        addrs(&["0.0.0.0:9300", "192.0.2.10:9301", "127.0.0.1:9301"])
    );
}

/// Asserts that the error of a loopback socket that cannot be bound says loopback, the address
/// with its port, and the system's reason.
#[test]
fn a_loopback_bind_error_names_loopback_the_port_and_the_reason() {
    let why = LoopbackBind {
        addr: addr("127.0.0.1:9300"),
        why: std::io::Error::other("address in use"),
    };
    let text = why.to_string();
    assert!(text.contains("loopback"), "{text}");
    assert!(text.contains("127.0.0.1:9300"), "{text}");
    assert!(text.contains("address in use"), "{text}");
}

/// Asserts that a connection taken from either of two sockets of `Listeners` has `TCP_NODELAY`
/// set, and that a client's own socket (which this code does not touch) does not.
/// It would catch an accept that returns the stream as the kernel made it, with Nagle's
/// algorithm on: the response head and a small body would then be two segments, the second held
/// until the client's delayed ACK.
#[tokio::test]
async fn accepted_connections_have_nodelay() {
    let sockets = vec![
        bind(addr("127.0.0.1:0")).expect("bound"),
        bind(addr("127.0.0.1:0")).expect("bound"),
    ];
    let mut listeners = Listeners::new(sockets);
    for at in listeners.local_addrs().expect("addresses") {
        let client = std::net::TcpStream::connect(at).expect("connected");
        let (stream, _) = std::future::poll_fn(|cx| listeners.poll_incoming(cx))
            .await
            .expect("accepted");
        assert!(stream.nodelay().expect("option read"));
        assert!(!client.nodelay().expect("option read"));
    }
}

/// Asserts that a failed `TCP_NODELAY` is only noted, and a successful one is too: neither
/// panics, so the connection is served either way.
#[test]
fn a_failed_nodelay_is_not_fatal() {
    note_nodelay(Ok(()));
    note_nodelay(Err(std::io::Error::other("refused")));
}

/// Asserts that the backlog given to the bind makes a listening socket that takes connections, for
/// one socket and for each socket of a set, and that the default is the 1024 the code had. (The
/// length of the queue is the kernel's to keep; that it is the one given is checked at the
/// settings, in `front_tests` and `oci_cli_tests`.)
#[tokio::test]
async fn a_socket_bound_with_a_backlog_takes_connections() {
    let listener = bind_with(addr("127.0.0.1:0"), 1).expect("bound");
    let at = listener.local_addr().expect("an address");
    let client = TcpStream::connect(at).await.expect("connected");
    let (_taken, from) = listener.accept().await.expect("accepted");
    assert_eq!(Some(from), client.local_addr().ok());
    let set = [addr("127.0.0.1:0"), addr("127.0.0.1:0")];
    assert_eq!(bind_all_with(&set, 11).expect("bound").len(), 2);
    assert_eq!(ListenTuning::default().backlog, 1024);
}

/// Asserts that the backlog given to the S3 bind of the server (`bind_s3`) is the one the sockets
/// listen with: with a backlog of 1, of six connections no more than the queue and the one the
/// kernel lets over it are established while nothing accepts, for each address; with the default
/// backlog more than that are.
#[tokio::test]
async fn the_s3_bind_listens_with_the_backlog_given() {
    let wanted = [addr("127.0.0.1:0"), addr("127.0.0.1:0")];
    let small = bind_s3(&wanted, 1).expect("bound");
    for listener in &small {
        let at = listener.local_addr().expect("an address");
        let held = established_unaccepted(at, 6).await;
        assert!((1..=3).contains(&held), "{held} established on {at}");
    }
    let large = bind_s3(&wanted, ListenTuning::default().backlog).expect("bound");
    for listener in &large {
        let at = listener.local_addr().expect("an address");
        assert!(established_unaccepted(at, 6).await > 3, "{at}");
    }
}
