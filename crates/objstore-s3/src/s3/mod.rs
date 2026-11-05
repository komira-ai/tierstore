//! An [`ObjectStore`] over the S3 REST API: path-style addressing, SigV4 signing,
//! plain HTTP.
//!
//! Written against S3 as MinIO and RustFS serve it. It signs every payload it has (the store
//! checks the body against the signed hash; a part streamed from a source, whose hash is
//! not known when the request is signed, is sent with `UNSIGNED-PAYLOAD`, see `parts.rs`),
//! refuses a ranged read answered with
//! anything but the exact range asked for, and deletes the exact object version on a
//! store with Object Lock, so a retained object is refused rather than hidden behind a
//! delete marker.
//!
//! [`ObjectStore::put_stream`] of an object over [`MULTIPART_THRESHOLD`] is a multipart
//! upload (`multipart.rs`): parts of [`part_size`] sent one at a time, each held in
//! memory only while it is sent, and the upload aborted on any failure.
//!
//! Beyond the trait, [`S3Store`] has the calls an object used for coordination needs
//! (`tagged.rs`): [`S3Store::head`] (size, entity tag, time of last write),
//! [`S3Store::get_all`], [`S3Store::put_if_match`] and [`S3Store::delete_if_match`] (`If-Match`),
//! [`S3Store::list_stamped`] (a listing with the time of last write).
//!
//! A caller that drives a multipart upload itself has the four calls one at a time
//! (`parts.rs`): [`S3Store::create_multipart`], [`S3Store::upload_part_stream`] (the part
//! is sent as it is read from a stream, never held whole), [`S3Store::complete_multipart`]
//! and [`S3Store::abort_multipart`]; [`S3Store::list_multipart_uploads`] lists the uploads
//! that are neither completed nor aborted.
//!
//! Not built yet: TLS (an `https` endpoint is refused).

#[cfg(feature = "testkit")]
pub mod fake;
mod multipart;
mod parts;
#[cfg(test)]
mod read_limit_tests;
mod sigv4;
mod streamed;
mod tagged;
mod times;
mod tuning;
mod xml;

pub use multipart::{
    MAX_PART_BYTES, MAX_PARTS, MIN_PART_BYTES, MULTIPART_THRESHOLD, PART_BYTES, part_size,
};
pub use parts::{OpenUpload, UploadsPage};
pub use streamed::{ObjectBody, ObjectRead};
pub use tagged::{ObjectHead, PutAnswer, StampedObject, StampedPage};
pub use tuning::{READ_RETRIES, S3Tuning};

use std::fmt;
use std::future::Future;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use bytes::{Bytes, BytesMut};
use md5::Md5;
use reqwest::header::{AUTHORIZATION, CONTENT_RANGE};
use reqwest::{Method, Response, StatusCode, Url};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::types::Token;
use crate::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use sigv4::{EMPTY_SHA256, Request, Signer, Utc};
use streamed::{check_partial, too_long};

/// How long [`S3Store::wait_answering`] waits between attempts.
pub const WAIT_RETRY: Duration = Duration::from_millis(250);

/// Whether a failed read may be sent again: it failed in transport and not by running
/// out of time (a store that is `request_timeout` slow stays slow, and a resend would
/// multiply the wait of a read that cannot succeed).
///
/// Two timeouts are not that. After the request went out, the kernel giving up on a
/// dead connection (`ETIMEDOUT`) says nothing of the store, and a fresh connection may
/// well answer; reqwest's `is_timeout` is true for it, as it is for any `io::Error` of
/// kind `TimedOut` in the cause chain, so that chain is searched. Before the request
/// went out (`is_connect`) every timeout stays refused: the client's connect limit also
/// ends as such an `io::Error`, and a store that does not answer a connect in time is
/// slow or down, not a dead connection. So are [`Stalled`] and the per-request limit
/// this module sets on the client, which put no such `io::Error` in the chain.
fn is_retryable(e: &ObjectStoreError) -> bool {
    match e {
        ObjectStoreError::Transport(cause) => {
            if cause.is::<Stalled>() {
                return false;
            }
            match cause.downcast_ref::<reqwest::Error>() {
                Some(r) if r.is_connect() => !r.is_timeout(),
                Some(r) => os_timed_out(r) || !r.is_timeout(),
                None => true,
            }
        }
        _ => false,
    }
}

