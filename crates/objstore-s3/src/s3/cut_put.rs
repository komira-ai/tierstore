//! The look at the key after a single PUT that got no answer.
//!
//! A PUT whose body failed (its source ended with an error, or short of its length) or whose
//! transport did (the connection went, or the store stopped reading for `request_timeout`) ends
//! with no answer from the store, and the store may still have kept what it read of it:
//! SeaweedFS 4.54 keeps such a PUT without `Content-Type` and without a checksum as an object of
//! the bytes that came, 0 of them for a body that failed before its first byte (`bound.rs`). That
//! object breaks the contract of [`ObjectStore::put_stream`](crate::ObjectStore::put_stream) (a
//! body that fails leaves nothing under the key), readers see the wrong length, and a create-only
//! writer of the key is refused for ever.
//!
//! So every single PUT carries a token of its own, the metadata `x-amz-meta-write-token`: the hex
//! of 128 bits drawn for that PUT, which no other PUT carries ([`token`]). After a PUT that got no
//! answer, once [`S3Tuning::cut_put_look_after`](super::S3Tuning::cut_put_look_after) has passed
//! (the store takes a body cut short when it sees the connection close, which is after this side
//! gave up on it), one HEAD looks at the key ([`judge`]):
//!
//! - no object, or an object of the length the PUT declared: nothing is done, whoever wrote it;
//! - an object of another length that carries this PUT's token: this PUT made it. It is deleted on
//!   its entity tag (`If-Match`), so an object written over it since the HEAD stays; `removed`;
//! - an object of another length without this PUT's token: this PUT did not make it. It was there
//!   before the PUT began (the store refused a create-only PUT on it, or did not take a plain PUT
//!   over it), or another writer made it since. It is left as it is; `left`;
//! - a HEAD or a DELETE that fails, or an object of this PUT with no entity tag to delete it on:
//!   the object stays; `failed`.
//!
//! The look never changes what the put gives: the error it ended with.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher as _;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use reqwest::{Method, StatusCode};

use super::tagged::header;
use super::{S3Store, failure};
use crate::{ObjectKey, ObjectStoreError};

/// The metadata a single PUT names itself by.
pub(super) const TOKEN_META: &str = "x-amz-meta-write-token";

/// What the looks after single PUTs that got no answer did, since the store was made (module
/// docs). An [`S3Store`] and its clones count together.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CutPuts {
    /// Objects of the wrong length a PUT that got no answer had made, deleted.
    pub removed: u64,
    /// Objects of the wrong length that such a PUT found and had not made (one there before it
    /// began, or another writer's), left as they are.
    pub left: u64,
    /// Looks that could not be made or finished (the HEAD or the DELETE failed, or the object of
    /// the PUT had no entity tag), so an object of the wrong length may have stayed.
    pub failed: u64,
}

/// The counts of [`CutPuts`], shared by the clones of a store.
#[derive(Debug, Default)]
pub(super) struct CutCounts {
    removed: AtomicU64,
    left: AtomicU64,
    failed: AtomicU64,
}

impl CutCounts {
    /// The counts so far.
    pub(super) fn read(&self) -> CutPuts {
        CutPuts {
            removed: self.removed.load(Ordering::Relaxed),
            left: self.left.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
        }
    }
}

/// A token no other PUT carries: the hex of two 64-bit hashes of a number this process gives no
/// other PUT, each keyed by keys the standard library draws at random for each hasher.
pub(super) fn token() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let half = |salt: u8| RandomState::new().hash_one((n, salt));
    format!("{:016x}{:016x}", half(0), half(1))
}

/// What a HEAD of the key said: the object's length if the answer gave one, its entity tag, and
/// the token it was written with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Seen {
    pub(super) size: Option<u64>,
    pub(super) etag: Option<String>,
    pub(super) token: Option<String>,
}

/// What to do about the object `seen` under the key of a PUT of `len` bytes that carried `token`
/// and got no answer (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Look {
    /// Nothing to delete; the look comes to this.
    Done(Outcome),
    /// The PUT's own object, of another length: delete it on this entity tag.
    Remove(String),
}

/// The judgement of the module docs: [`Outcome::Nothing`] for no object or one of the length
/// declared, [`Outcome::Left`] for another object of another length (or of a length the answer
/// did not give), the removal of the PUT's own object of another length, or [`Outcome::Failed`]
/// when it has no entity tag to delete it on.
pub(super) fn judge(seen: Option<&Seen>, len: u64, token: &str) -> Look {
    let Some(seen) = seen else {
        return Look::Done(Outcome::Nothing);
    };
    if seen.size == Some(len) {
        return Look::Done(Outcome::Nothing);
    }
    if seen.token.as_deref() != Some(token) {
        return Look::Done(Outcome::Left);
    }
    match &seen.etag {
        Some(etag) => Look::Remove(etag.clone()),
        None => Look::Done(Outcome::Failed),
    }
}

/// What the look came to, for its count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Nothing,
    Removed,
    Left,
    Failed,
}

/// What the DELETE of the PUT's own object came to: gone (404) or written over since the HEAD
/// (412) is not a removal, and not a failure either: the short object is not there any more.
pub(super) fn deleted(result: &Result<(), ObjectStoreError>) -> Outcome {
    match result {
        Ok(()) => Outcome::Removed,
        Err(ObjectStoreError::NotFound(_)) => Outcome::Nothing,
        Err(ObjectStoreError::Service { status: 412, .. }) => Outcome::Nothing,
        Err(_) => Outcome::Failed,
    }
}

