//! [`CheckUpstream`]: the upstream of a build node with a mount provider, which answers the reads of
//! the update check's objects itself.
//!
//! A read (`get`, `get_tagged`, `get_stream`, `open_read`, `head`) of the key of the check's pack,
//! of the list of one of its trees, or of the sidecar of [`WITH_SIDECAR`] is answered from memory,
//! as the upstream would answer it had a build service stored the objects: the object, ranged as
//! asked, tagged with the hex of its SHA-256. A read of the sidecar key of [`WITHOUT_SIDECAR`] is
//! `NotFound`, as the upstream answers for a tree with no sidecar, without asking it. Every other
//! call, and every call that is not a read, goes to the upstream unchanged.
//!
//! The keys are exact: nothing else under `cas/check/`, `cas/l/` or `cas/l2/` is answered here.
//! The answers are the objects' only bytes, which every reader checks as it checks any (a list
//! against its id, a sidecar against the list, a file against its SHA-256), so a job that names
//! the same key gets the same bytes the store would give it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use sha2::{Digest, Sha256};

use super::{TREES, WITH_SIDECAR, WITHOUT_SIDECAR, pack, pack_key};
use crate::{
    Backend, BodyDigests, ByteRange, Error, Listed, Meta, OpenUpload, Opened, PartBody, PutAnswer,
    PutClaim, PutKind, PutOutcome, ReadBody, ShortObjects, Span, StoreCopy, TreeDigest,
    UploadedPart, UploadsAfter, Window,
};

/// An upstream with the update check's objects in front of it.
pub(in crate::server) struct CheckUpstream {
    /// The check's objects, by key.
    objects: BTreeMap<String, Bytes>,
    /// The keys the check's objects say are not there: the sidecar of the tree without one.
    absent: BTreeSet<String>,
    /// The upstream.
    inner: Arc<dyn Backend>,
}

impl CheckUpstream {
    /// `inner`, with the check's objects in front of it.
    pub(in crate::server) fn new(inner: Arc<dyn Backend>) -> Self {
        let mut objects = BTreeMap::new();
        objects.insert(pack_key(), Bytes::from(pack()));
        for tree in TREES {
            objects.insert(tree.key(), Bytes::from_static(tree.list));
        }
        if let Some(sidecar) = WITH_SIDECAR.sidecar {
            objects.insert(WITH_SIDECAR.sidecar_key(), Bytes::from_static(sidecar));
        }
        let absent = BTreeSet::from([WITHOUT_SIDECAR.sidecar_key()]);
        Self {
            objects,
            absent,
            inner,
        }
    }

    /// The check's object at `key`: none when the key is not one of the check's, and `NotFound`
    /// for a key the check says is not there.
    fn object(&self, key: &str) -> Option<Result<&Bytes, Error>> {
        if self.absent.contains(key) {
            return Some(Err(Error::NotFound(key.to_owned())));
        }
        self.objects.get(key).map(Ok)
    }
}

/// What the upstream would say of `body`.
fn meta_of(body: &Bytes) -> Meta {
    Meta {
        size: body.len() as u64,
        tag: hex::encode(Sha256::digest(body)),
        modified: 0,
    }
}

/// `body`, cut to `range` when there is one.
fn ranged(body: &Bytes, range: Option<ByteRange>) -> Result<Bytes, Error> {
    match range {
        Some(range) => range.slice(body),
        None => Ok(body.clone()),
    }
}

#[async_trait]
impl Backend for CheckUpstream {
    fn needs_length(&self) -> bool {
        self.inner.needs_length()
    }

