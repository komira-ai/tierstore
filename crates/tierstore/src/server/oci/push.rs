//! Pushing to the OCI front: blob uploads, monolithic and chunked, and manifests.
//!
//! A front serves these only when it asks for keys (`--oci-require-auth`, see `auth`): an
//! anonymous front is read-only. The calls, and what each does with the store:
//!
//! - `POST /v2/<name>/blobs/uploads/?digest=<digest>`: a monolithic upload. The body is the
//!   blob; it is stored where its digest names it, 201.
//! - `POST /v2/<name>/blobs/uploads/`: begins an upload, 202, with the `Location` of the session
//!   and its id in `Docker-Upload-UUID`. The session is one small object, `<prefix>uploads/<id>/begin`.
//!   `mount` and `from` are ignored: there is one namespace, and a client that asked for a mount
//!   goes on with the upload, as the specification lets it.
//! - `PATCH <location>`: one chunk, 202. The chunk is stored as the object
//!   `<prefix>uploads/<id>/c<offset in 20 digits>`, as it arrives, with no hash and no copy in
//!   memory. A `Content-Range` (`<first>-<last>`) must start where the upload is (the sum of the
//!   chunks stored) and say as many bytes as the chunk has, or the answer is 416 and the upload is
//!   as it was (a chunk of unknown length is counted as it is read, and one that has more or
//!   fewer bytes than the range says is stored as nothing); with none the chunk is appended. A
//!   chunk with no byte is accepted and stored as nothing.
//! - `PUT <location>?digest=<digest>`: the last chunk, if the body has bytes, and the end of the
//!   upload, 201. The blob is made in one pass: the chunks stored, in order, and then the body, are
//!   read, hashed and handed to the store's streamed put as they pass, and the put is ended with an
//!   error, which stores nothing, if the hash is not the digest. The chunks are read in ranges of
//!   at most [`STAGE_READ`] bytes. A blob the store says it has already is not taken on that word:
//!   the bytes are read to their end and must hash to the digest, or the answer is the refusal
//!   and the upload is as it was. The chunks and the session are then deleted. Nothing is held
//!   whole, whatever the size of the blob; there is no limit on it.
//! - `DELETE <location>`: cancels the upload and deletes what it stored, 204.
//! - `PUT /v2/<name>/manifests/<reference>`: a manifest or an index. It is read whole (at most
//!   [`MAX_MANIFEST_BYTES`]), must be one (see `manifest`), must hash to the reference if that is
//!   a digest, and must name only blobs the front has: each digest of its config, layers and
//!   manifests is looked up first, and one that is not there is 400 `MANIFEST_BLOB_UNKNOWN`. It is
//!   stored by its digest. A tag as the reference then points at it, by a new version of the
//!   tag made with the create-only write, so that of two pushes of one tag with different digests
//!   exactly one is taken and the other is 409 (see `tags`).
//!
//! The store's calls run on a task of their own, so a client that goes away once its body is read
//! leaves the put to be carried to its end with its journal entry. A backend that must know the
//! length of a body before it is sent (S3 does, see
//! [`Backend::needs_length`](crate::Backend::needs_length)) is given the length, which is the
//! staged bytes and the `Content-Length` of the request, and a request with none is 411.
//!
//! A session is in the store and not in the memory of the process, so a restart or a handover loses
//! none. An upload that is neither finished nor cancelled keeps its chunks, which nothing collects.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures::stream::{self, StreamExt as _};
use http_body_util::BodyExt as _;
use hyper::body::Body as _;
use hyper::header::{CONTENT_LENGTH, CONTENT_RANGE, HeaderName, HeaderValue, LOCATION, RANGE};
use hyper::{HeaderMap, Response, StatusCode};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::sync::Mutex as AsyncMutex;

use super::body::{self, OciBody};
use super::metrics::Outcome;
use super::names::{self, Digest};
use super::tags;
use super::{
    CONTENT_DIGEST, MAX_MANIFEST_BYTES, OciFront, error, header_value, invalid_digest,
    invalid_name, manifest, store_failed,
};
use crate::server::ops::detached;
use crate::server::request::query_pairs;
use crate::{Backend, ByteRange, Error, Listed, PartBody};

pub(super) type Answer = (Outcome, Response<OciBody>);

const UPLOAD_UUID: HeaderName = HeaderName::from_static("docker-upload-uuid");