/// Whether an `io::Error` of kind `TimedOut` (the kernel's `ETIMEDOUT` on a dead
/// connection) is anywhere in the causes of `e`.
fn os_timed_out(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = e.source();
    while let Some(err) = source {
        if err
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::TimedOut)
        {
            return true;
        }
        source = err.source();
    }
    false
}

/// A read that got no byte from the store for `request_timeout`: no answer, or a body
/// that stopped arriving.
#[derive(Debug)]
struct Stalled;

impl fmt::Display for Stalled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("operation timed out: no byte arrived within the request timeout")
    }
}

impl std::error::Error for Stalled {}

/// An access key pair. `Debug` never shows the secret.
#[derive(Clone)]
pub struct Credentials {
    access_key_id: String,
    secret_access_key: String,
}

impl Credentials {
    /// A key pair.
    #[must_use]
    pub fn new(access_key_id: impl Into<String>, secret_access_key: impl Into<String>) -> Self {
        Self {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
        }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .finish()
    }
}

/// Where and how to reach one bucket.
#[derive(Clone, Debug)]
pub struct S3Config {
    /// `http://host[:port]`, with no path.
    pub endpoint: String,
    /// The signing region (MinIO and RustFS accept `us-east-1` unless configured).
    pub region: String,
    /// The bucket this store addresses.
    pub bucket: String,
    /// The key pair requests are signed with.
    pub credentials: Credentials,
    /// What the bucket supports, as the operator declares it; the conformance suite
    /// checks the declaration. `object_lock` needs a bucket created with Object Lock.
    pub capabilities: Capabilities,
    /// Limit on opening a connection.
    pub connect_timeout: Duration,
    /// Limit on a whole request, body included, for a request with no body or a small
    /// one. A ranged read is limited differently: it may take as long as it needs while
    /// bytes keep arriving, and fails when the answer, or the next piece of the body,
    /// takes longer than this to arrive.
    pub request_timeout: Duration,
    /// The slowest rate, in bytes a second, at which a request body may reach the
    /// store: a request with a body may take `request_timeout` plus the body's length
    /// at this rate. `None` gives every request `request_timeout` however large its
    /// body. A store shared with other tenants can take minutes over one large PUT; a
    /// flat limit cuts it however steadily it progresses. Reads do not use this rate:
    /// they are limited by progress (see `request_timeout`), which needs no length.
    pub min_body_rate: Option<NonZeroU64>,
}

/// Why an [`S3Config`] was refused.
#[derive(Debug, thiserror::Error)]
pub enum S3ConfigError {
    /// The endpoint is not `http://host[:port]`.
    #[error("endpoint {endpoint:?}: {why}")]
    Endpoint {
        /// The refused endpoint.
        endpoint: String,
        /// What is wrong with it.
        why: &'static str,
    },
    /// The bucket name is not a valid S3 bucket name.
    #[error(
        "bucket name {0:?} must be 3 to 63 of a-z 0-9 . - and start and end with a letter or digit"
    )]
    Bucket(String),
    /// The HTTP client could not be built.
    #[error("building the HTTP client")]
    Client(#[source] reqwest::Error),
}

/// One S3 bucket.
pub struct S3Store {
    /// Shared with a multipart upload's abort, which may run after its write was
    /// dropped.
    conn: Arc<Conn>,
}

/// What every request to the bucket needs: the client, the address, the signing key.
struct Conn {
    client: reqwest::Client,
    base: Url,
    /// `host[:port]` as the `Host` header carries it.
    host: String,
    bucket: String,
    region: String,
    credentials: Credentials,
    capabilities: Capabilities,
    request_timeout: Duration,
    min_body_rate: Option<NonZeroU64>,
    tuning: S3Tuning,
}

