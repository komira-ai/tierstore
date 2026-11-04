//! The S3 front over TLS: real handshakes against a listening server, and the stream of a
//! connection whose handshake has not run, driven directly.

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, ServerName};
use tierstore_testkit::dir::TestDir;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::{TlsAcceptor, TlsConnector};

use super::super::oci::s3_acceptor;
use super::super::oci::test_pki::{Ca, ca, dns, ip, leaf};
use super::super::{Drained, Server, ServerConfig};
use super::Handshaking;
use crate::{MemoryBackend, SystemClock};

const HEALTH: &[u8] = b"GET /healthz HTTP/1.1\r\nHost: s3\r\nConnection: close\r\n\r\n";

/// An acceptor for a leaf of `authority` with the names `sans`, and a connector that trusts the
/// authority.
fn tls_pair(
    dir: &TestDir,
    authority: &Ca,
    sans: Vec<rcgen::SanType>,
) -> (TlsAcceptor, TlsConnector) {
    let files = leaf(dir.path(), "front", authority, sans);
    let listen: SocketAddr = "127.0.0.1:9000".parse().expect("an address");
    let acceptor = s3_acceptor(&files.cert, &files.key, &[listen]).expect("an acceptor");
    let mut roots = rustls::RootCertStore::empty();
    let pem = authority.pem();
    for cert in CertificateDer::pem_slice_iter(pem.as_bytes()) {
        roots.add(cert.expect("a certificate")).expect("a root");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    (acceptor, TlsConnector::from(Arc::new(config)))
}

/// A server over an empty memory backend that serves HTTPS with `acceptor` on a loopback port,
/// with the port and the task that serves.
async fn serving(acceptor: TlsAcceptor) -> (Arc<Server>, u16, JoinHandle<Drained>) {
    let mut config = ServerConfig::new("tier-bucket", "AKID", "secret", Arc::new(SystemClock));
    config.drain_lb_wait = Duration::ZERO;
    config.tls = Some(acceptor);
    let server = Server::new(Arc::new(MemoryBackend::new()), config);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bound");
    let port = listener.local_addr().expect("an address").port();
    let task = tokio::spawn({
        let server = Arc::clone(&server);
        async move { server.serve_all(vec![listener]).await }
    });
    (server, port, task)
}

/// Dials `port` on loopback, takes `name` as the name it dials, and asks `/healthz` over TLS: the
/// whole reply, or the error of the handshake.
async fn health(
    connector: &TlsConnector,
    name: ServerName<'static>,
    port: u16,
) -> std::io::Result<String> {
    let tcp = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connected");
    let mut stream = connector.connect(name, tcp).await?;
    stream.write_all(HEALTH).await.expect("written");
    let mut reply = Vec::new();
    // A server that closes without a close_notify ends the read with an error after the reply.
    let _ = stream.read_to_end(&mut reply).await;
    Ok(String::from_utf8_lossy(&reply).into_owned())
}

fn by_ip() -> ServerName<'static> {
    ServerName::from(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

fn by_name(name: &str) -> ServerName<'static> {
    ServerName::try_from(name.to_owned()).expect("a DNS name")
}

/// Asserts that the drain of `server` ends with no request cut.
async fn drained(server: &Server, task: JoinHandle<Drained>) {
    server.begin_drain();
    let ended = task.await.expect("the server ended");
    assert_eq!(ended, Drained { abandoned: 0 });
}

/// Asserts a client that dials the front by IP address gets the answer over TLS when the
/// certificate carries that IP subjectAltName and no DNS name.
#[tokio::test]
async fn a_certificate_with_an_ip_name_is_served_to_a_client_that_dials_the_address() {
    let dir = TestDir::under(std::env::temp_dir(), "s3-tls-ip");
    let (acceptor, connector) = tls_pair(&dir, &ca(), vec![ip("127.0.0.1")]);
    let (server, port, task) = serving(acceptor).await;
    let reply = health(&connector, by_ip(), port).await.expect("handshake");
    assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
    drained(&server, task).await;
}

/// Asserts a client that dials the front by DNS name gets the answer over TLS when the
/// certificate carries that DNS subjectAltName and no IP address, though the front is at an IP
/// address.
#[tokio::test]
async fn a_certificate_with_a_dns_name_is_served_to_a_client_that_dials_the_name() {
    let dir = TestDir::under(std::env::temp_dir(), "s3-tls-dns");
    let (acceptor, connector) = tls_pair(&dir, &ca(), vec![dns("tierstore.test")]);
    let (server, port, task) = serving(acceptor).await;
    let reply = health(&connector, by_name("tierstore.test"), port).await;
    let reply = reply.expect("handshake");
    assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
    drained(&server, task).await;
}

/// Asserts a certificate with a DNS name and an IP address is served to a client by either.
#[tokio::test]
async fn a_certificate_with_both_names_is_served_by_either() {
    let dir = TestDir::under(std::env::temp_dir(), "s3-tls-both");
    let sans = vec![dns("tierstore.test"), ip("127.0.0.1")];
    let (acceptor, connector) = tls_pair(&dir, &ca(), sans);
    let (server, port, task) = serving(acceptor).await;
    for name in [by_ip(), by_name("tierstore.test")] {
        let reply = health(&connector, name, port).await.expect("handshake");
        assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
    }
    drained(&server, task).await;
}

/// Asserts that a client that sends plain HTTP to the TLS front gets no answer, that a client
/// that dials a name the certificate does not carry is refused by its own check, and that the
/// next client, which dials a name it carries, is served: a failed handshake costs the front
/// nothing.
#[tokio::test]
async fn a_failed_handshake_is_not_served_and_does_not_stop_the_front() {
    let dir = TestDir::under(std::env::temp_dir(), "s3-tls-failed");
    let (acceptor, connector) = tls_pair(&dir, &ca(), vec![dns("tierstore.test")]);
    let (server, port, task) = serving(acceptor).await;
    let mut plain = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connected");
    plain.write_all(HEALTH).await.expect("written");
    let mut reply = Vec::new();
    let _ = plain.read_to_end(&mut reply).await;
    assert!(
        !String::from_utf8_lossy(&reply).contains("HTTP/1.1"),
        "{reply:?}"
    );
    let wrong = health(&connector, by_name("other.test"), port).await;
    assert!(wrong.is_err(), "a certificate for another name is refused");
    let reply = health(&connector, by_name("tierstore.test"), port).await;
    let reply = reply.expect("handshake");
    assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
    drained(&server, task).await;
}

/// A connected pair of sockets on loopback: the server's end and the client's.
async fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bound");
    let at = listener.local_addr().expect("an address");
    let (accepted, connected) = tokio::join!(listener.accept(), TcpStream::connect(at));
    (accepted.expect("accepted").0, connected.expect("connected"))
}

