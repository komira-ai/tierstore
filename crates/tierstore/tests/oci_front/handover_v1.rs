//! `handover-v1`, the capability `tierstore --capabilities` lists, against the three behaviours it
//! promises the host updater (docs/design/tierstore-handover.md, sections 1, 4, 5 and 7): the
//! lock-based read-only mode, the wildcard listeners of both fronts, and the drain of both fronts
//! with `Connection: close` and a lingering close. The updater's `prepare` step reads the list to
//! choose a hand-over over a restart, so the list must not name it while any of the three is gone.
//!
//! The test runs the real binary as the new instance of a hand-over runs: a second build node on
//! a cache directory whose lock the first holds, listening on the wildcard of both families on one
//! port for each front. It needs no privilege; the hand-over itself, with the hook and nftables,
//! is T1 (tests/handover).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::process::Command;

use objstore_s3::s3::fake::FakeS3;

use crate::cache_node::{Launch, Node, counter, pattern, pull_whole_at, seed};
use crate::client::Conn;
use crate::rig::hex_of;

/// The capability, as the hook's `prepare` step looks for it.
const HANDOVER: &str = "handover-v1";

/// The two loopback addresses, one of each family, through which a client reaches a wildcard.
const LOOPBACKS: [IpAddr; 2] = [
    IpAddr::V4(Ipv4Addr::LOCALHOST),
    IpAddr::V6(Ipv6Addr::LOCALHOST),
];

/// The head of a request with its last empty line not sent: a request on the wire, begun and
/// not answered, which the drain answers when its head is complete.
const BEGUN_S3: &[u8] = b"GET /healthz HTTP/1.1\r\nhost: tier.test\r\n";
const BEGUN_OCI: &[u8] = b"GET /v2/ HTTP/1.1\r\nhost: oci.test\r\n";
/// What completes a begun head.
const END_OF_HEAD: &[u8] = b"\r\n";
/// A request a client writes after the server ended the stream: one the lingering close reads,
/// drops and counts.
const LATE: &[u8] = b"GET /healthz HTTP/1.1\r\nhost: late.test\r\n\r\n";

/// The fill and write families of the cache's block of `/metricz`: what a cache that writes to
/// its directory counts. A read-only instance writes nothing there, so each sample is 0.
const FILL_AND_WRITE: [&str; 7] = [
    "tierstore_cache_fills_total",
    "tierstore_cache_fills_skipped_total",
    "tierstore_cache_fills_skipped_busy_total",
    "tierstore_cache_fill_failures_total",
    "tierstore_cache_writes_through_total",
    "tierstore_cache_write_exists_total",
    "tierstore_cache_write_keep_skipped_total",
];

/// Two ports, one for each front, that are free on the wildcard of both families when this
/// returns: each is found by a socket on `[::]` (on Linux it holds the IPv4 port as well), and
/// both are held until both are found, so they differ. The node binds them itself, so the
/// sockets cannot be handed over (see `tierstore_testkit::port`).
fn free_ports() -> (u16, u16) {
    let bind = || TcpListener::bind("[::]:0").expect("a port on [::]");
    let (s3, oci) = (bind(), bind());
    let port = |probe: &TcpListener| probe.local_addr().expect("an address").port();
    (port(&s3), port(&oci))
}

/// The two wildcard addresses on `port`, the IPv6 one first.
fn wildcards(port: u16) -> Vec<String> {
    vec![format!("[::]:{port}"), format!("0.0.0.0:{port}")]
}

/// Asks `path` on `conn` and asserts the answer: status 200, `body`, and no `Connection: close`,
/// since no drain has begun.
async fn answered_open(conn: &mut Conn, path: &str, body: &str) {
    let reply = conn.request("GET", path, &[]).await.expect("an answer");
    assert_eq!(reply.status, 200, "{path}: {reply:?}");
    assert_eq!(String::from_utf8_lossy(&reply.body), body, "{path}");
    assert_eq!(reply.header("connection"), None, "{path}");
}

/// Every sample of the family `name` in the text of `/metricz`: the plain one, or each of its
/// labelled ones.
fn samples<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let named = text.lines().filter(|line| {
        line.strip_prefix(name)
            .is_some_and(|rest| rest.starts_with(' ') || rest.starts_with('{'))
    });
    named.collect()
}

/// Asserts the read-only mode of `node`, whose cache directory another instance holds the lock
/// of: `/readyz` says `ready read-only` on each family, the blob `missed` is served on each family
/// from the upstream (a miss at least once each time), and no fill or write family of `/metricz` counted
/// anything.
async fn read_only(node: &Node<'_>, missed: &[u8]) {
    let hex = hex_of(missed);
    for ip in LOOPBACKS {
        let mut conn = Conn::open_plain(SocketAddr::new(ip, node.s3_all[0].port()))
            .await
            .expect("a connection");
        answered_open(&mut conn, "/readyz", "ready read-only").await;
        pull_whole_at(SocketAddr::new(ip, node.oci_all[0].port()), &hex, missed).await;
    }
    let text = node.metrics().await;
    assert!(
        text.contains("tierstore_cache_mode{mode=\"read_only\"} 1\n"),
        "{text}"
    );
    assert!(
        counter(&text, "tierstore_cache_misses_total") >= 2,
        "{text}"
    );
    for family in FILL_AND_WRITE {
        let found = samples(&text, family);
        assert!(!found.is_empty(), "no {family} in {text}");
        for sample in found {
            assert!(sample.ends_with(" 0"), "{sample}");
        }
    }
}

/// The lines of `stderr` that name the late requests of a drain.
fn late_lines(stderr: &str) -> String {
    let late = stderr.lines().filter(|line| line.contains("too late"));
    late.map(|line| format!("{line}\n")).collect()
}