/// The code of an upload that is not open.
const UNKNOWN_UPLOAD: &str = "BLOB_UPLOAD_UNKNOWN";

/// The most bytes of a stored chunk that the end of an upload reads in one call, so that a chunk of
/// any size is held a piece at a time whatever the backend does with a read of a whole object.
pub(super) const STAGE_READ: u64 = 1 << 20;

/// Why a streamed body was refused, for the answer to send.
pub(super) enum Refusal {
    /// The bytes hashed to `found` (hex), not to the digest that was given.
    Digest { found: String },
    /// The body could not be read as it should: it stalled, or was cut.
    Body(String),
    /// The body has not as many bytes as its `Content-Range` says.
    Range,
}

/// What a streamed body came to: the bytes that passed and, if the body was refused, why.
#[derive(Default)]
pub(super) struct Tally {
    bytes: AtomicU64,
    refusal: Mutex<Option<Refusal>>,
}

impl Tally {
    pub(super) fn add(&self, count: usize) {
        self.bytes.fetch_add(count as u64, Ordering::SeqCst);
    }

    pub(super) fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::SeqCst)
    }

    /// Keeps the first reason only.
    pub(super) fn refuse(&self, why: Refusal) {
        // No method panics while holding the lock, so a poisoned slot is still whole.
        let mut slot = self.refusal.lock().unwrap_or_else(PoisonError::into_inner);
        slot.get_or_insert(why);
    }

    pub(super) fn take(&self) -> Option<Refusal> {
        let mut slot = self.refusal.lock().unwrap_or_else(PoisonError::into_inner);
        slot.take()
    }
}

/// Why a request body could not be read to its end, for the answer to send.
pub(super) struct Unread {
    status: StatusCode,
    message: String,
}

impl Unread {
    fn new(status: StatusCode, message: String) -> Self {
        Self { status, message }
    }
}

/// A request body, read a piece at a time. A wait for the next piece that takes longer than
/// `idle` ends the read, and so does a body that fails; the time the whole body takes is not
/// bounded, so a client that is slow and steady is read.
pub(super) struct Pieces {
    body: OciBody,
    idle: Duration,
}

impl Pieces {
    pub(super) fn new(body: OciBody, idle: Duration) -> Self {
        Self { body, idle }
    }

    /// The next piece, or `None` at the end of the body; a frame of trailers carries no byte and
    /// is passed over.
    pub(super) async fn next(&mut self) -> Result<Option<Bytes>, Unread> {
        loop {
            let waiting = tokio::time::timeout(self.idle, self.body.frame());
            let Ok(frame) = waiting.await else {
                let idle = self.idle;
                let message = format!("No byte of the request body arrived within {idle:?}.");
                return Err(Unread::new(StatusCode::BAD_REQUEST, message));
            };
            match frame {
                None => return Ok(None),
                Some(Err(_)) => {
                    let message = "The request body could not be read in full.".to_owned();
                    return Err(Unread::new(StatusCode::BAD_REQUEST, message));
                }
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        return Ok(Some(data));
                    }
                }
            }
        }
    }
}

/// The pieces of a request body as a stream for the backend to store from. A body that fails ends
/// the stream with an error, after the pieces that were fine, and the reason is in `tally`.
pub(super) fn pieces(reader: Pieces, tally: Arc<Tally>) -> PartBody<'static> {
    stream::unfold((Some(reader), tally), |(reader, tally)| async move {
        let mut reader = reader?;
        match reader.next().await {
            Ok(Some(piece)) => Some((Ok(piece), (Some(reader), tally))),
            Ok(None) => None,
            Err(why) => {
                tally.refuse(Refusal::Body(why.message));
                let failed = Error::Other("the request body failed".to_owned());
                // The error ends the stream: nothing is read after it.
                Some((Err(failed), (None, tally)))
            }
        }
    })
    .boxed()
}

