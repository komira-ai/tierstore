//! The writes of a cache with `write_through` on: each goes to the upstream before it is
//! acknowledged, so nothing is only local and there is nothing to recover.
//!
//! - **Plain put** (`put_stream_replacing`, a `PUT` with no `If-None-Match`): sent to the
//!   upstream as its plain put (see [`put_stream_replacing`](crate::Backend::put_stream_replacing)),
//!   with no HEAD before it, and the key is replaced where the upstream replaces. The cache
//!   [`forget`](Inner::forget)s the key before the put and again when the upstream has answered
//!   (also with an error, which may have followed a commit), so that neither a read that fills
//!   between the two nor an older copy is served after the overwrite, and then keeps the new
//!   copy under the tag the upstream's answer gives (see `keep`). Two puts of one key that run at
//!   once, one of them a plain put, keep no copy: the order in which the upstream took them is
//!   not known here, and the next read fills the object (see [`Writing`]). Where the upstream
//!   creates only (a proxy whose format is not `active-marks`) the plain put is the upstream's
//!   create-only one and its `AlreadyExists` is passed on.
//! - **Create-only put** ([`put_through`](Inner::put_through), in `keep`): one HEAD of the key
//!   goes to the upstream before the body is read (also when the upstream is a proxy of two
//!   tiers, whose own put refuses a key that only its first tier holds only after the body has
//!   been written there). An object that is there is
//!   [`AlreadyExists`](Error::AlreadyExists) at once, with no byte of the body read and no put
//!   sent (the S3 backend would otherwise upload all the parts of a large body and learn it at the
//!   completion). The HEAD only saves work: the upstream's create-only put is the one arbiter of
//!   two writers of one key, and its `AlreadyExists` is passed on as it is. An upstream that
//!   cannot answer the HEAD is the error of the put. With `head_before_put` off there is no HEAD:
//!   the put goes to the upstream as it is, for an upstream (a proxy whose format is
//!   `active-marks`) that refuses a key it holds before it reads the body, and the tag its answer
//!   gives (see `put_stream_answered`) is the cache's version of the copy, so the put costs the
//!   upstream its own requests and no more. Under a `cache = true` policy the object is
//!   also kept on the node (see `keep`).
//! - **Hot-only put** ([`put_stream_hot_only`](crate::Backend::put_stream_hot_only)): the same
//!   put, sent to the upstream as its hot-only put, which a proxy answers
//!   [`Full`](Error::Full) and stores nothing in when its first tier has no room; a write that
//!   is not to race the other tier's puts (a presigned put) passes through the cache as it is.
//! - **Multipart** (create, part, complete, abort): passed to the upstream as they are and not
//!   kept (the first read fills the object). A completion that succeeds drops the cache's entry
//!   for the key, if it has one.
//! - **Delete** ([`delete_through`](Inner::delete_through)): the entry is dropped, then the
//!   upstream deletes. A fill that is running for the key does not keep the object it read (see
//!   [`forget`](Inner::forget)).
//! - **A copy the upstream makes** ([`copy_through`](Inner::copy_through)): passed to the
//!   upstream's own copy ([`copy_in_store`](crate::Backend::copy_in_store)), so no byte of it
//!   passes through the node. A copy of an object forgets the key it writes before the copy and
//!   again when the upstream has answered, as a delete does, and keeps no copy of it: the node
//!   never held the bytes, and the first read fills the object as any miss (under a
//!   `cache = true` policy). A copy into a part is the upstream's alone, as a part is. A copy the
//!   upstream does not make comes back to the front, which reads the source and puts the bytes
//!   (a plain or a create-only put, above).
//! - **A put or a completion that replaces on a tag** (`If-Match`,
//!   [`swap_through`](Inner::swap_through) and [`complete_swap_through`](Inner::complete_swap_through)):
//!   sent to the upstream's own call ([`put_stream_if_match`](crate::Backend::put_stream_if_match),
//!   [`complete_upload_if_match`](crate::Backend::complete_upload_if_match)), with no HEAD before
//!   it: the upstream compares the tag and replaces in one step (an S3 bucket does), and its 412
//!   and 404 are passed on. The cache forgets the key before the call and when the upstream has
//!   answered, also with an error, as for a plain put, and keeps no copy: the next read fills
//!   the object. The call counts as a plain put among the puts of the object that run at once
//!   (see [`Writing`]), so that a put of the object beside it keeps no copy either.
//!
//! The keys under `_journal/` and `_tiering/` belong to a storage node and are refused with
//! [`Error::Reserved`], as a storage node does.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;

