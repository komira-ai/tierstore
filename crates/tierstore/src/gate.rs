//! A wrapper that parks a chosen call of another [`Backend`] until a test releases it, for tests
//! that need two calls to meet in one exact order.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::{oneshot, watch};

use crate::{
    Backend, ByteRange, Error, Listed, Meta, Op, OpenUpload, PartBody, ReadBody, UploadsAfter,
};

/// Where in a call a [`Gate`] parks it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Before the call reaches the wrapped backend: the store has not seen it.
    Pre,
    /// After the wrapped backend has done it: the store has seen it, and the caller has not yet
    /// been told how it went.
    Post,
}

/// What a released call does next.
enum Release {
    Pass,
    Fail(Error),
}

struct Slot {
    /// Whether a call has been taken by the gate; the gate takes one call only.
    taken: bool,
    /// The way to release the call that is parked.
    parked: Option<oneshot::Sender<Release>>,
}

struct Shared {
    op: Op,
    prefix: String,
    phase: Phase,
    slot: Mutex<Slot>,
    /// Whether a call was taken, for [`Gate::parked`] to wait on.
    taken: watch::Sender<bool>,
}

impl Shared {
    /// Takes the call, if it is the one this gate waits for and the gate has taken none yet, and
    /// gives the receiver that its release arrives on. Taking and parking are one step, so a test
    /// that sees the call taken can release it.
    fn take(&self, op: Op, key: &str, phase: Phase) -> Option<oneshot::Receiver<Release>> {
        let mut slot = lock(&self.slot);
        let wanted = (
            self.op,
            self.phase,
            key.starts_with(&self.prefix),
            slot.taken,
        );
        if wanted != (op, phase, true, false) {
            return None;
        }
        let (release, receiver) = oneshot::channel();
        slot.taken = true;
        slot.parked = Some(release);
        drop(slot);
        self.taken.send_replace(true);
        Some(receiver)
    }

