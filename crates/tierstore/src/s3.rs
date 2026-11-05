//! [`S3Backend`]: a [`Backend`] over the S3 client of `objstore-s3` (feature `s3`).
//!
//! The backend is an adapter and keeps one bit of configuration (how `delete_if_match` is done, below):
//! each call of the trait is the S3 request of the same meaning, sent by the client the
//! workspace already has (path-style addressing, SigV4, a timeout on every request), and each
//! answer is read into this crate's [`Error`].
//!
//! - `put` is a PUT with `If-None-Match: *`; the 412 is [`Error::AlreadyExists`]. Create-only
//!   (here, in `put_stream` and in `complete_upload`) needs a store that honours conditional
//!   writes: one that ignores `If-None-Match` on a put or on the completion of a multipart upload
//!   would replace an object that exists.
//! - `put_stream` is the store client's streamed write, given the chunks of the body as they
//!   arrive and their total length (the `Content-Length` of the request; without one the body
//!   is held first, since S3 takes no request of unknown length). A body of at most
//!   `server.s3_single_put_max` (5 GiB, S3's limit for one PUT, by default) is streamed as one PUT
//!   with `If-None-Match: *`, never held whole; a longer one is a multipart upload
//!   whose parts (`server.s3_part_bytes`, 16 MiB by default) are sent as they fill, completed with `If-None-Match: *` only after the body
//!   has ended with exactly its length, and aborted on any failure. Either way the object comes
//!   into being in the last request, after the whole body was read and checked by the caller, so
//!   a body that ends with an error, or is cut short, leaves no object and no staged key, and
//!   at most one part of the body is held at a time (a few chunks for a single PUT). A key that exists is
//!   [`Error::AlreadyExists`].
//! - `put_if_match` is a PUT with `If-Match: "<tag>"` (a tag given in quotes is not quoted
//!   twice); the 412 is [`Error::PreconditionFailed`] and a 404 is [`Error::NotFound`]. It
//!   gives the `ETag` of the answer, without its quotes.
//! - `get` is a GET, with `Range: bytes=<first>-<last>` for a range.
//! - `get_stream` is the same GET, ranged or not, whose answer body is given chunk by chunk as it
//!   arrives and never collected (the client reads it a piece at a time, each piece within
//!   `request_timeout` of the one before); a body that fails or ends short is an error as the
//!   last chunk. The request is sent again after a failure in transport before the response
//!   headers arrived, and not after: valid headers followed by a failure before the first body
//!   byte give the read, whose body then ends with the error.
//! - `open_read` is the same GET whose answer also gives the object's meta: the size (the
//!   `Content-Length`, or the total of the `Content-Range` of a ranged read), the `ETag` and the
//!   `Last-Modified`. A range with an end, or none, is sent as it is; the last bytes
//!   of an object (a suffix) and a range the store answers 416 to are a HEAD and then a ranged
//!   GET, as the size is needed to say what they cover. An answer without a `Content-Length`, a
//!   `Last-Modified` or an `ETag` is an [`Error::Other`].
//! - `head` is a HEAD: `Content-Length`, `ETag` (the content tag, without its quotes, as the
//!   other backends give it and the S3 front, which quotes it, expects) and `Last-Modified`.
//! - `get_tagged` is one GET, whole or ranged, and the content tag is the `ETag` (without its
//!   quotes) of that answer, so the bytes and the tag are those of one object whatever is written to
//!   the key meanwhile; an answer without an `ETag` is an [`Error::Other`].
//! - `put_stream_if_match` is `put_if_match` for a body that arrives as chunks: a body of at most
//!   `server.s3_single_put_max` is streamed as one PUT with `If-Match`, a longer one is a multipart upload
//!   whose completion carries `If-Match`, aborted on any failure, so the object is replaced only
//!   after the whole body was read and only if it still has the tag. A body of unknown length is
//!   not collected either: its first `server.s3_part_bytes` (16 MiB by default) are read, and a body
//!   that ends there is one PUT, while a longer one is a multipart upload whose parts are each sent as soon as they are full,
//!   so at most one part and one chunk are held. It is sent once. The tag
//!   that does not match is [`Error::PreconditionFailed`] and no object is [`Error::NotFound`].
//! - `delete` is a DELETE; a 404 is a success.
//! - `delete_if_match` is best effort, in one of two ways, which the tier's `conditional_delete`
//!   setting picks ([`S3Backend::with_conditional_delete`]; the backend does not ask the store).
//!   When it is on (set it for a store that enforces `If-Match` on a DELETE, as Amazon S3 does) it
//!   is a DELETE with `If-Match: "<tag>"`: a 412 is [`Error::PreconditionFailed`] and a 404 is
//!   [`Error::NotFound`]. When it is off, the default, it is a HEAD, a comparison of the tags,
//!   and a plain DELETE, which leaves a window in which another writer's replacement of the key
//!   is deleted; that is right on every store, only less tight. A store that takes the header
//!   and deletes anyway must have the setting off. A store that answers 404 or 412 for a key
//!   that is absent is the same to a caller: not the object it read.
//! - `list` and `list_page` are ListObjectsV2 with `prefix`, a `max-keys` of at most 1000,
//!   `start-after` on the first request and the `continuation-token` of each answer on the
//!   next. A page that repeats the token it was sent ends the listing with an
//!   [`Error::Other`].
//! - `create_upload` is CreateMultipartUpload and gives the upload id of the store.
//! - `upload_part_with_len` is UploadPart whose body is the chunks of the part as they
//!   arrive, never collected: the request carries the length as its `Content-Length`, is
//!   signed with `UNSIGNED-PAYLOAD`, and ends with the body, so a part of any size is held a
//!   few chunks at a time. A body that gives an error, also after its last byte, or more or
//!   fewer bytes than the length, stores nothing and gives that error ([`Error::Other`] for
//!   the length). `upload_part`, for a caller that does not know the length, holds the whole
//!   part first, since S3 takes no part of unknown length. The tag of a part is the `ETag` of
//!   the answer without its quotes, which is how the S3 front names a part to its client and
//!   how a completion names it again. The signature of a part carries no hash of its bytes, so
//!   the store has none to check them against; the connection is plain HTTP (the client
//!   refuses TLS), so a part is as safe on its way as the network between the proxy and the
//!   store.
//! - `complete_upload` is CompleteMultipartUpload with `If-None-Match: *`, so a key that
//!   exists is [`Error::AlreadyExists`] and the upload stays open. It gives the `ETag` the
//!   answer names for the object (without its quotes), which is what `head` gives it too, and
//!   needs no second request, which could fail after the object was made. On S3 the tag
//!   of an object made of parts is not the MD5 of its bytes, so it is not the tag of a `put`
//!   of the same bytes, unlike in the in-memory and the directory backend.
//! - `abort_upload` is AbortMultipartUpload.
//! - `list_uploads` is ListMultipartUploads (`prefix`, `max-uploads`, `key-marker` and
//!   `upload-id-marker`), whose pages the backend follows until it has the uploads asked for or
//!   the listing ends.
//! - An upload the store does not hold (`NoSuchUpload`) is [`Error::NotFound`] with the upload
//!   id, and a completion the store refuses for its parts (`InvalidPart`, `InvalidPartOrder`,
//!   `EntityTooSmall`) is [`Error::InvalidPart`]. S3 takes no part under 5 MiB but the last.
//!
//! Notes on the mapping:
//!
//! - A key is the client's [`ObjectKey`], which follows S3's rules (`objstore_s3::key_rules`):
//!   any text of 1 to 1024 bytes, space, `+`, `%`, `&`, `<`, `?`, `#` and non-ASCII text
//!   included, escaped by the client; a key or a prefix the rules refuse (over 1024 bytes, or a
//!   `.` or `..` segment) is [`Error::InvalidKey`] (S3 answer 400 `KeyTooLongError`).
//! - S3 answers 416 to a range that starts at or past the end of the object, and a range of
//!   length zero is no request at all. The trait wants no bytes for a range starting at the
//!   end and an [`Error::Other`] for one starting past it, so a HEAD decides both, and finds
//!   out whether the key exists. A 416 to a range that holds bytes of the object is an
//!   [`Error::Other`] too, not an empty read.
//! - A store reads the positions of a `Range` as signed 64-bit numbers, so a range that
//!   runs past 2^63 - 1 is cut there before it is sent (the object cannot be longer).
//! - A listing of more than 1000 objects is several requests: `list_page` follows the
//!   continuation tokens until it has `max_keys` objects or the listing ends, since a page
//!   shorter than `max_keys` means the last.
//! - The content tag is the `ETag`, without its quotes. S3 makes it the MD5 of the bytes of an object written by
//!   one PUT, which is what every write of this backend is, so equal bytes have equal tags
//!   in two buckets of one kind of store. A store that encrypts with keys of its own makes
//!   the tag from something else: compare tags only between backends of one store.
//! - `modified` of a [`Meta`] is the `Last-Modified` of a HEAD, in whole seconds. That of a
//!   [`Listed`] is the `LastModified` of the listing, as fine as the store sends it (MinIO
//!   sends milliseconds), so the two times of one object need not be equal.
//! - Keys must be ones the client can name without escaping (`A-Z a-z 0-9 - _ . ~`, and `/`
//!   between non-empty segments); any other key is an [`Error::Other`] before a request is
//!   made. The `start_after` of `list_page` is named as a key as well, so one that is not
//!   nameable (`a/`, say) is refused although another backend takes any string.
//! - A request that did not complete, and the statuses 408, 429 and every 5xx but 501 and 507,
//!   are [`Error::Unavailable`] (a server error may be gone on the next try). 507
//!   (Insufficient Storage, which is how MinIO reports a full drive) is [`Error::Full`]. 501 (Not
//!   Implemented: the store does not do what was asked) and anything else that is not a success
//!   is [`Error::Other`]. A 409 `ConditionalRequestConflict` (two writers racing a conditional
//!   write of one key; the store does not say which won) is [`Error::Unavailable`]: the write may
//!   have landed, so the caller reads the key to see whether the tag is the one it wrote, and does
//!   not send the write again as if it had not been applied (a retry is answered 412 if the other
//!   writer won, and also if this one did).
//!
//! Each request is bounded by the `request_timeout` of the [`S3Config`], and a read is sent
//! again after a failure in transport; a write never is. A part is the exception to the bound:
//! it is as slow as the client that sends it, so only a part that makes no progress for the
//! `request_timeout` is cut, as an [`Error::Unavailable`].

