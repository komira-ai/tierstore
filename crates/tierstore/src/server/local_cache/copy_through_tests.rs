//! The copy of a cache that writes through, called on the cache as a backend
//! ([`Backend::copy_in_store`]): the keys it refuses, the cache that does not write, and what the
//! cache does with its own copy of the key a copy writes.

use super::testing::{Rig, until};
use crate::{Backend, CopyTarget, Error, SourceConditions, StoreCopy};

/// The key the tests hold in the cache.
const HELD: &str = "cas/b/dst";

/// A copy of the object `source` (three bytes) into `key`.
fn copy<'a>(
    source: &'a str,
    key: &'a str,
    conditions: &'a SourceConditions,
    target: CopyTarget<'a>,
) -> StoreCopy<'a> {
    StoreCopy {
        source,
        source_size: 3,
        conditions,
        key,
        target,
    }
}

/// A part of an upload, from the whole of the source.
fn part() -> CopyTarget<'static> {
    CopyTarget::Part {
        upload_id: "u",
        number: 1,
        range: None,
    }
}

/// Fills the cache with `HELD` ("old") and waits until it holds the object.
async fn hold_old(rig: &Rig) {
    rig.put(HELD, b"old").await;
    assert_eq!(rig.get(HELD).await.unwrap(), "old");
    until("the cache holds the object", || rig.cache.held() == (3, 1)).await;
}

/// Asserts that a copy whose source is a key a storage node keeps for itself, or whose
/// destination is one, is refused as `Reserved` naming that key, for a copy of an object and for
/// a copy into a part, and that the upstream saw no call.
#[tokio::test]
async fn a_copy_of_a_reserved_key_is_refused_with_the_key_and_reaches_nothing() {
    let rig = Rig::writing("tscache-copy-reserved");
    let none = SourceConditions::default();
    for target in [CopyTarget::Object, part()] {
        let source = copy("_journal/x", HELD, &none, target);
        let made = rig.cache.copy_in_store(&source).await;
        assert_eq!(made, Err(Error::Reserved("_journal/x".to_owned())));
        let into = copy("cas/b/src", "_tiering/y", &none, target);
        let made = rig.cache.copy_in_store(&into).await;
        assert_eq!(made, Err(Error::Reserved("_tiering/y".to_owned())));
    }
    assert!(rig.upstream.call_log().is_empty());
}

/// Asserts that a cache that is not told to write refuses a copy, of an object and into a part,
/// with the answer of a read cache and without a call of the upstream.
#[tokio::test]
async fn a_cache_that_does_not_write_through_refuses_a_copy() {
    let rig = Rig::new("tscache-copy-read-only");
    let none = SourceConditions::default();
    for target in [CopyTarget::Object, part()] {
        let asked = copy("cas/b/src", HELD, &none, target);
        let made = rig.cache.copy_in_store(&asked).await;
        let Err(Error::Unsupported(text)) = made else {
            panic!("a read cache made a copy: {made:?}");
        };
        assert!(text.contains("read cache"), "{text}");
    }
    assert!(rig.upstream.call_log().is_empty());
}

/// Asserts that a copy of an object forgets the key it writes: the cache held the old object, the
/// upstream's object is another one by the time the copy is made, and after the copy the cache
/// holds nothing of the key, has no file for it, and serves what the upstream holds. The copy
/// itself is the upstream's: a backend that makes none answers `None`, and the cache passes that
/// on.
#[tokio::test]
async fn a_copy_of_an_object_forgets_the_destination_the_cache_held() {
    let rig = Rig::writing("tscache-copy-forgets");
    hold_old(&rig).await;
    assert!(rig.has_file(HELD));
    rig.replace(HELD, b"newer").await;

    let none = SourceConditions::default();
    let asked = copy("cas/b/src", HELD, &none, CopyTarget::Object);
    assert_eq!(rig.cache.copy_in_store(&asked).await, Ok(None));

    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(HELD));
    assert_eq!(rig.get(HELD).await.unwrap(), "newer");
}

/// Asserts that a copy into a part is the upstream's alone: the object the cache holds under the
/// key of the upload stays held and is served as it was.
#[tokio::test]
async fn a_copy_into_a_part_leaves_the_cached_object_held() {
    let rig = Rig::writing("tscache-copy-part");
    hold_old(&rig).await;

    let none = SourceConditions::default();
    let asked = copy("cas/b/src", HELD, &none, part());
    assert_eq!(rig.cache.copy_in_store(&asked).await, Ok(None));

    assert_eq!(rig.cache.held(), (3, 1));
    assert!(rig.has_file(HELD));
    assert_eq!(rig.get(HELD).await.unwrap(), "old");
}