impl S3Store {
    /// The look after a PUT of `len` bytes to `key` that carried `token` and got no answer
    /// (module docs).
    pub(super) async fn look_after_cut(&self, key: &ObjectKey, len: u64, token: &str) {
        tokio::time::sleep(self.conn.tuning.cut_put_look_after).await;
        let outcome = match self.head_written(key).await {
            Err(error) => {
                tracing::warn!(%key, %error, "could not look at the key of a PUT that got no answer");
                Outcome::Failed
            }
            Ok(seen) => match judge(seen.as_ref(), len, token) {
                Look::Done(outcome) => outcome,
                Look::Remove(etag) => deleted(&self.delete_if_match(key, &etag).await),
            },
        };
        self.count_look(key, len, outcome);
    }

    /// Counts and logs what the look at `key`, after a PUT of `len` bytes, came to.
    fn count_look(&self, key: &ObjectKey, len: u64, outcome: Outcome) {
        let counts = &self.conn.cut;
        let counter = match outcome {
            Outcome::Nothing => return,
            Outcome::Removed => {
                tracing::warn!(%key, len, "removed the object of the wrong length a PUT that got no answer left");
                &counts.removed
            }
            Outcome::Left => {
                tracing::warn!(%key, len, "the key holds an object of the wrong length that this PUT did not write; left");
                &counts.left
            }
            Outcome::Failed => {
                tracing::warn!(%key, len, "a PUT that got no answer may have left an object of the wrong length");
                &counts.failed
            }
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// One HEAD of `key`, not sent again: `None` for no object.
    async fn head_written(&self, key: &ObjectKey) -> Result<Option<Seen>, ObjectStoreError> {
        let resp = self
            .conn
            .send(Method::HEAD, Some(key), &[], Vec::new(), Bytes::new())
            .await?;
        match resp.status() {
            StatusCode::OK => {}
            StatusCode::NOT_FOUND => return Ok(None),
            _ => return Err(failure(resp).await),
        }
        let text = |name: &str| header(&resp, name).map(str::to_owned);
        Ok(Some(Seen {
            size: header(&resp, "content-length").and_then(|v| v.parse().ok()),
            etag: text("etag"),
            token: text(TOKEN_META),
        }))
    }

    /// What the looks after single PUTs that got no answer did so far (module docs).
    #[must_use]
    pub fn cut_puts(&self) -> CutPuts {
        self.conn.cut.read()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(size: Option<u64>, etag: Option<&str>, token: Option<&str>) -> Seen {
        Seen {
            size,
            etag: etag.map(str::to_owned),
            token: token.map(str::to_owned),
        }
    }

    /// Asserts every judgement: nothing for no object and for one of the declared length (the
    /// PUT's own or not); the PUT's own object of another length is removed on its tag, or is a
    /// failure without one; any other object of another length, or of no stated length, is left.
    #[test]
    fn only_the_puts_own_object_of_another_length_is_removed() {
        let ours = "aa";
        assert_eq!(judge(None, 10, ours), Look::Done(Outcome::Nothing));
        let whole = seen(Some(10), Some("\"t\""), Some(ours));
        assert_eq!(judge(Some(&whole), 10, ours), Look::Done(Outcome::Nothing));
        let theirs_whole = seen(Some(10), Some("\"t\""), None);
        assert_eq!(
            judge(Some(&theirs_whole), 10, ours),
            Look::Done(Outcome::Nothing)
        );
        let short = seen(Some(0), Some("\"t\""), Some(ours));
        assert_eq!(
            judge(Some(&short), 10, ours),
            Look::Remove("\"t\"".to_owned())
        );
        let untagged = seen(Some(0), None, Some(ours));
        assert_eq!(
            judge(Some(&untagged), 10, ours),
            Look::Done(Outcome::Failed)
        );
        let before = seen(Some(0), Some("\"t\""), None);
        assert_eq!(judge(Some(&before), 10, ours), Look::Done(Outcome::Left));
        let another = seen(Some(4), Some("\"t\""), Some("bb"));
        assert_eq!(judge(Some(&another), 10, ours), Look::Done(Outcome::Left));
        let no_length = seen(None, Some("\"t\""), None);
        assert_eq!(judge(Some(&no_length), 10, ours), Look::Done(Outcome::Left));
    }

    /// Asserts what each answer to the DELETE comes to: a removal, nothing for an object gone
    /// (404) or written over (412) since the HEAD, and a failure for any other error.
    #[test]
    fn a_delete_that_finds_the_object_gone_or_replaced_is_not_a_failure() {
        let key = ObjectKey::new("k").unwrap();
        assert_eq!(deleted(&Ok(())), Outcome::Removed);
        let gone = Err(ObjectStoreError::NotFound(key));
        assert_eq!(deleted(&gone), Outcome::Nothing);
        let service = |status| ObjectStoreError::Service {
            status,
            code: String::new(),
            message: String::new(),
        };
        assert_eq!(deleted(&Err(service(412))), Outcome::Nothing);
        assert_eq!(deleted(&Err(service(500))), Outcome::Failed);
    }

    /// Asserts that tokens are 32 hex digits and that two PUTs never carry the same one.
    #[test]
    fn each_put_has_a_token_of_its_own() {
        let tokens: std::collections::BTreeSet<String> = (0..64).map(|_| token()).collect();
        assert_eq!(tokens.len(), 64);
        for token in &tokens {
            assert_eq!(token.len(), 32);
            assert!(token.bytes().all(|b| b.is_ascii_hexdigit()), "{token}");
        }
    }
}
