//! The registry the front pulls from when its store has no object: an outbound HTTP client.
//!
//! With `params.upstream` in the file, tierstore is not only a server. On a request for a manifest
//! or a blob that the store does not have, the front opens a connection to the upstream registry,
//! asks for the object and stores what it is sent (see `pull`). This module is that client, and
//! the only code of the process that dials out for the registry front.
//!
//! - One connection for each request: nothing is pooled, so no state is shared between pulls.
//! - `GET`, and `HEAD` of a manifest to learn the digest a tag points at now, over HTTP/1.1,
//!   plain or over TLS. TLS trusts the system's CA certificates and,
//!   with `ca_file`, the certificates in that file.
//! - Login follows the challenge the upstream sends with its 401. `Basic` is answered with the
//!   configured user name and password. `Bearer` is answered with a token fetched from the
//!   challenge's `realm` (with `service` and `scope`), logged in with the same user name and
//!   password, or anonymously if none is configured. A token is used for one pull and not kept.
//!   The login answers only a challenge that the configured address itself sent: a 401 that comes
//!   after a redirect to another host or port is answered with the status 401 and no login. The
//!   `realm` of a challenge from the configured address is trusted with the login, as the registry
//!   token flow has it (the realm is often another host of the same operator).
//! - A redirect (301, 302, 303, 307, 308) is followed, at most `max_redirects` times, and
//!   carries no `Authorization`: a registry that sends blobs to a store redirects to a URL that
//!   carries its own authorization.
//! - The password is sent only over TLS or to a loopback address: a configuration that would send
//!   it in the clear is refused at start. A token request's `realm` that is neither is not refused:
//!   it is requested anonymously, without the login.
//! - `timeout` bounds each step: connecting and getting the headers of an answer, and the wait for
//!   each piece of a body. The time a whole body takes is not bounded.
//! - `recheck_timeout` bounds the re-check of a stored tag as a whole (see `pull`), whatever the
//!   number of requests it makes.
//!
//! A body is not held whole unless it is a manifest or a token (at most `params.max_manifest_bytes`):
//! a blob is handed on as a stream.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::{Bytes, BytesMut};
use futures::stream::{self, StreamExt as _};
use http_body_util::{BodyExt as _, Empty};
use hyper::body::Incoming;
use hyper::header::{
    ACCEPT, AUTHORIZATION, CONTENT_LENGTH, HOST, HeaderMap, HeaderValue, LOCATION, USER_AGENT,
    WWW_AUTHENTICATE,
};
use hyper::{Method, Request, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rustls::ClientConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use super::DEFAULT_MANIFEST_BYTES;
use super::tls::OciError;
use crate::{Error, PartBody};

/// What an [`Upstream`] is made of.
pub struct UpstreamSettings {
    /// The registry's address: `https://host[:port]` or `http://host[:port]`.
    pub url: String,
    /// The user name and password to log in with.
    pub login: Option<(String, String)>,
    /// PEM certificates the registry's certificate may be issued by, besides the system's.
    pub ca_pem: Option<Vec<u8>>,
    /// How long each step of a pull may take.
    pub timeout: Duration,
    /// The most redirects one request follows.
    pub max_redirects: u32,
    /// How long the whole re-check of a stored tag may take, all its requests together.
    pub recheck_timeout: Duration,
}

/// Why a pull from the upstream failed.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    /// The upstream has no such object (404).
    #[error("the upstream has no such object")]
    NotFound,
    /// The upstream answered with a status that is not 200.
    #[error("the upstream answered {0}")]
    Status(u16),
    /// The upstream could not be reached, or its answer could not be used.
    #[error("{0}")]
    Failed(String),
}

fn failed(why: impl std::fmt::Display) -> UpstreamError {
    UpstreamError::Failed(why.to_string())
}

/// The user name and password of the login.
struct Login {
    user: String,
    password: String,
}