use crate::journal::is_reserved;
use crate::{CopyTarget, Error, PartBody, PutAnswer, StoreCopy};

use super::calls::counting;
use super::keep::Sent;
use super::layout::Origin;
use super::metrics::{add, bump};
use super::{Inner, READ_ONLY, locked};

impl Inner {
    /// Whether the cache takes writes of `key`: it was told to, and the key is not one a storage
    /// node keeps for itself.
    pub(super) fn writable(&self, key: &str) -> Result<(), Error> {
        if !self.config.write_through {
            return Err(Error::Unsupported(READ_ONLY.to_owned()));
        }
        if is_reserved(key) {
            return Err(Error::Reserved(key.to_owned()));
        }
        Ok(())
    }

    /// A create-only put of a body held whole: the streamed put of a body of one chunk (see
    /// [`put_through`](Inner::put_through), in `keep`).
    pub(super) async fn put_whole(self: &Arc<Self>, key: &str, body: Bytes) -> Result<(), Error> {
        let len = body.len() as u64;
        let once: PartBody<'_> = stream::once(async move { Ok(body) }).boxed();
        self.put_through(key, Some(len), once, Sent::Creates)
            .await
            .map(drop)
    }

    /// Deletes the object: the entry first, then the upstream's. A read that fills the key
    /// between the two reads the object before the upstream deletes it, so the entry is dropped
    /// once more when the upstream has deleted it, and any fill still running is told not to
    /// keep what it read.
    pub(super) async fn delete_through(&self, key: &str) -> Result<(), Error> {
        self.writable(key)?;
        self.forget(key);
        let deleted = self.upstream.delete(key).await;
        // Also when the upstream answers with an error: a timeout, or a 5xx after the commit,
        // may have deleted the object, and a fill that began in between must not keep it. An
        // object that is still there is read in again by the next miss.
        self.forget(key);
        deleted
    }

    /// A copy the upstream makes of its own object (module docs). The source is refused as the
    /// key is, when it is one a storage node keeps for itself.
    pub(super) async fn copy_through(
        &self,
        copy: &StoreCopy<'_>,
    ) -> Result<Option<PutAnswer>, Error> {
        self.writable(copy.key)?;
        if is_reserved(copy.source) {
            return Err(Error::Reserved(copy.source.to_owned()));
        }
        if let CopyTarget::Part { .. } = copy.target {
            return self.upstream.copy_in_store(copy).await;
        }
        self.forget(copy.key);
        let copied = self.upstream.copy_in_store(copy).await;
        // Also on an error: a timeout, or a 5xx after the commit, may have replaced the object.
        self.forget(copy.key);
        copied
    }

    /// Starts a multipart upload of `key` at the upstream.
    pub(super) async fn create_through(&self, key: &str) -> Result<String, Error> {
        self.writable(key)?;
        self.upstream.create_upload(key).await
    }

    /// Sends a part to the upstream.
    pub(super) async fn part_through(
        &self,
        key: &str,
        id: &str,
        number: u32,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        self.writable(key)?;
        let passed = AtomicU64::new(0);
        let counted = counting(body, &passed);
        let sent = self
            .upstream
            .upload_part_with_len(key, id, number, len, counted);
        let tag = sent.await?;
        // The bytes of a part the upstream took count toward the put its completion makes.
        add(&self.counters.upstream_put_bytes, passed.into_inner());
        Ok(tag)
    }

    /// Completes the upload at the upstream, create-only; when the object is made, the entry for
    /// the key (of an object that was deleted behind the cache's back) is dropped.
    pub(super) async fn complete_through(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        self.writable(key)?;
        let completed = self.upstream.complete_upload(key, id, parts).await;
        let tag = completed.inspect_err(|error| self.counters.put_failed(error))?;
        bump(&self.counters.upstream_puts);
        self.forget(key);
        Ok(tag)
    }

    /// A put that replaces the object only if it has the tag `etag`, sent to the upstream as it
    /// is; gives the tag of the new object. See the module docs.
    pub(super) async fn swap_through(
        self: &Arc<Self>,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        etag: &str,
    ) -> Result<String, Error> {
        self.writable(key)?;
        let passed = AtomicU64::new(0);
        let counted = counting(body, &passed);
        let sent = self.swapping(
            key,
            self.upstream.put_stream_if_match(key, len, counted, etag),
        );
        let tag = sent.await?;
        add(&self.counters.upstream_put_bytes, passed.into_inner());
        bump(&self.counters.writes_through);
        Ok(tag)
    }

