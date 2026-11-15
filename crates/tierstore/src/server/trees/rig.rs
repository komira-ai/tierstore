//! What the tests of the trees front share: a server's keys, a core that records its calls, and a
//! gRPC client over HTTP/2 that signs as a client of the interface does.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::Request;
use hyper::body::Incoming;
use hyper::client::conn::http2::{self, SendRequest};
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost::Message;
use tierstore_trees_api::v1::{
    CreateTreeRequest, CreateTreeResponse, DeleteTreeRequest, DeleteTreeResponse,
    GetPlacementRequest, GetPlacementResponse, PlacementState,
};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tonic::{Code, Status};

use super::{Caller, Reason, Refusal, TreeCore};
use crate::TestClock;
use crate::server::auth::sign_on;
use crate::server::{AccessKey, Prefixes, ScopedKey, ServerConfig};

/// The time of the tests' clock.
pub(super) const NOW: u64 = 1_800_000_000_000;

/// The full key of the server of the tests.
pub(super) const FULL: (&str, &str) = ("AKIDFULL", "full-secret-0123456789");

/// A read-only key.
pub(super) const READER: (&str, &str) = ("AKIDREADER", "reader-secret-0123456789");

/// A scoped key that reads `cas/` and writes under `jobs/`.
pub(super) const SCOPED: (&str, &str) = ("scoped-job", "scoped-secret-0123456789");

/// The host a call is signed for.
pub(super) const HOST: &str = "tier.test";

/// A server's keys: the full key, a read-only key and a scoped key, at the time [`NOW`].
pub(super) fn config() -> ServerConfig {
    config_at(NOW)
}

/// The same server, with its clock at `now`.
pub(super) fn config_at(now: u64) -> ServerConfig {
    let clock = Arc::new(TestClock::new());
    clock.set(now);
    let mut config = ServerConfig::new("tier-bucket", FULL.0, FULL.1, clock);
    config.read_only_keys = vec![AccessKey::new(READER.0, READER.1)];
    let prefixes = Prefixes {
        read: &["cas/"],
        write: &["jobs/"],
        delete: &[],
    };
    config.scoped_keys = vec![ScopedKey::new(SCOPED.0, SCOPED.1, &prefixes).expect("a scoped key")];
    config
}

