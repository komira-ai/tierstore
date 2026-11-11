//! Holds: the pins a hold takes on the blobs it asked for, what its release and its end of time
//! do, and the refusals. The upstream is held at a gate so that each step is made while a fill is
//! in the middle, and the clock is the test's.

use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;

use super::expand::Bound;
use super::holds::{HoldError, Released, Spent};
use super::locked;
use super::metrics::Walked;
use super::pack_testing::distinct;
use super::pin_set::{HOLD_MOUNT, PinsFile};
use super::prefetch::Prefetched;
use super::prefetch_tests::{Stored, in_table, rig, stored, waiting};
use super::source::Blob;
use super::testing::{Rig, digest, until};
use crate::tree_manifest::TreeDigest;
use crate::{Backend, Op};

/// A tree digest for the tests: any 64 hex digits.
pub(super) fn tree(n: u8) -> String {
    digest(&[n])
}

/// The owners that pin the blob `hex` now.
pub(super) fn pinners(rig: &Rig, hex: &str) -> Vec<(String, String)> {
    let index = locked(&rig.cache.inner.index);
    let owners = index.pins.owners_of(hex);
    owners.into_iter().map(|o| (o.mount, o.tree)).collect()
}

/// The owner of the pins of `tree` of the hold `id`.
pub(super) fn held_by(id: &str, tree: &str) -> (String, String) {
    (HOLD_MOUNT.to_owned(), format!("{id}/{tree}"))
}

