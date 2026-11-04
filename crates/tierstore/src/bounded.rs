//! A backend whose every call is cut when it has not answered in time.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use crate::{Backend, ByteRange, Deadline, Error, Listed, Meta};

/// The store calls of a holder, each with a deadline: a call that does not answer within its limit
/// is dropped and answers [`Error::Unavailable`], so a stalled store never stalls the caller for
/// longer than the limit.
///
/// A call that reads or writes has one limit for all, `timeout`. A call that destroys (a delete)
/// is given the [`Deadline`] of the holder and starts only if the deadline is at least its margin
/// away; its limit is the shorter of the margin and what is left of the deadline, so that the
/// call is over before the holder's hold on the object is. A refused call is never sent. A call
/// that was cut may have been done: the store was not asked to undo it, and the caller must not
/// take the answer for "not done". No call is retried here.
pub struct Bounded {
    inner: Arc<dyn Backend>,
    timeout: Duration,
}

/// Runs `call`, which is not started before this is awaited, and gives its answer, or the error
/// of a call that did not answer within `limit`.
async fn within<T>(
    limit: Duration,
    what: &str,
    key: &str,
    call: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    match tokio::time::timeout(limit, call).await {
        Ok(answer) => answer,
        Err(_) => Err(Error::Unavailable(format!(
            "{what} of {key} did not answer within {limit:?}"
        ))),
    }
}

impl Bounded {
    /// `inner`, with calls that read or write cut at `timeout`.
    #[must_use]
    pub fn new(inner: Arc<dyn Backend>, timeout: Duration) -> Self {
        Self { inner, timeout }
    }

    /// As [`Backend::get`].
    ///
    /// # Errors
    /// As [`Backend::get`], and [`Error::Unavailable`] if the call was cut.
    pub async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        within(self.timeout, "get", key, self.inner.get(key, range)).await
    }

    /// As [`Backend::head`].
    ///
    /// # Errors
    /// As [`Backend::head`], and [`Error::Unavailable`] if the call was cut.
    pub async fn head(&self, key: &str) -> Result<Meta, Error> {
        within(self.timeout, "head", key, self.inner.head(key)).await
    }

    /// As [`Backend::list`].
    ///
    /// # Errors
    /// As [`Backend::list`], and [`Error::Unavailable`] if the call was cut.
    pub async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        within(self.timeout, "list", prefix, self.inner.list(prefix)).await
    }

    /// As [`Backend::put_tagged`]: create-only, and gives the content tag of what it wrote.
    ///
    /// # Errors
    /// As [`Backend::put_tagged`], and [`Error::Unavailable`] if the call was cut.
    pub async fn put_tagged(&self, key: &str, body: Bytes) -> Result<String, Error> {
        let call = self.inner.put_tagged(key, body);
        within(self.timeout, "put", key, call).await
    }

    /// As [`Backend::put_if_match`].
    ///
    /// # Errors
    /// As [`Backend::put_if_match`], and [`Error::Unavailable`] if the call was cut.
    pub async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        let call = self.inner.put_if_match(key, body, etag);
        within(self.timeout, "put", key, call).await
    }

    /// As [`Backend::delete`], started only if `deadline` allows it, and cut at the margin.
    ///
    /// # Errors
    /// As [`Backend::delete`]; [`Error::Unavailable`] if the call was cut, or was not sent
    /// because less than the margin of the deadline is left.
    pub async fn delete(&self, key: &str, deadline: &Deadline) -> Result<(), Error> {
        let limit = destroying_limit("delete", key, deadline)?;
        within(limit, "delete", key, self.inner.delete(key)).await
    }

    /// As [`Backend::delete_if_match`], started only if `deadline` allows it, and cut at the
    /// margin.
    ///
    /// # Errors
    /// As [`Backend::delete_if_match`]; [`Error::Unavailable`] if the call was cut, or was not
    /// sent because less than the margin of the deadline is left.
    pub async fn delete_if_match(
        &self,
        key: &str,
        etag: &str,
        deadline: &Deadline,
    ) -> Result<(), Error> {
        let limit = destroying_limit("delete", key, deadline)?;
        within(limit, "delete", key, self.inner.delete_if_match(key, etag)).await
    }
}

/// The limit of a call that destroys, or the error that says it may not start.
fn destroying_limit(what: &str, key: &str, deadline: &Deadline) -> Result<Duration, Error> {
    if !deadline.allows_start() {
        return Err(Error::Unavailable(format!(
            "{what} of {key} not sent: {:?} of the deadline is left, and {:?} is needed",
            deadline.remaining(),
            deadline.margin()
        )));
    }
    Ok(deadline.margin().min(deadline.remaining()))
}
