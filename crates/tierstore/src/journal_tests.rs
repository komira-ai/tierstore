//! The checkpoint of a proxy: how two are joined, and that what is stored never goes down.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use bytes::Bytes;

use crate::faulty::{FaultyBackend, Op};
use crate::journal::{Checkpoint, checkpoint_key, store_checkpoint, to_body};
use crate::{Backend, ByteRange, Error, Listed, MemoryBackend, Meta};

fn at(seq: u64, evict_seq: u64, evict_idx: u64) -> Checkpoint {
    Checkpoint {
        seq,
        evict_seq,
        evict_idx,
    }
}

async fn stored(hot: &MemoryBackend) -> Bytes {
    hot.get(&checkpoint_key("p"), None).await.unwrap()
}

/// Asserts how two checkpoints join: the larger `seq`, and the later position taken as a pair,
/// so that a higher `evict_seq` wins over a higher `evict_idx` and the index decides a tie.
#[test]
fn joined_takes_the_larger_seq_and_the_later_position() {
    let cases = [
        (at(2, 3, 1), at(4, 0, 0), at(4, 3, 1)),
        (at(4, 0, 0), at(2, 3, 1), at(4, 3, 1)),
        (at(5, 2, 7), at(5, 2, 7), at(5, 2, 7)),
        (at(5, 2, 1), at(5, 2, 4), at(5, 2, 4)),
        (at(5, 2, 4), at(5, 2, 1), at(5, 2, 4)),
        (at(5, 3, 0), at(5, 2, 9), at(5, 3, 0)),
        (at(5, 2, 9), at(5, 3, 0), at(5, 3, 0)),
    ];
    for (left, right, joined) in cases {
        assert_eq!(left.joined(&right), joined, "{left:?} {right:?}");
    }
}

/// Asserts that a checkpoint that is not stored yet is stored as it is, and given back.
#[tokio::test]
async fn an_absent_checkpoint_is_created_as_wanted() {
    let hot = MemoryBackend::new();
    let wanted = at(2, 1, 0);
    assert_eq!(store_checkpoint(&hot, "p", &wanted).await.unwrap(), wanted);
    assert_eq!(stored(&hot).await, to_body(&wanted));
}

/// Asserts that a stored checkpoint that is behind is replaced by the wanted one.
#[tokio::test]
async fn a_checkpoint_behind_is_replaced() {
    let hot = MemoryBackend::new();
    store_checkpoint(&hot, "p", &at(1, 0, 0)).await.unwrap();
    let wanted = at(3, 2, 1);
    assert_eq!(store_checkpoint(&hot, "p", &wanted).await.unwrap(), wanted);
    assert_eq!(stored(&hot).await, to_body(&wanted));
}

/// Asserts that a stored checkpoint that is ahead, or equal, is not written at all (its content
/// tag is the same after), and that what is stored is given back.
#[tokio::test]
async fn a_checkpoint_ahead_or_equal_is_left_alone() {
    let hot = MemoryBackend::new();
    let ahead = at(4, 3, 0);
    store_checkpoint(&hot, "p", &ahead).await.unwrap();
    let tag = hot.head(&checkpoint_key("p")).await.unwrap().tag;
    for wanted in [at(2, 1, 0), ahead.clone()] {
        let result = store_checkpoint(&hot, "p", &wanted).await.unwrap();
        assert_eq!(result, ahead);
        assert_eq!(hot.head(&checkpoint_key("p")).await.unwrap().tag, tag);
        assert_eq!(stored(&hot).await, to_body(&ahead));
    }
}

/// Asserts that a stored checkpoint ahead in one number and a wanted one ahead in the other are
/// stored as the join of the two.
#[tokio::test]
async fn a_checkpoint_ahead_in_part_is_stored_as_the_join() {
    let hot = MemoryBackend::new();
    store_checkpoint(&hot, "p", &at(4, 0, 0)).await.unwrap();
    let result = store_checkpoint(&hot, "p", &at(2, 3, 1)).await.unwrap();
    assert_eq!(result, at(4, 3, 1));
    assert_eq!(stored(&hot).await, to_body(&at(4, 3, 1)));
}