use std::future::Future;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use futures::future;
use futures::stream;
use objstore_s3::s3::{S3Store, StampedObject};
use objstore_s3::{
    BodyStream, KeyPrefix, ListToken, ObjectKey, ObjectStore, ObjectStoreError, PageSize,
};
use tokio::sync::mpsc;

use crate::backend::open_by_head;
use crate::{
    Backend, ByteRange, Error, Listed, Meta, OpenUpload, Opened, PartBody, ReadBody, Span,
    UploadedPart, UploadsAfter, Window,
};

pub use objstore_s3::Capabilities;
pub use objstore_s3::s3::{Credentials, S3Config, S3ConfigError, S3Tuning};

/// The greatest byte position a `Range` names. A store reads the positions as signed 64-bit
/// numbers and refuses or ignores a larger one, so a range is cut here before it is sent.
const LAST_POSITION: u64 = u64::MAX >> 1;

/// The default of how many chunks of a part wait for the request that sends them to take them
/// ([`S3Backend::with_chunks_waiting`]).
pub(crate) const CHUNKS_WAITING: usize = 1;

/// The codes with which a store refuses a completion for the parts it names.
const INVALID_PART_CODES: [&str; 3] = ["InvalidPart", "InvalidPartOrder", "EntityTooSmall"];

