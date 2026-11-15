//! The trees listener over real sockets: calls over HTTP/2, plain and over TLS, the statuses a
//! caller sees, and the stop.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::Full;
use hyper::client::conn::http2::SendRequest;
use prost::Message as _;
use tierstore_testkit::dir::TestDir;
use tierstore_trees_api::v1::{
    CreateTreeRequest, CreateTreeResponse, DeleteTreeRequest, DeleteTreeResponse, Entry, File,
    GetPlacementRequest, GetPlacementResponse, Location, S3Location, Source, entry, location,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;
use tonic::Code;

use super::listener::{TreesLimits, accept_loop};
use super::rig::{
    Answer, FULL, FakeCore, READER, SCOPED, assert_refused, config, connect, connect_driven,
    framed, send, signed_request,
};
use super::service::{Served, TreesFront};
use super::{Caller, Reason, Refusal, TreeCore, TreesFlags, trees_setup_of};
use crate::server::oci::test_pki::{ca, ip, leaf};
use crate::server::{Incoming, ListenTuning, ServerConfig};

/// A listener that is serving, and what a client of it needs.
struct Running {
    addr: SocketAddr,
    stop: watch::Sender<bool>,
    task: JoinHandle<usize>,
    server: ServerConfig,
    connector: Option<TlsConnector>,
    _dir: TestDir,
}

/// What a test changes of the default: plain HTTP/2 at a port the system picks, no core.
#[derive(Default)]
struct Wanted {
    core: Option<Arc<dyn TreeCore>>,
    tls: bool,
    max_connections: Option<u32>,
    lifetime: Option<Duration>,
    max_request_bytes: Option<u64>,
}

async fn run(wanted: Wanted) -> Running {
    let dir = TestDir::under(std::env::temp_dir(), "trees-listener");
    let mut flags = TreesFlags {
        listen: vec!["127.0.0.1:0".parse().expect("an address")],
        max_connections: wanted.max_connections,
        connection_lifetime: wanted.lifetime,
        max_request_bytes: wanted.max_request_bytes,
        ..TreesFlags::default()
    };
    let mut connector = None;
    if wanted.tls {
        let authority = ca();
        let files = leaf(dir.path(), "front", &authority, vec![ip("127.0.0.1")]);
        flags.tls_cert = Some(files.cert);
        flags.tls_key = Some(files.key);
        connector = Some(connector_for(&authority.pem()));
    }
    let server = config();
    let mut setup = trees_setup_of(&flags, &server, false)
        .expect("setup")
        .expect("a front");
    setup.core = wanted.core;
    let listener = setup.bind().await.expect("bound");
    let addr = listener.local_addrs().expect("addresses")[0];
    let (stop, stopped) = watch::channel(false);
    let task = tokio::spawn(listener.serve(stopped));
    Running {
        addr,
        stop,
        task,
        server,
        connector,
        _dir: dir,
    }
}

/// A TLS client that trusts the CA of `pem` and speaks HTTP/2.
fn connector_for(pem: &str) -> TlsConnector {
    use rustls::pki_types::pem::PemObject as _;
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls::pki_types::CertificateDer::pem_slice_iter(pem.as_bytes()) {
        roots.add(cert.expect("a certificate")).expect("a root");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    TlsConnector::from(Arc::new(config))
}

impl Running {
    async fn client(&self) -> SendRequest<Full<Bytes>> {
        connect(self.addr, self.connector.as_ref()).await
    }

    /// One call of `method` with `message`, signed with `key`.
    async fn call(
        &self,
        sender: &mut SendRequest<Full<Bytes>>,
        key: (&str, &str),
        method: &str,
        message: &impl prost::Message,
    ) -> Answer {
        let authority = self.addr.to_string();
        let request = signed_request(&self.server, key, &authority, method, framed(message));
        send(sender, request).await
    }

    /// Stops the listener and returns how many connections it cut.
    async fn stopped(self) -> usize {
        self.stop.send(true).expect("a listener");
        self.task.await.expect("the listener ended")
    }
}

fn recording() -> (Arc<FakeCore>, Wanted) {
    let core = Arc::new(FakeCore::default());
    let wanted = Wanted {
        core: Some(Arc::clone(&core) as Arc<dyn TreeCore>),
        ..Wanted::default()
    };
    (core, wanted)
}

fn create_request() -> CreateTreeRequest {
    let file = File {
        mode: None,
        sha256: vec![1; 32],
        size: 1,
        from: Some(Source {
            key: "cas/b/1".to_owned(),
            offset: 0,
        }),
    };
    CreateTreeRequest {
        entries: vec![Entry {
            path: "bin/cc".to_owned(),
            kind: Some(entry::Kind::File(file)),
        }],
        at: vec![Location {
            r#where: Some(location::Where::S3(S3Location {
                bucket: "tier-bucket".to_owned(),
                prefix: "jobs/1/".to_owned(),
            })),
        }],
        ..CreateTreeRequest::default()
    }
}

