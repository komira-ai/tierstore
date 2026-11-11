//! Holds: the pins a hold takes on the blobs it asked for, what its release and its end of time
//! do, and the refusals. The upstream is held at a gate so that each step is made while a fill is
//! in the middle, and the clock is the test's.

use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;

use super::holds::{HoldError, Released};
use super::locked;
use super::pack_testing::distinct;
use super::pin_set::{HOLD_MOUNT, PinsFile};
use super::prefetch::Prefetched;
use super::prefetch_tests::{Stored, in_table, rig, stored, waiting};
use super::source::Blob;
use super::testing::{Rig, digest, until};
use crate::{Backend, Op};

/// A tree digest for the tests: any 64 hex digits.
fn tree(n: u8) -> String {
    digest(&[n])
}

/// The owners that pin the blob `hex` now.
fn pinners(rig: &Rig, hex: &str) -> Vec<(String, String)> {
    let index = locked(&rig.cache.inner.index);
    let owners = index.pins.owners_of(hex);
    owners.into_iter().map(|o| (o.mount, o.tree)).collect()
}

/// The owner of the pins of `tree` of the hold `id`.
fn held_by(id: &str, tree: &str) -> (String, String) {
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

/// The names of the trees the pins file lists, each with the mount and the blobs, as the file has
/// them now.
fn file_of(rig: &Rig) -> serde_json::Value {
    rig.cache.inner.save_pins(true);
    let text = std::fs::read(rig.config.path.join("pins")).unwrap();
    serde_json::from_slice(&text).unwrap()
}

/// Asserts that the first pin of a mount for a tree a hold lists moves every pin of the hold on
/// that tree to the attachment, the blobs the request did not name included: the owners of each
/// blob are then the attachment alone, the pinned bytes do not change, the hold counts the
/// handover once however many pins follow, and the pins file lists the attachment's tree with both
/// blobs and no tree of the hold.
#[tokio::test]
async fn attach_moves_the_holds_pins_to_the_attachment() {
    let rig = rig("thold-handover", 1);
    let (a, b) = (stored(&rig, 1, 3000).await, stored(&rig, 2, 2000).await);
    let t = tree(1);
    for blob in [&a, &b] {
        assert_eq!(rig.cache.fill_blob(&blob.blob()).await, Ok(()));
    }
    assert_eq!(
        rig.cache.open_hold("job", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    for blob in [&a, &b] {
        assert_eq!(
            rig.cache.hold_blob("job", &t, &blob.blob()),
            Ok(Prefetched::Present)
        );
    }
    assert_eq!(pinners(&rig, &b.sha), vec![held_by("job", &t)]);
    assert!(
        rig.cache
            .metrics()
            .contains("tierstore_cache_hold_handovers_total 0\n")
    );

    let attached = (String::from("boot-1"), t.clone());
    let answer = rig
        .cache
        .pin("boot-1", &t, std::slice::from_ref(&a.sha))
        .unwrap();
    assert_eq!(answer.pinned, 1);
    assert_eq!(pinners(&rig, &a.sha), vec![attached.clone()]);
    assert_eq!(pinners(&rig, &b.sha), vec![attached.clone()]);
    assert_eq!(rig.cache.pinned_bytes(), 5000);
    assert_eq!(
        rig.cache
            .pin("boot-1", &t, std::slice::from_ref(&b.sha))
            .unwrap()
            .pinned,
        1
    );
    assert!(
        rig.cache
            .metrics()
            .contains("tierstore_cache_hold_handovers_total 1\n")
    );

    let mut blobs = vec![a.sha.clone(), b.sha.clone()];
    blobs.sort();
    let want = serde_json::json!({"trees": [{"mount": "boot-1", "tree": t, "blobs": blobs}]});
    assert_eq!(file_of(&rig), want);
}

/// Asserts that a blob whose fill ends after the tree was attached is pinned for each attachment
/// of the tree, not for the hold, and that a second mount that attaches the tree is pinned for
/// too.
#[tokio::test]
async fn blob_filled_after_attach_is_pinned_for_the_attachment() {
    let rig = rig("thold-after", 1);
    let a = stored(&rig, 1, 3000).await;
    let t = tree(1);
    let gate = rig.tamper.hold_key(&a.key);
    assert_eq!(
        rig.cache.open_hold("job", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &a.blob()),
        Ok(Prefetched::Queued)
    );
    until("the worker is at a", || rig.calls_on(Op::Get, &a.key) == 1).await;
    assert_eq!(rig.cache.pin("boot-1", &t, &[]).unwrap().pinned, 0);
    assert_eq!(rig.cache.pin("boot-2", &t, &[]).unwrap().pinned, 0);

    gate.add_permits(64);
    until("a is pinned when it lands", || {
        rig.cache.pinned_bytes() == 3000
    })
    .await;
    let mut owners = pinners(&rig, &a.sha);
    owners.sort();
    let want = vec![
        ("boot-1".to_owned(), t.clone()),
        ("boot-2".to_owned(), t.clone()),
    ];
    assert_eq!(owners, want);
    let filled = Released {
        trees: 1,
        filled: 1,
        ..Released::default()
    };
    assert_eq!(rig.cache.release_hold("job"), Ok(filled));
}

/// Asserts that the release of a hold after the handover leaves the attachment's pins, that the
/// unpin of the tree (the detach) then ends them, and that a fill that ends after that is pinned
/// for nobody: the hold gave the tree up and the attachment is gone.
#[tokio::test]
async fn release_after_attach_leaves_the_attachments_pins() {
    let rig = rig("thold-release", 1);
    let (a, b) = (stored(&rig, 1, 3000).await, stored(&rig, 2, 2000).await);
    let t = tree(1);
    assert_eq!(rig.cache.fill_blob(&a.blob()).await, Ok(()));
    let gate = rig.tamper.hold_key(&b.key);
    assert_eq!(
        rig.cache.open_hold("job", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &a.blob()),
        Ok(Prefetched::Present)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &b.blob()),
        Ok(Prefetched::Queued)
    );
    assert_eq!(
        rig.cache
            .pin("boot-1", &t, std::slice::from_ref(&a.sha))
            .unwrap()
            .pinned,
        1
    );

    // The release leaves the fill that runs, and the attachment's pin of a.
    let released = Released {
        trees: 1,
        present: 1,
        pending: 1,
        ..Released::default()
    };
    assert_eq!(rig.cache.release_hold("job"), Ok(released));
    assert_eq!(
        pinners(&rig, &a.sha),
        vec![("boot-1".to_owned(), t.clone())]
    );
    assert_eq!(rig.cache.pinned_bytes(), 3000);

    assert_eq!(rig.cache.unpin("boot-1", &t), Ok(1));
    assert_eq!(rig.cache.pinned_bytes(), 0);
    gate.add_permits(64);
    until("b is in the cache", || rig.cache.held() == (5000, 2)).await;
    assert_eq!(rig.cache.pinned_bytes(), 0);
}

/// Asserts that a hold opened for a tree that a mount has pinned already pins nothing for itself:
/// the blob the cache holds and the blob that fills later are both pinned for the attachment
/// alone, and no handover is counted, since no pin of the hold moved.
#[tokio::test]
async fn attach_before_hydrate_leaves_the_hold_pinning_nothing() {
    let rig = rig("thold-before", 1);
    let (a, b) = (stored(&rig, 1, 3000).await, stored(&rig, 2, 2000).await);
    let t = tree(1);
    assert_eq!(rig.cache.fill_blob(&a.blob()).await, Ok(()));
    assert_eq!(
        rig.cache
            .pin("boot-1", &t, std::slice::from_ref(&a.sha))
            .unwrap()
            .pinned,
        1
    );
    let gate = rig.tamper.hold_key(&b.key);

    assert_eq!(
        rig.cache.open_hold("job", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &a.blob()),
        Ok(Prefetched::Present)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &b.blob()),
        Ok(Prefetched::Queued)
    );
    gate.add_permits(64);
    until("b is in the cache", || rig.cache.held() == (5000, 2)).await;
    until("b is pinned", || rig.cache.pinned_bytes() == 5000).await;

    let attached = ("boot-1".to_owned(), t.clone());
    assert_eq!(pinners(&rig, &a.sha), vec![attached.clone()]);
    assert_eq!(pinners(&rig, &b.sha), vec![attached]);
    assert!(
        rig.cache
            .metrics()
            .contains("tierstore_cache_hold_handovers_total 0\n")
    );
    let filled = Released {
        trees: 1,
        filled: 1,
        present: 1,
        ..Released::default()
    };
    assert_eq!(rig.cache.release_hold("job"), Ok(filled));
    assert_eq!(rig.cache.pinned_bytes(), 5000);
}

/// Asserts that the attachments of a handed tree follow the mounts: a mount that unpins the tree
/// stops being pinned for while another mount of it still is, and a mount that sweeps the tree
/// away with `pins_live` stops being pinned for too.
#[tokio::test]
async fn a_fill_pins_for_the_attachments_that_are_left() {
    let rig = rig("thold-left", 1);
    let (a, b) = (stored(&rig, 1, 300).await, stored(&rig, 2, 200).await);
    let t = tree(1);
    let (gate_a, gate_b) = (rig.tamper.hold_key(&a.key), rig.tamper.hold_key(&b.key));
    assert_eq!(
        rig.cache.open_hold("job", None, std::slice::from_ref(&t)),
        Ok(1)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &a.blob()),
        Ok(Prefetched::Queued)
    );
    for mount in ["boot-1", "boot-2", "boot-3"] {
        assert_eq!(rig.cache.pin(mount, &t, &[]).unwrap().pinned, 0);
    }
    assert_eq!(rig.cache.unpin("boot-1", &t), Ok(0));
    assert_eq!(rig.cache.pins_live("boot-2", &[]), Ok(0));
    gate_a.add_permits(64);
    until("a is pinned", || rig.cache.pinned_bytes() == 300).await;
    assert_eq!(
        pinners(&rig, &a.sha),
        vec![("boot-3".to_owned(), t.clone())]
    );

    // The tree is still listed by boot-3: a sweep that names it keeps it.
    assert_eq!(
        rig.cache.pins_live("boot-3", std::slice::from_ref(&t)),
        Ok(0)
    );
    assert_eq!(
        rig.cache.hold_blob("job", &t, &b.blob()),
        Ok(Prefetched::Queued)
    );
    gate_b.add_permits(64);
    until("b is pinned", || rig.cache.pinned_bytes() == 500).await;
    assert_eq!(
        pinners(&rig, &b.sha),
        vec![("boot-3".to_owned(), t.clone())]
    );

    assert_eq!(rig.cache.unpin("boot-3", &t), Ok(2));
    assert_eq!(rig.cache.pinned_bytes(), 0);
}

/// Asserts that the handover leaves the pins file as it was: the file has the same keys, one
/// entry for the attachment's tree with its blobs and none for the hold, so the previous release
/// reads it as the pins of an attachment, and a cache opened over the directory again pins those
/// blobs for the attachment and has no hold.
#[tokio::test]
async fn handed_over_pins_are_in_the_pins_file_and_a_restart_keeps_them() {
    let rig = rig("thold-compat", 1);
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
    assert_eq!(file_of(&rig), serde_json::json!({"trees": []}));
    assert_eq!(rig.cache.pin("boot-1", &t, &[]).unwrap().pinned, 0);
    let want = serde_json::json!({"trees": [{"mount": "boot-1", "tree": t, "blobs": [a.sha]}]});
    assert_eq!(file_of(&rig), want);

    let restarted = rig.reopen();
    assert_eq!(restarted.pinned_bytes(), 3000);
    assert_eq!(
        restarted.release_hold("job"),
        Err(HoldError::NoSuchHold("job".to_owned()))
    );
    assert_eq!(restarted.unpin("boot-1", &t), Ok(1));
    assert_eq!(restarted.pinned_bytes(), 0);
}