/// A stream whose handshake has not run, over the server's end of a pair, and the other end with a
/// connector for a client of it.
async fn fresh(dir: &TestDir) -> (Handshaking, TcpStream, TlsConnector) {
    let (acceptor, connector) = tls_pair(dir, &ca(), vec![dns("tierstore.test")]);
    let (server, client) = pair().await;
    let peer = server.peer_addr().expect("a peer");
    (Handshaking::new(&acceptor, server, peer), client, connector)
}

/// Asserts that a write is what drives the handshake when it is the first thing done with the
/// stream: it waits for the client's hello, and then the bytes written are the client's to read.
#[tokio::test]
async fn a_first_write_runs_the_handshake_and_then_writes() {
    let dir = TestDir::under(std::env::temp_dir(), "s3-tls-write");
    let (mut stream, client, connector) = fresh(&dir).await;
    let writing = async {
        stream.write_all(b"hi").await.expect("written");
        stream.shutdown().await.expect("closed");
    };
    let reading = async {
        let mut tls = connector
            .connect(by_name("tierstore.test"), client)
            .await
            .expect("handshake");
        let mut got = Vec::new();
        let _ = tls.read_to_end(&mut got).await;
        got
    };
    let ((), got) = tokio::join!(writing, reading);
    assert_eq!(got, b"hi");
}

/// Asserts that a flush is what drives the handshake when it is the first thing done, and that a
/// stream whose handshake is done flushes and closes.
#[tokio::test]
async fn a_first_flush_runs_the_handshake() {
    let dir = TestDir::under(std::env::temp_dir(), "s3-tls-flush");
    let (mut stream, client, connector) = fresh(&dir).await;
    let flushing = async {
        stream.flush().await.expect("flushed");
        stream.shutdown().await.expect("closed");
    };
    let connecting = async {
        let tls = connector.connect(by_name("tierstore.test"), client).await;
        tls.expect("handshake")
    };
    let ((), _tls) = tokio::join!(flushing, connecting);
}

/// Asserts that a client that sends bytes that are not TLS ends the stream: the first read says
/// the handshake failed, and each later read, write and flush says the pipe is broken, and that
/// closing it is not an error.
#[tokio::test]
async fn a_stream_whose_handshake_failed_carries_nothing() {
    let dir = TestDir::under(std::env::temp_dir(), "s3-tls-broken");
    let (mut stream, mut client, _) = fresh(&dir).await;
    client.write_all(HEALTH).await.expect("written");
    let mut buf = [0_u8; 8];
    let first = stream
        .read(&mut buf)
        .await
        .expect_err("the handshake failed");
    assert_ne!(first.kind(), ErrorKind::BrokenPipe);
    let read = stream.read(&mut buf).await.expect_err("closed");
    assert_eq!(read.kind(), ErrorKind::BrokenPipe);
    let write = stream.write(b"x").await.expect_err("closed");
    assert_eq!(write.kind(), ErrorKind::BrokenPipe);
    let flush = stream.flush().await.expect_err("closed");
    assert_eq!(flush.kind(), ErrorKind::BrokenPipe);
    stream.shutdown().await.expect("closing a closed stream");
}

/// Asserts that closing a stream before its handshake runs drops the connection without a word:
/// the client reads the end of the stream, and the stream carries nothing after.
#[tokio::test]
async fn closing_before_the_handshake_drops_the_connection() {
    let dir = TestDir::under(std::env::temp_dir(), "s3-tls-drop");
    let (mut stream, mut client, _) = fresh(&dir).await;
    stream.shutdown().await.expect("closed");
    let mut got = Vec::new();
    client
        .read_to_end(&mut got)
        .await
        .expect("the end of the stream");
    assert!(got.is_empty(), "{got:?}");
    let mut buf = [0_u8; 1];
    let read = stream.read(&mut buf).await.expect_err("closed");
    assert_eq!(read.kind(), ErrorKind::BrokenPipe);
}