/// `source` counted into `tally` as it passes and, with `verify`, hashed: at its end the hash must
/// be the digest, or the stream ends with an error, which a backend takes as a body to store
/// nothing of, and `tally` has the hash that was found.
pub(super) fn counted(
    source: PartBody<'static>,
    verify: Option<Digest>,
    tally: Arc<Tally>,
) -> PartBody<'static> {
    let pass = (
        source,
        verify.map(|want| (Sha256::new(), want)),
        tally,
        false,
    );
    stream::unfold(pass, |(mut source, mut verify, tally, done)| async move {
        if done {
            return None;
        }
        match source.next().await {
            Some(Ok(piece)) => {
                if let Some((hash, _)) = &mut verify {
                    hash.update(&piece);
                }
                tally.add(piece.len());
                Some((Ok(piece), (source, verify, tally, false)))
            }
            Some(Err(failure)) => Some((Err(failure), (source, verify, tally, true))),
            None => {
                let (hash, want) = verify.take()?;
                let found = hex::encode(hash.finalize());
                if found == want.hex() {
                    return None;
                }
                let failure = Error::BadDigest(want.text());
                tally.refuse(Refusal::Digest { found });
                Some((Err(failure), (source, None, tally, true)))
            }
        }
    })
    .boxed()
}

/// `source` that ends with an error, which a backend takes as a body to store nothing of, unless it
/// gives exactly `want` bytes: a longer body is refused at the piece that passes the count, a
/// shorter one at its end, and `tally` has the reason.
pub(super) fn measured(
    source: PartBody<'static>,
    want: u64,
    tally: Arc<Tally>,
) -> PartBody<'static> {
    stream::unfold(
        (source, 0_u64, tally, false),
        move |(mut source, seen, tally, done)| async move {
            if done {
                return None;
            }
            match source.next().await {
                Some(Ok(piece)) => {
                    let seen = seen + piece.len() as u64;
                    if seen > want {
                        tally.refuse(Refusal::Range);
                        let failed = Error::Other("the chunk is longer than its range".to_owned());
                        return Some((Err(failed), (source, seen, tally, true)));
                    }
                    Some((Ok(piece), (source, seen, tally, false)))
                }
                Some(Err(failure)) => Some((Err(failure), (source, seen, tally, true))),
                None if seen == want => None,
                None => {
                    tally.refuse(Refusal::Range);
                    let failed = Error::Other("the chunk is shorter than its range".to_owned());
                    Some((Err(failed), (source, seen, tally, true)))
                }
            }
        },
    )
    .boxed()
}

/// An empty answer, with `status`.
fn bare(status: StatusCode, outcome: Outcome) -> Answer {
    let mut response = Response::new(body::empty());
    *response.status_mut() = status;
    let length = HeaderValue::from(0);
    response.headers_mut().insert(CONTENT_LENGTH, length);
    (outcome, response)
}

/// Where an upload is.
fn session_path(name: &str, id: &str) -> String {
    format!("/v2/{name}/blobs/uploads/{id}")
}

/// The `Range` of an upload that holds `size` bytes.
fn range_held(size: u64) -> HeaderValue {
    header_value(&format!("0-{}", size.saturating_sub(1)))
}

/// 202: the upload `id` holds `size` bytes.
fn accepted(name: &str, id: &str, size: u64) -> Answer {
    let (outcome, mut response) = bare(StatusCode::ACCEPTED, Outcome::Accepted);
    let headers = response.headers_mut();
    headers.insert(LOCATION, header_value(&session_path(name, id)));
    headers.insert(UPLOAD_UUID, header_value(id));
    headers.insert(RANGE, range_held(size));
    (outcome, response)
}

/// 201: the object is stored at `location`, and `digest` names it.
fn created(location: &str, digest: &Digest) -> Answer {
    let (outcome, mut response) = bare(StatusCode::CREATED, Outcome::Created);
    let headers = response.headers_mut();
    headers.insert(LOCATION, header_value(location));
    headers.insert(CONTENT_DIGEST, header_value(&digest.text()));
    (outcome, response)
}

fn unknown_upload() -> Answer {
    let message = "no upload with this id is open";
    let response = error(StatusCode::NOT_FOUND, UNKNOWN_UPLOAD, message, Value::Null);
    (Outcome::NotFound, response)
}

/// 416: the chunk is not the next bytes of the upload `id`, which holds `size`.
fn out_of_order(name: &str, id: &str, size: u64) -> Answer {
    let message = "the chunk does not start where the upload is, or its range is wrong";
    let mut response = error(
        StatusCode::RANGE_NOT_SATISFIABLE,
        "BLOB_UPLOAD_INVALID",
        message,
        json!({ "received": size }),
    );
    let headers = response.headers_mut();
    headers.insert(LOCATION, header_value(&session_path(name, id)));
    headers.insert(UPLOAD_UUID, header_value(id));
    headers.insert(RANGE, range_held(size));
    (Outcome::Unsatisfiable, response)
}