/// Asserts that a build node lists `handover-v1` only with what the name promises. The binary's
/// `--capabilities` names it; then a second node on the cache directory of a first, launched
/// with `[::]:P` and `0.0.0.0:P` for its S3 front and `[::]:Q` and `0.0.0.0:Q` for its OCI front:
///
/// - wildcard listeners: the start succeeds, the start line names the four addresses, and each
///   front answers on `127.0.0.1` and on `::1`;
/// - read-only mode: `/readyz` says `ready read-only` while the first node holds the lock, a blob
///   read through it is served from the upstream, and every sample of its fill and write
///   families stays 0;
/// - the drain, on each front: an idle connection reads a clean end of stream at the end of the
///   settle; a request whose head was begun before SIGTERM is answered, after the settle, with
///   `Connection: close` and then a clean end of stream; a request each of those four
///   connections writes after its end of stream is read by the lingering close and counted, so
///   the exit note names two late connections on each front, and the node exits 0.
///
/// Catches: `handover-v1` left out of the list; and, with it listed, a second instance that takes
/// the cache for read-write or fills it beside the holder, an IPv6 socket that is not
/// `IPV6_V6ONLY` (the IPv4 wildcard on its port is then refused and the node never starts), an
/// answer in the drain without `Connection: close` on either front, and a drain that closes a
/// connection without reading what the client still sends (the late requests are not counted,
/// and the client's next read may be a reset).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handover_v1_is_listed_with_the_three_behaviours_it_promises() {
    let out = Command::new(env!("CARGO_BIN_EXE_tierstore"))
        .arg("--capabilities")
        .output()
        .expect("ran");
    assert_eq!(out.status.code(), Some(0));
    let listed = String::from_utf8(out.stdout).expect("text");
    assert!(listed.lines().any(|name| name == HANDOVER), "{listed:?}");
    // A host without IPv6 cannot have the two wildcards of a port: the test says so on stderr,
    // with the reason the system gave, and asserts nothing more.
    if let Err(why) = TcpListener::bind("[::1]:0") {
        eprintln!("skipped: this host has no IPv6 loopback ({why}); there is no [::]:P to bind");
        return;
    }

    let fake = FakeS3::start().await;
    let missed = pattern(120_000);
    seed(&fake, &hex_of(&missed), &missed).await;
    let holder = Node::start(&fake, "handover-v1-holder", "");
    let mut probe = Conn::open_plain(holder.s3.parse().expect("an address"))
        .await
        .expect("a connection");
    answered_open(&mut probe, "/readyz", "ready read-write").await;
    drop(probe);

    let (s3_port, oci_port) = free_ports();
    let mut launch = Launch::loopback("1s");
    launch.listen = wildcards(s3_port);
    launch.oci_listen = wildcards(oci_port);
    launch.cache = Some(holder.cache_dir.clone());
    launch.stderr = true;
    let next = Node::launch(&fake, "handover-v1-next", &launch);
    let named = |all: &[SocketAddr]| all.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(named(&next.s3_all), wildcards(s3_port));
    assert_eq!(named(&next.oci_all), wildcards(oci_port));

    read_only(&next, &missed).await;

    // Two connections to each front, over both families: one idle after an answer, one with the
    // head of a request begun. The bytes are written before the signal, and the settle is long
    // beside the time the node takes to read them.
    let (v4, v6) = (LOOPBACKS[0], LOOPBACKS[1]);
    let mut s3_idle = Conn::open_plain(SocketAddr::new(v4, s3_port))
        .await
        .expect("a connection");
    answered_open(&mut s3_idle, "/healthz", "ok").await;
    let mut oci_idle = Conn::open_plain(SocketAddr::new(v6, oci_port))
        .await
        .expect("a connection");
    answered_open(&mut oci_idle, "/v2/", "{}").await;
    let mut s3_begun = Conn::open_plain(SocketAddr::new(v6, s3_port))
        .await
        .expect("a connection");
    s3_begun.write_raw(BEGUN_S3).await.expect("sent");
    let mut oci_begun = Conn::open_plain(SocketAddr::new(v4, oci_port))
        .await
        .expect("a connection");
    oci_begun.write_raw(BEGUN_OCI).await.expect("sent");

    next.signal_stop();
    // The end of the settle: each idle connection reads a clean end of stream, then writes a
    // request the server must read and drop, not answer with a reset.
    for idle in [&mut s3_idle, &mut oci_idle] {
        assert_eq!(idle.read_to_end().await.expect("a clean end"), b"");
        idle.write_raw(LATE).await.expect("written after the end");
        idle.shutdown().await.expect("closed");
    }
    // The drain has begun: a begun request is answered with `Connection: close`, and then the
    // stream ends cleanly.
    for (begun, body) in [(&mut s3_begun, "ok"), (&mut oci_begun, "{}")] {
        begun.write_raw(END_OF_HEAD).await.expect("sent");
        let reply = begun.reply(false).await.expect("an answer");
        let text = String::from_utf8_lossy(&reply.body);
        assert_eq!((reply.status, text.as_ref()), (200, body));
        assert_eq!(reply.header("connection"), Some("close"), "{reply:?}");
        assert_eq!(begun.read_to_end().await.expect("a clean end"), b"");
        begun.write_raw(LATE).await.expect("written after the end");
        begun.shutdown().await.expect("closed");
    }

    let (code, stderr) = next.exit_and_stderr().await;
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(
        late_lines(&stderr),
        "tierstore: 2 connections sent a request too late in the drain of the S3 front\n\
         tierstore: 2 connections sent a request too late in the drain of the OCI front\n",
        "{stderr}"
    );
    assert_eq!(holder.stop().await, Some(0));
}