/// An upstream registry, and how to reach and log in to it.
pub struct Upstream {
    base: Uri,
    login: Option<Login>,
    tls: TlsConnector,
    timeout: Duration,
    max_redirects: u32,
    /// The most bytes of a token read from a realm.
    body_limit: u64,
    recheck_timeout: Duration,
}

/// The answer of the upstream to a request that it granted: the headers, and the body still to
/// be read.
pub struct Reply {
    headers: HeaderMap,
    body: Incoming,
    idle: Duration,
}

impl Reply {
    /// The text of the header `name` of the answer, if it has one.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }

    /// The `Content-Length` the upstream announced, if it did.
    #[must_use]
    pub fn length(&self) -> Option<u64> {
        let text = self.headers.get(CONTENT_LENGTH)?.to_str().ok()?;
        text.parse().ok()
    }

    /// The whole body, if it is at most `limit` bytes.
    ///
    /// # Errors
    /// [`UpstreamError::Failed`] for a body that is longer, stalls, or fails.
    pub async fn bytes(self, limit: u64) -> Result<Bytes, UpstreamError> {
        let mut whole = BytesMut::new();
        let mut pieces = self.pieces();
        while let Some(piece) = pieces.next().await {
            let piece = piece.map_err(failed)?;
            let room = usize::try_from(limit).unwrap_or(usize::MAX) - whole.len();
            if piece.len() > room {
                return Err(failed(format!(
                    "the body is over the limit of {limit} bytes"
                )));
            }
            whole.extend_from_slice(&piece);
        }
        Ok(whole.freeze())
    }

    /// The body as a stream for the store to take from, a piece at a time. A wait for the next
    /// piece that takes longer than the upstream's timeout, or a body that fails, ends the stream
    /// with an error after the pieces that were fine.
    #[must_use]
    pub fn pieces(self) -> PartBody<'static> {
        let idle = self.idle;
        stream::unfold(Some(self.body), move |state| async move {
            let mut body = state?;
            loop {
                let waiting = tokio::time::timeout(idle, body.frame());
                match waiting.await {
                    Err(_) => {
                        let why = format!("the upstream sent no byte for {idle:?}");
                        return Some((Err(Error::Other(why)), None));
                    }
                    Ok(None) => return None,
                    Ok(Some(Err(why))) => {
                        return Some((Err(Error::Other(why.to_string())), None));
                    }
                    Ok(Some(Ok(frame))) => {
                        if let Ok(data) = frame.into_data() {
                            return Some((Ok(data), Some(body)));
                        }
                    }
                }
            }
        })
        .boxed()
    }
}

/// What a 401 asks the client to do.
#[derive(Debug, PartialEq, Eq)]
enum Challenge {
    Basic,
    Bearer(Vec<(String, String)>),
    Other,
}

/// The `key="value"` pairs of the challenge parameters in `text`, keys in lower case. A quoted
/// value may hold commas (a `scope` does); a backslash in it is not an escape.
fn params(mut rest: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    loop {
        rest = rest.trim_start_matches(|c: char| c == ',' || c.is_whitespace());
        let Some((key, after)) = rest.split_once('=') else {
            return found;
        };
        let after = after.trim_start();
        let (value, tail) = match after.strip_prefix('"') {
            Some(quoted) => quoted.split_once('"').unwrap_or((quoted, "")),
            None => {
                let (value, tail) = after.split_once(',').unwrap_or((after, ""));
                (value.trim(), tail)
            }
        };
        found.push((key.trim().to_ascii_lowercase(), value.to_owned()));
        rest = tail;
    }
}

/// The challenge in a `WWW-Authenticate` value.
fn challenge(value: &str) -> Challenge {
    let (scheme, rest) = value.trim().split_once(' ').unwrap_or((value.trim(), ""));
    match scheme.to_ascii_lowercase().as_str() {
        "basic" => Challenge::Basic,
        "bearer" => Challenge::Bearer(params(rest)),
        _ => Challenge::Other,
    }
}