/// One bucket of an S3-compatible store, as a [`Backend`].
#[derive(Debug)]
pub struct S3Backend {
    store: S3Store,
    /// Whether `delete_if_match` sends `If-Match` with the DELETE; otherwise it reads the tag
    /// first. Set from the tier's `conditional_delete` setting; off unless set.
    conditional_delete: bool,
    /// How many chunks of a part wait for the request that sends them to take them.
    chunks_waiting: usize,
}

impl S3Backend {
    /// A backend for the bucket `config` names. Does no I/O.
    ///
    /// The capabilities of `config` are replaced: the backend needs the conditional put
    /// whatever the caller claims (a create-only put that replaces is not [`Backend::put`]),
    /// and has no use for Object Lock.
    ///
    /// # Errors
    /// [`S3ConfigError`] if the endpoint or the bucket name is refused.
    pub fn new(config: S3Config) -> Result<Self, S3ConfigError> {
        Self::new_tuned(config, S3Tuning::default())
    }

    /// As [`S3Backend::new`], with the idle-connection and read-retry settings `tuning` gives.
    ///
    /// # Errors
    /// [`S3ConfigError`] if the endpoint or the bucket name is refused.
    pub fn new_tuned(mut config: S3Config, tuning: S3Tuning) -> Result<Self, S3ConfigError> {
        config.capabilities = Capabilities {
            conditional_put: true,
            object_lock: false,
        };
        Ok(Self {
            store: S3Store::new_tuned(config, tuning)?,
            conditional_delete: false,
            chunks_waiting: CHUNKS_WAITING,
        })
    }

    /// This backend holding `chunks` chunks of a part at most between the body it is given and
    /// the request that sends it ([`CHUNKS_WAITING`] if this is not called); zero is taken as one.
    #[must_use]
    pub fn with_chunks_waiting(mut self, chunks: usize) -> Self {
        self.chunks_waiting = chunks.max(1);
        self
    }

    /// How many chunks of a part wait for the request that sends them.
    #[cfg(test)]
    pub(crate) fn chunks_waiting(&self) -> usize {
        self.chunks_waiting
    }

    /// This backend with `delete_if_match` sent as a DELETE with `If-Match` (`true`), or as a read
    /// of the tag before a plain DELETE (`false`, as [`S3Backend::new`] makes it). The store is
    /// not asked which it enforces: the caller says, from what its store documents.
    #[must_use]
    pub fn with_conditional_delete(mut self, enabled: bool) -> Self {
        self.conditional_delete = enabled;
        self
    }

    /// The bytes of `range` of the object, from the client's ranged read.
    async fn read_range(
        &self,
        object: &ObjectKey,
        key: &str,
        range: ByteRange,
    ) -> Result<Bytes, Error> {
        if let Some(asked) = client_range(range) {
            match self.store.get_range(object, asked).await {
                Err(ObjectStoreError::InvalidRange { .. }) => {}
                answer => return answer.map_err(|e| map_error(e, key)),
            }
        }
        let nothing = self.nothing(object, key, range).await;
        nothing.map(|_| Bytes::new())
    }