fn length_required() -> Answer {
    let message = "a Content-Length is needed: this store does not take a body of unknown length";
    let response = error(
        StatusCode::LENGTH_REQUIRED,
        "BLOB_UPLOAD_INVALID",
        message,
        Value::Null,
    );
    (Outcome::Invalid, response)
}

fn body_refused(message: &str) -> Answer {
    let response = error(
        StatusCode::BAD_REQUEST,
        "BLOB_UPLOAD_INVALID",
        message,
        Value::Null,
    );
    (Outcome::Invalid, response)
}

fn digest_refused(found: &str, wanted: &Digest) -> Answer {
    let message = "the bytes of the upload do not hash to the digest given";
    let detail = json!({ "digest": wanted.text(), "found": format!("sha256:{found}") });
    let response = error(StatusCode::BAD_REQUEST, "DIGEST_INVALID", message, detail);
    (Outcome::Invalid, response)
}

fn manifest_invalid(message: &str, detail: Value) -> Answer {
    let response = error(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", message, detail);
    (Outcome::Invalid, response)
}

/// A new upload id: 32 lowercase hex digits from the system's random generator. A generator that
/// fails leaves zeros, and the create-only write of the session's object then refuses the second
/// upload that has them, so two uploads never share an id.
fn new_id() -> String {
    let mut bytes = [0_u8; 16];
    let _ = SystemRandom::new().fill(&mut bytes);
    hex::encode(bytes)
}

/// The value of the `Content-Range` of a chunk, `<first>-<last>`.
fn chunk_range(text: &str) -> Option<(u64, u64)> {
    let (first, last) = text.split_once('-')?;
    let (first, last) = (first.parse::<u64>().ok()?, last.parse::<u64>().ok()?);
    (last >= first).then_some((first, last))
}

/// The number of bytes the `Content-Range` of a chunk says it has, if it has one that is a range.
fn declared(headers: &HeaderMap) -> Option<u64> {
    let value = headers.get(CONTENT_RANGE)?;
    let (first, last) = value.to_str().ok().and_then(chunk_range)?;
    (last - first).checked_add(1)
}

/// Whether a chunk of `len` bytes (if its length is known) with these headers is the next bytes
/// of an upload that holds `size`: with a `Content-Range`, it starts at `size` and says as many
/// bytes as the chunk has (a chunk of unknown length is held to them as it is read, see
/// [`measured`]); with none, it is appended.
fn in_order(headers: &HeaderMap, size: u64, len: Option<u64>) -> bool {
    let Some(value) = headers.get(CONTENT_RANGE) else {
        return true;
    };
    let Some((first, last)) = value.to_str().ok().and_then(chunk_range) else {
        return false;
    };
    first == size && len.is_none_or(|bytes| bytes.checked_sub(1) == Some(last - first))
}

/// Deletes `keys`, in order; a key that cannot be deleted is left, and said on stderr.
async fn discard(backend: &dyn Backend, keys: &[String]) {
    for key in keys {
        if let Err(failure) = backend.delete(key).await {
            tracing::warn!("tierstore: oci: deleting {key}: {failure}");
        }
    }
}

/// The bytes of the body of a manifest, at most [`MAX_MANIFEST_BYTES`].
async fn manifest_bytes(body: OciBody, idle: Duration) -> Result<Bytes, Unread> {
    let limit = usize::try_from(MAX_MANIFEST_BYTES).unwrap_or(usize::MAX);
    let mut pieces = Pieces::new(body, idle);
    let mut whole = BytesMut::new();
    while let Some(piece) = pieces.next().await? {
        if piece.len() > limit - whole.len() {
            let message = format!("The request body is over the limit of {limit} bytes.");
            return Err(Unread::new(StatusCode::PAYLOAD_TOO_LARGE, message));
        }
        whole.extend_from_slice(&piece);
    }
    Ok(whole.freeze())
}

/// What an upload has when its blob is made: the chunks stored, the session they belong to (a
/// monolithic upload has none), and the number of bytes the final body is to have if its
/// `Content-Range` says.
#[derive(Default)]
struct Upload<'a> {
    chunks: Vec<Listed>,
    session: Option<&'a str>,
    declared: Option<u64>,
}

