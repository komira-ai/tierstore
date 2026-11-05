//! A read-only cache whose index was scanned before the holder of the directory deleted an
//! object and put it again with another body: what it serves, and what it says in a HEAD.
//!
//! In these tests `Rig::cache`, which takes no lock, stands for the holder: its `delete` and
//! `put` are the real removal and publication of the files. The caches of `Rig::locked` are
//! the holder's guard (the first) and the reader (the second), which scanned the directory
//! before the holder changed the object.

use super::layout::{meta_path, name_of};
use super::metrics::read;
use super::testing::{Rig, age, digest, pattern, tree};
use super::{LocalCache, Mode};
use crate::{Backend, ByteRange, Op};

/// The key of the object that the holder replaces.
const KEY: &str = "cas/b/replaced";

/// The size of the object before the holder replaces it.
const OLD: usize = 40;

/// A body of `len` bytes that differs from `pattern(len)` in every byte.
fn other(len: usize) -> Vec<u8> {
    pattern(len).into_iter().map(|byte| !byte).collect()
}

/// A rig whose holder has put `KEY` with `OLD` bytes, a guard that holds the lock, and a reader
/// that scanned the object, and the holder's replacement of the object by `body`.
async fn replaced_by(name: &str, body: &[u8]) -> (Rig, LocalCache, LocalCache) {
    let rig = Rig::writing(name);
    rig.cache
        .put(KEY, pattern(OLD).into())
        .await
        .expect("the first put");
    let guard = rig.locked();
    let reader = rig.locked();
    assert_eq!(reader.held(), (OLD as u64, 1));
    assert_eq!(reader.mode(), Mode::ReadOnly);
    rig.cache.delete(KEY).await.expect("the delete");
    rig.cache
        .put(KEY, body.to_vec().into())
        .await
        .expect("the second put");
    assert_eq!(std::fs::read(rig.file(KEY)).unwrap(), body);
    (rig, guard, reader)
}

/// The ranges that a client may ask of an object of `old` bytes that is `new` bytes now: none,
/// the middle, one that crosses the old end, one that starts past the old end, one that starts
/// at the new end.
fn ranges(old: usize, new: usize) -> Vec<Option<ByteRange>> {
    let at = |offset: usize, len: usize| Some(ByteRange::new(offset as u64, len as u64));
    let mut ranges = vec![
        None,
        at(3, 7),
        at(old - 10, 20),
        at(old + 5, 10),
        at(new, 4),
    ];
    ranges.retain(|range| range.is_none_or(|range| range.offset() <= new as u64));
    ranges
}

/// Asserts that the reader answers every read of `KEY`, whole and by range, with the bytes the
/// upstream has now, each from the upstream (none is a hit: the index describes the object that
/// was deleted), and that a HEAD says what the upstream says; and that the reader writes nothing.
async fn reads_the_new_object(name: &str, new: Vec<u8>) {
    let (rig, guard, reader) = replaced_by(name, &new).await;
    let root = &rig.config.path;
    age(root);
    let before = tree(root);
    let mut asked = 0;
    for range in ranges(OLD, new.len()) {
        let want = rig
            .tamper
            .get(KEY, range)
            .await
            .expect("the upstream's bytes");
        let got = reader.get(KEY, range).await;
        assert_eq!(got.as_deref().ok(), Some(&want[..]), "{range:?}");
        asked += 1;
    }
    assert_eq!(read(&reader.counters().hits), 0);
    assert_eq!(read(&reader.counters().misses), asked);
    let head = reader.head(KEY).await.expect("a head");
    let want = rig.tamper.head(KEY).await.expect("the upstream's head");
    assert_eq!((head.size, head.tag), (want.size, want.tag));
    assert_eq!(tree(root), before);
    assert_eq!(reader.held(), (OLD as u64, 1));
    assert_eq!(reader.mode(), Mode::ReadOnly);
    assert_eq!(guard.mode(), Mode::ReadWrite);
}

/// Asserts (T13) that an object the holder deleted and put again with a larger body is read from
/// the upstream by a read-only cache that scanned the smaller one: the whole read has all the new
/// bytes (not the first `OLD`), a range that crosses the old end has its whole length, a range
/// that starts past the old end is answered and not refused as past the end. Catches an open
/// that checks only that the file is at least as long as the window of the old size.
#[tokio::test]
async fn a_larger_object_put_again_is_read_whole_from_the_upstream() {
    reads_the_new_object("tscache-stale-larger", pattern(OLD + 25)).await;
}

/// Asserts (T13) the same for a body of the same size and other bytes: the reader does not
/// pair the tag and size it scanned with the new bytes. Catches a hit that trusts the index
/// when the length of the file is the one it expects.
#[tokio::test]
async fn an_object_put_again_with_the_same_size_is_read_from_the_upstream() {
    reads_the_new_object("tscache-stale-same", other(OLD)).await;
}