    /// The read of `key`, or of `range` of it, as the chunks of the body of one GET and the tag
    /// of that answer.
    async fn open(&self, key: &str, range: Option<ByteRange>) -> Result<Answered, Error> {
        let object = object_key(key)?;
        let answer = match range {
            None => self.store.get_stream(&object, None).await,
            Some(range) => match client_range(range) {
                Some(asked) => self.store.get_stream(&object, Some(asked)).await,
                None => return self.nothing(&object, key, range).await,
            },
        };
        if let (Err(ObjectStoreError::InvalidRange { .. }), Some(range)) = (&answer, range) {
            return self.nothing(&object, key, range).await;
        }
        let read = answer.map_err(|e| map_error(e, key))?;
        let name = key.to_owned();
        let body = read
            .body
            .map(move |piece| piece.map_err(|e| map_error(e, &name)));
        Ok((body.boxed(), read.etag))
    }

    /// The read of `range`, which is empty or which S3 answered 416 to: a HEAD says how long the
    /// object is, and so whether the 416 was right. No bytes, and the entity tag of the HEAD, if the
    /// range holds none of the object; an error if it holds some, or starts past the end.
    async fn nothing(
        &self,
        object: &ObjectKey,
        key: &str,
        range: ByteRange,
    ) -> Result<Answered, Error> {
        let head = self.store.head(object).await;
        let head = head.map_err(|e| map_error(e, key))?;
        let size = head.size;
        if range.offset() > size {
            return Err(Error::Other(format!(
                "range starting at {} is past the end of the {size}-byte object",
                range.offset()
            )));
        }
        let held = range.size().min(size - range.offset());
        if held > 0 {
            return Err(Error::Other(format!(
                "the store refused the range of {held} bytes at {} of the {size}-byte object",
                range.offset()
            )));
        }
        Ok((stream::empty().boxed(), Some(head.etag)))
    }

