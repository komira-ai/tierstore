//! The HTTP client of the harness: plain HTTP/1.1 with keep-alive to the store's filer and volume
//! servers. A request is a `HEAD` or a `GET` (whole, or one byte range); the answer's status,
//! length and entity tag come back with the body as a stream.
//!
//! The statuses are split once, here. `404` and every `2xx` come back as `Ok` with the status in
//! the answer, so a caller can tell a missing object from a broken store; every other status, a
//! request that was not answered in time and every transport failure is an [`Error::Http`] naming
//! the URL.

use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::TryStreamExt as _;
use futures::stream::BoxStream;
use http::header::{ACCEPT, CONTENT_LENGTH, ETAG, RANGE};
use http::{HeaderMap, Method, Request};
use http_body_util::{BodyExt as _, Empty};
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use crate::Error;

/// The body of an answer: its pieces as they arrive, each an error naming the URL if the
/// connection breaks.
pub type BodyStream = BoxStream<'static, Result<Bytes, Error>>;

/// The answer to a `HEAD`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    /// The status: `404` or a `2xx`.
    pub status: u16,
    /// The `Content-Length` the answer names, if it has one.
    pub len: Option<u64>,
    /// The entity tag, without its quotes.
    pub etag: Option<String>,
}

/// The answer to a `GET`: the status and headers, and the body still to be read.
pub struct Response {
    /// The status: `404` or a `2xx`.
    pub status: u16,
    /// The entity tag, without its quotes.
    pub etag: Option<String>,
    /// The `Content-Length` of the body, if the answer has one.
    pub len: Option<u64>,
    /// The body, piece by piece.
    pub body: BodyStream,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("etag", &self.etag)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

/// A client that keeps its connections to the servers it has talked to and sends their next
/// request on one of them. Clones share the connections.
#[derive(Debug, Clone)]
pub struct HttpClient {
    client: Client<HttpConnector, Empty<Bytes>>,
    timeout: Duration,
}

impl HttpClient {
    /// A client that gives up on a request whose answer's headers do not arrive within
    /// `timeout_ms` milliseconds. The body that follows is not timed.
    #[must_use]
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            client: Client::builder(TokioExecutor::new()).build_http(),
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    /// `HEAD url`.
    ///
    /// # Errors
    /// [`Error::Http`] for a status that is neither `404` nor a `2xx`, for a request that got no
    /// answer in time and for a transport failure.
    pub async fn head(&self, url: &str) -> Result<Head, Error> {
        let answer = self.send(Method::HEAD, url, None, false).await?;
        if !is_answer(answer.status().as_u16()) {
            return Err(http_error(
                url,
                format!("status {}", answer.status().as_u16()),
            ));
        }
        Ok(Head {
            status: answer.status().as_u16(),
            len: content_length(answer.headers()),
            etag: etag(answer.headers()),
        })
    }

    /// `GET url`, of the byte range `[start, end)` if one is given, asking for JSON if
    /// `accept_json`.
    ///
    /// A range is sent as `Range: bytes=start-(end-1)` and is answered with `206`, or with `200`
    /// when the answer is the whole object, which is when `start` is 0 and the answer's length
    /// does not pass `end`. Any other status of a ranged read is an error, so is an empty range
    /// (nothing is sent for it).
    ///
    /// # Errors
    /// [`Error::Http`] for a status that is not `404` or one this read accepts, for an empty
    /// range, for a request that got no answer in time and for a transport failure.
    pub async fn get(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
        accept_json: bool,
    ) -> Result<Response, Error> {
        if let Some((start, end)) = range
            && end <= start
        {
            return Err(http_error(url, format!("empty range {start}..{end}")));
        }
        let answer = self.send(Method::GET, url, range, accept_json).await?;
        let status = answer.status().as_u16();
        let len = content_length(answer.headers());
        let accepted = match range {
            None => is_answer(status),
            Some((start, end)) => {
                status == 404
                    || status == 206
                    || (status == 200 && start == 0 && len.is_some_and(|l| l <= end))
            }
        };
        if !accepted {
            return Err(http_error(url, format!("status {status}")));
        }
        let etag = etag(answer.headers());
        let owned_url = url.to_owned();
        let body = answer
            .into_body()
            .into_data_stream()
            .map_err(move |e| http_error(&owned_url, format!("body: {}", describe(&e))))
            .boxed();
        Ok(Response {
            status,
            etag,
            len,
            body,
        })
    }

    /// `GET url` whole: its body, which must be a `200`'s.
    ///
    /// # Errors
    /// [`Error::Http`] for every status but `200` (a missing object too), for a body that breaks
    /// off, for a request that got no answer in time and for a transport failure.
    pub async fn get_bytes(&self, url: &str, accept_json: bool) -> Result<Bytes, Error> {
        let answer = self.get(url, None, accept_json).await?;
        if answer.status != 200 {
            return Err(http_error(url, format!("status {}", answer.status)));
        }
        let all = answer
            .body
            .try_fold(Vec::new(), |mut all, piece| async move {
                all.extend_from_slice(&piece);
                Ok(all)
            })
            .await?;
        Ok(Bytes::from(all))
    }

    /// Sends one request and waits for the answer's headers.
    async fn send(
        &self,
        method: Method,
        url: &str,
        range: Option<(u64, u64)>,
        accept_json: bool,
    ) -> Result<http::Response<Incoming>, Error> {
        let mut builder = Request::builder().method(method).uri(url);
        if accept_json {
            builder = builder.header(ACCEPT, "application/json");
        }
        if let Some((start, end)) = range {
            builder = builder.header(RANGE, format!("bytes={start}-{}", end - 1));
        }
        let request = builder
            .body(Empty::new())
            .map_err(|e| http_error(url, format!("bad request: {e}")))?;
        match tokio::time::timeout(self.timeout, self.client.request(request)).await {
            Err(_) => Err(http_error(
                url,
                format!("no answer within {} ms", self.timeout.as_millis()),
            )),
            Ok(Err(e)) => Err(http_error(url, format!("transport: {}", describe(&e)))),
            Ok(Ok(answer)) => Ok(answer),
        }
    }
}

/// An [`Error::Http`] of `url`.
fn http_error(url: &str, why: String) -> Error {
    Error::Http {
        url: url.to_owned(),
        why,
    }
}

/// Whether a status is one the client hands to its caller: `404` and every `2xx`.
fn is_answer(status: u16) -> bool {
    status == 404 || (200..300).contains(&status)
}

/// The `Content-Length` of an answer.
fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}

/// The entity tag of an answer, without its quotes.
fn etag(headers: &HeaderMap) -> Option<String> {
    headers
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim_matches('"').to_owned())
}

/// An error and the chain of errors under it, on one line.
fn describe(e: &(dyn std::error::Error + 'static)) -> String {
    let mut text = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        text.push_str(": ");
        text.push_str(&c.to_string());
        cause = c.source();
    }
    text
}
