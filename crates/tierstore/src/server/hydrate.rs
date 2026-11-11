//! The hydrate and release calls of a node with a local cache (experimental: the shape of the
//! calls may change until the measurement of the design says they stay).
//!
//! A caller that is about to hand a job scheduler a list of trees to mount sends the same list here
//! first, so that the blobs of the trees are in the cache, or on their way, when the job opens
//! them. Both calls are `POST`s on the S3 listener, signed as any request, to a path outside every
//! bucket (a bucket's own keys are never read as these paths):
//!
//! - `POST /_tierstore/v1/hydrate`, body `{"hold": "<id>", "expires_ms": n, "trees": ["<sha256>", ...]}`:
//!   `hold` and `trees` are required (a tree is the 64 lower-case hex digits of a root manifest),
//!   `expires_ms` is optional (the hold ends by itself after it, cut to `hold_max`; its default is
//!   `hold_max`), and a field the call does not know is ignored, so that a later field is additive.
//!   It is answered `202` with `{"hold": "<id>", "accepted": n}` (`n` the distinct trees of the
//!   call) as soon as the call is accepted: nothing is fetched before the answer. The cache then
//!   expands the trees on a task of its own (see `local_cache::expand`) and the blobs are filled by
//!   the prefetch workers, so a job that opens one that is still being fetched waits for that fetch.
//!   The same `hold` sent again adds its trees to the hold and changes nothing else.
//! - `POST /_tierstore/v1/release`, body `{"hold": "<id>"}`: ends the hold, unpins what it pinned,
//!   drops its blobs that still wait for a worker and that nothing else wants, and stops the
//!   expansion of its trees that is still running. It is answered `200` with
//!   `{"trees", "expanded", "cut", "refused", "blobs": {"filled", "present", "dropped", "failed",
//!   "pending"}}`, and `404` `NoSuchHold` for a hold that is not open (the caller ignores it).
//!
//! The answers that are not 2xx: `400` for a body that is not the shape, a hold id that is empty or
//! over 128 bytes, or a tree that is not a digest; `403` for a key that may not call (see below);
//! `413` for more trees than `hydrate_max_trees` (the body is read up to what that many digests
//! take); `429` when `hold_max_count` holds are open; `501` `HydrateNotAvailable` for a node that
//! has no local cache, or whose `prefetch_concurrency` is 0; `503` `SlowDown` for a cache that does
//! not hold the lock of its directory. A server that does not know the path (a release before this
//! one) answers a signed `POST` to it `404` `NoSuchBucket`, and under a bucket `501`
//! `NotImplemented`, so a caller reads either as "unsupported" and backs off.
//!
//! The full key and a read-only key may call: the call gives a way to spend cache space and
//! upstream reads, bounded by the keys of `[local_cache]`, and no way to read what the key may not.
//! A scoped key may call if it has a read prefix, and then expands only what it may read (each
//! manifest and each `source.key` is judged against its read prefixes). A presigned URL, and a
//! presigning key, may not.

use std::sync::Arc;

use bytes::Bytes;
use hyper::body::Body;
use hyper::http::request::Parts;
use hyper::{Method, StatusCode};
use serde::Deserialize;
use serde_json::json;

use crate::tree_manifest::TreeDigest;

use super::Server;
use super::auth::{Refusal, Signed};
use super::keys::Role;
use super::local_cache::LocalCache;
use super::local_cache::expand::HydrateError;
use super::local_cache::holds::HoldError;
use super::presign::PUT_ONLY;
use super::reply::{Answer, S3Error, answer};
use super::request::{BoxError, Check, Reader};
use super::scoped::{NOT_ALLOWED, Scopes};

/// The path of the hydrate call.
const HYDRATE: &str = "/_tierstore/v1/hydrate";

/// The path of the release call.
const RELEASE: &str = "/_tierstore/v1/release";

/// The longest id of a hold, in bytes.
const HOLD_ID_BYTES: usize = 128;

/// The bytes of the body of a call that are not its trees: the names, the id, the time.
const BODY_BASE: usize = 1024;

/// The bytes one tree takes in the body: 64 digits, the quotes, the comma and some white space.
const BODY_PER_TREE: usize = 128;

/// Which of the two calls a request is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Call {
    Hydrate,
    Release,
}

/// The call a request is, if it is a `POST` to one of the two paths.
pub(super) fn asked(method: &Method, path: &str) -> Option<Call> {
    match (method == Method::POST, path) {
        (true, HYDRATE) => Some(Call::Hydrate),
        (true, RELEASE) => Some(Call::Release),
        _ => None,
    }
}

/// The body of a hydrate call. A field that is not here is ignored.
#[derive(Deserialize)]
struct Hydrate {
    hold: String,
    #[serde(default)]
    expires_ms: Option<u64>,
    trees: Vec<String>,
}

/// The body of a release call. A field that is not here is ignored.
#[derive(Deserialize)]
struct Release {
    hold: String,
}

fn malformed(text: &str) -> S3Error {
    S3Error::new(StatusCode::BAD_REQUEST, "InvalidRequest", text)
}

/// Refuses an id that is empty or too long.
fn check_hold(hold: &str) -> Result<(), S3Error> {
    if hold.is_empty() || hold.len() > HOLD_ID_BYTES {
        let text = format!("The hold id must be 1 to {HOLD_ID_BYTES} bytes.");
        return Err(malformed(&text));
    }
    Ok(())
}

/// The trees of a call, as digests.
fn trees_of(names: &[String]) -> Result<Vec<TreeDigest>, S3Error> {
    let mut trees = Vec::with_capacity(names.len());
    for (place, name) in names.iter().enumerate() {
        let tree = TreeDigest::from_hex(name).ok_or_else(|| {
            malformed(&format!(
                "trees[{place}] is not the lower-case hex of a SHA-256."
            ))
        })?;
        trees.push(tree);
    }
    Ok(trees)
}