/// Whether a secret may be sent to `uri`: it is `https`, or its host is a loopback address.
fn may_carry_secret(uri: &Uri) -> bool {
    let host = uri.host().unwrap_or_default().trim_matches(['[', ']']);
    let loopback = host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    uri.scheme_str() == Some("https") || loopback
}

fn bad_setting(why: impl std::fmt::Display) -> OciError {
    OciError::Upstream(why.to_string())
}

/// The TLS client: the system's CA certificates and those in `ca_pem`.
fn connector(ca_pem: Option<&[u8]>) -> Result<TlsConnector, OciError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    if let Some(pem) = ca_pem {
        let found = CertificateDer::pem_slice_iter(pem).collect::<Result<Vec<_>, _>>();
        let certs =
            found.map_err(|why| bad_setting(format!("ca_file: not PEM certificates: {why}")))?;
        if certs.is_empty() {
            return Err(bad_setting("ca_file: it holds no certificate"));
        }
        for cert in certs {
            roots
                .add(cert)
                .map_err(|why| bad_setting(format!("ca_file: {why}")))?;
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let built = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map(|builder| builder.with_root_certificates(roots).with_no_client_auth());
    let mut config = built.map_err(bad_setting)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsConnector::from(Arc::new(config)))
}

impl Upstream {
    /// The client for `settings`.
    ///
    /// # Errors
    /// [`OciError::Upstream`] if the address is not `http://` or `https://` and a host and no
    /// more (no user name, path or query), if a login would travel in the clear (plain HTTP to
    /// an address that is not loopback), or if `ca_pem` holds no certificate.
    pub fn new(settings: UpstreamSettings) -> Result<Self, OciError> {
        let bad = |why: &str| bad_setting(format!("url: {why}"));
        // The text is not repeated in a message: it may hold a secret that does not belong.
        let base = settings
            .url
            .parse::<Uri>()
            .map_err(|_| bad("it is not an address"))?;
        if !matches!(base.scheme_str(), Some("http" | "https")) {
            return Err(bad("it must start with https:// or http://"));
        }
        if base
            .authority()
            .is_some_and(|authority| authority.as_str().contains('@'))
        {
            return Err(bad(
                "it must not hold a user name or password: use username and password_file",
            ));
        }
        if base.path() != "/" || base.query().is_some() {
            return Err(bad("it must be a host and a port, with no path or query"));
        }
        if settings.login.is_some() && !may_carry_secret(&base) {
            return Err(bad(
                "a password is sent only over https, or to a loopback address",
            ));
        }
        let login = settings
            .login
            .map(|(user, password)| Login { user, password });
        Ok(Self {
            base,
            login,
            tls: connector(settings.ca_pem.as_deref())?,
            timeout: settings.timeout,
            max_redirects: settings.max_redirects,
            body_limit: DEFAULT_MANIFEST_BYTES,
            recheck_timeout: settings.recheck_timeout,
        })
    }

    /// The client, reading a token of at most `bytes` ([`DEFAULT_MANIFEST_BYTES`] if this is not
    /// called); the front passes `params.max_manifest_bytes`.
    #[must_use]
    pub fn with_body_limit(mut self, bytes: u64) -> Self {
        self.body_limit = bytes;
        self
    }

    /// How long the whole re-check of a stored tag may take.
    #[must_use]
    pub fn recheck_timeout(&self) -> Duration {
        self.recheck_timeout
    }

    /// `GET path` (which starts with `/`), logged in as the upstream's challenge asks, with
    /// redirects followed.
    ///
    /// # Errors
    /// [`UpstreamError::NotFound`] for a 404, [`UpstreamError::Status`] for any other status
    /// that is not 200, and [`UpstreamError::Failed`] for a failure to reach the upstream or to
    /// log in to it.
    pub async fn get(&self, path: &str, accept: &str) -> Result<Reply, UpstreamError> {
        self.request(Method::GET, path, accept).await
    }