/// Asserts a signed call over plain HTTP/2 reaches the core with the key that signed it and is
/// answered with the core's answer, for each of the three calls of the service.
#[tokio::test]
async fn a_signed_call_reaches_the_core_over_plain_http2() {
    let (core, wanted) = recording();
    let running = run(wanted).await;
    let mut sender = running.client().await;
    let answer = running
        .call(&mut sender, FULL, "CreateTree", &create_request())
        .await;
    assert_eq!(answer.status.code(), Code::Ok);
    let made = CreateTreeResponse::decode(answer.message.as_slice()).expect("an answer");
    assert_eq!(
        (made.placement.as_str(), made.entries, made.created),
        ("p1", 1, true)
    );

    let ask = GetPlacementRequest {
        placement: "p1".to_owned(),
        list: None,
    };
    let answer = running
        .call(&mut sender, READER, "GetPlacement", &ask)
        .await;
    assert_eq!(answer.status.code(), Code::Ok);
    let shown = GetPlacementResponse::decode(answer.message.as_slice()).expect("an answer");
    assert_eq!(shown.placement, "p1");

    let gone = DeleteTreeRequest {
        placement: "p1".to_owned(),
    };
    let answer = running.call(&mut sender, SCOPED, "DeleteTree", &gone).await;
    assert_eq!(answer.status.code(), Code::Ok);
    assert_eq!(
        DeleteTreeResponse::decode(answer.message.as_slice()),
        Ok(DeleteTreeResponse {})
    );

    let seen = core.seen.lock().expect("the lock");
    let who: Vec<(&str, &str)> = seen
        .iter()
        .map(|(m, c): &(&str, Caller)| (*m, c.key_id.as_str()))
        .collect();
    assert_eq!(
        who,
        [
            ("CreateTree", FULL.0),
            ("GetPlacement", READER.0),
            ("DeleteTree", SCOPED.0)
        ]
    );
    drop(seen);
    drop(sender);
    assert_eq!(running.stopped().await, 0);
}

/// Asserts the same call over TLS is answered the same, that the handshake offers HTTP/2, and that
/// a client that does not speak TLS is dropped without hurting the listener.
#[tokio::test]
async fn a_signed_call_reaches_the_core_over_tls() {
    let (_core, mut wanted) = recording();
    wanted.tls = true;
    let running = run(wanted).await;
    let mut sender = running.client().await;
    let answer = running
        .call(&mut sender, FULL, "CreateTree", &create_request())
        .await;
    assert_eq!(answer.status.code(), Code::Ok);
    assert_eq!(
        CreateTreeResponse::decode(answer.message.as_slice())
            .expect("an answer")
            .placement,
        "p1"
    );

    let tcp = TcpStream::connect(running.addr)
        .await
        .expect("a connection");
    let name = rustls::pki_types::ServerName::IpAddress(running.addr.ip().into());
    let connector = running.connector.as_ref().expect("a connector");
    let tls = connector.connect(name, tcp).await.expect("a handshake");
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));

    let mut plain = TcpStream::connect(running.addr)
        .await
        .expect("a connection");
    plain
        .write_all(b"not a TLS record\r\n\r\n")
        .await
        .expect("written");
    let mut nothing = Vec::new();
    let _ = plain.read_to_end(&mut nothing).await;
    let answer = running
        .call(
            &mut sender,
            FULL,
            "GetPlacement",
            &GetPlacementRequest::default(),
        )
        .await;
    assert_eq!(answer.status.code(), Code::Ok);
    drop((sender, tls));
    assert_eq!(running.stopped().await, 0);
}