impl fmt::Debug for S3Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Store")
            .field("base", &self.conn.base.as_str())
            .field("bucket", &self.conn.bucket)
            .field("capabilities", &self.conn.capabilities)
            .finish_non_exhaustive()
    }
}

impl S3Store {
    /// A store for `config.bucket`. Does no I/O.
    pub fn new(config: S3Config) -> Result<Self, S3ConfigError> {
        Self::new_tuned(config, S3Tuning::default())
    }

    /// A store for `config.bucket` with the idle-connection and read-retry settings `tuning`
    /// gives. Does no I/O.
    pub fn new_tuned(config: S3Config, tuning: S3Tuning) -> Result<Self, S3ConfigError> {
        let bad = |why| S3ConfigError::Endpoint {
            endpoint: config.endpoint.clone(),
            why,
        };
        let base = Url::parse(&config.endpoint).map_err(|_| bad("not a URL"))?;
        match base.scheme() {
            "http" => {}
            "https" => return Err(bad("TLS is not built yet; use http")),
            _ => return Err(bad("the scheme must be http")),
        }
        if !base.username().is_empty() || base.password().is_some() {
            return Err(bad("credentials belong in `credentials`, not the URL"));
        }
        if base.path() != "/" || base.query().is_some() || base.fragment().is_some() {
            return Err(bad("path-style addressing needs an endpoint with no path"));
        }
        // An `http` URL always has a host, and the userinfo was refused above, so the
        // authority is the host with its port, if it is not the scheme's default.
        let host = base.authority().to_owned();
        if !valid_bucket(&config.bucket) {
            return Err(S3ConfigError::Bucket(config.bucket));
        }
        let client = client_builder(config.connect_timeout, tuning.pool_idle_timeout)
            .build()
            .map_err(S3ConfigError::Client)?;
        let conn = Conn {
            client,
            base,
            host,
            bucket: config.bucket,
            region: config.region,
            credentials: config.credentials,
            capabilities: config.capabilities,
            request_timeout: config.request_timeout,
            min_body_rate: config.min_body_rate,
            tuning,
        };
        Ok(Self {
            conn: Arc::new(conn),
        })
    }
}

/// A client builder with the given limits and idle pool timeout. `request_timeout` is
/// not a client-wide limit: that would bound the whole of a long read. Each request
/// sets its own (`Conn::send`, `Conn::send_read`).
fn client_builder(
    connect_timeout: Duration,
    pool_idle_timeout: Duration,
) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .pool_idle_timeout(pool_idle_timeout)
}

impl Conn {
    /// The limit on a whole request with a `body_len`-byte body.
    fn timeout_for(&self, body_len: usize) -> Duration {
        let Some(rate) = self.min_body_rate else {
            return self.request_timeout;
        };
        let nanos = (body_len as u128).saturating_mul(1_000_000_000) / u128::from(rate.get());
        self.request_timeout.saturating_add(Duration::from_nanos(
            u64::try_from(nanos).unwrap_or(u64::MAX),
        ))
    }