    /// At most `limit` of the objects under `prefix` that sort after `start_after`, in key
    /// order, asking for a page of the client at a time.
    async fn listed(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Listed>, Error> {
        let under = KeyPrefix::new(prefix).map_err(|_| Error::InvalidKey(prefix.to_owned()))?;
        let after = start_after.map(object_key).transpose()?;
        let mut after = after.map(|key| ListToken::after(&key));
        let mut listed = Vec::new();
        while listed.len() < limit {
            let room = limit - listed.len();
            let size = u16::try_from(room).ok().and_then(PageSize::new);
            let size = size.unwrap_or(PageSize::MAX);
            let page = self.store.list_stamped(&under, after.as_ref(), size);
            let page = page.await.map_err(|e| map_error(e, prefix))?;
            listed.extend(page.objects.into_iter().map(listed_of));
            let Some(next) = page.next else { break };
            if after.as_ref() == Some(&next) {
                let why = format!("{prefix:?} listing stalled: the token did not change");
                return Err(Error::Other(why));
            }
            after = Some(next);
        }
        listed.truncate(limit);
        Ok(listed)
    }
}

/// The range as the client names it, cut where a store reads positions as signed 64-bit numbers,
/// or `None` for a range that holds no byte.
fn client_range(range: ByteRange) -> Option<objstore_s3::ByteRange> {
    let room = LAST_POSITION.saturating_sub(range.offset());
    let len = range.size().min(room);
    objstore_s3::ByteRange::new(range.offset(), len)
}

/// The key as the client names it, or [`Error::InvalidKey`] if S3's rules refuse it (see
/// `objstore_s3::key_rules`: empty, over 1024 bytes, or a `.` or `..` segment).
fn object_key(key: &str) -> Result<ObjectKey, Error> {
    ObjectKey::new(key).map_err(|_| Error::InvalidKey(key.to_owned()))
}

/// An object of a listing of the client as one of this crate.
fn listed_of(object: StampedObject) -> Listed {
    Listed {
        key: object.key.as_str().to_owned(),
        size: object.size,
        modified: object.modified_millis,
        tag: unquoted(&object.etag),
    }
}

/// An upload of a listing of the client as one of this crate.
fn open_of(upload: objstore_s3::s3::OpenUpload) -> OpenUpload {
    OpenUpload {
        key: upload.key.as_str().to_owned(),
        id: upload.upload_id,
        initiated: upload.initiated_millis,
    }
}

/// A part of a listing of the client as one of this crate: its tag is the `ETag` without quotes,
/// as [`Backend::upload_part`] gives it.
fn part_of(part: objstore_s3::s3::StoredPart) -> UploadedPart {
    UploadedPart {
        number: part.number,
        tag: unquoted(&part.etag),
        size: part.size,
        modified: part.modified_millis,
    }
}

/// What a failed call of the client means in the terms of this crate, for the call on `key`.
fn map_error(error: ObjectStoreError, key: &str) -> Error {
    let key = key.to_owned();
    let message = error.to_string();
    match error {
        ObjectStoreError::NotFound(_) => Error::NotFound(key),
        ObjectStoreError::AlreadyExists(_) => Error::AlreadyExists(key),
        ObjectStoreError::Transport(_) => Error::Unavailable(message),
        // 409 `ConditionalRequestConflict`: another write of the key was in flight when this
        // conditional write arrived (S3 and MinIO both answer it for two writers racing a
        // create-only put). Neither the store's documentation nor its answer says which writer
        // won, so the outcome is unknown (this write may have landed), and the answer is the one
        // the documentation gives for it: try again. A retry gets a 412 if the other writer won,
        // and also if this one did, so a caller reads the key before it believes a 412.
        ObjectStoreError::Service { ref code, .. } if code == "ConditionalRequestConflict" => {
            Error::Unavailable(message)
        }
        ObjectStoreError::Service { status, .. } => by_status(status, key, message),
        _ => Error::Other(message),
    }
}

/// What an answer of the store with this HTTP status means.
fn by_status(status: u16, key: String, message: String) -> Error {
    match status {
        412 => Error::PreconditionFailed(key),
        507 => Error::Full(message),
        501 => Error::Other(message),
        408 | 429 | 500..=599 => Error::Unavailable(message),
        _ => Error::Other(message),
    }
}

/// What a failed call on the upload `id` of `key` means: an upload the store does not hold is
/// [`Error::NotFound`] with the id, a completion it refuses for its parts is
/// [`Error::InvalidPart`], and anything else is what it is for any call on `key`.
fn upload_error(error: ObjectStoreError, key: &str, id: &str) -> Error {
    let code = service_code(&error);
    if code == "NoSuchUpload" {
        return Error::NotFound(id.to_owned());
    }
    if INVALID_PART_CODES.contains(&code) {
        return Error::InvalidPart(error.to_string());
    }
    map_error(error, key)
}

/// The S3 code of an answer of the store that refused, or the empty text for any other error.
fn service_code(error: &ObjectStoreError) -> &str {
    match error {
        ObjectStoreError::Service { code, .. } => code.as_str(),
        _ => "",
    }
}

/// The tag of an object or of a part as the other backends give it and the front names it:
/// the `ETag` without its quotes.
fn unquoted(etag: &str) -> String {
    etag.trim_matches('"').to_owned()
}

/// What a read opens: the chunks of its body and the entity tag (as the store sends it,
/// if it sends one) of the answer they come from.
type Answered = (ReadBody, Option<String>);

/// The content tag of a read from the `ETag` of its answer, without its quotes. An answer that
/// has none cannot tell which object was read, so a read that must say is an error.
fn answered_tag(etag: Option<String>, key: &str) -> Result<String, Error> {
    let said = etag.ok_or_else(|| Error::Other(format!("{key}: the read's answer has no ETag")))?;
    Ok(unquoted(&said))
}

/// A content tag as an entity tag is sent to the store in `If-Match`: in quotes, once.
fn quoted(tag: &str) -> String {
    format!("\"{}\"", tag.trim_matches('"'))
}

/// The bytes of `body`, collected, for a part of a length that is not known.
async fn collect(mut body: PartBody<'_>) -> Result<Bytes, Error> {
    let mut whole = BytesMut::new();
    while let Some(chunk) = body.next().await {
        whole.extend_from_slice(&chunk?);
    }
    Ok(whole.freeze())
}

/// A part that is held whole, as a length and a body of one chunk.
fn held(whole: Bytes) -> (u64, PartBody<'static>) {
    let len = whole.len() as u64;
    (len, stream::once(future::ready(Ok(whole))).boxed())
}

/// Sends the chunks of `body` to `sink`, to be the body of the request that sends them to the
/// store, and gives the error that ended `body`, if one did. It stops when the request is over
/// and takes no more.
async fn feed(
    mut body: PartBody<'_>,
    sink: mpsc::Sender<Result<Bytes, ObjectStoreError>>,
) -> Option<Error> {
    while let Some(chunk) = body.next().await {
        let sent = match chunk {
            Ok(bytes) => sink.send(Ok(bytes)).await,
            Err(error) => {
                let ended = ObjectStoreError::Body(error.to_string());
                let _ = sink.send(Err(ended)).await;
                return Some(error);
            }
        };
        if sent.is_err() {
            return None;
        }
    }
    None
}

/// Sends `body` to the store as the body of the request that `send` makes, which is given its
/// length and the chunks as they arrive. A body whose length is not known is held first, since
/// S3 takes no request of unknown length. Gives the error of `body`, if it ended with one,
/// else the answer of the store.
async fn streamed<T, F, Fut>(
    body: PartBody<'_>,
    len: Option<u64>,
    waiting: usize,
    send: F,
) -> Result<Result<T, ObjectStoreError>, Error>
where
    F: FnOnce(u64, BodyStream) -> Fut,
    Fut: Future<Output = Result<T, ObjectStoreError>>,
{
    let (len, body) = match len {
        Some(len) => (len, body),
        None => held(collect(body).await?),
    };
    flowing(body, waiting, |source| send(len, source)).await
}

/// Sends `body` to the store as the body of the request that `send` makes, which is given the
/// chunks as they arrive and never holds more than a few of them (the channel between them is
/// bounded, so a slow store slows `body`). Gives the error of `body`, if it ended with one, else
/// the answer of the store.
async fn flowing<T, F, Fut>(
    body: PartBody<'_>,
    waiting: usize,
    send: F,
) -> Result<Result<T, ObjectStoreError>, Error>
where
    F: FnOnce(BodyStream) -> Fut,
    Fut: Future<Output = Result<T, ObjectStoreError>>,
{
    let (sink, mut chunks) = mpsc::channel(waiting);
    let source: BodyStream = stream::poll_fn(move |cx| chunks.poll_recv(cx)).boxed();
    let (ended, sent) = futures::join!(feed(body, sink), send(source));
    match ended {
        Some(error) => Err(error),
        None => Ok(sent),
    }
}

#[async_trait]
impl Backend for S3Backend {
    /// S3 takes no request of unknown length: given none, the backend holds the body first.
    fn needs_length(&self) -> bool {
        true
    }

    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        let object = object_key(key)?;
        let put = self.store.put_new(&object, body, None);
        put.await.map_err(|e| map_error(e, key))
    }

