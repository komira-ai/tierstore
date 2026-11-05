//! The calls beyond [`ObjectStore`](crate::ObjectStore) that a store used for coordination
//! needs: the entity tag and time of last write of an object (HEAD), a replace and a delete
//! that only an object with a given tag takes (`If-Match`), a read of the whole object, and a listing
//! that gives the time each object was last written.
//!
//! They are inherent methods of [`S3Store`], so the trait and the other stores are as they
//! were. A read is sent again after a failure in transport, as a ranged read is (see
//! [`S3Tuning::read_retries`]); the replace and the delete are sent once, since a connection that fails
//! before the answer leaves it unknown whether the object was replaced or deleted, and only the
//! caller can read it back.
//!
//! A create-only write (`If-None-Match: *`) is
//! [`put_new`](crate::ObjectStore::put_new) on a store that claims
//! [`Capabilities::conditional_put`](crate::Capabilities::conditional_put).
//!
//! A conditional write is sent once, as the replace and the delete are:
//! an answer of 409 `ConditionalRequestConflict` (two writers racing for one key, which the store
//! does not rank) does not say which of them won, so the write may or may not have landed, and
//! sending it again could be answered 412 to a writer that had won. The 409 is returned as it is
//! (an [`ObjectStoreError::Service`] with status 409), and the caller reads the object to see
//! whether the tag is the one it wrote.

use std::future::Future;

use bytes::Bytes;
use reqwest::{Method, Response, StatusCode};

use super::{S3Store, S3Tuning, failure, is_retryable, times, transport, xml};
use crate::{KeyPrefix, ListToken, ObjectKey, ObjectStoreError, PageSize};

/// What a HEAD says about an object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectHead {
    /// The object's size in bytes.
    pub size: u64,
    /// The entity tag as the store sends it, quotes included. For an object written by one
    /// PUT, S3 makes it the hex MD5 of the bytes, so equal bytes have equal tags.
    pub etag: String,
    /// When the object was last written, in milliseconds since the Unix epoch. S3 keeps
    /// whole seconds, so the last three digits are zero.
    pub modified_millis: u64,
}

/// One object of [`S3Store::list_stamped`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StampedObject {
    /// The object's key.
    pub key: ObjectKey,
    /// The object's size in bytes.
    pub size: u64,
    /// When the object was last written, in milliseconds since the Unix epoch, as the listing
    /// states it: S3 sends whole seconds, and another store may send milliseconds. It need not
    /// equal [`ObjectHead::modified_millis`] of the same object.
    pub modified_millis: u64,
}

/// One page of [`S3Store::list_stamped`], in ascending key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StampedPage {
    /// The objects on this page.
    pub objects: Vec<StampedObject>,
    /// Where the next page starts, or `None` if this is the last page.
    pub next: Option<ListToken>,
}

/// Runs `once` until it answers: it is run again after a failure in transport, with the
/// waits and the count of a ranged read.
pub(super) async fn retrying<T, Fut>(
    tuning: &S3Tuning,
    mut once: impl FnMut() -> Fut,
) -> Result<T, ObjectStoreError>
where
    Fut: Future<Output = Result<T, ObjectStoreError>>,
{
    let mut wait = tuning.read_retry_wait;
    for _ in 0..tuning.read_retries {
        match once().await {
            Err(e) if is_retryable(&e) => {
                tracing::debug!(error = %e, "read failed in transport; sending it again");
                tokio::time::sleep(wait).await;
                wait *= 2;
            }
            answer => return answer,
        }
    }
    once().await
}

/// The text of a header of an answer, if it is there, is text, and is not empty.
pub(super) fn header<'a>(resp: &'a Response, name: &str) -> Option<&'a str> {
    let value = resp.headers().get(name)?.to_str().ok()?;
    (!value.is_empty()).then_some(value)
}

/// What the answer to a PUT says of the object it made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutAnswer {
    /// The `ETag` of the answer, quotes included, if it has one.
    pub etag: Option<String>,
    /// The `Last-Modified` of the answer in milliseconds since the Unix epoch (whole seconds, as
    /// the header has them), if it has one that is a date.
    pub modified_millis: Option<u64>,
}

impl S3Store {
    /// Replaces the object only if its entity tag is `etag` (`If-Match`, the tag sent as
    /// it is), and gives the tag of the new object.
    ///
    /// # Errors
    /// [`ObjectStoreError::NotFound`] if no object has the key. [`ObjectStoreError::Service`]
    /// with status 412 if the object has another tag; it is left as it was.
    /// [`ObjectStoreError::Protocol`] if the answer carries no `ETag`.
    pub async fn put_if_match(
        &self,
        key: &ObjectKey,
        body: Bytes,
        etag: &str,
    ) -> Result<String, ObjectStoreError> {
        let condition = ("if-match", etag.to_owned());
        self.conditional_write(key, body, condition).await
    }