/// Asserts that a stored checkpoint that does not decode is an error that names the proxy, and
/// is left as it is.
#[tokio::test]
async fn a_corrupt_stored_checkpoint_is_an_error_and_is_kept() {
    let hot = MemoryBackend::new();
    hot.put(&checkpoint_key("p"), Bytes::from_static(b"{"))
        .await
        .unwrap();
    let error = store_checkpoint(&hot, "p", &at(1, 0, 0)).await.unwrap_err();
    let Error::Other(message) = error else {
        panic!("{error:?}");
    };
    assert!(
        message.starts_with("corrupt checkpoint of p: "),
        "{message}"
    );
    assert_eq!(stored(&hot).await, Bytes::from_static(b"{"));
}

/// Asserts that a HEAD that fails is given back and nothing is written.
#[tokio::test]
async fn a_head_that_fails_is_given_back_and_nothing_is_written() {
    let mem = Arc::new(MemoryBackend::new());
    let hot = FaultyBackend::new(mem.clone());
    let down = Error::Unavailable("on purpose".to_owned());
    hot.fail_next_on(Op::Head, "_tiering/checkpoint/", down.clone());
    let error = store_checkpoint(&hot, "p", &at(1, 0, 0)).await.unwrap_err();
    assert_eq!(error, down);
    assert!(mem.head(&checkpoint_key("p")).await.is_err());
}

/// Asserts that a write that finds the object changed since the HEAD (the write is conditional
/// on the HEAD's tag) fails with the precondition error and leaves what is stored alone.
#[tokio::test]
async fn a_checkpoint_changed_since_the_head_is_not_overwritten() {
    let mem = Arc::new(MemoryBackend::new());
    let hot = FaultyBackend::new(mem.clone());
    store_checkpoint(&hot, "p", &at(1, 0, 0)).await.unwrap();
    let refused = Error::PreconditionFailed("changed".to_owned());
    hot.fail_next_on(Op::PutIfMatch, "_tiering/checkpoint/", refused.clone());
    let error = store_checkpoint(&hot, "p", &at(2, 0, 0)).await.unwrap_err();
    assert_eq!(error, refused);
    assert_eq!(stored(&mem).await, to_body(&at(1, 0, 0)));
}

/// Asserts that a create that fails (the object is absent after the HEAD, and the write is
/// refused) is given back and nothing is stored.
#[tokio::test]
async fn a_create_that_fails_is_given_back_and_nothing_is_stored() {
    let mem = Arc::new(MemoryBackend::new());
    let hot = FaultyBackend::new(mem.clone());
    let refused = Error::Full("on purpose".to_owned());
    hot.fail_next_on(Op::Put, "_tiering/checkpoint/", refused.clone());
    let error = store_checkpoint(&hot, "p", &at(1, 0, 0)).await.unwrap_err();
    assert_eq!(error, refused);
    assert!(mem.head(&checkpoint_key("p")).await.is_err());
}

/// A backend that, on the first GET of the checkpoint of `p`, first replaces the stored
/// checkpoint with `body`, as another writer would between the HEAD and the GET of the caller.
struct ReplaceBeforeGet {
    under: Arc<MemoryBackend>,
    body: Bytes,
    spent: AtomicBool,
}

#[async_trait]
impl Backend for ReplaceBeforeGet {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.under.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.under.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        if key == checkpoint_key("p") && !self.spent.swap(true, Ordering::SeqCst) {
            let tag = self.under.head(key).await.unwrap().tag;
            self.under
                .put_if_match(key, self.body.clone(), &tag)
                .await
                .unwrap();
        }
        self.under.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.under.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.under.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.under.list(prefix).await
    }
}

/// Asserts that a checkpoint written between the HEAD and the GET is not overwritten by a merge
/// of the newer content: the write is conditional on the HEAD's older tag, so it fails with the
/// precondition error and what the other writer stored stays.
#[tokio::test]
async fn a_checkpoint_written_between_the_head_and_the_get_is_kept() {
    let mem = Arc::new(MemoryBackend::new());
    store_checkpoint(mem.as_ref(), "p", &at(1, 0, 0))
        .await
        .unwrap();
    let newer = at(5, 2, 0);
    let hot = ReplaceBeforeGet {
        under: mem.clone(),
        body: to_body(&newer),
        spent: AtomicBool::new(false),
    };
    let error = store_checkpoint(&hot, "p", &at(7, 0, 0)).await.unwrap_err();
    assert!(matches!(error, Error::PreconditionFailed(_)), "{error:?}");
    assert_eq!(stored(&mem).await, to_body(&newer));
}