    /// The tag is the `ETag` of the answer to this PUT.
    async fn put_tagged(&self, key: &str, body: Bytes) -> Result<String, Error> {
        let object = object_key(key)?;
        let put = self.store.put_new_tagged(&object, body);
        let tag = put.await.map_err(|e| map_error(e, key))?;
        Ok(unquoted(&tag))
    }

    async fn put_stream(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        let object = object_key(key)?;
        let store = &self.store;
        let sending = |len, source| store.put_stream(&object, len, source);
        let sent = streamed(body, len, self.chunks_waiting, sending).await?;
        sent.map_err(|e| map_error(e, key))
    }

    /// The tag is the `ETag` of the answer to the write that made the object (the PUT, or the
    /// completion of the multipart upload).
    async fn put_stream_tagged(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        let object = object_key(key)?;
        let store = &self.store;
        let sending = |len, source| store.put_stream_new_tagged(&object, len, source);
        let sent = streamed(body, len, self.chunks_waiting, sending).await?;
        let tag = sent.map_err(|e| map_error(e, key))?;
        Ok(unquoted(&tag))
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        let object = object_key(key)?;
        let etag = quoted(etag);
        let put = self.store.put_if_match(&object, body, &etag);
        let tag = put.await.map_err(|e| map_error(e, key))?;
        Ok(unquoted(&tag))
    }

    async fn put_stream_if_match(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        etag: &str,
    ) -> Result<String, Error> {
        let object = object_key(key)?;
        let etag = quoted(etag);
        let store = &self.store;
        let (object, etag) = (&object, &etag);
        let sending = |source| async move {
            match len {
                Some(len) => store.put_stream_if_match(object, len, source, etag).await,
                None => {
                    store
                        .put_stream_if_match_unsized(object, source, etag)
                        .await
                }
            }
        };
        let sent = flowing(body, self.chunks_waiting, sending).await?;
        let tag = sent.map_err(|e| map_error(e, key))?;
        Ok(unquoted(&tag))
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        let object = object_key(key)?;
        let Some(range) = range else {
            let all = self.store.get_all(&object);
            return all.await.map_err(|e| map_error(e, key));
        };
        self.read_range(&object, key, range).await
    }

    async fn get_tagged(
        &self,
        key: &str,
        range: Option<ByteRange>,
    ) -> Result<(Bytes, String), Error> {
        if range.is_some() {
            let (body, etag) = self.open(key, range).await?;
            return Ok((collect(body).await?, answered_tag(etag, key)?));
        }
        let object = object_key(key)?;
        let all = self.store.get_all_tagged(&object);
        let (bytes, etag) = all.await.map_err(|e| map_error(e, key))?;
        Ok((bytes, answered_tag(etag, key)?))
    }

    async fn get_stream(&self, key: &str, range: Option<ByteRange>) -> Result<ReadBody, Error> {
        Ok(self.open(key, range).await?.0)
    }