    fn single_put_max(&self) -> Option<u64> {
        self.inner.single_put_max()
    }

    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put(key, body).await
    }

    async fn put_tagged(&self, key: &str, body: Bytes) -> Result<String, Error> {
        self.inner.put_tagged(key, body).await
    }

    async fn put_stream(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.inner.put_stream(key, len, body).await
    }

    async fn put_stream_answered(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        hot_only: bool,
    ) -> Result<Option<PutAnswer>, Error> {
        self.inner
            .put_stream_answered(key, len, body, hot_only)
            .await
    }

    async fn put_stream_hot_only(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.inner.put_stream_hot_only(key, len, body).await
    }

    async fn put_stream_replacing(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        claim: &PutClaim,
    ) -> Result<Option<PutAnswer>, Error> {
        self.inner.put_stream_replacing(key, len, body, claim).await
    }

    async fn put_stream_digested(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        kind: PutKind<'_>,
        digests: BodyDigests,
    ) -> Result<Option<PutAnswer>, Error> {
        self.inner
            .put_stream_digested(key, len, body, kind, digests)
            .await
    }

    fn short_objects(&self) -> ShortObjects {
        self.inner.short_objects()
    }

    async fn put_stream_tagged(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        self.inner.put_stream_tagged(key, len, body).await
    }

    async fn put_stream_sha256(
        &self,
        key: &str,
        len: u64,
        body: PartBody<'_>,
        sha256: TreeDigest,
    ) -> Result<PutOutcome, Error> {
        self.inner.put_stream_sha256(key, len, body, sha256).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.inner.put_if_match(key, body, etag).await
    }

    async fn put_stream_if_match(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        etag: &str,
    ) -> Result<String, Error> {
        self.inner.put_stream_if_match(key, len, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        match self.object(key) {
            Some(found) => ranged(found?, range),
            None => self.inner.get(key, range).await,
        }
    }

    async fn get_tagged(
        &self,
        key: &str,
        range: Option<ByteRange>,
    ) -> Result<(Bytes, String), Error> {
        match self.object(key) {
            Some(found) => {
                let body = found?;
                Ok((ranged(body, range)?, meta_of(body).tag))
            }
            None => self.inner.get_tagged(key, range).await,
        }
    }

    async fn get_stream(&self, key: &str, range: Option<ByteRange>) -> Result<ReadBody, Error> {
        match self.object(key) {
            Some(found) => {
                let read = ranged(found?, range)?;
                Ok(futures::stream::once(std::future::ready(Ok(read))).boxed())
            }
            None => self.inner.get_stream(key, range).await,
        }
    }

    fn same_version(&self, a: &Meta, b: &Meta) -> bool {
        self.inner.same_version(a, b)
    }

    async fn open_read(&self, key: &str, span: Option<Span>) -> Result<Opened, Error> {
        match self.object(key) {
            Some(found) => {
                let body = found?;
                let meta = meta_of(body);
                let window = Window::of(span, meta.size);
                let read = window.slice(body);
                let body = futures::stream::once(std::future::ready(Ok(read))).boxed();
                Ok(Opened { meta, window, body })
            }
            None => self.inner.open_read(key, span).await,
        }
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        match self.object(key) {
            Some(found) => Ok(meta_of(found?)),
            None => self.inner.head(key).await,
        }
    }

    async fn copy_in_store(&self, copy: &StoreCopy<'_>) -> Result<Option<PutAnswer>, Error> {
        self.inner.copy_in_store(copy).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> Result<(), Error> {
        self.inner.delete_if_match(key, etag).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        self.inner.list_page(prefix, start_after, max_keys).await
    }

    async fn create_upload(&self, key: &str) -> Result<String, Error> {
        self.inner.create_upload(key).await
    }

    async fn upload_part(
        &self,
        key: &str,
        id: &str,
        number: u32,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        self.inner.upload_part(key, id, number, body).await
    }

    async fn upload_part_with_len(
        &self,
        key: &str,
        id: &str,
        number: u32,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        self.inner
            .upload_part_with_len(key, id, number, len, body)
            .await
    }

    async fn complete_upload(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        self.inner.complete_upload(key, id, parts).await
    }

    async fn complete_upload_if_match(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
        etag: &str,
    ) -> Result<String, Error> {
        self.inner
            .complete_upload_if_match(key, id, parts, etag)
            .await
    }

    async fn complete_upload_replacing(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        self.inner.complete_upload_replacing(key, id, parts).await
    }

    async fn abort_upload(&self, key: &str, id: &str) -> Result<(), Error> {
        self.inner.abort_upload(key, id).await
    }

    async fn list_uploads(
        &self,
        prefix: &str,
        after: Option<UploadsAfter<'_>>,
        max_uploads: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        self.inner.list_uploads(prefix, after, max_uploads).await
    }

    async fn list_parts(
        &self,
        key: &str,
        id: &str,
        after: Option<u32>,
        max_parts: usize,
    ) -> Result<Vec<UploadedPart>, Error> {
        self.inner.list_parts(key, id, after, max_parts).await
    }
}