impl OciFront {
    /// The directory of the objects of the upload `id`.
    fn upload_dir(&self, id: &str) -> String {
        format!("{}uploads/{id}/", self.prefix.as_str())
    }

    /// The upload `id`: the chunks it holds, in order. 404 if it is not open (or `id` is not one
    /// this front makes).
    async fn open(&self, id: &str) -> Result<Vec<Listed>, Answer> {
        if !names::is_upload_id(id) {
            return Err(unknown_upload());
        }
        let dir = self.upload_dir(id);
        let failed = |failure: Error| store_failed(&failure, &dir, UNKNOWN_UPLOAD, Value::Null);
        self.backend
            .head(&format!("{dir}begin"))
            .await
            .map_err(failed)?;
        let mut chunks = self
            .backend
            .list(&format!("{dir}c"))
            .await
            .map_err(failed)?;
        chunks.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(chunks)
    }

    /// The chunks stored as one stream, in order, each read in ranges of at most [`STAGE_READ`]
    /// bytes by the sizes the listing gave, so that no read holds a whole chunk.
    fn staged(&self, chunks: Vec<Listed>) -> PartBody<'static> {
        let backend = Arc::clone(&self.backend);
        let reads = chunks.into_iter().flat_map(|chunk| {
            let Listed { key, size, .. } = chunk;
            (0..size.div_ceil(STAGE_READ)).map(move |at| {
                let offset = at * STAGE_READ;
                let range = ByteRange::new(offset, STAGE_READ.min(size - offset));
                (key.clone(), range)
            })
        });
        stream::iter(reads)
            .then(move |(key, range)| {
                let backend = Arc::clone(&backend);
                async move { backend.get(&key, Some(range)).await }
            })
            .boxed()
    }

    /// `POST /v2/<name>/blobs/uploads/`: a monolithic upload with a `digest` in the query, else
    /// the beginning of one.
    pub(super) async fn start_upload(
        &self,
        name: &str,
        query: Option<&str>,
        body: OciBody,
        idle: Duration,
    ) -> Answer {
        if !names::valid_name(name) {
            return invalid_name(name);
        }
        let pairs = query_pairs(query).unwrap_or_default();
        if let Some((_, text)) = pairs.iter().find(|(key, _)| key == "digest") {
            let Some(digest) = Digest::parse(text) else {
                return invalid_digest(text);
            };
            let upload = Upload::default();
            return self.finish(name, upload, digest, body, idle).await;
        }
        let id = new_id();
        let dir = self.upload_dir(&id);
        match self.backend.put(&format!("{dir}begin"), Bytes::new()).await {
            Ok(()) => accepted(name, &id, 0),
            Err(failure) => store_failed(&failure, &dir, UNKNOWN_UPLOAD, Value::Null),
        }
    }

    /// `PATCH <location>`: stores a chunk.
    pub(super) async fn append(
        &self,
        name: &str,
        id: &str,
        headers: &HeaderMap,
        body: OciBody,
        idle: Duration,
    ) -> Answer {
        if !names::valid_name(name) {
            return invalid_name(name);
        }
        let staged = match self.open(id).await {
            Ok(staged) => staged,
            Err(answer) => return answer,
        };
        let size: u64 = staged.iter().map(|chunk| chunk.size).sum();
        let len = body.size_hint().exact();
        if !in_order(headers, size, len) {
            return out_of_order(name, id, size);
        }
        if len.is_none() && self.backend.needs_length() {
            return length_required();
        }
        let tally = Arc::new(Tally::default());
        let want = declared(headers);
        let stored = if len == Some(0) {
            Ok(())
        } else {
            let mut reader = Pieces::new(body, idle);
            // A body of unknown length that turns out to be empty stores no object: an empty
            // chunk at this offset would take the place of the next one.
            let first = loop {
                match reader.next().await {
                    Ok(Some(piece)) if piece.is_empty() => {}
                    Ok(Some(piece)) => break Some(piece),
                    Ok(None) => break None,
                    Err(why) => return body_refused(&why.message),
                }
            };
            let Some(first) = first else {
                return match want {
                    Some(_) => out_of_order(name, id, size),
                    None => accepted(name, id, size),
                };
            };
            let head = stream::once(std::future::ready(Ok::<Bytes, Error>(first)));
            let mut source = head.chain(pieces(reader, Arc::clone(&tally))).boxed();
            if let Some(want) = want {
                source = measured(source, want, Arc::clone(&tally));
            }
            let stream = counted(source, None, Arc::clone(&tally));
            let key = format!("{}c{size:020}", self.upload_dir(id));
            let backend = Arc::clone(&self.backend);
            detached(async move { backend.put_stream(&key, len, stream).await }).await
        };
        match tally.take() {
            Some(Refusal::Body(message)) => return body_refused(&message),
            Some(Refusal::Range) => return out_of_order(name, id, size),
            Some(Refusal::Digest { .. }) | None => {}
        }
        match stored {
            Ok(()) => accepted(name, id, size + tally.bytes()),
            // Another chunk was stored at this offset between the listing and the put.
            Err(Error::AlreadyExists(_)) => out_of_order(name, id, size),
            Err(failure) => store_failed(&failure, id, UNKNOWN_UPLOAD, Value::Null),
        }
    }

    /// `PUT <location>?digest=<digest>`: the end of an upload.
    pub(super) async fn finish_upload(
        &self,
        name: &str,
        id: &str,
        query: Option<&str>,
        headers: &HeaderMap,
        body: OciBody,
        idle: Duration,
    ) -> Answer {
        if !names::valid_name(name) {
            return invalid_name(name);
        }
        let staged = match self.open(id).await {
            Ok(staged) => staged,
            Err(answer) => return answer,
        };
        let pairs = query_pairs(query).unwrap_or_default();
        let text = pairs
            .iter()
            .find(|(key, _)| key == "digest")
            .map_or("", |(_, text)| text.as_str());
        let Some(digest) = Digest::parse(text) else {
            return invalid_digest(text);
        };
        let size: u64 = staged.iter().map(|chunk| chunk.size).sum();
        if !in_order(headers, size, body.size_hint().exact()) {
            return out_of_order(name, id, size);
        }
        let upload = Upload {
            chunks: staged,
            session: Some(id),
            declared: declared(headers),
        };
        self.finish(name, upload, digest, body, idle).await
    }

    /// Makes the blob `digest` from the chunks of `upload` (the upload's session, if it has one,
    /// has them) followed by the body, and then deletes the chunks and the session's object. See
    /// the module docs for the one pass.
    ///
    /// The blob is stored, and the chunks deleted, only if the bytes were read to their end and hash
    /// to `digest`: a store that answers that the blob exists without reading the body (a proxy
    /// whose cold tier has it) has the rest of the stream read here, so that the same bytes are
    /// required of a blob that is there already.
    async fn finish(
        &self,
        name: &str,
        upload: Upload<'_>,
        digest: Digest,
        body: OciBody,
        idle: Duration,
    ) -> Answer {
        let Upload {
            chunks,
            session,
            declared,
        } = upload;
        let held: u64 = chunks.iter().map(|chunk| chunk.size).sum();
        let len = body.size_hint().exact().map(|bytes| bytes + held);
        if len.is_none() && self.backend.needs_length() {
            return length_required();
        }
        let tally = Arc::new(Tally::default());
        let reader = Pieces::new(body, idle);
        let mut doomed: Vec<String> = chunks.iter().map(|chunk| chunk.key.clone()).collect();
        doomed.extend(session.map(|id| format!("{}begin", self.upload_dir(id))));
        let mut last = pieces(reader, Arc::clone(&tally));
        if let Some(want) = declared {
            last = measured(last, want, Arc::clone(&tally));
        }
        let source = self.staged(chunks).chain(last).boxed();
        let whole = counted(source, Some(digest.clone()), Arc::clone(&tally));
        let rest = Arc::new(AsyncMutex::new(whole.fuse()));
        let feed = stream::unfold(Arc::clone(&rest), |rest| async move {
            let next = rest.lock().await.next().await;
            next.map(|piece| (piece, rest))
        })
        .boxed();
        let key = self.prefix.key(&digest);
        let backend = Arc::clone(&self.backend);
        let stored = detached(async move {
            match backend.put_stream(&key, len, feed).await {
                Ok(()) => {}
                Err(Error::AlreadyExists(_)) => {
                    // The blob is there: the bytes are still to be read and found to be it.
                    loop {
                        let next = rest.lock().await.next().await;
                        match next {
                            Some(Ok(_)) => {}
                            Some(Err(failure)) => return Err(failure),
                            None => break,
                        }
                    }
                }
                Err(failure) => return Err(failure),
            }
            discard(backend.as_ref(), &doomed).await;
            Ok(())
        })
        .await;
        match tally.take() {
            Some(Refusal::Digest { found }) => return digest_refused(&found, &digest),
            Some(Refusal::Body(message)) => return body_refused(&message),
            Some(Refusal::Range) => return out_of_order(name, session.unwrap_or(""), held),
            None => {}
        }
        match stored {
            Ok(()) => created(&format!("/v2/{name}/blobs/{}", digest.text()), &digest),
            Err(failure) => store_failed(&failure, &digest.text(), UNKNOWN_UPLOAD, Value::Null),
        }
    }

    /// `DELETE <location>`: cancels an upload.
    pub(super) async fn cancel(&self, name: &str, id: &str) -> Answer {
        if !names::valid_name(name) {
            return invalid_name(name);
        }
        let staged = match self.open(id).await {
            Ok(staged) => staged,
            Err(answer) => return answer,
        };
        let mut doomed: Vec<String> = staged.into_iter().map(|chunk| chunk.key).collect();
        doomed.push(format!("{}begin", self.upload_dir(id)));
        discard(self.backend.as_ref(), &doomed).await;
        bare(StatusCode::NO_CONTENT, Outcome::NoContent)
    }

    /// `PUT /v2/<name>/manifests/<reference>`: stores a manifest or an index by its digest.
    pub(super) async fn put_manifest(
        &self,
        name: &str,
        reference: &str,
        body: OciBody,
        idle: Duration,
    ) -> Answer {
        if !names::valid_name(name) {
            return invalid_name(name);
        }
        let wanted = match Digest::parse(reference) {
            Some(digest) => Some(digest),
            None if names::is_tag(reference) => None,
            None => return invalid_digest(reference),
        };
        let bytes = match manifest_bytes(body, idle).await {
            Ok(bytes) => bytes,
            Err(why) => {
                let response = error(why.status, "MANIFEST_INVALID", &why.message, Value::Null);
                return (Outcome::Invalid, response);
            }
        };
        let digest = Digest::of_hex(hex::encode(Sha256::digest(&bytes)));
        let tag = wanted.is_none().then(|| self.tag_prefix(name, reference));
        if let Some(wanted) = wanted.filter(|wanted| *wanted != digest) {
            return digest_refused(digest.hex(), &wanted);
        }
        if let Err(why) = manifest::media_type(&bytes) {
            return manifest_invalid(&why, json!({ "digest": digest.text() }));
        }
        for text in manifest::references(&bytes) {
            let Some(blob) = Digest::parse(&text) else {
                let message =
                    "the manifest names a digest that is not sha256:<64 lowercase hex digits>";
                return manifest_invalid(message, json!({ "digest": text }));
            };
            let key = self.prefix.key(&blob);
            match self.backend.head(&key).await {
                Ok(_) => {}
                Err(Error::NotFound(_)) => {
                    let message = "the manifest names a blob that is not in the store";
                    let detail = json!({ "digest": text });
                    let response = error(
                        StatusCode::BAD_REQUEST,
                        "MANIFEST_BLOB_UNKNOWN",
                        message,
                        detail,
                    );
                    return (Outcome::Invalid, response);
                }
                Err(failure) => {
                    return store_failed(&failure, &key, "MANIFEST_BLOB_UNKNOWN", Value::Null);
                }
            }
        }
        let key = self.prefix.key(&digest);
        let backend = Arc::clone(&self.backend);
        let pointer = digest.clone();
        let stored = detached(async move {
            match backend.put(&key, bytes).await {
                Ok(()) | Err(Error::AlreadyExists(_)) => {}
                Err(failure) => return Err(failure),
            }
            match tag {
                Some(prefix) => tags::point(backend.as_ref(), &prefix, &pointer).await,
                None => Ok(()),
            }
        })
        .await;
        match stored {
            Ok(()) => created(&format!("/v2/{name}/manifests/{}", digest.text()), &digest),
            Err(Error::PreconditionFailed(_)) => tags::conflict(reference),
            Err(failure) => store_failed(&failure, &digest.text(), "MANIFEST_UNKNOWN", Value::Null),
        }
    }
}