    /// One GET, whole or ranged, whose answer has the object's size (its `Content-Length`, or the
    /// total of its `Content-Range`), tag and time of last write, and whose body is given as
    /// it arrives. A span that is the last bytes of the object (`Suffix`) cannot be sent without
    /// the size, so it is a HEAD and a ranged GET (see [`open_by_head`]), as is a span the store
    /// answers 416 to, which the HEAD turns into the unsatisfiable window with the object's meta.
    /// An answer without a `Content-Length` or `Last-Modified` (a store that sends neither is not
    /// S3) or without an `ETag` is an [`Error::Other`]: the meta cannot be made up.
    async fn open_read(&self, key: &str, span: Option<Span>) -> Result<Opened, Error> {
        let asked = match span {
            None => None,
            Some(Span::Suffix(_)) => return open_by_head(self, key, span).await,
            Some(Span::From { first, last }) => {
                let len = last.map_or(u64::MAX, |last| {
                    last.saturating_sub(first).saturating_add(1)
                });
                match client_range(ByteRange::new(first, len)) {
                    Some(asked) => Some(asked),
                    None => return open_by_head(self, key, span).await,
                }
            }
        };
        let object = object_key(key)?;
        let read = match self.store.get_stream(&object, asked).await {
            Err(ObjectStoreError::InvalidRange { .. }) => {
                return open_by_head(self, key, span).await;
            }
            answer => answer.map_err(|e| map_error(e, key))?,
        };
        let lacks = |what: &str| Error::Other(format!("{key}: the read's answer has no {what}"));
        let size = read.size.ok_or_else(|| lacks("Content-Length"))?;
        let modified = read.modified_millis.ok_or_else(|| lacks("Last-Modified"))?;
        let meta = Meta {
            size,
            tag: answered_tag(read.etag, key)?,
            modified,
        };
        let name = key.to_owned();
        let body = read
            .body
            .map(move |piece| piece.map_err(|e| map_error(e, &name)));
        Ok(Opened {
            window: Window::of(span, size),
            meta,
            body: body.boxed(),
        })
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        let object = object_key(key)?;
        let head = self.store.head(&object);
        let head = head.await.map_err(|e| map_error(e, key))?;
        Ok(Meta {
            size: head.size,
            tag: unquoted(&head.etag),
            modified: head.modified_millis,
        })
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        let object = object_key(key)?;
        let delete = self.store.delete(&object);
        delete.await.map_err(|e| map_error(e, key))
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> Result<(), Error> {
        let object = object_key(key)?;
        if self.conditional_delete {
            let etag = quoted(etag);
            let delete = self.store.delete_if_match(&object, &etag);
            return delete.await.map_err(|e| map_error(e, key));
        }
        let head = self.store.head(&object).await;
        let head = head.map_err(|e| map_error(e, key))?;
        if unquoted(&head.etag) != unquoted(etag) {
            return Err(Error::PreconditionFailed(key.to_owned()));
        }
        let delete = self.store.delete(&object);
        delete.await.map_err(|e| map_error(e, key))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.listed(prefix, None, usize::MAX).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        self.listed(prefix, start_after, max_keys).await
    }

    async fn create_upload(&self, key: &str) -> Result<String, Error> {
        let object = object_key(key)?;
        let create = self.store.create_multipart(&object);
        create.await.map_err(|e| map_error(e, key))
    }

    async fn upload_part(
        &self,
        key: &str,
        id: &str,
        number: u32,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        let sized = self.upload_part_with_len(key, id, number, None, body);
        sized.await
    }

    async fn upload_part_with_len(
        &self,
        key: &str,
        id: &str,
        number: u32,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        let object = object_key(key)?;
        let store = &self.store;
        let sending = |len, source| store.upload_part_stream(&object, id, number, len, source);
        let sent = streamed(body, len, self.chunks_waiting, sending).await?;
        let etag = sent.map_err(|e| upload_error(e, key, id))?;
        Ok(unquoted(&etag))
    }

    async fn complete_upload(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        let object = object_key(key)?;
        let named: Vec<(u32, String)> = parts
            .iter()
            .map(|(number, tag)| (*number, quoted(tag)))
            .collect();
        let complete = self.store.complete_multipart(&object, id, &named);
        let etag = complete.await.map_err(|e| upload_error(e, key, id))?;
        Ok(unquoted(&etag))
    }

    async fn abort_upload(&self, key: &str, id: &str) -> Result<(), Error> {
        let object = object_key(key)?;
        let abort = self.store.abort_multipart(&object, id);
        abort.await.map_err(|e| upload_error(e, key, id))
    }

    async fn list_uploads(
        &self,
        prefix: &str,
        after: Option<UploadsAfter<'_>>,
        max_uploads: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        let under = KeyPrefix::new(prefix).map_err(|_| Error::InvalidKey(prefix.to_owned()))?;
        let mut marker = match after {
            Some((key, id)) => Some((object_key(key)?, id.map(str::to_owned))),
            None => None,
        };
        let mut open = Vec::new();
        while open.len() < max_uploads {
            let room = max_uploads - open.len();
            let size = u16::try_from(room).ok().and_then(PageSize::new);
            let size = size.unwrap_or(PageSize::MAX);
            let from = marker.as_ref().map(|(key, id)| (key, id.as_deref()));
            let page = self.store.list_multipart_uploads(&under, from, size);
            let page = page.await.map_err(|e| map_error(e, prefix))?;
            open.extend(page.uploads.into_iter().map(open_of));
            let Some((key, id)) = page.next else { break };
            // A store that has no id to give for the end of a page gives the empty one.
            let next = (key, Some(id).filter(|id| !id.is_empty()));
            if marker.as_ref() == Some(&next) {
                let why =
                    format!("{prefix:?} listing of uploads stalled: the marker did not change");
                return Err(Error::Other(why));
            }
            marker = Some(next);
        }
        open.truncate(max_uploads);
        Ok(open)
    }

    async fn list_parts(
        &self,
        key: &str,
        id: &str,
        after: Option<u32>,
        max_parts: usize,
    ) -> Result<Vec<UploadedPart>, Error> {
        let object = object_key(key)?;
        let mut marker = after;
        let mut stored: Vec<UploadedPart> = Vec::new();
        while stored.len() < max_parts {
            let room = max_parts - stored.len();
            let size = u16::try_from(room).ok().and_then(PageSize::new);
            let size = size.unwrap_or(PageSize::MAX);
            let page = self.store.list_parts(&object, id, marker, size);
            let page = page.await.map_err(|e| upload_error(e, key, id))?;
            stored.extend(page.parts.into_iter().map(part_of));
            let Some(next) = page.next else { break };
            if marker.is_some_and(|before| next <= before) {
                let why = format!("{key:?} listing of parts stalled: the marker did not advance");
                return Err(Error::Other(why));
            }
            marker = Some(next);
        }
        stored.truncate(max_parts);
        Ok(stored)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn service(status: u16) -> ObjectStoreError {
        ObjectStoreError::Service {
            status,
            code: "Code".to_owned(),
            message: "message".to_owned(),
        }
    }

    /// Asserts that the number of chunks given is how many wait for the request that sends them:
    /// the feeder takes that many chunks of the body and one more (in hand, blocked) while the
    /// request has taken none, for one, four and nine.
    #[tokio::test]
    async fn the_chunks_waiting_given_are_how_many_chunks_wait_for_the_request() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        for (waiting, held) in [(1_usize, 1_usize), (4, 4), (9, 9)] {
            let pulled = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&pulled);
            let chunks = stream::iter((0..30).map(|_| Ok(Bytes::from_static(b"x"))));
            let body: PartBody<'_> = chunks
                .inspect(move |_| {
                    counted.fetch_add(1, Ordering::SeqCst);
                })
                .boxed();
            let request = |source| async move {
                for _ in 0..50 {
                    tokio::task::yield_now().await;
                }
                drop(source);
                Ok::<usize, ObjectStoreError>(pulled.load(Ordering::SeqCst))
            };
            let seen = flowing(body, waiting, request).await;
            assert_eq!(seen.unwrap().unwrap(), held + 1, "{waiting}");
        }
    }