    /// Creates the object only if the key has none (`If-None-Match: *`), and gives
    /// the tag of the new object as the store sends it: the `ETag` of the answer to this write,
    /// so a caller that must own what it wrote holds the tag of its own object and not of
    /// whatever a later HEAD would show.
    ///
    /// # Errors
    /// [`ObjectStoreError::Unsupported`] unless the store claims
    /// [`Capabilities::conditional_put`](crate::Capabilities::conditional_put): without the
    /// header the write would replace an object that exists.
    /// [`ObjectStoreError::AlreadyExists`] if the key has an object.
    /// [`ObjectStoreError::Protocol`] if the answer carries no `ETag`.
    pub async fn put_new_tagged(
        &self,
        key: &ObjectKey,
        body: Bytes,
    ) -> Result<String, ObjectStoreError> {
        if !self.conn.capabilities.conditional_put {
            return Err(ObjectStoreError::Unsupported(
                "a create-only write on a store without conditional puts",
            ));
        }
        let condition = ("if-none-match", "*".to_owned());
        self.conditional_write(key, body, condition).await
    }

    /// [`put_new_tagged`](Self::put_new_tagged) for a caller that can do without the tag: the
    /// answer of the write as it is, with the `ETag` and the `Last-Modified` it carries, each
    /// `None` if the answer has none (S3's answer to a PUT has an `ETag` and no `Last-Modified`).
    /// An answer with no `ETag` is not an error here: the object is made.
    ///
    /// # Errors
    /// As [`put_new_tagged`](Self::put_new_tagged), except that no `ETag` is not a
    /// [`ObjectStoreError::Protocol`].
    pub async fn put_new_answered(
        &self,
        key: &ObjectKey,
        body: Bytes,
    ) -> Result<PutAnswer, ObjectStoreError> {
        if !self.conn.capabilities.conditional_put {
            return Err(ObjectStoreError::Unsupported(
                "a create-only write on a store without conditional puts",
            ));
        }
        let condition = ("if-none-match", "*".to_owned());
        self.conditional_answer(key, body, condition).await
    }