    /// `HEAD path`, with the login, timeout and redirects of [`get`](Self::get): the headers of
    /// the answer, and no body.
    ///
    /// # Errors
    /// As [`get`](Self::get).
    pub async fn head(&self, path: &str, accept: &str) -> Result<Reply, UpstreamError> {
        self.request(Method::HEAD, path, accept).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        accept: &str,
    ) -> Result<Reply, UpstreamError> {
        let authority = self
            .base
            .authority()
            .map_or("", |authority| authority.as_str());
        let scheme = self.base.scheme_str().unwrap_or_default();
        let url = format!("{scheme}://{authority}{path}")
            .parse::<Uri>()
            .map_err(|_| failed("the path is not valid"))?;
        let (first, reached) = self.follow(&method, &url, accept, None).await?;
        if first.status() != StatusCode::UNAUTHORIZED {
            return settle(first, self.timeout);
        }
        // Only the configured address may ask for the login: a challenge after a redirect to
        // another origin is not answered.
        if reached.authority() != self.base.authority() || reached.scheme() != self.base.scheme() {
            return Err(UpstreamError::Status(401));
        }
        let asked = first.headers().get(WWW_AUTHENTICATE);
        let asked = asked
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let auth = self.answer(challenge(asked)).await?;
        let (second, _) = self.follow(&method, &url, accept, Some(auth)).await?;
        settle(second, self.timeout)
    }

    /// The `Authorization` that answers a challenge.
    async fn answer(&self, asked: Challenge) -> Result<HeaderValue, UpstreamError> {
        match asked {
            Challenge::Basic => match &self.login {
                Some(login) => Ok(basic(login)),
                None => Err(UpstreamError::Status(401)),
            },
            Challenge::Bearer(pairs) => {
                let token = self.token(&pairs).await?;
                HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|_| failed("the token is not a header value"))
            }
            Challenge::Other => Err(UpstreamError::Status(401)),
        }
    }

    /// A token from the realm a `Bearer` challenge names.
    async fn token(&self, pairs: &[(String, String)]) -> Result<String, UpstreamError> {
        let find = |key: &str| pairs.iter().find(|(name, _)| name == key);
        let Some((_, realm)) = find("realm") else {
            return Err(failed("the challenge names no realm"));
        };
        let mut query = Vec::new();
        for (name, value) in pairs
            .iter()
            .filter(|(name, _)| name == "service" || name == "scope")
        {
            query.push(format!(
                "{name}={}",
                utf8_percent_encode(value, NON_ALPHANUMERIC)
            ));
        }
        let joiner = if realm.contains('?') { '&' } else { '?' };
        let url = format!("{realm}{joiner}{}", query.join("&"));
        let url = url
            .parse::<Uri>()
            .map_err(|_| failed("the realm is not an address"))?;
        if !matches!(url.scheme_str(), Some("http" | "https")) {
            return Err(failed("the realm is not an http or https address"));
        }
        let login = self.login.as_ref().filter(|_| may_carry_secret(&url));
        let auth = login.map(basic);
        let (reply, _) = self
            .follow(&Method::GET, &url, "application/json", auth)
            .await?;
        let reply = settle(reply, self.timeout)?;
        let body = reply.bytes(self.body_limit).await?;
        let json: Value =
            serde_json::from_slice(&body).map_err(|_| failed("the token is not JSON"))?;
        let token = json.get("token").or_else(|| json.get("access_token"));
        match token.and_then(Value::as_str) {
            Some(token) => Ok(token.to_owned()),
            None => Err(failed("the answer of the realm holds no token")),
        }
    }

    /// `method url` with `auth`, and the redirects it meets followed without it. The answer comes
    /// with the address it came from.
    async fn follow(
        &self,
        method: &Method,
        url: &Uri,
        accept: &str,
        auth: Option<HeaderValue>,
    ) -> Result<(hyper::Response<Incoming>, Uri), UpstreamError> {
        let mut url = url.clone();
        let mut auth = auth;
        for _ in 0..=self.max_redirects {
            let response = self.send(method, &url, accept, auth.as_ref()).await?;
            if !response.status().is_redirection() {
                return Ok((response, url));
            }
            let location = response.headers().get(LOCATION);
            let location = location.and_then(|value| value.to_str().ok());
            url = resolve(&url, location.unwrap_or_default())?;
            auth = None;
        }
        Err(failed("too many redirects"))
    }

    /// One request, on a connection of its own.
    async fn send(
        &self,
        method: &Method,
        url: &Uri,
        accept: &str,
        auth: Option<&HeaderValue>,
    ) -> Result<hyper::Response<Incoming>, UpstreamError> {
        let https = url.scheme_str() == Some("https");
        let host = url.host().unwrap_or_default().trim_matches(['[', ']']);
        let port = port_of(url);
        let authority = url.authority().map_or("", |authority| authority.as_str());
        let target = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_owned(),
        };
        let mut request = Request::builder()
            .method(method)
            .uri(target)
            .header(HOST, authority)
            .header(ACCEPT, accept)
            .header(USER_AGENT, "tierstore")
            .body(Empty::<Bytes>::new())
            .map_err(failed)?;
        if let Some(auth) = auth {
            request.headers_mut().insert(AUTHORIZATION, auth.clone());
        }
        let attempt = async {
            let tcp = TcpStream::connect((host, port)).await.map_err(failed)?;
            if https {
                let name = ServerName::try_from(host.to_owned()).map_err(failed)?;
                let stream = self.tls.connect(name, tcp).await.map_err(failed)?;
                exchange(stream, request).await
            } else {
                exchange(tcp, request).await
            }
        };
        match tokio::time::timeout(self.timeout, attempt).await {
            Ok(done) => done,
            Err(_) => Err(failed(format!("no answer within {:?}", self.timeout))),
        }
    }
}