    /// Signs and sends one request, limited as a whole to [`Conn::timeout_for`] its
    /// body. `key` of `None` addresses the bucket.
    async fn send(
        &self,
        method: Method,
        key: Option<&ObjectKey>,
        query: &[(&str, String)],
        headers: Vec<(&'static str, String)>,
        body: Bytes,
    ) -> Result<Response, ObjectStoreError> {
        let req = self
            .signed(method, key, query, headers, &body)
            .timeout(self.timeout_for(body.len()));
        let req = if body.is_empty() { req } else { req.body(body) };
        req.send().await.map_err(transport)
    }

    /// Signs and sends a GET with no whole-request limit: the answer's headers must
    /// arrive within `request_timeout`; the body is then read with [`Conn::within`] a
    /// piece at a time.
    async fn send_read(
        &self,
        key: &ObjectKey,
        headers: Vec<(&'static str, String)>,
    ) -> Result<Response, ObjectStoreError> {
        let req = self.signed(Method::GET, Some(key), &[], headers, &Bytes::new());
        let resp = self.within(req.send()).await?.map_err(transport)?;
        #[cfg(test)]
        read_limit_tests::took_head();
        Ok(resp)
    }

    /// Runs `step`, failing as a stalled read if it takes longer than `request_timeout`.
    async fn within<T>(&self, step: impl Future<Output = T>) -> Result<T, ObjectStoreError> {
        tokio::time::timeout(self.request_timeout, step)
            .await
            .map_err(|_| ObjectStoreError::Transport(Arc::new(Stalled)))
    }

    /// The body of a 2xx answer that may be at most `limit` bytes, read a piece at a time: it
    /// may take as long as it needs while a piece arrives within `request_timeout` of the one
    /// before. An answer whose `Content-Length` is past `limit` fails before any of its body is
    /// read, and a body that goes past `limit` (one with no length, or one that lies about it)
    /// fails at the piece that takes it there, with the rest unread.
    async fn read_body_within(
        &self,
        mut resp: Response,
        limit: u64,
    ) -> Result<Bytes, ObjectStoreError> {
        if resp.content_length().is_some_and(|length| length > limit) {
            return Err(too_long(limit));
        }
        let mut body = BytesMut::new();
        while let Some(piece) = self.within(resp.chunk()).await?.map_err(transport)? {
            #[cfg(test)]
            read_limit_tests::took_body(piece.len());
            if body.len() as u64 + piece.len() as u64 > limit {
                return Err(too_long(limit));
            }
            body.extend_from_slice(&piece);
        }
        Ok(body.freeze())
    }

    /// The signed request, with no limit set and no body attached.
    fn signed(
        &self,
        method: Method,
        key: Option<&ObjectKey>,
        query: &[(&str, String)],
        headers: Vec<(&'static str, String)>,
        body: &[u8],
    ) -> reqwest::RequestBuilder {
        // PERF: hashing a 128 MiB segment takes a fraction of a second on this task's
        // thread; move it off the runtime if upload latency shows it.
        let payload = if body.is_empty() {
            EMPTY_SHA256.to_owned()
        } else {
            hex::encode(Sha256::digest(body))
        };
        self.signed_payload(method, key, query, headers, &payload)
    }

    /// The signed request for a body whose hex SHA-256 is `payload` (or `UNSIGNED-PAYLOAD`),
    /// with no limit set and no body attached.
    fn signed_payload(
        &self,
        method: Method,
        key: Option<&ObjectKey>,
        query: &[(&str, String)],
        mut headers: Vec<(&'static str, String)>,
        payload: &str,
    ) -> reqwest::RequestBuilder {
        // `ObjectKey` holds only unreserved characters and `/`, so the path needs no
        // encoding and is signed exactly as sent.
        let path = match key {
            Some(k) => format!("/{}/{}", self.bucket, k.as_str()),
            None => format!("/{}", self.bucket),
        };
        #[expect(
            clippy::disallowed_methods,
            reason = "a request is signed with the machine's time, which the service checks; a clock for the store is a follow-up"
        )]
        let now = SystemTime::now();
        let amz_date = Utc::of(now, false).amz();
        headers.push(("x-amz-content-sha256", payload.to_owned()));
        headers.push(("x-amz-date", amz_date.clone()));
        let authorization = Signer {
            access_key_id: &self.credentials.access_key_id,
            secret_access_key: &self.credentials.secret_access_key,
            region: &self.region,
        }
        .authorization(
            &Request {
                method: method.as_str(),
                host: &self.host,
                path: &path,
                query,
                headers: &headers,
                payload_sha256: payload,
            },
            &amz_date,
        );

        let mut url = self.base.clone();
        url.set_path(&path);
        let q = sigv4::canonical_query(query);
        url.set_query((!q.is_empty()).then_some(q.as_str()));
        let mut req = self
            .client
            .request(method, url)
            .header(AUTHORIZATION, authorization);
        for (name, value) in headers {
            req = req.header(name, value);
        }
        req
    }
}

impl S3Store {
    /// Creates the bucket, with Object Lock if the store claims it. Succeeds if this
    /// key pair already owns the bucket. For setup and tests; the store's users never call it.
    pub async fn create_bucket(&self) -> Result<(), ObjectStoreError> {
        let mut headers = Vec::new();
        if self.conn.capabilities.object_lock {
            headers.push(("x-amz-bucket-object-lock-enabled", "true".to_owned()));
        }
        let resp = self
            .conn
            .send(Method::PUT, None, &[], headers, Bytes::new())
            .await?;
        match resp.status() {
            StatusCode::OK => Ok(()),
            _ => match failure(resp).await {
                ObjectStoreError::Service { code, .. } if code == "BucketAlreadyOwnedByYou" => {
                    Ok(())
                }
                e => Err(e),
            },
        }
    }

    /// Waits, for at most `within`, until the service answers an HTTP request. Any
    /// answer counts, whatever its status: a running S3 service answers this unsigned
    /// `GET /` with an error such as 403. Only a request that fails to complete (refused,
    /// reset, timed out) is retried, every [`WAIT_RETRY`]. On the deadline it returns
    /// the last attempt's error. The deadline is read on tokio's clock, so a test that
    /// pauses it decides when it comes. For setup and tests, to wait for a store that
    /// is starting; the store's users never call it.
    pub async fn wait_answering(&self, within: Duration) -> Result<(), ObjectStoreError> {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self
                .conn
                .client
                .get(self.conn.base.clone())
                .timeout(left)
                .send()
                .await
            {
                Ok(_) => return Ok(()),
                Err(e) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(transport(e));
                    }
                    tokio::time::sleep(left.min(WAIT_RETRY)).await;
                }
            }
        }
    }
}