    /// A PUT that carries `condition` (`If-Match` or `If-None-Match`), sent once (a 409 leaves it
    /// unknown whether it was applied, see the module docs). Gives the `ETag` of the answer.
    async fn conditional_write(
        &self,
        key: &ObjectKey,
        body: Bytes,
        condition: (&'static str, String),
    ) -> Result<String, ObjectStoreError> {
        let answer = self.conditional_answer(key, body, condition).await?;
        answer
            .etag
            .ok_or_else(|| ObjectStoreError::Protocol(format!("PUT of {key}: no ETag")))
    }

    /// As [`conditional_write`](Self::conditional_write), giving the answer's details.
    async fn conditional_answer(
        &self,
        key: &ObjectKey,
        body: Bytes,
        condition: (&'static str, String),
    ) -> Result<PutAnswer, ObjectStoreError> {
        let (name, value) = condition;
        let headers = vec![(name, value)];
        let resp = self
            .conn
            .send(Method::PUT, Some(key), &[], headers, body)
            .await?;
        match resp.status() {
            StatusCode::OK => Ok(PutAnswer {
                etag: header(&resp, "etag").map(str::to_owned),
                modified_millis: header(&resp, "last-modified").and_then(times::http_date_millis),
            }),
            StatusCode::NOT_FOUND => Err(ObjectStoreError::NotFound(key.clone())),
            StatusCode::PRECONDITION_FAILED if name == "if-none-match" => {
                Err(ObjectStoreError::AlreadyExists(key.clone()))
            }
            _ => Err(failure(resp).await),
        }
    }

    /// Deletes the object only if its entity tag is `etag` (`If-Match` on the DELETE, the tag
    /// sent as it is). Sent once, as [`put_if_match`](Self::put_if_match) is: after a failure in
    /// transport the caller reads the object to see whether it is gone.
    ///
    /// Only a store that enforces the header refuses a wrong tag. Amazon S3 does; some
    /// S3-compatible stores take the header and delete anyway, so the caller that needs the
    /// refusal asks the store once (a wrong tag, then a read) before it relies on this call.
    /// On a store with Object Lock the call is not supported: a delete there names the version.
    ///
    /// # Errors
    /// [`ObjectStoreError::NotFound`] if the store answers 404, as it does where there is no
    /// object. [`ObjectStoreError::Service`] with status 412 if the object has another tag; it
    /// is left as it was. [`ObjectStoreError::Unsupported`] with Object Lock on.
    pub async fn delete_if_match(
        &self,
        key: &ObjectKey,
        etag: &str,
    ) -> Result<(), ObjectStoreError> {
        if self.conn.capabilities.object_lock {
            return Err(ObjectStoreError::Unsupported(
                "delete_if_match with Object Lock",
            ));
        }
        let headers = vec![("if-match", etag.to_owned())];
        let sent = self
            .conn
            .send(Method::DELETE, Some(key), &[], headers, Bytes::new())
            .await?;
        match sent.status() {
            StatusCode::NO_CONTENT | StatusCode::OK => Ok(()),
            StatusCode::NOT_FOUND => Err(ObjectStoreError::NotFound(key.clone())),
            _ => Err(failure(sent).await),
        }
    }

    /// The object's size, entity tag and time of last write.
    ///
    /// # Errors
    /// [`ObjectStoreError::NotFound`] if no object has the key.
    /// [`ObjectStoreError::Protocol`] if the answer lacks a size, a tag or a readable time.
    pub async fn head(&self, key: &ObjectKey) -> Result<ObjectHead, ObjectStoreError> {
        retrying(&self.conn.tuning, move || self.head_once(key)).await
    }

    async fn head_once(&self, key: &ObjectKey) -> Result<ObjectHead, ObjectStoreError> {
        let resp = self
            .conn
            .send(Method::HEAD, Some(key), &[], Vec::new(), Bytes::new())
            .await?;
        match resp.status() {
            StatusCode::OK => {}
            StatusCode::NOT_FOUND => return Err(ObjectStoreError::NotFound(key.clone())),
            _ => return Err(failure(resp).await),
        }
        let bad = |what: &str| ObjectStoreError::Protocol(format!("HEAD of {key}: {what}"));
        let size = header(&resp, "content-length")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| bad("missing or bad Content-Length"))?;
        let etag = header(&resp, "etag").ok_or_else(|| bad("no ETag"))?;
        let modified_millis = header(&resp, "last-modified")
            .and_then(times::http_date_millis)
            .ok_or_else(|| bad("missing or bad Last-Modified"))?;
        Ok(ObjectHead {
            size,
            etag: etag.to_owned(),
            modified_millis,
        })
    }

    /// The whole object.
    ///
    /// # Errors
    /// [`ObjectStoreError::NotFound`] if no object has the key.
    pub async fn get_all(&self, key: &ObjectKey) -> Result<Bytes, ObjectStoreError> {
        let read = retrying(&self.conn.tuning, move || self.get_all_once(key)).await;
        read.map(|(bytes, _)| bytes)
    }

    /// The whole object and its entity tag (quotes
    /// included, if the answer has one), all from the one answer, so that they belong to one
    /// object whatever is written to the key meanwhile.
    ///
    /// # Errors
    /// [`ObjectStoreError::NotFound`] if no object has the key.
    pub async fn get_all_tagged(
        &self,
        key: &ObjectKey,
    ) -> Result<(Bytes, Option<String>), ObjectStoreError> {
        retrying(&self.conn.tuning, move || self.get_all_once(key)).await
    }

    async fn get_all_once(
        &self,
        key: &ObjectKey,
    ) -> Result<(Bytes, Option<String>), ObjectStoreError> {
        let resp = self
            .conn
            .send(Method::GET, Some(key), &[], Vec::new(), Bytes::new())
            .await?;
        match resp.status() {
            StatusCode::OK => {
                let etag = header(&resp, "etag").map(str::to_owned);
                let bytes = resp.bytes().await.map_err(transport)?;
                Ok((bytes, etag))
            }
            StatusCode::NOT_FOUND => Err(ObjectStoreError::NotFound(key.clone())),
            _ => Err(failure(resp).await),
        }
    }

    /// One page of the objects under `prefix`, as [`list`](crate::ObjectStore::list) gives
    /// it, with the time each was last written.
    ///
    /// # Errors
    /// As [`list`](crate::ObjectStore::list), and [`ObjectStoreError::Protocol`] if an
    /// object is listed without a readable time.
    pub async fn list_stamped(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<StampedPage, ObjectStoreError> {
        let body = retrying(&self.conn.tuning, move || {
            self.list_body(prefix, after, max_keys)
        })
        .await?;
        xml::stamped_page(&body)
    }
}