    fn release(&self, how: Release) -> bool {
        let parked = lock(&self.slot).parked.take();
        parked.is_some_and(|release| release.send(how).is_ok())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // No method panics while holding the lock, so a poisoned one is still whole.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The test's hold on one call: it waits for the call to park and lets it go.
///
/// A gate takes the first call that matches it and no other, so what the test does after the
/// release is not held up again.
pub struct Gate {
    shared: Arc<Shared>,
}

impl Gate {
    /// Returns once the gate has taken its call, and at every later time too.
    pub async fn parked(&self) {
        let mut taken = self.shared.taken.subscribe();
        taken.wait_for(|taken| *taken).await.ok();
    }

    /// Lets the parked call go on as if it had never been held. It is `false`, and nothing
    /// happens, when no call is parked: none was taken yet, or it was released already, or its
    /// caller gave up on it.
    pub fn release(&self) -> bool {
        self.shared.release(Release::Pass)
    }

    /// Lets the parked call go on, but with `error` as its answer instead of its own. At
    /// [`Phase::Pre`] the wrapped backend never sees the call; at [`Phase::Post`] it has done the
    /// call, and the caller is told it failed. The result is as for [`Gate::release`].
    pub fn release_with(&self, error: Error) -> bool {
        self.shared.release(Release::Fail(error))
    }

    /// As [`Gate::release_with`] with [`Error::Unavailable`]: at [`Phase::Post`] the answer of a
    /// call whose effect was made and whose reply was lost.
    pub fn fail_post(&self) -> bool {
        let error = Error::Unavailable("the call failed after the store did it, on purpose".into());
        self.release_with(error)
    }
}

/// Passes every call to the backend it wraps, except the ones a [`Gate`] takes: it holds them
/// until the test releases them.
///
/// A test makes one wrapper for each actor of a race over the same wrapped backend, and a gate
/// on one wrapper holds only that actor's call.
pub struct GateBackend {
    inner: Arc<dyn Backend>,
    gates: Mutex<Vec<Arc<Shared>>>,
}

impl GateBackend {
    /// A wrapper with no gate, which passes everything through.
    #[must_use]
    pub fn new(inner: Arc<dyn Backend>) -> Self {
        Self {
            inner,
            gates: Mutex::new(Vec::new()),
        }
    }

    /// Holds the next call of kind `op` on a key that starts with `key_prefix` (for a list call,
    /// the key is the prefix listed) at `phase`, until the returned gate releases it. A gate is
    /// looked at when a call reaches the place it names, so a call that is already under way when
    /// the gate is made, and has not yet reached that place, is held too (a `Post` gate holds a
    /// call that was started before it and is still with the store). A test that needs a call
    /// not to be held makes the gate before it starts the call.
    #[must_use]
    pub fn gate(&self, op: Op, key_prefix: &str, phase: Phase) -> Gate {
        let shared = Arc::new(Shared {
            op,
            prefix: key_prefix.to_owned(),
            phase,
            slot: Mutex::new(Slot {
                taken: false,
                parked: None,
            }),
            taken: watch::channel(false).0,
        });
        lock(&self.gates).push(Arc::clone(&shared));
        Gate { shared }
    }

    /// Parks the call at `phase` if a gate takes it, and gives the error a release put in place
    /// of its answer.
    async fn hold(&self, op: Op, key: &str, phase: Phase) -> Result<(), Error> {
        let receiver = lock(&self.gates)
            .iter()
            .find_map(|gate| gate.take(op, key, phase));
        let Some(receiver) = receiver else {
            return Ok(());
        };
        match receiver.await.unwrap_or(Release::Pass) {
            Release::Pass => Ok(()),
            Release::Fail(error) => Err(error),
        }
    }

    /// Runs `call`, a future not yet polled, between the two places a gate can hold it.
    async fn through<T>(
        &self,
        op: Op,
        key: &str,
        call: impl Future<Output = Result<T, Error>> + Send,
    ) -> Result<T, Error> {
        self.hold(op, key, Phase::Pre).await?;
        let answer = call.await;
        self.hold(op, key, Phase::Post).await?;
        answer
    }
}

#[async_trait]
impl Backend for GateBackend {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.through(Op::Put, key, self.inner.put(key, body)).await
    }

    /// A tagged put is a call of kind [`Op::Put`].
    async fn put_tagged(&self, key: &str, body: Bytes) -> Result<String, Error> {
        let call = self.inner.put_tagged(key, body);
        self.through(Op::Put, key, call).await
    }

    fn needs_length(&self) -> bool {
        self.inner.needs_length()
    }

    /// A streamed put is a call of kind [`Op::Put`].
    async fn put_stream(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        let call = self.inner.put_stream(key, len, body);
        self.through(Op::Put, key, call).await
    }

    /// As `put_stream`, a call of kind [`Op::Put`].
    async fn put_stream_hot_only(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        let call = self.inner.put_stream_hot_only(key, len, body);
        self.through(Op::Put, key, call).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        let call = self.inner.put_if_match(key, body, etag);
        self.through(Op::PutIfMatch, key, call).await
    }

    /// A streamed replacement is a call of kind [`Op::PutIfMatch`].
    async fn put_stream_if_match(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        etag: &str,
    ) -> Result<String, Error> {
        let call = self.inner.put_stream_if_match(key, len, body, etag);
        self.through(Op::PutIfMatch, key, call).await
    }

    /// A reservation is a call of kind [`Op::PutReservation`].
    async fn put_reservation(&self, key: &str, body: Bytes) -> Result<String, Error> {
        let call = self.inner.put_reservation(key, body);
        self.through(Op::PutReservation, key, call).await
    }

    /// As `put_reservation`, a call of kind [`Op::PutReservation`].
    async fn put_reservation_if_match(
        &self,
        key: &str,
        body: Bytes,
        etag: &str,
    ) -> Result<String, Error> {
        let call = self.inner.put_reservation_if_match(key, body, etag);
        self.through(Op::PutReservation, key, call).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.through(Op::Get, key, self.inner.get(key, range)).await
    }

    /// A read with the mark is a call of kind [`Op::Get`].
    async fn get_marked(
        &self,
        key: &str,
        range: Option<ByteRange>,
    ) -> Result<(Bytes, bool, String), Error> {
        let call = self.inner.get_marked(key, range);
        self.through(Op::Get, key, call).await
    }

    /// A streamed read is a call of kind [`Op::Get`]; at [`Phase::Post`] the body is held back
    /// whole, not chunk by chunk.
    async fn get_stream(&self, key: &str, range: Option<ByteRange>) -> Result<ReadBody, Error> {
        let call = self.inner.get_stream(key, range);
        self.through(Op::Get, key, call).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.through(Op::Head, key, self.inner.head(key)).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.through(Op::Delete, key, self.inner.delete(key)).await
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> Result<(), Error> {
        let call = self.inner.delete_if_match(key, etag);
        self.through(Op::DeleteIfMatch, key, call).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.through(Op::List, prefix, self.inner.list(prefix))
            .await
    }

    /// A page is a call of kind [`Op::List`], and is the wrapped backend's own page, not a cut of
    /// its whole listing.
    async fn list_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        let call = self.inner.list_page(prefix, start_after, max_keys);
        self.through(Op::List, prefix, call).await
    }

    /// Asked of the wrapped backend as it is: it makes no call a gate holds.
    async fn settle_conditional_delete(&self, key: &str) -> Option<String> {
        self.inner.settle_conditional_delete(key).await
    }

    async fn create_upload(&self, key: &str) -> Result<String, Error> {
        let call = self.inner.create_upload(key);
        self.through(Op::CreateUpload, key, call).await
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
        let call = self.inner.upload_part_with_len(key, id, number, len, body);
        self.through(Op::UploadPart, key, call).await
    }

    async fn complete_upload(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        let call = self.inner.complete_upload(key, id, parts);
        self.through(Op::CompleteUpload, key, call).await
    }

    async fn abort_upload(&self, key: &str, id: &str) -> Result<(), Error> {
        let call = self.inner.abort_upload(key, id);
        self.through(Op::AbortUpload, key, call).await
    }

    async fn list_uploads(
        &self,
        prefix: &str,
        after: Option<UploadsAfter<'_>>,
        max_uploads: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        let call = self.inner.list_uploads(prefix, after, max_uploads);
        self.through(Op::ListUploads, prefix, call).await
    }
}
