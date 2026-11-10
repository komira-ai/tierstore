//! The cache of open cache files: what it keeps and what it closes.

use std::fs::File;
use std::sync::Arc;

use super::FdCache;

fn file() -> Arc<File> {
    Arc::new(File::open("/dev/null").unwrap())
}

/// Asserts that a file inserted is had again, as the same descriptor, and one never inserted is
/// not.
#[test]
fn a_file_inserted_is_had_again() {
    let cache = FdCache::new(4, 2);
    let kept = file();
    cache.insert(7, Arc::clone(&kept));
    assert!(Arc::ptr_eq(&cache.get(7).unwrap(), &kept));
    assert!(cache.get(8).is_none());
    assert_eq!(cache.len(), 1);
}

/// Asserts that a shard that is full closes its oldest file, and keeps the newer; that files of
/// another shard are not touched; and that inserting a file again keeps one place for it.
#[test]
fn a_full_shard_closes_its_oldest_file() {
    let cache = FdCache::new(2, 2);
    // Odd numbers share a shard, even ones the other.
    for ino in [1, 3, 5] {
        cache.insert(ino, file());
    }
    cache.insert(2, file());
    assert!(cache.get(1).is_none());
    assert!(cache.get(3).is_some() && cache.get(5).is_some());
    assert!(cache.get(2).is_some());
    cache.insert(3, file());
    cache.insert(7, file());
    // 3 was inserted twice, and the oldest is 3 (its first place), then 5.
    assert!(cache.get(3).is_none());
    assert!(cache.get(5).is_some() && cache.get(7).is_some());
    assert_eq!(cache.len(), 3);
}

/// Asserts that a cache made with no shards and no room still keeps one file.
#[test]
fn a_cache_of_no_size_keeps_one_file() {
    let cache = FdCache::new(0, 0);
    cache.insert(1, file());
    cache.insert(2, file());
    assert_eq!(cache.len(), 1);
    assert!(cache.get(2).is_some());
}