/// Asserts a client that opens a TLS connection and says nothing is dropped when the handshake
/// time is out, on the paused clock, and that the next client is served.
#[tokio::test(start_paused = true)]
async fn a_client_that_never_finishes_the_handshake_is_dropped() {
    let (_core, mut wanted) = recording();
    wanted.tls = true;
    let running = run(wanted).await;
    let mut silent = TcpStream::connect(running.addr)
        .await
        .expect("a connection");
    let mut nothing = Vec::new();
    let read = silent.read_to_end(&mut nothing).await;
    assert!(read.is_ok() || read.is_err());
    assert!(nothing.is_empty());
    let mut sender = running.client().await;
    let answer = running
        .call(
            &mut sender,
            FULL,
            "GetPlacement",
            &GetPlacementRequest::default(),
        )
        .await;
    assert_eq!(answer.status.code(), Code::Ok);
    drop(sender);
    assert_eq!(running.stopped().await, 0);
}

/// Asserts a node with no core answers a good key `UNAVAILABLE` `NotServing`.
#[tokio::test]
async fn a_node_with_no_core_is_not_serving() {
    let running = run(Wanted::default()).await;
    let mut sender = running.client().await;
    let answer = running
        .call(
            &mut sender,
            FULL,
            "GetPlacement",
            &GetPlacementRequest::default(),
        )
        .await;
    assert_refused(&answer.status, Code::Unavailable, "NotServing");
    drop(sender);
    assert_eq!(running.stopped().await, 0);
}

/// Asserts what a caller sees of a key that is wrong, by the exact status: no signature, an unknown
/// key and a wrong secret are `UNAUTHENTICATED`, the first with the S3 front's words; a read-only
/// key's create and a scoped key's create from a source it may not read are `PERMISSION_DENIED`,
/// naming what failed; and the core is asked of none of them.
#[tokio::test]
async fn a_wrong_key_is_refused_with_its_status_and_the_core_is_not_asked() {
    let (core, wanted) = recording();
    let running = run(wanted).await;
    let mut sender = running.client().await;
    let message = create_request();

    let authority = running.addr.to_string();
    let mut bare = signed_request(
        &running.server,
        FULL,
        &authority,
        "CreateTree",
        framed(&message),
    );
    bare.headers_mut().remove("authorization");
    let answer = send(&mut sender, bare).await;
    assert_refused(
        &answer.status,
        Code::Unauthenticated,
        "unauthenticated: Access Denied: requests must be signed with AWS Signature Version 4.",
    );
    let answer = running
        .call(&mut sender, ("AKIDOTHER", "x"), "CreateTree", &message)
        .await;
    assert_refused(
        &answer.status,
        Code::Unauthenticated,
        "unauthenticated: The access key id or the region is not one this server accepts.",
    );
    let answer = running
        .call(&mut sender, (FULL.0, "wrong"), "CreateTree", &message)
        .await;
    assert_refused(
        &answer.status,
        Code::Unauthenticated,
        "unauthenticated: The request signature does not match the one calculated from the request.",
    );

    let answer = running
        .call(&mut sender, READER, "CreateTree", &message)
        .await;
    assert_refused(
        &answer.status,
        Code::PermissionDenied,
        "access denied: Access Denied: this access key may read objects and list the bucket only.",
    );
    let mut outside = create_request();
    if let Some(entry::Kind::File(file)) = outside.entries[0].kind.as_mut() {
        file.from = Some(Source {
            key: "other/x".to_owned(),
            offset: 0,
        });
    }
    let answer = running
        .call(&mut sender, SCOPED, "CreateTree", &outside)
        .await;
    assert_refused(
        &answer.status,
        Code::PermissionDenied,
        "access denied: this key may not read other/x",
    );
    assert!(core.seen.lock().expect("the lock").is_empty());
    drop(sender);
    assert_eq!(running.stopped().await, 0);
}

/// Asserts a refusal the core makes reaches the caller as the status of its reason with what it
/// names, and a path that is no method of the service is `UNIMPLEMENTED` for a good key.
#[tokio::test]
async fn a_refusal_of_the_core_and_an_unknown_method_reach_the_caller() {
    let core = Arc::new(FakeCore {
        refuse: Some(Reason::LocationNotEmpty),
        ..FakeCore::default()
    });
    let wanted = Wanted {
        core: Some(core as Arc<dyn TreeCore>),
        ..Wanted::default()
    };
    let running = run(wanted).await;
    let mut sender = running.client().await;
    let answer = running
        .call(&mut sender, FULL, "CreateTree", &create_request())
        .await;
    assert_refused(
        &answer.status,
        Code::AlreadyExists,
        "location not empty: bin/cc",
    );
    let answer = running
        .call(
            &mut sender,
            FULL,
            "NoSuchCall",
            &GetPlacementRequest::default(),
        )
        .await;
    assert_eq!(answer.status.code(), Code::Unimplemented);
    drop(sender);
    assert_eq!(running.stopped().await, 0);
}

