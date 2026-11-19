//! The evictor lock `packs-v2/.lock` (section 7): exactly one holder per cache directory.
//!
//! Section 14 row: "No eviction before the grace window; an entry locked by a second process is
//! never evicted" (the lock part: a second evictor is refused).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::io::ErrorKind;
use std::time::Duration;

use support::scratch;
use tierstore_evict::{EVICTOR_LOCK, EvictorLock, LockError, Pause, SleepPause};

#[test]
fn a_second_evictor_of_one_cache_is_refused_until_the_first_is_dropped() {
    let dir = scratch("evictor-lock");
    let first = EvictorLock::acquire(dir.path()).unwrap();
    assert_eq!(first.path(), dir.path().join("packs-v2").join(EVICTOR_LOCK));

    // flock is per open file, so a second open in this process stands for a second process.
    let e = EvictorLock::acquire(dir.path()).unwrap_err();
    match &e {
        LockError::Locked { path, source } => {
            assert_eq!(path, first.path());
            assert_eq!(source.kind(), ErrorKind::WouldBlock);
        }
        other => panic!("not Locked: {other:?}"),
    }
    let LockError::Locked { source, .. } = &e else {
        unreachable!()
    };
    assert_eq!(
        e.to_string(),
        format!("locking {}: {source}", first.path().display())
    );

    drop(first);
    let again = EvictorLock::acquire(dir.path()).unwrap();
    assert!(again.path().exists());
}

#[test]
fn a_cache_directory_that_is_a_file_is_an_error_naming_the_directory() {
    let dir = scratch("evictor-lock-file");
    let file = dir.path().join("cache");
    std::fs::write(&file, b"").unwrap();
    let e = EvictorLock::acquire(&file).unwrap_err();
    assert!(
        matches!(&e, LockError::Io { op: "making", path, .. } if *path == file.join("packs-v2"))
    );
    assert!(
        e.to_string()
            .starts_with(&format!("making {}: ", file.join("packs-v2").display()))
    );
}

#[test]
fn a_lock_path_that_cannot_be_opened_is_an_error_naming_it() {
    let dir = scratch("evictor-lock-dir");
    let lock = dir.path().join("packs-v2").join(EVICTOR_LOCK);
    std::fs::create_dir_all(&lock).unwrap();
    let e = EvictorLock::acquire(dir.path()).unwrap_err();
    assert!(matches!(&e, LockError::Io { op: "opening", path, .. } if *path == lock));
    assert!(std::error::Error::source(&e).is_some());
}

#[test]
fn the_production_pause_backs_off_from_one_to_sixty_four_milliseconds() {
    let backoffs: Vec<u64> = (0..9)
        .map(|a| SleepPause::backoff(a).as_millis() as u64)
        .collect();
    assert_eq!(backoffs, [1, 2, 4, 8, 16, 32, 64, 64, 64]);
    assert_eq!(SleepPause::backoff(u32::MAX), Duration::from_millis(64));
    // It only sleeps; no outcome depends on how long.
    SleepPause.pause(0);
}