/// One ranged read, sent once.
impl S3Store {
    async fn get_range_once(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let header = format!("bytes={}-{}", range.offset(), range.last());
        let resp = self.conn.send_read(key, vec![("range", header)]).await?;
        match resp.status() {
            StatusCode::PARTIAL_CONTENT => {}
            StatusCode::OK => {
                return Err(ObjectStoreError::Protocol(format!(
                    "ranged read of {key} {range:?} answered 200: the store ignored Range"
                )));
            }
            StatusCode::NOT_FOUND => return Err(ObjectStoreError::NotFound(key.clone())),
            StatusCode::RANGE_NOT_SATISFIABLE => {
                return Err(ObjectStoreError::InvalidRange {
                    key: key.clone(),
                    range,
                });
            }
            _ => return Err(self.conn.within(failure(resp)).await?),
        }
        let content_range = resp
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = self.conn.read_body_within(resp, range.size()).await?;
        check_partial(content_range.as_deref(), range, body.len())
            .map_err(|why| ObjectStoreError::Protocol(format!("ranged read of {key}: {why}")))?;
        Ok(body)
    }
}

impl ObjectStore for S3Store {
    fn capabilities(&self) -> Capabilities {
        self.conn.capabilities
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        let mut headers = Vec::new();
        if self.conn.capabilities.conditional_put {
            headers.push(("if-none-match", "*".to_owned()));
        }
        if let Some(until) = retain_until {
            if !self.conn.capabilities.object_lock {
                return Err(ObjectStoreError::Unsupported("Object Lock retention"));
            }
            headers.push(("x-amz-object-lock-mode", "COMPLIANCE".to_owned()));
            headers.push((
                "x-amz-object-lock-retain-until-date",
                Utc::of(until, true).iso8601(),
            ));
            // S3 requires a body checksum on a PUT that sets retention.
            headers.push((
                "content-md5",
                base64::engine::general_purpose::STANDARD.encode(Md5::digest(&body)),
            ));
        }
        let resp = self
            .conn
            .send(Method::PUT, Some(key), &[], headers, body)
            .await?;
        match resp.status() {
            StatusCode::OK => Ok(()),
            StatusCode::PRECONDITION_FAILED if self.conn.capabilities.conditional_put => {
                Err(ObjectStoreError::AlreadyExists(key.clone()))
            }
            _ => Err(failure(resp).await),
        }
    }