/// Asserts a request longer than `params.max_request_bytes` is refused by its length, and a shorter
/// one is not.
#[tokio::test]
async fn a_request_over_the_limit_is_refused() {
    let (_core, mut wanted) = recording();
    wanted.max_request_bytes = Some(200);
    let running = run(wanted).await;
    let mut sender = running.client().await;
    let answer = running
        .call(&mut sender, FULL, "CreateTree", &create_request())
        .await;
    assert_eq!(answer.status.code(), Code::Ok);
    let mut long = create_request();
    long.entries[0].path = "x".repeat(300);
    let answer = running.call(&mut sender, FULL, "CreateTree", &long).await;
    assert_eq!(answer.status.code(), Code::OutOfRange);
    assert!(
        answer.status.message().contains("too large"),
        "{:?}",
        answer.status
    );
    drop(sender);
    assert_eq!(running.stopped().await, 0);
}

/// Asserts that after a stop the listener's sockets are closed, a connection that is idle is
/// closed, and nothing was cut.
#[tokio::test]
async fn a_stop_closes_the_sockets_and_the_idle_connections() {
    let (_core, wanted) = recording();
    let running = run(wanted).await;
    let mut sender = running.client().await;
    let answer = running
        .call(
            &mut sender,
            FULL,
            "GetPlacement",
            &GetPlacementRequest::default(),
        )
        .await;
    assert_eq!(answer.status.code(), Code::Ok);
    let addr = running.addr;
    assert_eq!(running.stopped().await, 0);
    assert!(TcpStream::connect(addr).await.is_err());
    assert!(sender.ready().await.is_err());
}

/// A core whose create call waits until it is told to go on.
struct Blocking {
    started: Notify,
    release: Notify,
}

#[async_trait]
impl TreeCore for Blocking {
    async fn create(
        &self,
        _: &Caller,
        _: CreateTreeRequest,
    ) -> Result<CreateTreeResponse, Refusal> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(CreateTreeResponse::default())
    }

    async fn placement(
        &self,
        _: &Caller,
        _: GetPlacementRequest,
    ) -> Result<GetPlacementResponse, Refusal> {
        Err(Refusal::of(Reason::NotServing))
    }

    async fn delete(
        &self,
        _: &Caller,
        _: DeleteTreeRequest,
    ) -> Result<DeleteTreeResponse, Refusal> {
        Err(Refusal::of(Reason::NotServing))
    }
}

/// Asserts that a call in flight at the stop is cut when the drain deadline passes (the paused
/// clock moves to it once nothing else can run), and that it is counted.
#[tokio::test(start_paused = true)]
async fn a_call_still_running_at_the_drain_deadline_is_cut() {
    let core = Arc::new(Blocking {
        started: Notify::new(),
        release: Notify::new(),
    });
    let wanted = Wanted {
        core: Some(Arc::clone(&core) as Arc<dyn TreeCore>),
        ..Wanted::default()
    };
    let running = run(wanted).await;
    let mut sender = running.client().await;
    let authority = running.addr.to_string();
    let request = signed_request(
        &running.server,
        FULL,
        &authority,
        "CreateTree",
        framed(&create_request()),
    );
    let calling = tokio::spawn(async move { send(&mut sender, request).await });
    core.started.notified().await;
    assert_eq!(running.stopped().await, 1);
    calling.abort();
}

/// Asserts that a call in flight at the stop is finished before the connection closes, when it
/// finishes within the drain deadline: nothing is cut.
#[tokio::test]
async fn a_call_in_flight_at_the_stop_is_finished() {
    let core = Arc::new(Blocking {
        started: Notify::new(),
        release: Notify::new(),
    });
    let wanted = Wanted {
        core: Some(Arc::clone(&core) as Arc<dyn TreeCore>),
        ..Wanted::default()
    };
    let running = run(wanted).await;
    let mut sender = running.client().await;
    let authority = running.addr.to_string();
    let request = signed_request(
        &running.server,
        FULL,
        &authority,
        "CreateTree",
        framed(&create_request()),
    );
    let calling = tokio::spawn(async move { send(&mut sender, request).await });
    core.started.notified().await;
    running.stop.send(true).expect("a listener");
    core.release.notify_one();
    let answer = calling.await.expect("the call ended");
    assert_eq!(answer.status.code(), Code::Ok);
    assert_eq!(running.task.await.expect("the listener ended"), 0);
}