/// A core that records the calls it gets and answers them, or refuses them with `refuse`.
#[derive(Default)]
pub(super) struct FakeCore {
    /// The method and the caller of each call, in order.
    pub(super) seen: Mutex<Vec<(&'static str, Caller)>>,
    /// The refusal every call is answered with, if any.
    pub(super) refuse: Option<Reason>,
}

impl FakeCore {
    fn note(&self, method: &'static str, caller: &Caller) -> Result<(), Refusal> {
        self.seen
            .lock()
            .expect("the lock")
            .push((method, caller.clone()));
        match self.refuse {
            Some(reason) => Err(Refusal::about(reason, "bin/cc")),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl TreeCore for FakeCore {
    async fn create(
        &self,
        caller: &Caller,
        request: CreateTreeRequest,
    ) -> Result<CreateTreeResponse, Refusal> {
        self.note("CreateTree", caller)?;
        Ok(CreateTreeResponse {
            tree: vec![7; 32],
            placement: "p1".to_owned(),
            entries: request.entries.len() as u64,
            created: true,
            ..CreateTreeResponse::default()
        })
    }

    async fn placement(
        &self,
        caller: &Caller,
        request: GetPlacementRequest,
    ) -> Result<GetPlacementResponse, Refusal> {
        self.note("GetPlacement", caller)?;
        Ok(GetPlacementResponse {
            placement: request.placement,
            state: PlacementState::Ready as i32,
            ..GetPlacementResponse::default()
        })
    }

    async fn delete(
        &self,
        caller: &Caller,
        _request: DeleteTreeRequest,
    ) -> Result<DeleteTreeResponse, Refusal> {
        self.note("DeleteTree", caller)?;
        Ok(DeleteTreeResponse {})
    }
}

/// What a call was answered with.
#[derive(Debug)]
pub(super) struct Answer {
    /// The status: `Ok` with the message's bytes, or the refusal.
    pub(super) status: Status,
    /// The bytes of the answer's message, without the gRPC frame.
    pub(super) message: Vec<u8>,
}

/// The gRPC frame of `message`: not compressed, its length, and it.
pub(super) fn framed(message: &impl Message) -> Vec<u8> {
    let bytes = message.encode_to_vec();
    let mut frame = vec![0_u8];
    frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    frame.extend_from_slice(&bytes);
    frame
}

/// The head of a call of `method`, signed with `key` as a client of the interface signs it, for
/// the server of `config`: the host is the authority of the address, and goes in the `:authority`
/// of the request, not in a header of its own.
pub(super) fn signed_request(
    config: &ServerConfig,
    key: (&str, &str),
    authority: &str,
    method: &str,
    body: Vec<u8>,
) -> Request<Full<Bytes>> {
    let path = tierstore_trees_api::path(method);
    let extra = [("content-type", "application/grpc"), ("te", "trailers")];
    let signed = sign_on(
        authority,
        config,
        key,
        ("POST", &path),
        &extra,
        "UNSIGNED-PAYLOAD",
        &body,
    );
    let (mut parts, body) = signed.into_parts();
    parts.headers.remove("host");
    parts.uri = format!("http://{authority}{path}").parse().expect("a URI");
    Request::from_parts(parts, body)
}

/// Sends `request` and reads the answer: the status of the headers, or of the trailers.
pub(super) async fn send(
    sender: &mut SendRequest<Full<Bytes>>,
    request: Request<Full<Bytes>>,
) -> Answer {
    let response = sender.send_request(request).await.expect("an answer");
    answer_of(response).await
}

async fn answer_of(response: hyper::Response<Incoming>) -> Answer {
    let (parts, body) = response.into_parts();
    let collected = body.collect().await.expect("the body");
    let trailers = collected.trailers().cloned();
    let status = Status::from_header_map(&parts.headers)
        .or_else(|| trailers.as_ref().and_then(Status::from_header_map))
        .expect("a grpc-status");
    let data = collected.to_bytes();
    let message = if data.len() >= 5 {
        data[5..].to_vec()
    } else {
        Vec::new()
    };
    Answer { status, message }
}

/// An HTTP/2 connection to `addr`, over TLS with `connector` or in the clear.
pub(super) async fn connect(
    addr: SocketAddr,
    connector: Option<&TlsConnector>,
) -> SendRequest<Full<Bytes>> {
    connect_driven(addr, connector).await.0
}

/// [`connect`], and the task that drives the connection, which ends when the connection does.
pub(super) async fn connect_driven(
    addr: SocketAddr,
    connector: Option<&TlsConnector>,
) -> (SendRequest<Full<Bytes>>, tokio::task::JoinHandle<()>) {
    let tcp = TcpStream::connect(addr).await.expect("a connection");
    match connector {
        Some(connector) => {
            let name = rustls::pki_types::ServerName::IpAddress(addr.ip().into());
            let tls = connector.connect(name, tcp).await.expect("a handshake");
            handshake(TokioIo::new(tls)).await
        }
        None => handshake(TokioIo::new(tcp)).await,
    }
}

async fn handshake<T>(io: TokioIo<T>) -> (SendRequest<Full<Bytes>>, tokio::task::JoinHandle<()>)
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (sender, connection) = http2::handshake(TokioExecutor::new(), io)
        .await
        .expect("an HTTP/2 handshake");
    let driving = tokio::spawn(async move {
        let _ = connection.await;
    });
    (sender, driving)
}

/// Asserts the status is `code` with exactly `message`.
pub(super) fn assert_refused(status: &Status, code: Code, message: &str) {
    assert_eq!(status.code(), code, "{status:?}");
    assert_eq!(status.message(), message);
}