/// Asserts that a hold pins the blobs it asked for as they land in the cache, the one a worker
/// fills when the fill ends and the one the cache held already at once, that the release says what
/// became of each, and that it takes the pins away and leaves the blobs in the cache. The open
/// holds are a gauge.
#[tokio::test]
async fn hold_pins_filled_blobs_and_release_unpins() {
    let rig = rig("thold-pins", 1);
    let (a, b) = (stored(&rig, 1, 3000).await, stored(&rig, 2, 2000).await);
    let t = tree(1);
    assert_eq!(
        rig.cache.open_hold("job-1", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    assert!(
        rig.cache
            .metrics()
            .contains("tierstore_cache_holds_open 1\n")
    );
    assert_eq!(rig.cache.fill_blob(&b.blob()).await, Ok(()));
    let gate = rig.tamper.hold_key(&a.key);

    assert_eq!(
        rig.cache.hold_blob("job-1", &t, &a.blob()),
        Ok(Prefetched::Queued)
    );
    assert_eq!(
        rig.cache.hold_blob("job-1", &t, &b.blob()),
        Ok(Prefetched::Present)
    );
    until("the worker is at a", || rig.calls_on(Op::Get, &a.key) == 1).await;
    // The blob the cache held is pinned at once; the one being filled is not in the cache yet.
    assert_eq!(rig.cache.pinned_bytes(), 2000);

    gate.add_permits(64);
    until("a is pinned when it lands", || {
        rig.cache.pinned_bytes() == 5000
    })
    .await;
    assert_eq!(pinners(&rig, &a.sha), vec![held_by("job-1", &t)]);
    let released = rig.cache.release_hold("job-1");
    let want = Released {
        trees: 1,
        filled: 1,
        present: 1,
        ..Released::default()
    };
    assert_eq!(released, Ok(want));
    assert_eq!(rig.cache.pinned_bytes(), 0);
    assert_eq!(rig.cache.held(), (5000, 2));
    assert!(
        rig.cache
            .metrics()
            .contains("tierstore_cache_holds_open 0\n")
    );
}

/// Asserts that a blob asked for by two trees of one hold and by a second hold is fetched once,
/// that the tree sent twice adds nothing, that the blob is pinned for each of the three owners
/// and its bytes are charged once, and that the answer of each release counts the blob under the
/// hold's own state.
#[tokio::test]
async fn duplicate_trees_fetch_each_blob_once() {
    let rig = rig("thold-dup", 1);
    let a = stored(&rig, 1, 3000).await;
    let (t1, t2) = (tree(1), tree(2));
    let gate = rig.tamper.hold_key(&a.key);
    assert_eq!(
        rig.cache.open_hold("h1", None, &[t1.clone(), t2.clone()]),
        Ok(2)
    );
    assert_eq!(
        rig.cache.open_hold("h1", None, std::slice::from_ref(&t1)),
        Ok(2)
    );
    assert_eq!(
        rig.cache.open_hold("h2", None, std::slice::from_ref(&t1)),
        Ok(1)
    );

    assert_eq!(
        rig.cache.hold_blob("h1", &t1, &a.blob()),
        Ok(Prefetched::Queued)
    );
    assert_eq!(
        rig.cache.hold_blob("h1", &t2, &a.blob()),
        Ok(Prefetched::Joined)
    );
    assert_eq!(
        rig.cache.hold_blob("h1", &t1, &a.blob()),
        Ok(Prefetched::Joined)
    );
    assert_eq!(
        rig.cache.hold_blob("h2", &t1, &a.blob()),
        Ok(Prefetched::Joined)
    );
    gate.add_permits(64);
    until("a is pinned", || rig.cache.pinned_bytes() == 3000).await;
    until("the table is empty", || in_table(&rig) == 0).await;

    assert_eq!(rig.calls_on(Op::Get, &a.key), 1);
    let mut owners = pinners(&rig, &a.sha);
    owners.sort();
    let mut want = vec![held_by("h1", &t1), held_by("h1", &t2), held_by("h2", &t1)];
    want.sort();
    assert_eq!(owners, want);
    let filled = |trees| Released {
        trees,
        filled: 1,
        ..Released::default()
    };
    assert_eq!(rig.cache.release_hold("h1"), Ok(filled(2)));
    assert_eq!(rig.cache.release_hold("h2"), Ok(filled(1)));
    assert_eq!(rig.cache.pinned_bytes(), 0);
}

/// Asserts that releasing one hold of a blob leaves the pin of the other hold, and that the
/// release of the second ends the last pin.
#[tokio::test]
async fn release_of_one_hold_keeps_the_pin_of_another() {
    let rig = rig("thold-two", 1);
    let a = stored(&rig, 1, 3000).await;
    let t = tree(1);
    assert_eq!(rig.cache.fill_blob(&a.blob()).await, Ok(()));
    for id in ["h1", "h2"] {
        assert_eq!(
            rig.cache.open_hold(id, None, std::slice::from_ref(&t)),
            Ok(1)
        );
        assert_eq!(
            rig.cache.hold_blob(id, &t, &a.blob()),
            Ok(Prefetched::Present)
        );
    }
    assert_eq!(pinners(&rig, &a.sha).len(), 2);
    assert_eq!(rig.cache.pinned_bytes(), 3000);

    let present = Released {
        trees: 1,
        present: 1,
        ..Released::default()
    };
    assert_eq!(rig.cache.release_hold("h1"), Ok(present));
    assert_eq!(pinners(&rig, &a.sha), vec![held_by("h2", &t)]);
    assert_eq!(rig.cache.pinned_bytes(), 3000);
    assert_eq!(rig.cache.release_hold("h2"), Ok(present));
    assert_eq!(rig.cache.pinned_bytes(), 0);
}

/// Asserts that a pin past `pin_limit` is refused and the blob kept by recency only: the first
/// blob fills and is pinned, the second fills and is in the cache but not pinned, and the release
/// counts both as filled.
#[tokio::test]
async fn pin_past_limit_is_kept_by_recency_only() {
    let rig = Rig::with("thold-limit", |config| {
        config.prefetch.concurrency = 1;
        config.pin_limit = 3000;
    });
    let (a, b) = (stored(&rig, 1, 3000).await, stored(&rig, 2, 2000).await);
    let t = tree(1);
    assert_eq!(
        rig.cache.open_hold("job", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &a.blob()),
        Ok(Prefetched::Queued)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &b.blob()),
        Ok(Prefetched::Queued)
    );
    until("both are filled", || {
        rig.cache.held() == (5000, 2) && in_table(&rig) == 0
    })
    .await;

    assert_eq!(rig.cache.pinned_bytes(), 3000);
    assert_eq!(pinners(&rig, &a.sha).len(), 1);
    assert!(pinners(&rig, &b.sha).is_empty());
    let released = rig.cache.release_hold("job");
    assert_eq!(released.map(|r| r.filled), Ok(2));
    assert_eq!(rig.cache.pinned_bytes(), 0);
}

/// Asserts the end of a hold's time on the cache's clock, noticed at a tick: a hold with no time
/// given lasts `hold_max`, a time longer than `hold_max` is cut to it, a shorter time is kept, a
/// hold opened again keeps its time and gains its trees; a hold that ends at its time unpins, drops
/// the blob it queued that nobody else wants, and is gone for a release, and one that has not yet
/// reached its time is still open the millisecond before.
#[tokio::test]
async fn hold_expires_at_hold_max() {
    let rig = Rig::with("thold-expire", |config| {
        config.prefetch.concurrency = 1;
        config.prefetch.hold_max = Duration::from_secs(60);
    });
    let (a, b, c) = (
        stored(&rig, 1, 300).await,
        stored(&rig, 2, 310).await,
        stored(&rig, 3, 320).await,
    );
    let t = tree(1);
    let gate = rig.tamper.hold_key(&a.key);
    assert_eq!(rig.cache.prefetch(&a.blob()), Prefetched::Queued);
    until("the worker is at a", || rig.calls_on(Op::Get, &a.key) == 1).await;
    assert_eq!(rig.cache.fill_blob(&c.blob()).await, Ok(()));

    let trees = [t.clone()];
    assert_eq!(rig.cache.open_hold("short", Some(10_000), &trees), Ok(1));
    assert_eq!(rig.cache.open_hold("long", Some(3_600_000), &trees), Ok(1));
    assert_eq!(rig.cache.open_hold("default", None, &trees), Ok(1));
    assert_eq!(
        rig.cache.hold_blob("short", &t, &b.blob()),
        Ok(Prefetched::Queued)
    );
    assert_eq!(
        rig.cache.hold_blob("short", &t, &c.blob()),
        Ok(Prefetched::Present)
    );
    assert_eq!(waiting(&rig), 1);
    rig.clock.advance(5_000);
    let more = [tree(2)];
    assert_eq!(rig.cache.open_hold("short", Some(3_600_000), &more), Ok(2));

    let open = |rig: &Rig, n: u64| {
        let line = format!("tierstore_cache_holds_open {n}\n");
        assert!(rig.cache.metrics().contains(&line), "{line}");
    };
    rig.clock.advance(4_999);
    assert!(rig.cache.tick().is_ok());
    open(&rig, 3);
    assert_eq!(rig.cache.pinned_bytes(), 320);

    rig.clock.advance(1);
    assert!(rig.cache.tick().is_ok());
    open(&rig, 2);
    assert_eq!(rig.cache.pinned_bytes(), 0);
    assert_eq!(waiting(&rig), 0);
    assert_eq!(
        rig.cache.release_hold("short"),
        Err(HoldError::NoSuchHold("short".to_owned()))
    );

    rig.clock.advance(49_999);
    assert!(rig.cache.tick().is_ok());
    open(&rig, 2);
    rig.clock.advance(1);
    assert!(rig.cache.tick().is_ok());
    open(&rig, 0);
    for id in ["long", "default"] {
        assert_eq!(
            rig.cache.release_hold(id),
            Err(HoldError::NoSuchHold(id.to_owned()))
        );
    }
    gate.add_permits(64);
    until("a is kept", || rig.cache.held() == (620, 2)).await;
    assert_eq!(rig.calls_on(Op::Get, &b.key), 0);
}

/// Asserts what a release does to blobs that wait: with the only worker held at a blob, a blob
/// only this hold queued is dropped from the queue and the table without a request, a blob a
/// second hold also queued stays queued, and the fill that is running is left alone; the answer
/// counts the dropped one and the two left. The two that stay are filled, and the second hold pins
/// the one it asked for.
#[tokio::test]
async fn release_drops_queued_blobs_nobody_else_wants_and_leaves_running_fills() {
    let rig = rig("thold-release", 1);
    let (a, b, c) = (
        stored(&rig, 1, 3000).await,
        stored(&rig, 2, 2000).await,
        stored(&rig, 3, 1000).await,
    );
    let t = tree(1);
    let gate = rig.tamper.hold_key(&a.key);
    for id in ["h1", "h2"] {
        assert_eq!(
            rig.cache.open_hold(id, None, std::slice::from_ref(&t)),
            Ok(1)
        );
    }
    assert_eq!(
        rig.cache.hold_blob("h1", &t, &a.blob()),
        Ok(Prefetched::Queued)
    );
    until("the worker is at a", || rig.calls_on(Op::Get, &a.key) == 1).await;
    assert_eq!(
        rig.cache.hold_blob("h1", &t, &b.blob()),
        Ok(Prefetched::Queued)
    );
    assert_eq!(
        rig.cache.hold_blob("h1", &t, &c.blob()),
        Ok(Prefetched::Queued)
    );
    assert_eq!(
        rig.cache.hold_blob("h2", &t, &c.blob()),
        Ok(Prefetched::Joined)
    );
    assert_eq!((waiting(&rig), in_table(&rig)), (2, 3));

    let released = rig.cache.release_hold("h1");
    let want = Released {
        trees: 1,
        dropped: 1,
        pending: 2,
        ..Released::default()
    };
    assert_eq!(released, Ok(want));
    assert_eq!((waiting(&rig), in_table(&rig)), (1, 2));
    assert_eq!(rig.calls_on(Op::Get, &b.key), 0);

    gate.add_permits(64);
    until("a and c are kept", || {
        rig.cache.held() == (4000, 2) && in_table(&rig) == 0
    })
    .await;
    assert_eq!(rig.calls_on(Op::Get, &b.key), 0);
    assert_eq!(pinners(&rig, &c.sha), vec![held_by("h2", &t)]);
    assert!(pinners(&rig, &a.sha).is_empty());
    let filled = Released {
        trees: 1,
        filled: 1,
        ..Released::default()
    };
    assert_eq!(rig.cache.release_hold("h2"), Ok(filled));
    assert_eq!(rig.cache.pinned_bytes(), 0);
}

/// Asserts that a hold is told what the queue does with a blob when its turn comes: a blob that
/// waited longer than `prefetch_queue_ttl` is dropped for the hold, and one the cache holds by
/// then is present for it and pinned, with no request for either.
#[tokio::test]
async fn a_hold_is_told_when_the_queue_drops_its_blob() {
    let rig = Rig::with("thold-queue-drops", |config| {
        config.prefetch.concurrency = 1;
        config.prefetch.ttl = Duration::from_secs(10);
    });
    let (a, old, kept) = (
        stored(&rig, 1, 300).await,
        stored(&rig, 2, 310).await,
        stored(&rig, 3, 320).await,
    );
    let t = tree(1);
    let gate = rig.tamper.hold_key(&a.key);
    assert_eq!(rig.cache.prefetch(&a.blob()), Prefetched::Queued);
    until("the worker is at a", || rig.calls_on(Op::Get, &a.key) == 1).await;
    assert_eq!(
        rig.cache.open_hold("job", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &old.blob()),
        Ok(Prefetched::Queued)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &kept.blob()),
        Ok(Prefetched::Queued)
    );
    let entry = (320, "tag".to_owned(), 0);
    locked(&rig.cache.inner.index).insert(&super::layout::name_of(&kept.key).hex, entry, None);
    rig.clock.advance(10_001);

    gate.add_permits(64);
    until("the table is empty", || in_table(&rig) == 0).await;
    assert_eq!(rig.calls_on(Op::Get, &old.key), 0);
    assert_eq!(rig.calls_on(Op::Get, &kept.key), 0);
    assert_eq!(rig.cache.pinned_bytes(), 320);
    let want = Released {
        trees: 1,
        present: 1,
        dropped: 1,
        ..Released::default()
    };
    assert_eq!(rig.cache.release_hold("job"), Ok(want));
}