/// The port `url` names, or the default of its scheme.
fn port_of(url: &Uri) -> u16 {
    let default = if url.scheme_str() == Some("https") {
        443
    } else {
        80
    };
    url.port_u16().unwrap_or(default)
}

/// Sends `request` on `stream` and gives the response once its headers are in. The connection is
/// driven by a task that ends with the response body.
async fn exchange<S>(
    stream: S,
    request: Request<Empty<Bytes>>,
) -> Result<hyper::Response<Incoming>, UpstreamError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(failed)?;
    tokio::spawn(async move {
        // How the connection ended is what the response said: a failure shows in the body.
        let _ = connection.await;
    });
    sender.send_request(request).await.map_err(failed)
}

/// The reply for a response that is a 200; the error for any other.
fn settle(response: hyper::Response<Incoming>, idle: Duration) -> Result<Reply, UpstreamError> {
    let (parts, body) = response.into_parts();
    match parts.status {
        StatusCode::OK => Ok(Reply {
            headers: parts.headers,
            body,
            idle,
        }),
        StatusCode::NOT_FOUND => Err(UpstreamError::NotFound),
        other => Err(UpstreamError::Status(other.as_u16())),
    }
}

/// `Authorization: Basic` for `login`.
fn basic(login: &Login) -> HeaderValue {
    let pair = format!("{}:{}", login.user, login.password);
    let mut value = HeaderValue::from_str(&format!("Basic {}", STANDARD.encode(pair)))
        .unwrap_or_else(|_| HeaderValue::from_static(""));
    value.set_sensitive(true);
    value
}

/// Where `location` (from a redirect) leads from `from`: an address, or a path on the same host.
fn resolve(from: &Uri, location: &str) -> Result<Uri, UpstreamError> {
    let target = if location.starts_with('/') {
        let authority = from.authority().map_or("", |authority| authority.as_str());
        let scheme = from.scheme_str().unwrap_or_default();
        format!("{scheme}://{authority}{location}")
    } else {
        location.to_owned()
    };
    let uri = target
        .parse::<Uri>()
        .map_err(|_| failed("a redirect leads nowhere"))?;
    if matches!(uri.scheme_str(), Some("http" | "https")) {
        Ok(uri)
    } else {
        Err(failed("a redirect leads nowhere"))
    }
}

#[cfg(test)]
#[path = "upstream_tests.rs"]
mod tests;