/// Asserts a connection that has lived as long as `params.connection_lifetime` is closed by the
/// server, on the paused clock, and the listener goes on serving.
#[tokio::test(start_paused = true)]
async fn a_connection_ends_at_its_lifetime() {
    let (_core, mut wanted) = recording();
    wanted.lifetime = Some(Duration::from_secs(60));
    let running = run(wanted).await;
    let (mut old, driving) = connect_driven(running.addr, None).await;
    assert!(old.ready().await.is_ok());
    driving.await.expect("the connection ended");
    assert!(old.ready().await.is_err());
    let mut fresh = running.client().await;
    let answer = running
        .call(
            &mut fresh,
            FULL,
            "GetPlacement",
            &GetPlacementRequest::default(),
        )
        .await;
    assert_eq!(answer.status.code(), Code::Ok);
    drop(fresh);
    assert_eq!(running.stopped().await, 0);
}

/// Asserts that with room for one connection a second is served once the first has gone.
#[tokio::test]
async fn a_connection_over_the_limit_is_served_when_a_slot_is_free() {
    let (_core, mut wanted) = recording();
    wanted.max_connections = Some(1);
    let running = run(wanted).await;
    let mut first = running.client().await;
    let answer = running
        .call(
            &mut first,
            FULL,
            "GetPlacement",
            &GetPlacementRequest::default(),
        )
        .await;
    assert_eq!(answer.status.code(), Code::Ok);
    let addr = running.addr;
    let second = tokio::spawn(async move { connect(addr, None).await });
    drop(first);
    let mut second = second.await.expect("the second connection");
    let answer = running
        .call(
            &mut second,
            FULL,
            "GetPlacement",
            &GetPlacementRequest::default(),
        )
        .await;
    assert_eq!(answer.status.code(), Code::Ok);
    drop(second);
    assert_eq!(running.stopped().await, 0);
}

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

/// Asserts a failed accept does not end the loop: after the pause the loop asks again, and a stop
/// then ends it with nothing cut.
#[tokio::test(start_paused = true)]
async fn a_failed_accept_is_followed_by_another_ask() {
    let asked = Arc::new(AtomicUsize::new(0));
    let served = Served::new(Arc::new(config()), TreesFront::new(None), 1024);
    let limits = TreesLimits {
        max_connections: 1,
        handshake_timeout: Duration::from_secs(1),
        lifetime: Duration::from_secs(60),
        drain_deadline: Duration::from_secs(1),
        listen: ListenTuning::default(),
    };
    let (stop, stopped) = watch::channel(false);
    let looping = tokio::spawn(accept_loop(
        Failing {
            asked: Arc::clone(&asked),
        },
        None,
        served,
        limits,
        stopped,
    ));
    tokio::time::sleep(limits.listen.accept_pause * 2).await;
    assert_eq!(asked.load(Ordering::SeqCst), 2);
    stop.send(true).expect("a loop");
    assert_eq!(looping.await.expect("the loop ended"), 0);
}

/// Asserts a second listener binds the address of a running one, which a live update needs (its
/// sockets are not handed over), and that the first one stopped leaves the second serving.
#[tokio::test]
async fn a_listener_binds_beside_a_running_one() {
    let (_core, wanted) = recording();
    let first = run(wanted).await;
    let flags = TreesFlags {
        listen: vec![first.addr],
        ..TreesFlags::default()
    };
    let setup = trees_setup_of(&flags, &first.server, false)
        .expect("setup")
        .expect("a front");
    let second = setup.bind().await.expect("bound beside the first");
    let (stop, stopped) = watch::channel(false);
    let serving = tokio::spawn(second.serve(stopped));
    let addr = first.addr;
    assert_eq!(first.stopped().await, 0);
    let mut sender = connect(addr, None).await;
    let server = config();
    let request = signed_request(
        &server,
        FULL,
        &addr.to_string(),
        "GetPlacement",
        framed(&GetPlacementRequest::default()),
    );
    let answer = send(&mut sender, request).await;
    assert_refused(&answer.status, Code::Unavailable, "NotServing");
    drop(sender);
    stop.send(true).expect("a listener");
    assert_eq!(serving.await.expect("the listener ended"), 0);
}
