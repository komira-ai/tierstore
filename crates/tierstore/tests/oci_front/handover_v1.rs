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

use std::collections::BTreeMap;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use bytes::Bytes;
use objstore_s3::s3::fake::FakeS3;
use objstore_s3::s3::{Credentials, S3Config, S3Store};
use objstore_s3::{Capabilities, ObjectKey, ObjectStore};

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

/// The settle of the new instance: longer than the test, so that everything it asserts of the
/// drain happens while the drain has begun and the settle has not ended. Nothing waits for it to
/// end: the node is killed when the test is over.
const SETTLE: &str = "1h";

/// The longest the test waits for an event of the node. It only stops a test whose node never
/// does what it waits for, and then the test fails: nothing asserts how long anything takes.
const SAFETY: Duration = Duration::from_secs(60);

/// The key of `[local_cache]` of both nodes: a client's put goes to the upstream, and a node
/// that holds the lock keeps a copy.
const WRITE_THROUGH: &str = "write_through = true";

/// A request a client writes after the server ended the stream: one the lingering close reads,
/// drops and counts.
const LATE: &[u8] = b"GET /healthz HTTP/1.1\r\nhost: late.test\r\n\r\n";

/// The fill families of the cache's block of `/metricz`. A read-only instance fills nothing, so
/// each of their samples is 0.
const FILL: [&str; 4] = [
    "tierstore_cache_fills_total",
    "tierstore_cache_fills_skipped_total",
    "tierstore_cache_fills_skipped_busy_total",
    "tierstore_cache_fill_failures_total",
];

/// The key a client puts through the read-only instance.
const WRITTEN: &str = "oci/written-beside-the-holder";

/// Two ports, one for each front, that are free on the wildcard of both families when this
/// returns: each is found by a socket on `[::]` (on Linux with the default
/// `net.ipv6.bindv6only = 0` it holds the IPv4 port as well), and both are held until both are
/// found, so they differ. The node binds them itself, so the sockets cannot be handed over (see
/// `tierstore_testkit::port`).
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

/// A plain connection to `ip` on `port`.
async fn conn(ip: IpAddr, port: u16) -> Conn {
    let opened = Conn::open_plain(SocketAddr::new(ip, port)).await;
    opened.expect("a connection")
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
/// labelled ones. At least one.
#[track_caller]
fn samples<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let named = text.lines().filter(|line| {
        line.strip_prefix(name)
            .is_some_and(|rest| rest.starts_with(' ') || rest.starts_with('{'))
    });
    let found: Vec<&str> = named.collect();
    assert!(!found.is_empty(), "no {name} in {text}");
    found
}

/// Every file and directory under `root`, with its size (0 for a directory) and mode: what a
/// read-only instance must leave as it is.
fn tree(root: &Path) -> BTreeMap<PathBuf, (u64, u32)> {
    let mut files = BTreeMap::new();
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).expect("a directory") {
            let path = entry.expect("an entry").path();
            let meta = fs::symlink_metadata(&path).expect("metadata");
            let size = if meta.is_dir() { 0 } else { meta.len() };
            let relative = path.strip_prefix(root).expect("under the root").to_owned();
            files.insert(relative, (size, meta.permissions().mode()));
            if meta.is_dir() {
                dirs.push(path);
            }
        }
    }
    files
}

/// The data file and the sidecar of `key` in the cache directory `cache`, as the cache names
/// them: by the SHA-256 of the key.
fn object_files(cache: &Path, key: &str) -> (PathBuf, PathBuf) {
    let named = hex_of(key.as_bytes());
    let dir = cache.join("obj").join(&named[..2]).join(&named[2..4]);
    let sidecar = dir.join(format!("{named}.meta"));
    (dir.join(named), sidecar)
}

