//! The `ObjectStore` trait, the keys, ranges and pages it speaks in, and its S3 backend
//! ([`s3`]: SigV4 over plain HTTP or TLS, path-style or virtual-hosted-style, multipart,
//! conditional writes), with a loopback S3 service for tests ([`s3::fake`], feature `testkit`).
//!
//! The crate depends on no other crate of its repository; `tierstore` adapts its S3 client. What a caller needs of a store is in
//! [`ObjectStore`]; what a store may additionally claim (conditional writes, Object Lock) is in
//! [`Capabilities`].
//!
//! One store value addresses one bucket.

mod body;
pub mod key_rules;
pub mod listing;
pub mod s3;
mod types;

use std::future::Future;
use std::time::SystemTime;

use bytes::Bytes;

pub use body::{BodyStream, gather, too_long, too_short};
pub use types::{
    ByteRange, Capabilities, ContentBoundPut, KeyError, KeyPrefix, ListPage, ListToken, ObjectInfo,
    ObjectKey, ObjectStoreError, PageSize,
};

/// An object store bucket, as the store's callers use it.
///
/// The futures are `Send` so a store can be driven from any runtime thread, and a store
/// is `'static` so a task spawned to write a segment can own a handle on it.
pub trait ObjectStore: Send + Sync + 'static {
    /// What this store claims beyond the base contract.
    fn capabilities(&self) -> Capabilities;

    /// Writes an immutable object.
    ///
    /// With [`Capabilities::conditional_put`], an existing key is refused with
    /// [`ObjectStoreError::AlreadyExists`] and left unchanged. With `retain_until`, the
    /// object may not be deleted before that time; a store without
    /// [`Capabilities::object_lock`] refuses that with [`ObjectStoreError::Unsupported`].
    fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> impl Future<Output = Result<(), ObjectStoreError>> + Send;

    /// Writes an immutable object of exactly `len` bytes, taken from `body` as it is
    /// sent, so the writer never holds the object whole.
    ///
    /// The object is written only if `body` gives `len` bytes and then ends. A body that
    /// yields an error, or more or fewer bytes, leaves nothing under `key` and fails the
    /// write (with the body's error, or [`ObjectStoreError::Body`]). With
    /// [`Capabilities::conditional_put`], an existing key is refused with
    /// [`ObjectStoreError::AlreadyExists`] and left unchanged, as by
    /// [`ObjectStore::put_new`]. No retention.
    ///
    /// The default gathers the body whole and calls [`ObjectStore::put_new`]: for the
    /// in-memory fake and the test stores that wrap another. The S3 store sends a large
    /// object as one streamed PUT up to [`s3::S3Tuning::single_put_max`] (5 GiB by default) and
    /// as a multipart upload, a part at a time, above it, aborting it on any failure; the local-directory store writes the pieces to its
    /// file as they come.
    ///
    /// A store that keeps the bytes of a PUT whose body was cut short breaks this contract:
    /// SeaweedFS 4.54 can keep a PUT without `Content-Type` whose body carries no checksum as an
    /// object of the bytes that came, 0 of them when the body failed before its first byte (see
    /// `s3/bound.rs`). The S3 store mends it there after the fact: its PUT carries a token of its
    /// own, and one that got no answer is followed by one look at the key, which deletes an object
    /// of another length than `len` that carries the token, and no other object (see
    /// `s3/cut_put.rs`). A write whose key names the SHA-256 of the bytes is
    /// [`ObjectStore::put_stream_content_bound`], which sends the digest for the store to check,
    /// so that such a store refuses the body cut short and keeps nothing.
    fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> impl Future<Output = Result<(), ObjectStoreError>> + Send {
        async move {
            let bytes = gather(body, len).await?;
            self.put_new(key, bytes, None).await
        }
    }

    /// Writes the object of exactly `len` bytes whose SHA-256 is `sha256` under a key bound to
    /// that content (the key names the digest, so whoever writes it means these bytes), from
    /// `body` as it is sent, as [`ObjectStore::put_stream`] does, and repairs a damaged object
    /// under the key.
    ///
    /// An object already under the key that is `len` bytes and names no other digest (or none:
    /// one written without it) is kept and `body` is not sent ([`ContentBoundPut::Existed`]).
    /// One of another length, or one whose stored checksum (or, where the store keeps none for
    /// it, whose recorded digest) is another, is damaged: it is replaced by the object written, on the condition that it is still the
    /// object that was judged ([`ContentBoundPut::Replaced`]). The key is not checked against
    /// `sha256`: binding the two is the caller's.
    ///
    /// The default is [`ObjectStore::put_stream`], whose create-only refusal is
    /// [`ContentBoundPut::Existed`]: it sends no digest and replaces nothing, for a store that
    /// never keeps a write cut short (the in-memory fake, the local-directory store). The S3
    /// store sends the digest as `x-amz-checksum-sha256`, which a store that does flexible
    /// checksums checks against the bytes it took before it keeps the object, and records it as
    /// the metadata `x-amz-meta-sha256`.
    ///
    /// # Errors
    /// As [`ObjectStore::put_stream`]. The S3 store: [`ObjectStoreError::Unsupported`] unless
    /// it claims [`Capabilities::conditional_put`]; a [`ObjectStoreError::Service`] error 412
    /// if the damaged object changed between the HEAD that judged it and the write that
    /// replaces it, and [`ObjectStoreError::NotFound`] if it was deleted in between (nothing is
    /// written; the next write judges again); [`ObjectStoreError::Protocol`] if the HEAD of a
    /// damaged object gives no entity tag to make the replacement conditional on (nothing is
    /// written); a [`ObjectStoreError::Service`] error 400 (code `BadDigest` among others) for a
    /// body the store found not to be `sha256`.
    fn put_stream_content_bound(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
        sha256: [u8; 32],
    ) -> impl Future<Output = Result<ContentBoundPut, ObjectStoreError>> + Send {
        let _ = sha256;
        async move {
            match self.put_stream(key, len, body).await {
                Ok(()) => Ok(ContentBoundPut::Created),
                Err(ObjectStoreError::AlreadyExists(_)) => Ok(ContentBoundPut::Existed),
                Err(error) => Err(error),
            }
        }
    }

    /// Reads the bytes of `range` that exist (see [`ByteRange`]).
    fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> impl Future<Output = Result<Bytes, ObjectStoreError>> + Send;

    /// Whether the store may hold `key`: `false` only when the store knows, without a
    /// request over the network, that it does not (a local store's file is gone, or is
    /// not a whole object). A store that would need a request answers `true`, the
    /// default, and a read finds out. A caller asks before it serves a cached result,
    /// once per object the result needs.
    fn may_hold(
        &self,
        key: &ObjectKey,
    ) -> impl Future<Output = Result<bool, ObjectStoreError>> + Send {
        let _ = key;
        std::future::ready(Ok(true))
    }

    /// The length in bytes of the object under `key`, or `None` if there is no object
    /// (a missing key is an answer, not an error, so a caller tells it from a store
    /// that failed). Without reading the object: a HEAD for a store over the network.
    ///
    /// An object still being written is not there: a store that stages an upload
    /// answers `None` until the write is whole. The default refuses with
    /// [`ObjectStoreError::Unsupported`], so a store that does not implement it fails
    /// loudly instead of answering a length it did not look up.
    fn len(
        &self,
        key: &ObjectKey,
    ) -> impl Future<Output = Result<Option<u64>, ObjectStoreError>> + Send {
        let _ = key;
        std::future::ready(Err(ObjectStoreError::Unsupported("object length")))
    }

    /// Deletes an object. Deleting an absent key succeeds, so a retried delete is safe.
    /// An object under retention is refused and stays readable.
    fn delete(&self, key: &ObjectKey) -> impl Future<Output = Result<(), ObjectStoreError>> + Send;

    /// One page of the keys under `prefix`, in ascending order, starting where `after`
    /// says (or at the beginning). For rebuild and the weekly sweep only: the listing
    /// need not be consistent with recent writes.
    fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> impl Future<Output = Result<ListPage, ObjectStoreError>> + Send;
}