    /// Asserts that a backend made with `with_chunks_waiting(0)` holds one chunk, not none: the
    /// channel of the bodies is never made with a depth of zero, which it refuses.
    #[test]
    fn a_depth_of_zero_chunks_waiting_is_one() {
        let config = S3Config {
            endpoint: "http://127.0.0.1:9".to_owned(),
            region: "us-east-1".to_owned(),
            bucket: "tierstore-test".to_owned(),
            credentials: Credentials::new("k", "s"),
            capabilities: Capabilities::default(),
            connect_timeout: std::time::Duration::from_secs(1),
            request_timeout: std::time::Duration::from_secs(1),
            min_body_rate: None,
        };
        let backend = S3Backend::new(config).unwrap();
        assert_eq!(backend.chunks_waiting, CHUNKS_WAITING);
        assert_eq!(backend.with_chunks_waiting(0).chunks_waiting, 1);
    }

    /// Asserts that each answer of the client is the error of this crate the trait documents
    /// for it: not found, already exists and a failed precondition name the key, a request
    /// that did not complete and the statuses 408, 429 and every 5xx but 501 and 507 are
    /// unavailable, 507 is full, and any other answer (501 among them) is an other error that
    /// says what it was.
    #[test]
    fn answers_of_the_client_are_the_errors_the_trait_documents() {
        let key = ObjectKey::new("k").unwrap();
        let name = "k".to_owned();
        let found = map_error(ObjectStoreError::NotFound(key.clone()), "k");
        assert_eq!(found, Error::NotFound(name.clone()));
        let exists = map_error(ObjectStoreError::AlreadyExists(key), "k");
        assert_eq!(exists, Error::AlreadyExists(name.clone()));
        let failed = map_error(service(412), "k");
        assert_eq!(failed, Error::PreconditionFailed(name));

        let reset = Arc::new(std::io::Error::other("reset"));
        let down = map_error(ObjectStoreError::Transport(reset), "k");
        assert!(down.is_unavailable(), "{down}");
        for status in [408, 429, 500, 502, 503, 504, 505, 506, 508, 599] {
            let down = map_error(service(status), "k");
            assert!(down.is_unavailable(), "{status}");
        }
        let full = map_error(service(507), "k");
        assert!(matches!(full, Error::Full(_)), "{full}");
        for status in [400, 403, 409, 501] {
            let other = map_error(service(status), "k");
            assert!(matches!(other, Error::Other(_)), "{status}: {other}");
        }
        // Two writers racing a create-only put: the store does not say who won, so the answer
        // is retryable; a 409 of any other code is still an other error.
        let racing = ObjectStoreError::Service {
            status: 409,
            code: "ConditionalRequestConflict".to_owned(),
            message: "conflict".to_owned(),
        };
        let conflict = map_error(racing, "k");
        assert!(conflict.is_unavailable(), "{conflict}");
        let odd = map_error(ObjectStoreError::Protocol("odd".to_owned()), "k");
        assert!(odd.to_string().contains("odd"), "{odd}");
    }
}