/// Puts in the holder's cache directory what a holder at work leaves there, which a read-only
/// open must not touch: a fill in progress under `.stage/`, and the sidecar of an object whose
/// publish has renamed the sidecar and not yet linked the data file.
fn plant_holder_work(cache: &Path) {
    let staged = cache.join(".stage").join("fill-in-progress");
    fs::write(staged, b"half a fill").expect("a stage file");
    let (data, sidecar) = object_files(cache, "oci/blobs/sha256/being-published");
    fs::create_dir_all(sidecar.parent().expect("a parent")).expect("the object directory");
    fs::write(&sidecar, br#"{"size":3}"#).expect("a sidecar");
    assert!(!data.exists());
}

/// The client of the S3 front at `at`, with the node's key.
fn s3_client(at: &str) -> S3Store {
    let config = S3Config {
        endpoint: format!("http://{at}"),
        region: "us-east-1".to_owned(),
        bucket: "tier-bucket".to_owned(),
        credentials: Credentials::new("AKIDCACHE", "cache-secret"),
        capabilities: Capabilities {
            conditional_put: true,
            object_lock: false,
        },
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
        min_body_rate: None,
    };
    S3Store::new(config).expect("a client")
}

/// Asserts that both wildcards of each front of `node` are bound and served: the start line names
/// exactly them, and each front answers on `127.0.0.1` and on `::1` (the S3 front's `/readyz`,
/// the OCI front's pull of `missed`, a miss each time).
async fn both_families_answer(node: &Node<'_>, ports: (u16, u16), missed: &[u8]) {
    let named = |all: &[SocketAddr]| all.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(named(&node.s3_all), wildcards(ports.0));
    assert_eq!(named(&node.oci_all), wildcards(ports.1));
    for ip in LOOPBACKS {
        answered_open(&mut conn(ip, ports.0).await, "/readyz", "ready read-only").await;
        pull_whole_at(SocketAddr::new(ip, ports.1), &hex_of(missed), missed).await;
    }
}

/// Asserts the read-only mode of `node`, whose cache directory `cache` the holder has the lock
/// of: the blob `kept`, which the holder filled before `node` started, is a hit from the shared
/// directory; a put goes to the upstream and is not kept; no fill family counted anything; and
/// the directory, with the holder's stage file and half-published sidecar, is still `before`,
/// what it was before `node` started.
async fn read_only(
    node: &Node<'_>,
    fake: &FakeS3,
    cache: &Path,
    before: &BTreeMap<PathBuf, (u64, u32)>,
    kept: &[u8],
) {
    pull_whole_at(node.oci, &hex_of(kept), kept).await;
    let key = ObjectKey::new(WRITTEN).expect("a key");
    let client = s3_client(&node.s3);
    let put = client
        .put_new(&key, Bytes::from_static(b"through"), None)
        .await;
    put.expect("a put through the read-only instance");
    let upstream = fake.keys();
    assert!(
        upstream.iter().any(|k| k.ends_with(WRITTEN)),
        "{upstream:?}"
    );

    let text = node.metrics().await;
    assert!(
        text.contains("tierstore_cache_mode{mode=\"read_only\"} 1\n"),
        "{text}"
    );
    assert!(counter(&text, "tierstore_cache_hits_total") >= 1, "{text}");
    assert!(
        counter(&text, "tierstore_cache_misses_total") >= 2,
        "{text}"
    );
    for family in FILL {
        for sample in samples(&text, family) {
            assert!(sample.ends_with(" 0"), "{sample}");
        }
    }
    assert_eq!(
        counter(&text, "tierstore_cache_writes_through_total"),
        1,
        "{text}"
    );
    assert_eq!(
        counter(&text, "tierstore_cache_write_exists_total"),
        0,
        "{text}"
    );
    for sample in samples(&text, "tierstore_cache_write_keep_skipped_total") {
        let read_only = sample.contains("reason=\"read_only\"");
        let want = if read_only { " 1" } else { " 0" };
        assert!(sample.ends_with(want), "{sample}");
    }
    assert_eq!(&tree(cache), before);
    assert!(!object_files(cache, WRITTEN).0.exists(), "the put was kept");
    let mut ready = conn(LOOPBACKS[1], node.s3_all[0].port()).await;
    answered_open(&mut ready, "/readyz", "ready read-only").await;
}

/// Asserts the end of a connection the drain closed: a clean end of stream, not a reset, and a
/// request the client writes after it is taken by the server, which goes on reading.
async fn ends_cleanly_and_reads_on(conn: &mut Conn) {
    assert_eq!(conn.read_to_end().await.expect("a clean end"), b"");
    conn.write_raw(LATE).await.expect("written after the end");
    conn.shutdown().await.expect("closed");
}

/// Asserts the drain of both fronts of `node`, in its draining phase (its settle outlasts the
/// test): once SIGTERM is sent, the first `/readyz` that says 503 on a kept-alive S3 connection
/// carries `Connection: close`, as does the next answer of the OCI front on a kept-alive
/// connection; each of the two connections then ends with a clean end of stream; and the request
/// each writes after it is read and counted by the lingering close: each front's late counter
/// reaches exactly 1.
async fn drains_with_close_and_linger(node: &Node<'_>, ports: (u16, u16)) {
    let (v4, v6) = (LOOPBACKS[0], LOOPBACKS[1]);
    let mut s3 = conn(v6, ports.0).await;
    answered_open(&mut s3, "/readyz", "ready read-only").await;
    let mut oci = conn(v4, ports.1).await;
    answered_open(&mut oci, "/v2/", "{}").await;

    node.signal_stop();
    // The drain begins at some point after the signal: `/readyz` is asked on a connection of its
    // own each time until it says 503. An answer in the instant the drain begins may be a 200
    // that already says `Connection: close` (the status and the header are read apart), so
    // these answers are not judged.
    let draining = async {
        loop {
            let mut probe = conn(v4, ports.0).await;
            let reply = probe.request("GET", "/readyz", &[]).await;
            if reply.is_some_and(|reply| reply.status == 503) {
                break;
            }
        }
    };
    let begun = tokio::time::timeout(SAFETY, draining).await;
    begun.expect("the drain never began");
    let reply = s3.request("GET", "/readyz", &[]).await.expect("an answer");
    assert_eq!(reply.status, 503, "{reply:?}");
    assert_eq!(reply.header("connection"), Some("close"), "{reply:?}");
    ends_cleanly_and_reads_on(&mut s3).await;

    // The binary begins the OCI front's drain before the S3 front's.
    let reply = oci.request("GET", "/v2/", &[]).await.expect("an answer");
    assert_eq!(reply.status, 200, "{reply:?}");
    assert_eq!(reply.header("connection"), Some("close"), "{reply:?}");
    ends_cleanly_and_reads_on(&mut oci).await;

    // The S3 front still serves in the settle: its `/metricz` shows each count once the lingering
    // close has read the late request.
    let names = [
        "tierstore_drain_late_requests_total",
        "tierstore_oci_drain_late_requests_total",
    ];
    let counted = async {
        loop {
            let text = node.metrics().await;
            if names.iter().all(|name| counter(&text, name) >= 1) {
                break text;
            }
        }
    };
    let text = tokio::time::timeout(SAFETY, counted).await;
    let text = text.expect("the late requests were never counted");
    for name in names {
        assert_eq!(counter(&text, name), 1, "{text}");
    }
}

/// The lines of `stderr` that name the late requests of a drain.
fn late_lines(stderr: &str) -> String {
    let late = stderr.lines().filter(|line| line.contains("too late"));
    late.map(|line| format!("{line}\n")).collect()
}

/// Asserts the end of the settle of `node` on both fronts: a connection idle since its answer
/// before SIGTERM, one to each front, reads a clean end of stream when the settle ends, and the
/// request it writes after that is read and counted by the lingering close, so the node's exit
/// note names one late connection on each front; and the node exits 0.
async fn idle_connections_linger_at_the_end_of_the_settle(node: Node<'_>) {
    let ip = LOOPBACKS[0];
    let mut s3 = conn(ip, node.s3_all[0].port()).await;
    answered_open(&mut s3, "/healthz", "ok").await;
    let mut oci = conn(ip, node.oci_all[0].port()).await;
    answered_open(&mut oci, "/v2/", "{}").await;
    node.signal_stop();
    ends_cleanly_and_reads_on(&mut s3).await;
    ends_cleanly_and_reads_on(&mut oci).await;
    let (code, stderr) = node.exit_and_stderr().await;
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(
        late_lines(&stderr),
        "tierstore: 1 connections sent a request too late in the drain of the S3 front\n\
         tierstore: 1 connections sent a request too late in the drain of the OCI front\n",
        "{stderr}"
    );
}

/// Asserts that a build node lists `handover-v1` only with what the name promises. The binary's
/// `--capabilities` names it. A first node holds the lock of a cache directory, has filled a blob
/// into it, and has a stage file and a half-published sidecar there; then a second node on that
/// directory, with write-through, is launched with `[::]:P` and `0.0.0.0:P` for its S3 front and
/// `[::]:Q` and `0.0.0.0:Q` for its OCI front:
///
/// - wildcard listeners: the start succeeds, the start line names the four addresses, and each
///   front answers on `127.0.0.1` and on `::1`;
/// - read-only mode: `/readyz` says `ready read-only`; the holder's blob is a hit; a blob the
///   holder does not have is served from the upstream and not filled; a client's put reaches the
///   upstream (`writes_through_total` 1) and is not kept (`write_keep_skipped_total` 1 for
///   `read_only`, 0 for every other reason); no fill family counts anything; and the directory is
///   as it was, the holder's stage file and sidecar included;
/// - the drain, on each front, while the drain has begun and the settle has not ended (the new
///   instance): once `/readyz` says 503, the next answer on a kept-alive connection of each front
///   carries `Connection: close`; each connection then reads a clean end of stream, and a request
///   it writes after that is read and counted by the lingering close (each late counter 1);
/// - the drain at the end of the settle (the holder, whose settle is short): a connection to each
///   front idle since before SIGTERM reads a clean end of stream when the settle ends, the
///   request it writes after that is counted (the exit note names one on each front), and the
///   node exits 0.
///
/// Catches: `handover-v1` left out of the list; and, with it listed, a second instance that takes
/// the cache for read-write, fills or keeps beside the holder, or clears the holder's stage or
/// sidecar at its start; an IPv6 socket that is not `IPV6_V6ONLY` (the IPv4 wildcard on its port
/// is then refused and the node never starts); an answer in the drain without `Connection: close`
/// on either front, including one that adds it only at the end of the settle; and a drain that
/// closes a connection, answered or idle at the end of the settle, without reading what the
/// client still sends (the late request is not counted, and the client's next read may be a
/// reset).
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
    let (kept, missed) = (pattern(100_000), pattern(120_000));
    seed(&fake, &hex_of(&kept), &kept).await;
    seed(&fake, &hex_of(&missed), &missed).await;
    let mut first = Launch::loopback("50ms");
    first.local = WRITE_THROUGH.to_owned();
    first.stderr = true;
    let holder = Node::launch(&fake, "handover-v1-holder", &first);
    let mut probe = conn(LOOPBACKS[0], holder.s3_all[0].port()).await;
    answered_open(&mut probe, "/readyz", "ready read-write").await;
    drop(probe);
    holder.pull_whole(&hex_of(&kept), &kept).await;
    let filled = holder.metrics().await;
    assert_eq!(
        counter(&filled, "tierstore_cache_fills_total"),
        1,
        "{filled}"
    );
    plant_holder_work(&holder.cache_dir);
    let before = tree(&holder.cache_dir);
    assert!(
        before.contains_key(Path::new(".stage/fill-in-progress")),
        "{before:?}"
    );

    let ports = free_ports();
    let mut launch = Launch::loopback(SETTLE);
    launch.listen = wildcards(ports.0);
    launch.oci_listen = wildcards(ports.1);
    launch.cache = Some(holder.cache_dir.clone());
    launch.local = WRITE_THROUGH.to_owned();
    let next = Node::launch(&fake, "handover-v1-next", &launch);

    both_families_answer(&next, ports, &missed).await;
    read_only(&next, &fake, &holder.cache_dir, &before, &kept).await;
    drains_with_close_and_linger(&next, ports).await;

    // The new instance's settle outlasts the test: it is killed.
    next.kill().await;
    idle_connections_linger_at_the_end_of_the_settle(holder).await;
}