    /// The completion of an upload that replaces the object only if it has the tag `etag`; as
    /// [`swap_through`](Self::swap_through).
    pub(super) async fn complete_swap_through(
        self: &Arc<Self>,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
        etag: &str,
    ) -> Result<String, Error> {
        self.writable(key)?;
        let sent = self.upstream.complete_upload_if_match(key, id, parts, etag);
        self.swapping(key, sent).await
    }

    /// Runs `call`, a write of the upstream that replaces `key` on a tag: registered as a plain
    /// put of the object for its length, with the key forgotten before and after it. Counts the
    /// write, or its failure.
    async fn swapping(
        self: &Arc<Self>,
        key: &str,
        call: impl Future<Output = Result<String, Error>>,
    ) -> Result<String, Error> {
        let writing = self
            .cacheable(key)
            .map(|name| self.begin_write(&name.hex, true));
        self.forget(key);
        let sent = call.await;
        self.forget(key);
        drop(writing);
        let tag = sent.inspect_err(|error| self.counters.put_failed(error))?;
        bump(&self.counters.upstream_puts);
        Ok(tag)
    }

    /// Aborts the upload at the upstream.
    pub(super) async fn abort_through(&self, key: &str, id: &str) -> Result<(), Error> {
        self.writable(key)?;
        self.upstream.abort_upload(key, id).await
    }

    /// Drops the entry for `key`, and marks the fill that is running for it, if there is one, so
    /// that it does not put what it read in the cache.
    ///
    /// The mark is made before the entry is dropped. A fill puts its object in the index and then
    /// looks for the mark (see `fetch`), so whichever comes first, either the fill sees the mark
    /// and removes its own entry, or the entry is in the index when it is dropped here.
    pub(super) fn forget(&self, key: &str) {
        self.forget_large(key);
        let Some(name) = self.cacheable(key) else {
            return;
        };
        // What was kept by extents of the object is of a version that is gone.
        self.drop_partial(&name.hex);
        // An entry of origin `span` is no object of the upstream: nothing was deleted there,
        // and the blob a mount pinned stays.
        if locked(&self.index)
            .get(&name.hex)
            .is_some_and(|entry| entry.origin == Origin::Span)
        {
            return;
        }
        {
            let flights = locked(&self.flights);
            if flights.contains_key(&name.hex) {
                locked(&self.invalidated).insert(name.hex.clone());
            }
        }
        self.drop_entry(&name.hex);
    }

    /// Whether the fill of `hex` was marked by [`forget`](Self::forget).
    pub(super) fn is_invalidated(&self, hex: &str) -> bool {
        locked(&self.invalidated).contains(hex)
    }
}

/// The puts of one object that are running on this node.
#[derive(Default)]
pub(super) struct Writers {
    /// How many.
    running: usize,
    /// Whether one of them, now or before, was a plain put.
    replaces: bool,
    /// Whether two of them ran at once with a plain put among them: the order in which the
    /// upstream took them is not known, so none of them keeps a copy.
    overlapped: bool,
}

/// A put of an object that is running, from before its body is sent until its keep has ended.
/// It tells the keep whether another put of the object ran at the same time as a plain put
/// (see [`Writers`]), so that a keep never puts in the cache the copy of a write that the
/// upstream took before another one.
pub(super) struct Writing {
    inner: Arc<Inner>,
    hex: String,
}

impl Writing {
    /// Whether another put of the object ran while this one did, with a plain put among them.
    /// Looked for after the entry is in the index (see `is_invalidated`): a put that begins
    /// after the look forgets the entry, and one that began before it is seen here.
    pub(super) fn overlapped(&self) -> bool {
        locked(&self.inner.writers)
            .get(&self.hex)
            .is_some_and(|group| group.overlapped)
    }
}

impl Drop for Writing {
    fn drop(&mut self) {
        let mut writers = locked(&self.inner.writers);
        let group = writers.entry(self.hex.clone()).or_default();
        group.running = group.running.saturating_sub(1);
        if group.running == 0 {
            let _ = writers.remove(&self.hex);
        }
    }
}

impl Inner {
    /// Registers a put of the object `hex`; `replaces` is true for a plain put.
    pub(super) fn begin_write(self: &Arc<Self>, hex: &str, replaces: bool) -> Writing {
        {
            let mut writers = locked(&self.writers);
            let group = writers.entry(hex.to_owned()).or_default();
            // Not short-circuited: no branch for a look at a group that is alone.
            group.overlapped |= (group.running > 0) & (replaces | group.replaces);
            group.replaces |= replaces;
            group.running += 1;
        }
        Writing {
            inner: Arc::clone(self),
            hex: hex.to_owned(),
        }
    }
}