/// Asserts (T13) the same for a smaller body.
#[tokio::test]
async fn a_smaller_object_put_again_is_read_from_the_upstream() {
    reads_the_new_object("tscache-stale-smaller", pattern(OLD - 15)).await;
}

/// Asserts (T13) that the reader goes on serving the object it scanned while the holder has not
/// changed it: the hit is local, there is no read of the upstream, and the HEAD is answered by
/// the index. The check of the sidecar is no reason to give up the hits of a directory that has
/// not changed.
#[tokio::test]
async fn an_object_the_holder_did_not_change_is_still_a_local_hit() {
    let rig = Rig::writing("tscache-stale-same-object");
    rig.cache.put(KEY, pattern(OLD).into()).await.unwrap();
    let _guard = rig.locked();
    let reader = rig.locked();
    let (gets, heads) = (rig.calls(Op::Get), rig.calls(Op::Head));
    let range = Some(ByteRange::new(5, 10));
    assert_eq!(reader.get(KEY, None).await.unwrap(), pattern(OLD));
    assert_eq!(reader.get(KEY, range).await.unwrap(), pattern(OLD)[5..15]);
    assert_eq!(reader.head(KEY).await.unwrap().size, OLD as u64);
    assert_eq!(read(&reader.counters().hits), 2);
    assert_eq!(read(&reader.counters().misses), 0);
    assert_eq!((rig.calls(Op::Get), rig.calls(Op::Head)), (gets, heads));
}

/// Asserts (T13) that a key named by its content, which a delete and a put cannot change, is
/// still a local hit in a read-only cache after the holder deleted and put it again: the name
/// says the bytes.
#[tokio::test]
async fn a_key_named_by_its_content_is_unaffected_by_a_delete_and_put() {
    let body = pattern(OLD);
    let key = format!("cas/l/{}", digest(&body));
    let rig = Rig::writing("tscache-stale-content");
    rig.cache.put(&key, body.clone().into()).await.unwrap();
    let _guard = rig.locked();
    let reader = rig.locked();
    rig.cache.delete(&key).await.unwrap();
    rig.cache.put(&key, body.clone().into()).await.unwrap();
    assert!(meta_path(&rig.config.path, &name_of(&key).hex).is_file());
    let gets = rig.calls(Op::Get);
    assert_eq!(reader.get(&key, None).await.unwrap(), body);
    assert_eq!(read(&reader.counters().hits), 1);
    assert_eq!(rig.calls(Op::Get), gets);
}

/// Asserts (T13) that a read-write cache is not changed by the check: after a delete and a put
/// of a larger body through the cache itself it reads the new object whole as a hit, as the
/// holder always did.
#[tokio::test]
async fn a_read_write_cache_serves_the_object_it_put_again() {
    let rig = Rig::writing("tscache-stale-rw");
    let holder = rig.locked();
    let new = pattern(OLD + 25);
    holder.put(KEY, pattern(OLD).into()).await.unwrap();
    holder.delete(KEY).await.unwrap();
    holder.put(KEY, new.clone().into()).await.unwrap();
    assert_eq!(holder.mode(), Mode::ReadWrite);
    assert_eq!(holder.get(KEY, None).await.unwrap(), new);
    assert_eq!(read(&holder.counters().hits), 1);
}

/// Asserts (T13) that an object whose sidecar the holder removed (it is deleting the object, or
/// has not renamed the new sidecar in yet) is a miss for the reader, in a read and in a HEAD,
/// answered by the upstream, and that the reader keeps its entry and removes nothing.
#[tokio::test]
async fn an_object_whose_sidecar_is_gone_is_a_miss_that_drops_nothing() {
    let rig = Rig::writing("tscache-stale-no-sidecar");
    rig.cache.put(KEY, pattern(OLD).into()).await.unwrap();
    let guard = rig.locked();
    let reader = rig.locked();
    std::fs::remove_file(meta_path(&rig.config.path, &name_of(KEY).hex)).unwrap();
    age(&rig.config.path);
    let before = tree(&rig.config.path);
    let heads = rig.calls(Op::Head);
    assert_eq!(reader.get(KEY, None).await.unwrap(), pattern(OLD));
    assert_eq!(reader.head(KEY).await.unwrap().size, OLD as u64);
    assert_eq!(read(&reader.counters().hits), 0);
    assert_eq!(read(&reader.counters().misses), 1);
    assert!(rig.calls(Op::Head) > heads);
    assert_eq!(tree(&rig.config.path), before);
    assert_eq!(reader.held(), (OLD as u64, 1));
    assert_eq!(guard.mode(), Mode::ReadWrite);
}