    /// One PUT for a body of at most [`MULTIPART_THRESHOLD`] bytes, gathered first; a
    /// multipart upload for a larger one, aborted on any failure.
    async fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> Result<(), ObjectStoreError> {
        if len <= MULTIPART_THRESHOLD {
            let bytes = crate::gather(body, len).await?;
            return self.put_new(key, bytes, None).await;
        }
        multipart::put(&self.conn, key, len, body, None)
            .await
            .map(drop)
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let mut wait = self.conn.tuning.read_retry_wait;
        for _ in 0..self.conn.tuning.read_retries {
            match self.get_range_once(key, range).await {
                Err(e) if is_retryable(&e) => {
                    tracing::debug!(%key, error = %e, "ranged read failed in transport; sending it again");
                    tokio::time::sleep(wait).await;
                    wait *= 2;
                }
                answer => return answer,
            }
        }
        self.get_range_once(key, range).await
    }

    /// A HEAD: the `Content-Length` of the object, or `None` for a 404.
    async fn len(&self, key: &ObjectKey) -> Result<Option<u64>, ObjectStoreError> {
        match self.head(key).await {
            Ok(head) => Ok(Some(head.size)),
            Err(ObjectStoreError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        let mut query = Vec::new();
        if self.conn.capabilities.object_lock {
            // A bucket with Object Lock is versioned: a plain DELETE would succeed by
            // adding a delete marker and keep the bytes. Delete the current version
            // itself, which the store refuses while it is retained.
            let head = self
                .conn
                .send(Method::HEAD, Some(key), &[], Vec::new(), Bytes::new())
                .await?;
            match head.status() {
                StatusCode::OK => {}
                StatusCode::NOT_FOUND => return Ok(()),
                _ => return Err(failure(head).await),
            }
            if let Some(v) = head
                .headers()
                .get("x-amz-version-id")
                .and_then(|v| v.to_str().ok())
                .filter(|v| !v.is_empty() && *v != "null")
            {
                query.push(("versionId", v.to_owned()));
            }
        }
        let resp = self
            .conn
            .send(Method::DELETE, Some(key), &query, Vec::new(), Bytes::new())
            .await?;
        match resp.status() {
            StatusCode::NO_CONTENT | StatusCode::OK | StatusCode::NOT_FOUND => Ok(()),
            _ => Err(failure(resp).await),
        }
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        let body = self.list_body(prefix, after, max_keys).await?;
        xml::list_page(&body)
    }
}

impl S3Store {
    /// The body of the answer to one ListObjectsV2 request.
    async fn list_body(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<Bytes, ObjectStoreError> {
        let mut query = vec![
            ("list-type", "2".to_owned()),
            ("prefix", prefix.as_str().to_owned()),
            ("max-keys", max_keys.get().to_string()),
        ];
        match after.map(|t| &t.0) {
            Some(Token::Continue(t)) => query.push(("continuation-token", t.clone())),
            Some(Token::After(key) | Token::Kept { after: key, .. }) => {
                query.push(("start-after", key.as_str().to_owned()))
            }
            None => {}
        }
        let resp = self
            .conn
            .send(Method::GET, None, &query, Vec::new(), Bytes::new())
            .await?;
        if resp.status() != StatusCode::OK {
            return Err(failure(resp).await);
        }
        resp.bytes().await.map_err(transport)
    }
}

/// A transport failure, without the request URL: the text reaches REAPI clients (a CAS
/// `Unavailable` status), and the URL names the store's address, bucket and key. The
/// kind and causes ("operation timed out", "connection reset") stay.
fn transport(e: reqwest::Error) -> ObjectStoreError {
    ObjectStoreError::Transport(std::sync::Arc::new(e.without_url()))
}

/// The error a non-success answer carries.
async fn failure(resp: Response) -> ObjectStoreError {
    let status = resp.status().as_u16();
    let body = resp.bytes().await.unwrap_or_default();
    let (code, message) = xml::error_code_message(&body);
    ObjectStoreError::Service {
        status,
        code,
        message,
    }
}

fn valid_bucket(b: &str) -> bool {
    let edge_ok = |c: Option<char>| c.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    (3..=63).contains(&b.len())
        && b.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
        && edge_ok(b.chars().next())
        && edge_ok(b.chars().last())
}

#[cfg(test)]
mod tests;
