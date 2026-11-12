//! The create-only put that names the SHA-256 of its body
//! ([`Backend::put_stream_sha256`](crate::Backend::put_stream_sha256)): the body checked as it
//! passes, and the answer to a key that exists, which is a success when the object there is the
//! one the put meant to make.
//!
//! The check is made when the declared length has passed, in place of the chunk that completes
//! it, so that a backend that stops reading at the declared length (S3's streamed PUT does) still
//! sends the error and not the last byte; a body that ends short, or runs past the length, ends
//! with the error too. A backend stores nothing of a body that ends with an error (the contract of
//! [`Backend::put_stream`](crate::Backend::put_stream)), so a body that is not its digest is never
//! published.

use bytes::Bytes;
use futures::{StreamExt as _, stream};
use sha2::{Digest as _, Sha256};

use crate::{Backend, Error, PartBody, TreeDigest};

/// What a create-only put that names the SHA-256 of its body did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutOutcome {
    /// The put made the object.
    Created,
    /// The key held an object of the same size and SHA-256 already, so nothing was written: the
    /// object is the one the put meant to make (a put sent again after its answer was lost).
    Existed,
}

/// The check of a body as it passes: its length and its SHA-256 against those declared.
struct Check {
    key: String,
    len: u64,
    want: TreeDigest,
    seen: u64,
    /// The hash of the bytes so far; none once the body has been checked.
    hasher: Option<Sha256>,
}

impl Check {
    fn bad(&self) -> Error {
        Error::BadDigest(self.key.clone())
    }

    /// Takes a chunk: an error if the body has run past its length, or if it completes the
    /// length and the whole does not hash to the digest.
    fn take(&mut self, chunk: &Bytes) -> Result<(), Error> {
        self.seen = self.seen.saturating_add(chunk.len() as u64);
        if self.seen > self.len {
            return Err(self.bad());
        }
        if let Some(hasher) = &mut self.hasher {
            hasher.update(chunk);
        }
        if self.seen == self.len {
            return self.finish();
        }
        Ok(())
    }

    /// The end of the body, or of its length: the hash is checked once.
    fn finish(&mut self) -> Result<(), Error> {
        let Some(hasher) = self.hasher.take() else {
            return Ok(());
        };
        let got: [u8; 32] = hasher.finalize().into();
        if self.seen == self.len && got == *self.want.as_bytes() {
            Ok(())
        } else {
            Err(self.bad())
        }
    }
}

/// `body`, passed on as it is, checked to be `len` bytes that hash to `sha256` (see the module
/// docs). Its error is [`Error::BadDigest`] naming `key`; the error of `body`, if it ends with
/// one, is passed on as it is. Nothing is polled after an error.
pub(crate) fn checked<'a>(
    key: &str,
    len: u64,
    sha256: TreeDigest,
    body: PartBody<'a>,
) -> PartBody<'a> {
    let check = Check {
        key: key.to_owned(),
        len,
        want: sha256,
        seen: 0,
        hasher: Some(Sha256::new()),
    };
    stream::unfold(Some((body, check)), |state| async move {
        let (mut body, mut check) = state?;
        match body.next().await {
            Some(Ok(chunk)) => match check.take(&chunk) {
                Ok(()) => Some((Ok(chunk), Some((body, check)))),
                Err(error) => Some((Err(error), None)),
            },
            Some(Err(error)) => Some((Err(error), None)),
            None => check.finish().err().map(|error| (Err(error), None)),
        }
    })
    .boxed()
}

/// The provided [`Backend::put_stream_sha256`]: [`Backend::put_stream`] of the checked body, and
/// for a key that exists, a read of the object there.
pub(crate) async fn put<B: Backend + ?Sized>(
    backend: &B,
    key: &str,
    len: u64,
    body: PartBody<'_>,
    sha256: TreeDigest,
) -> Result<PutOutcome, Error> {
    let body = checked(key, len, sha256, body);
    match backend.put_stream(key, Some(len), body).await {
        Ok(()) => Ok(PutOutcome::Created),
        Err(Error::AlreadyExists(_)) => read_and_compare(backend, key, len, sha256).await,
        Err(other) => Err(other),
    }
}

/// The answer to a put whose key exists, by a read of the whole object: [`PutOutcome::Existed`] if
/// it is `len` bytes that hash to `sha256`, [`Error::AlreadyExists`] if not. The read stops at the
/// first byte past `len`.
///
/// # Errors
/// [`Error::Conflict`] if the object is gone by the time it is read (a delete came between the
/// two calls; a retry gets the true answer), and the error of the read.
async fn read_and_compare<B: Backend + ?Sized>(
    backend: &B,
    key: &str,
    len: u64,
    sha256: TreeDigest,
) -> Result<PutOutcome, Error> {
    let mut body = match backend.get_stream(key, None).await {
        Ok(body) => body,
        Err(Error::NotFound(_)) => return Err(Error::Conflict(key.to_owned())),
        Err(other) => return Err(other),
    };
    let mut hasher = Sha256::new();
    let mut seen = 0u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        seen = seen.saturating_add(chunk.len() as u64);
        if seen > len {
            return Err(Error::AlreadyExists(key.to_owned()));
        }
        hasher.update(&chunk);
    }
    let got: [u8; 32] = hasher.finalize().into();
    if seen == len && got == *sha256.as_bytes() {
        Ok(PutOutcome::Existed)
    } else {
        Err(Error::AlreadyExists(key.to_owned()))
    }
}