/// The error a failed call is answered with.
fn refused(error: &HydrateError) -> S3Error {
    let text = error.to_string();
    match error {
        HydrateError::Off => S3Error::new(
            StatusCode::NOT_IMPLEMENTED,
            "HydrateNotAvailable",
            format!("This tierstore does not take hydrate calls: {text}."),
        ),
        HydrateError::TooManyTrees(_) => {
            S3Error::new(StatusCode::PAYLOAD_TOO_LARGE, "EntityTooLarge", text)
        }
        HydrateError::Hold(HoldError::ReadOnly) => S3Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "SlowDown",
            "This tierstore does not hold the lock of its cache directory now; retry after a moment.",
        )
        .with("retry-after", "1"),
        HydrateError::Hold(HoldError::TooMany(_)) => {
            S3Error::new(StatusCode::TOO_MANY_REQUESTS, "TooManyRequests", text)
                .with("retry-after", "1")
        }
        HydrateError::Hold(HoldError::NoSuchHold(_)) => {
            S3Error::new(StatusCode::NOT_FOUND, "NoSuchHold", text)
        }
        HydrateError::Hold(HoldError::NoSuchTree { .. } | HoldError::NotADigest(_)) => {
            malformed(&text)
        }
    }
}

/// What the key that signed the call may read: everything, or what a scoped key's prefixes cover.
///
/// # Errors
/// An `AccessDenied` for a presigned URL, a presigning key, and a scoped key with no read prefix.
fn reach(parts: &Parts, signed: &Signed) -> Result<Option<Arc<Scopes>>, Refusal> {
    if signed.presign.is_some() || !parts.headers.contains_key("authorization") {
        return Err(NOT_ALLOWED);
    }
    match &signed.role {
        Role::Full | Role::ReadOnly => Ok(None),
        Role::PresignPut => Err(PUT_ONLY),
        Role::Scoped(scopes) if scopes.reads() => Ok(Some(Arc::clone(scopes))),
        Role::Scoped(_) => Err(NOT_ALLOWED),
    }
}

/// The answer of a call that went through: JSON, with the status.
fn ok(status: StatusCode, body: &serde_json::Value) -> Answer {
    let headers = [("content-type", "application/json".to_owned())];
    answer(status, &headers, Bytes::from(body.to_string()))
}

impl Server {
    /// Does the call a signed request is: see the module. The body is read here, up to what
    /// `hydrate_max_trees` digests take, and checked against the hash the signature covers.
    pub(super) async fn hydrate<B>(
        &self,
        parts: &Parts,
        call: Call,
        signed: &Signed,
        body: B,
    ) -> Result<Answer, S3Error>
    where
        B: Body + Send,
        B::Data: Send,
        B::Error: Into<BoxError>,
    {
        let reads = reach(parts, signed)?;
        let Some(cache) = &self.config.cache else {
            return Err(refused(&HydrateError::Off));
        };
        cache.hydrate_ready().map_err(|error| refused(&error))?;
        let trees = cache.config().prefetch.max_trees;
        let limit = self
            .config
            .max_request_bytes
            .min(BODY_BASE.saturating_add(trees.saturating_mul(BODY_PER_TREE)));
        let check = Check::new(&signed.payload, &parts.headers, false)?;
        let config = &self.config;
        let bytes = Reader::new(body, limit, config.body_idle_timeout)
            .with_floor(
                config.put_min_bytes_per_sec,
                config.put_rate_grace,
                Arc::clone(&config.rate_clock),
            )
            .read_all(check)
            .await?;
        match call {
            Call::Hydrate => hydrate(cache, &bytes, reads),
            Call::Release => release(cache, &bytes),
        }
    }
}

/// Takes a hydrate call whose body is `bytes`.
fn hydrate(
    cache: &LocalCache,
    bytes: &[u8],
    reads: Option<Arc<Scopes>>,
) -> Result<Answer, S3Error> {
    let shape = "The body must be a JSON object with \"hold\" (a string), \"trees\" (an array of \
                 strings) and, optionally, \"expires_ms\" (a number).";
    let request: Hydrate = serde_json::from_slice(bytes).map_err(|_| malformed(shape))?;
    check_hold(&request.hold)?;
    let trees = trees_of(&request.trees)?;
    let accepted = cache
        .hydrate(&request.hold, request.expires_ms, &trees, reads)
        .map_err(|error| refused(&error))?;
    let body = json!({"hold": request.hold, "accepted": accepted});
    Ok(ok(StatusCode::ACCEPTED, &body))
}

/// Takes a release call whose body is `bytes`.
fn release(cache: &LocalCache, bytes: &[u8]) -> Result<Answer, S3Error> {
    let shape = "The body must be a JSON object with \"hold\" (a string).";
    let request: Release = serde_json::from_slice(bytes).map_err(|_| malformed(shape))?;
    check_hold(&request.hold)?;
    let released = cache
        .release_hold(&request.hold)
        .map_err(|error| refused(&HydrateError::Hold(error)))?;
    let body = json!({
        "trees": released.trees,
        "expanded": released.expanded,
        "cut": released.cut,
        "refused": released.refused,
        "blobs": {
            "filled": released.filled,
            "present": released.present,
            "dropped": released.dropped,
            "failed": released.failed,
            "pending": released.pending,
        },
    });
    Ok(ok(StatusCode::OK, &body))
}

#[cfg(test)]
#[path = "hydrate_tests.rs"]
mod tests;