/// A blob of `len` bytes that the upstream does not have.
fn missing(seed: u8, len: usize) -> Stored {
    let sha = digest(&[seed, seed]);
    Stored {
        key: format!("cas/l/{sha}"),
        sha,
        body: vec![0; len],
    }
}

/// Asserts that a blob whose fill ends without it is failed for the hold (and pinned for no one),
/// and that a blob the queue turns away (here, larger than the whole budget) is dropped for it.
#[tokio::test]
async fn a_blob_that_fails_or_is_turned_away_is_counted_for_the_hold() {
    let rig = rig("thold-failed", 1);
    let t = tree(1);
    assert_eq!(
        rig.cache.open_hold("job", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    let gone = missing(1, 100);
    let huge = missing(2, 0);
    let big = Blob {
        size: rig.config.budget + 1,
        ..huge.blob()
    };

    assert_eq!(
        rig.cache.hold_blob("job", &t, &gone.blob()),
        Ok(Prefetched::Queued)
    );
    until("the fill ended", || in_table(&rig) == 0).await;
    assert_eq!(
        rig.cache.hold_blob("job", &t, &big),
        Ok(Prefetched::TooLarge)
    );
    assert_eq!(rig.cache.pinned_bytes(), 0);
    let want = Released {
        trees: 1,
        failed: 1,
        dropped: 1,
        ..Released::default()
    };
    assert_eq!(rig.cache.release_hold("job"), Ok(want));
}

/// Asserts the refusals, each by its exact message: a tree that is not a digest opens nothing, the
/// hold past `hold_max_count` is turned away while one already open may be opened again, a blob of
/// a hold that is not open, of a tree it is not for, or named by what is not a digest queues
/// nothing, a release of a hold that is not open (also one released already) is refused, and a
/// cache that does not hold the lock opens no hold.
#[tokio::test]
async fn holds_refuse_what_they_cannot_keep() {
    let rig = Rig::with("thold-refuse", |config| config.prefetch.hold_max_count = 2);
    let a = stored(&rig, 1, 300).await;
    let t = tree(1);
    let bad = HoldError::NotADigest("XYZ".to_owned());
    assert_eq!(
        rig.cache.open_hold("a", None, &["XYZ".to_owned()]),
        Err(bad)
    );
    assert_eq!(
        rig.cache.open_hold("a", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    assert_eq!(rig.cache.open_hold("b", None, &[]), Ok(0));
    let many = rig
        .cache
        .open_hold("c", None, std::slice::from_ref(&t))
        .unwrap_err();
    assert_eq!(many, HoldError::TooMany(2));
    assert_eq!(many.to_string(), "2 holds are open already");
    assert_eq!(
        rig.cache.open_hold("a", None, std::slice::from_ref(&t)),
        Ok(1)
    );

    let none = rig.cache.hold_blob("nope", &t, &a.blob()).unwrap_err();
    assert_eq!(none, HoldError::NoSuchHold("nope".to_owned()));
    assert_eq!(none.to_string(), "no hold \"nope\" is open");
    let other = tree(2);
    let wrong = rig.cache.hold_blob("a", &other, &a.blob()).unwrap_err();
    assert_eq!(
        wrong.to_string(),
        format!("the hold \"a\" is not for the tree \"{other}\"")
    );
    let unnamed = Blob {
        sha256: "ABC",
        ..a.blob()
    };
    let error = rig.cache.hold_blob("a", &t, &unnamed).unwrap_err();
    assert_eq!(
        error.to_string(),
        "\"ABC\" is not the lower-case hex of a SHA-256"
    );
    assert_eq!((waiting(&rig), in_table(&rig)), (0, 0));
    assert_eq!(rig.calls_on(Op::Get, &a.key), 0);

    assert_eq!(rig.cache.release_hold("a").map(|r| r.trees), Ok(1));
    assert_eq!(
        rig.cache.release_hold("a"),
        Err(HoldError::NoSuchHold("a".to_owned()))
    );

    let _holder = rig.locked();
    let waiting_cache = rig.locked();
    let refused = waiting_cache.open_hold("a", None, &[t]).unwrap_err();
    assert_eq!(refused, HoldError::ReadOnly);
    assert_eq!(
        refused.to_string(),
        "this cache does not hold the lock of its directory"
    );
}

/// Asserts that the pins of a hold are the cache's memory only: `pins_live` of a mount leaves them
/// while it takes away the pins of an attachment it does not list, the pins file has the
/// attachment's pins and none of the hold's, and a cache opened over the directory again (a
/// restart) has no hold and no pin of one, so a pin never outlives its hold.
#[tokio::test]
async fn hold_pins_are_not_in_the_pins_file_and_pins_live_leaves_them() {
    let rig = rig("thold-file", 1);
    let a = stored(&rig, 1, 3000).await;
    let t = tree(1);
    assert_eq!(rig.cache.fill_blob(&a.blob()).await, Ok(()));
    assert_eq!(
        rig.cache.open_hold("job", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &a.blob()),
        Ok(Prefetched::Present)
    );
    let sha = vec![a.sha.clone()];
    assert_eq!(rig.cache.pin("boot-1", "tree-x", &sha).unwrap().pinned, 1);
    assert_eq!(pinners(&rig, &a.sha).len(), 2);

    assert_eq!(rig.cache.pins_live("boot-1", &["tree-x".to_owned()]), Ok(0));
    assert_eq!(pinners(&rig, &a.sha).len(), 2);
    rig.cache.inner.save_pins(true);
    let text = std::fs::read(rig.config.path.join("pins")).unwrap();
    let file: PinsFile = serde_json::from_slice(&text).unwrap();
    let trees: Vec<(&str, &str)> = file
        .trees
        .iter()
        .map(|pins| (pins.mount.as_str(), pins.tree.as_str()))
        .collect();
    assert_eq!(trees, vec![("boot-1", "tree-x")]);

    assert_eq!(rig.cache.pins_live("boot-1", &[]), Ok(1));
    assert_eq!(pinners(&rig, &a.sha), vec![held_by("job", &t)]);
    assert_eq!(rig.cache.pinned_bytes(), 3000);
    rig.cache.inner.save_pins(true);
    let text = std::fs::read(rig.config.path.join("pins")).unwrap();
    assert_eq!(
        serde_json::from_slice::<PinsFile>(&text)
            .unwrap()
            .trees
            .len(),
        0
    );

    let restarted = rig.reopen();
    assert_eq!(restarted.pinned_bytes(), 0);
    assert_eq!(
        restarted.release_hold("job"),
        Err(HoldError::NoSuchHold("job".to_owned()))
    );
}

/// Asserts that no caller can name the mount id the holds pin under: `pin`, `unpin` and
/// `pins_live` refuse it with the exact words and change nothing, so a hold's pins are the hold's
/// to end and no request makes pins that nothing ends.
#[tokio::test]
async fn the_mount_of_the_holds_cannot_be_named_by_a_caller() {
    let rig = rig("thold-reserved", 1);
    let a = stored(&rig, 1, 300).await;
    let t = tree(1);
    assert_eq!(rig.cache.fill_blob(&a.blob()).await, Ok(()));
    assert_eq!(
        rig.cache.open_hold("job", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &a.blob()),
        Ok(Prefetched::Present)
    );
    let words = "the mount id \"hold\" is reserved for holds";
    let sha = vec![a.sha.clone()];
    let owner = format!("job/{t}");

    let pin = rig.cache.pin(HOLD_MOUNT, "x", &sha).unwrap_err();
    assert_eq!(pin.to_string(), words);
    let unpin = rig.cache.unpin(HOLD_MOUNT, &owner).unwrap_err();
    assert_eq!(unpin.to_string(), words);
    let live = rig.cache.pins_live(HOLD_MOUNT, &[]).unwrap_err();
    assert_eq!(live.to_string(), words);
    assert_eq!(pinners(&rig, &a.sha), vec![held_by("job", &t)]);
}

/// Asserts that a blob a keep (the write-through of an S3 PUT) is committing, which a hold finds
/// in the table of the fills and joins, is pinned for the hold when the keep ends: the hold is
/// settled by the end of the flight, not by the fill task alone. The keep is held at its link.
#[tokio::test]
async fn a_blob_joined_to_a_keep_is_pinned_when_the_keep_commits() {
    let rig = Rig::with("thold-keep", |config| {
        config.write_through = true;
        config.prefetch.concurrency = 1;
    });
    let body = distinct(7, 3000);
    let sha = digest(&body);
    let key = format!("cas/l/{sha}");
    let t = tree(1);
    let gate = rig.disk.hold_links();
    let (cache, name, bytes) = (rig.cache.clone(), key.clone(), Bytes::from(body.clone()));
    let putting = tokio::spawn(async move {
        let part = stream::once(async move { Ok(bytes) }).boxed();
        cache.put_stream(&name, Some(3000), part).await
    });
    until("the link began", || {
        !rig.disk.linked.lock().unwrap().is_empty()
    })
    .await;

    assert_eq!(
        rig.cache.open_hold("job", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    let blob = Blob {
        sha256: &sha,
        size: 3000,
        from: &key,
        offset: 0,
    };
    assert_eq!(
        rig.cache.hold_blob("job", &t, &blob),
        Ok(Prefetched::Joined)
    );
    assert_eq!(rig.cache.pinned_bytes(), 0);

    gate.open();
    until("the blob is pinned when the keep ends", || {
        rig.cache.pinned_bytes() == 3000
    })
    .await;
    assert_eq!(pinners(&rig, &sha), vec![held_by("job", &t)]);
    assert!(putting.await.unwrap().is_ok());
    let want = Released {
        trees: 1,
        filled: 1,
        ..Released::default()
    };
    assert_eq!(rig.cache.release_hold("job"), Ok(want));
}

/// Asserts that a hold belongs to the key that opened it: another key that names the id finds no
/// hold, to add trees or to end it, and the hold is as it was; the key that opened it ends it. It
/// would catch a hold that any key may release or add to.
#[tokio::test]
async fn a_hold_is_the_key_that_opened_it_and_no_other_may_add_to_it_or_end_it() {
    let rig = rig("thold-owner", 1);
    let (t1, t2) = (tree(1), tree(2));
    let inner = &rig.cache.inner;
    let opened = inner.open_hold("job-42", "full", None, std::slice::from_ref(&t1));
    assert!(opened.is_ok());

    let absent = HoldError::NoSuchHold("job-42".to_owned());
    let more = inner.open_hold("job-42", "scoped", None, std::slice::from_ref(&t2));
    assert_eq!(more.err(), Some(absent));
    let absent = HoldError::NoSuchHold("job-42".to_owned());
    assert_eq!(rig.cache.release_hold_of("job-42", "scoped"), Err(absent));
    assert!(
        rig.cache
            .metrics()
            .contains("tierstore_cache_holds_open 1\n")
    );

    let own = inner.open_hold("job-42", "full", None, &[t1.clone(), t2.clone()]);
    assert_eq!(own.map(|opened| opened.added), Ok(vec![t2]));
    let ended = rig.cache.release_hold_of("job-42", "full");
    assert_eq!(ended.map(|released| released.trees), Ok(2));
}

/// Asserts that the generation of a hold tells it from a later hold of the same id: after a
/// release and a new hold of the name, the first hold's generation is not live, a blob handed over
/// for it is refused and queued nowhere, trees offered for it start no task and it is given none,
/// while the new hold takes all of these. It would catch an expansion that writes into the hold
/// that took the name of the one it was started for.
#[tokio::test]
async fn an_expansion_of_an_ended_hold_does_not_write_into_the_hold_that_took_its_name() {
    let rig = rig("thold-gen", 1);
    let blob = stored(&rig, 1, 300).await;
    let t = tree(1);
    let root = TreeDigest::from_hex(&t).unwrap();
    let inner = &rig.cache.inner;
    let names = [t.clone()];
    let first = inner.open_hold("h", "k", None, &names).unwrap().generation;
    assert_eq!(rig.cache.release_hold("h").map(|r| r.trees), Ok(1));
    let second = inner.open_hold("h", "k", None, &names).unwrap().generation;
    assert_ne!(first, second);

    assert!(!inner.hold_live(("h", first)));
    assert!(inner.hold_live(("h", second)));
    let absent = HoldError::NoSuchHold("h".to_owned());
    assert_eq!(inner.hold_blob(("h", first), &t, &blob.blob()), Err(absent));
    assert!(!inner.queue_walk(("h", first), &[root]));
    assert!(inner.next_tree(("h", first)).is_none());
    assert_eq!((waiting(&rig), in_table(&rig)), (0, 0));
    assert_eq!(rig.calls_on(Op::Get, &blob.key), 0);

    let handed = inner.hold_blob(("h", second), &t, &blob.blob());
    assert_eq!(handed, Ok(Prefetched::Queued));
}

/// Asserts that a hold has one task: the first trees offered start it, trees offered while it runs
/// start none and are taken by it in the order offered, and once it has found the list empty the
/// next trees start a new one. The bounds it has spent are kept for that one. It would catch a
/// hold that expands with a task per call.
#[tokio::test]
async fn a_hold_has_one_task_that_takes_its_trees_in_the_order_offered() {
    let rig = rig("thold-task", 1);
    let inner = &rig.cache.inner;
    let ids = [tree(1), tree(2), tree(3)];
    let [one, two, three] = ids.clone().map(|id| TreeDigest::from_hex(&id).unwrap());
    let generation = inner.open_hold("h", "k", None, &ids).unwrap().generation;
    let at = ("h", generation);

    assert!(!inner.queue_walk(at, &[]));
    assert!(inner.queue_walk(at, &[one]));
    assert!(!inner.queue_walk(at, &[two, three]));
    let spent = Spent {
        blobs: 4,
        bytes: 9,
        reached: Some(Bound::Blobs),
    };
    let (first, before) = inner.next_tree(at).unwrap();
    assert_eq!((first, before.blobs), (one, 0));
    inner.hold_walked(at, Walked::Cut, spent);
    let (second, kept) = inner.next_tree(at).unwrap();
    assert_eq!((second, kept.blobs, kept.bytes), (two, 4, 9));
    assert_eq!(kept.reached, Some(Bound::Blobs));
    assert_eq!(inner.next_tree(at).map(|(tree, _)| tree), Some(three));
    assert!(inner.next_tree(at).is_none());
    assert!(inner.queue_walk(at, &[one]));
}

/// Asserts that a hold is for at most `hydrate_max_trees` trees however many calls added them: a
/// call that would take it past the bound is refused with the count and adds none of its trees,
/// and a tree it is for already costs nothing. It would catch a bound that restarts with each call.
#[tokio::test]
async fn a_hold_takes_at_most_max_trees_over_all_its_calls() {
    let rig = Rig::with("thold-trees", |config| config.prefetch.max_trees = 2);
    let inner = &rig.cache.inner;
    let ids = [tree(1), tree(2), tree(3)];
    assert!(inner.open_hold("h", "k", None, &ids[..2]).is_ok());
    let over = inner.open_hold("h", "k", None, &ids[2..]).err().unwrap();
    assert_eq!(over, HoldError::TooManyTrees { given: 3, max: 2 });
    assert_eq!(over.to_string(), "3 trees in the hold, at most 2");
    let all = inner.open_hold("g", "k", None, &ids).err().unwrap();
    assert_eq!(all, HoldError::TooManyTrees { given: 3, max: 2 });
    assert!(inner.open_hold("h", "k", None, &ids[..1]).is_ok());
    assert_eq!(rig.cache.release_hold("h").map(|r| r.trees), Ok(2));
    assert_eq!(
        rig.cache.release_hold("g"),
        Err(HoldError::NoSuchHold("g".to_owned()))
    );
}
