//! A registry in the process, for the tests of the pull-through: a real HTTP/1.1 server on a
//! loopback port the system picks (or on any stream, such as one end of a duplex), whose
//! answers each test scripts. No name is resolved and nothing leaves the host.

use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt as _};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt as _, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

/// What a request asked, as the registry saw it.
#[derive(Clone, Debug)]
pub(crate) struct Seen {
    /// The path and the query.
    pub(crate) path: String,
    pub(crate) authorization: Option<String>,
    pub(crate) accept: Option<String>,
    pub(crate) host: String,
}

/// How an answer's body ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ending {
    /// With the `Content-Length` of the bytes.
    Whole,
    /// With no `Content-Length`: the length is not announced.
    Chunked,
    /// The headers announce this many bytes, the body is sent, and then the connection fails.
    Cut(usize),
    /// The headers announce one byte more than the body, which is sent, and then nothing.
    Stall,
    /// No answer at all.
    Hang,
}

/// One scripted answer.
#[derive(Clone, Debug)]
pub(crate) struct Fake {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
    pub(crate) ending: Ending,
}

impl Fake {
    /// 200 with `body`, whole.
    pub(crate) fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self::of(200, body, Ending::Whole)
    }

    /// `status` with no body.
    pub(crate) fn status(status: u16) -> Self {
        Self::of(status, Vec::new(), Ending::Whole)
    }

    /// 200 with `body`, its length not announced.
    pub(crate) fn chunked(body: impl Into<Vec<u8>>) -> Self {
        Self::of(200, body, Ending::Chunked)
    }

    /// 200 whose body ends as `ending` says.
    pub(crate) fn ending(body: impl Into<Vec<u8>>, ending: Ending) -> Self {
        Self::of(200, body, ending)
    }

    /// 401 with the challenge `challenge`.
    pub(crate) fn challenge(challenge: &str) -> Self {
        Self::status(401).with("www-authenticate", challenge)
    }

    /// A redirect with `status` to `location`.
    pub(crate) fn redirect(status: u16, location: &str) -> Self {
        Self::status(status).with("location", location)
    }

    /// The answer with a header more.
    pub(crate) fn with(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    fn of(status: u16, body: impl Into<Vec<u8>>, ending: Ending) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
            ending,
        }
    }

    fn response(self) -> Response<UnsyncBoxBody<Bytes, io::Error>> {
        let announced = match self.ending {
            Ending::Whole => Some(self.body.len()),
            Ending::Cut(announce) => Some(announce),
            Ending::Stall => Some(self.body.len() + 1),
            Ending::Chunked | Ending::Hang => None,
        };
        let data = Frame::data(Bytes::from(self.body));
        let mut body: BoxStream<'static, io::Result<Frame<Bytes>>> =
            stream::iter(vec![Ok(data)]).boxed();
        if matches!(self.ending, Ending::Cut(_)) {
            // The error comes after one yield, so the connection flushes the head and the piece
            // before it fails: a cut body, not a connection that dies before its headers.
            let cut = stream::once(async {
                tokio::task::yield_now().await;
                Err(io::Error::other("cut on purpose"))
            });
            body = body.chain(cut).boxed();
        }
        if self.ending == Ending::Stall {
            body = body.chain(stream::pending()).boxed();
        }
        let mut response = Response::new(StreamBody::new(body).boxed_unsync());
        *response.status_mut() = hyper::StatusCode::from_u16(self.status).expect("a status");
        for (name, value) in &self.headers {
            let name = hyper::header::HeaderName::from_bytes(name.as_bytes()).expect("a name");
            let value = hyper::header::HeaderValue::from_str(value).expect("a value");
            response.headers_mut().append(name, value);
        }
        if let Some(length) = announced {
            let length = hyper::header::HeaderValue::from(length);
            response
                .headers_mut()
                .insert(hyper::header::CONTENT_LENGTH, length);
        }
        response
    }
}

/// What answers: a function of the request.
pub(crate) type Handler = dyn Fn(&Seen) -> Fake + Send + Sync;

/// Answers the requests on `stream` with `handler`, and notes each in `seen`.
pub(crate) async fn serve<S>(stream: S, handler: Arc<Handler>, seen: Arc<Mutex<Vec<Seen>>>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = service_fn(move |request: Request<Incoming>| {
        let handler = Arc::clone(&handler);
        let seen = Arc::clone(&seen);
        async move {
            let header = |name: &str| {
                let value = request.headers().get(name);
                value
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            };
            let asked = Seen {
                path: request.uri().to_string(),
                authorization: header("authorization"),
                accept: header("accept"),
                host: header("host").unwrap_or_default(),
            };
            seen.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(asked.clone());
            let fake = handler(&asked);
            if fake.ending == Ending::Hang {
                let never: Infallible = std::future::pending().await;
                match never {}
            }
            Ok::<_, Infallible>(fake.response())
        }
    });
    // How a connection ends is the test's to see in what the client got.
    let _ = http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await;
}

/// A registry listening on a loopback port.
pub(crate) struct Registry {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    accepting: JoinHandle<()>,
}

impl Registry {
    /// A registry that speaks plain HTTP and answers with `handler`.
    pub(crate) async fn start(handler: impl Fn(&Seen) -> Fake + Send + Sync + 'static) -> Self {
        Self::bind(None, Arc::new(handler)).await
    }

    /// A registry that speaks HTTPS with `acceptor`.
    pub(crate) async fn start_tls(
        acceptor: TlsAcceptor,
        handler: impl Fn(&Seen) -> Fake + Send + Sync + 'static,
    ) -> Self {
        Self::bind(Some(acceptor), Arc::new(handler)).await
    }

    async fn bind(acceptor: Option<TlsAcceptor>, handler: Arc<Handler>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let addr = listener.local_addr().expect("an address");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let accepting = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("a connection");
                let (handler, seen) = (Arc::clone(&handler), Arc::clone(&noted));
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    match acceptor {
                        Some(acceptor) => {
                            // A client that distrusts the certificate ends the handshake.
                            if let Ok(tls) = acceptor.accept(stream).await {
                                serve(tls, handler, seen).await;
                            }
                        }
                        None => serve(stream, handler, seen).await,
                    }
                });
            }
        });
        Self {
            addr,
            seen,
            accepting,
        }
    }

    /// `host:port`.
    pub(crate) fn authority(&self) -> String {
        self.addr.to_string()
    }

    /// `http://host:port`.
    pub(crate) fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The requests seen so far, in order.
    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.accepting.abort();
    }
}
