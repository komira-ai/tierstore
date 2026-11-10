//! The pins of the cache: what eviction does with a pinned object, the limit, what still removes
//! a pinned object, the file and how the next holder of the lock reads it, and the answers a
//! cache that is not the holder gives. Time is the rig's clock, moved by hand.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use super::hold::Trace;
use super::layout;
use super::metrics::read;
use super::pin_set::PinsFile;
use super::testing::{Rig, digest, pattern};
use super::{CacheConfig, Mode, PinAnswer, PinError, Tick};
use crate::{Backend as _, Error};

const MOUNT: &str = "boot-1";

/// The digests (names of `cas/l/` keys) of the objects `from` to `to` of `size` bytes, put in the
/// upstream and read through the cache in order, so that the first is the least recently used.
/// A fill that takes the cache past its high water mark runs a pass.
async fn filled_from(rig: &Rig, range: std::ops::Range<usize>, size: usize) -> Vec<String> {
    let mut digests = Vec::new();
    for n in range {
        let mut body = pattern(size);
        body[0] = n as u8;
        let hex = digest(&body);
        let key = format!("cas/l/{hex}");
        rig.put(&key, &body).await;
        assert_eq!(rig.get(&key).await.unwrap().len(), size);
        digests.push(hex);
    }
    digests
}

async fn filled(rig: &Rig, count: usize, size: usize) -> Vec<String> {
    filled_from(rig, 0..count, size).await
}

/// A cache of a thousand bytes whose pass runs above 900 and evicts down to 750, and that may pin `limit`.
fn small(name: &str, limit: u64) -> Rig {
    let rig = Rig::with(name, |config| {
        config.budget = 1000;
        config.high_water = 90;
        config.low_water = 75;
        config.critical = 100;
        config.free_floor = 0;
        config.pin_limit = limit;
    });
    rig.disk.set_space(1000, 1000);
    rig
}

fn has(rig: &Rig, hex: &str) -> bool {
    rig.has_file(&format!("cas/l/{hex}"))
}

fn pins_file(rig: &Rig) -> PinsFile {
    serde_json::from_slice(&std::fs::read(rig.config.path.join("pins")).unwrap()).unwrap()
}

fn one(hex: &str) -> Vec<String> {
    vec![hex.to_owned()]
}

/// Asserts that an entry a tree pins survives the pass that a fill past the high water mark runs
/// (the pass that would have taken it, the oldest), the next oldest going in its place; that it is
/// counted in `pin_skips` and not in `pinned_skips`; that it stays charged to the budget; and that
/// once the tree is unpinned the next pass takes it.
#[tokio::test]
async fn a_pinned_entry_survives_an_eviction_pass_over_budget() {
    let rig = small("tscache-pin-survive", 1000);
    let mut blobs = filled(&rig, 3, 250).await;
    let _ = rig.cache.pin(MOUNT, "tree-a", &one(&blobs[0])).unwrap();
    blobs.extend(filled_from(&rig, 3..4, 250).await);
    assert!(has(&rig, &blobs[0]));
    assert!(!has(&rig, &blobs[1]));
    assert!(has(&rig, &blobs[2]) && has(&rig, &blobs[3]));
    // Still charged to the budget.
    assert_eq!(rig.cache.held(), (750, 3));
    let counters = rig.cache.counters();
    assert_eq!(read(&counters.pin_skips), 1);
    assert_eq!(read(&counters.pinned_skips), 0);
    let text = rig.cache.metrics();
    assert!(text.contains("tierstore_cache_pin_skips_total 1\n"));
    assert!(text.contains("tierstore_cache_pinned_skips_total 0\n"));
    assert_eq!(rig.cache.pinned_bytes(), 250);

    assert_eq!(rig.cache.unpin(MOUNT, "tree-a"), Ok(1));
    assert_eq!(rig.cache.pinned_bytes(), 0);
    let _ = filled_from(&rig, 4..5, 250).await;
    assert!(!has(&rig, &blobs[0]));
    assert_eq!(read(&rig.cache.counters().pin_skips), 1);
    assert_eq!(rig.cache.evict(), super::evict::Pass::default());
}

/// Asserts that a pin that would take the pinned bytes past `pin_limit` is answered as refused,
/// that the refused blob is counted as used (the pass that follows takes the unpinned one that
/// is older, not it: without the touch it is the second oldest and goes), and that the pinned
/// bytes are the one blob's.
#[tokio::test]
async fn a_pin_past_pin_limit_is_refused_and_the_blob_is_kept_by_recency() {
    let rig = small("tscache-pin-limit", 300);
    let blobs = filled(&rig, 3, 250).await;
    let first = rig.cache.pin(MOUNT, "tree-a", &one(&blobs[0])).unwrap();
    assert_eq!(
        first,
        PinAnswer {
            pinned: 1,
            refused: 0,
            absent: 0
        }
    );
    let second = rig.cache.pin(MOUNT, "tree-b", &one(&blobs[1])).unwrap();
    assert_eq!(
        second,
        PinAnswer {
            pinned: 0,
            refused: 1,
            absent: 0
        }
    );
    // A blob a tree pins already is shared with another tree at no cost.
    let shared = rig.cache.pin(MOUNT, "tree-b", &one(&blobs[0])).unwrap();
    assert_eq!(
        shared,
        PinAnswer {
            pinned: 1,
            refused: 0,
            absent: 0
        }
    );
    assert_eq!(rig.cache.pinned_bytes(), 250);

    let _ = filled_from(&rig, 3..4, 250).await;
    assert!(has(&rig, &blobs[0]) && has(&rig, &blobs[1]));
    assert!(!has(&rig, &blobs[2]));
}

/// Asserts that a limit of zero refuses every pin and that a blob the cache does not hold is
/// answered as absent and not pinned.
#[tokio::test]
async fn a_zero_limit_refuses_and_a_blob_the_cache_lacks_is_absent() {
    let rig = small("tscache-pin-zero", 0);
    let blobs = filled(&rig, 1, 100).await;
    let missing = digest(b"never read");
    let both = vec![blobs[0].clone(), missing];
    let answer = rig.cache.pin(MOUNT, "t", &both).unwrap();
    assert_eq!(
        answer,
        PinAnswer {
            pinned: 0,
            refused: 1,
            absent: 1
        }
    );
    assert_eq!(rig.cache.pinned_bytes(), 0);
}

/// Asserts that a scrub that finds bad bytes removes a pinned entry and its pins.
#[tokio::test]
async fn a_scrub_removes_a_pinned_entry_with_bad_bytes() {
    let rig = small("tscache-pin-scrub", 1000);
    let blobs = filled(&rig, 1, 100).await;
    assert_eq!(rig.cache.pin(MOUNT, "t", &blobs).unwrap().pinned, 1);
    let path = rig.file(&format!("cas/l/{}", blobs[0]));
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] ^= 0xff;
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    std::fs::write(&path, bytes).unwrap();

    let mut state = super::Scrubber::new(1 << 20);
    assert_eq!(rig.cache.scrub_once(&mut state).dropped, 1);
    assert!(!has(&rig, &blobs[0]));
    assert_eq!(rig.cache.held(), (0, 0));
    assert_eq!(rig.cache.pinned_bytes(), 0);
    assert_eq!(rig.cache.unpin(MOUNT, "t"), Ok(0));
}

/// Asserts that a revalidation that finds the object gone, and a write-through delete, remove a
/// pinned entry and its pins, and that the blob read again is not pinned.
#[tokio::test]
async fn a_revalidation_and_a_write_through_delete_remove_a_pinned_entry() {
    let rig = Rig::with("tscache-pin-remove", |config| {
        config.write_through = true;
        config.revalidate_after = Some(Duration::from_millis(1000));
    });
    let blobs = filled(&rig, 2, 100).await;
    let key = |n: usize| format!("cas/l/{}", blobs[n]);
    assert_eq!(rig.cache.pin(MOUNT, "t", &blobs).unwrap().pinned, 2);
    assert_eq!(rig.cache.pinned_bytes(), 200);

    rig.tamper.delete(&key(0)).await.unwrap();
    rig.clock.advance(1001);
    assert!(matches!(rig.get(&key(0)).await, Err(Error::NotFound(_))));
    assert!(!has(&rig, &blobs[0]));
    assert_eq!(rig.cache.pinned_bytes(), 100);

    assert_eq!(rig.cache.delete(&key(1)).await, Ok(()));
    assert!(!has(&rig, &blobs[1]));
    assert_eq!(rig.cache.pinned_bytes(), 0);
    rig.put(&key(1), &{
        let mut body = pattern(100);
        body[0] = 1;
        body
    })
    .await;
    assert_eq!(rig.get(&key(1)).await.unwrap().len(), 100);
    assert_eq!(rig.cache.pinned_bytes(), 0);
}

/// Asserts that a pin made after the pass chose the object but before it removed it holds: the
/// object stays, with its files, and is counted as skipped for the pin.
#[tokio::test]
async fn a_pin_made_while_the_pass_removes_the_object_holds() {
    let rig = small("tscache-pin-race", 1000);
    let blobs = filled(&rig, 3, 250).await;
    let cache = rig.cache.clone();
    let target = blobs[0].clone();
    rig.cache.inner.set_trace(Trace {
        between_delete_steps: Some(Arc::new(move || {
            let _ = cache.pin(MOUNT, "late", &one(&target)).unwrap();
        })),
        ..Trace::default()
    });
    let _ = filled_from(&rig, 3..4, 250).await;
    assert!(has(&rig, &blobs[0]));
    assert!(!has(&rig, &blobs[1]));
    assert_eq!(read(&rig.cache.counters().pin_skips), 1);
    assert_eq!(rig.cache.held(), (750, 3));
}

/// Asserts that trees share a blob and that `unpin` and `pins_live` take away exactly what they
/// name: a second tree's pin stays when the first is unpinned, and `pins_live` drops the trees
/// that are not listed and the pins of another mount.
#[tokio::test]
async fn unpin_and_pins_live_take_away_what_they_name() {
    let rig = small("tscache-pin-live", 1000);
    let blobs = filled(&rig, 3, 100).await;
    let _ = rig.cache.pin(MOUNT, "a", &blobs[..2]).unwrap();
    let _ = rig.cache.pin(MOUNT, "b", &blobs[1..]).unwrap();
    let _ = rig.cache.pin("boot-0", "a", &blobs[..1]).unwrap();
    assert_eq!(rig.cache.pinned_bytes(), 300);
    // The first blob is still pinned by the other mount's tree.
    assert_eq!(rig.cache.unpin(MOUNT, "a"), Ok(2));
    assert_eq!(rig.cache.pinned_bytes(), 300);
    assert_eq!(rig.cache.unpin(MOUNT, "a"), Ok(0));
    let live = vec!["b".to_owned(), "gone".to_owned()];
    assert_eq!(rig.cache.pins_live(MOUNT, &live), Ok(1));
    assert_eq!(rig.cache.pinned_bytes(), 200);
    assert_eq!(rig.cache.pins_live(MOUNT, &[]), Ok(1));
    assert_eq!(rig.cache.pinned_bytes(), 0);
}

/// Asserts the refusals: a name that is not a digest pins nothing of the request, and a cache that
/// does not hold the lock answers every operation with `ReadOnly`.
#[tokio::test]
async fn a_bad_digest_pins_nothing_and_a_read_only_cache_refuses() {
    let rig = small("tscache-pin-refuse", 1000);
    let blobs = filled(&rig, 1, 100).await;
    let bad = vec![blobs[0].clone(), "ABC".to_owned()];
    let error = rig.cache.pin(MOUNT, "t", &bad).unwrap_err();
    assert_eq!(error, PinError::NotADigest("ABC".to_owned()));
    assert_eq!(
        error.to_string(),
        "\"ABC\" is not the lower-case hex of a SHA-256"
    );
    assert_eq!(rig.cache.pinned_bytes(), 0);

    let holder = rig.locked();
    let waiting = rig.locked();
    assert_eq!(waiting.mode(), Mode::ReadOnly);
    assert_eq!(waiting.pin(MOUNT, "t", &blobs), Err(PinError::ReadOnly));
    assert_eq!(waiting.unpin(MOUNT, "t"), Err(PinError::ReadOnly));
    assert_eq!(waiting.pins_live(MOUNT, &[]), Err(PinError::ReadOnly));
    assert_eq!(waiting.pinned_bytes(), 0);
    assert_eq!(holder.pin(MOUNT, "t", &blobs).unwrap().pinned, 1);
}

/// Asserts that the scan of a directory reads `obj/` and `.stage/` only: a release that has no
/// pins starts over a cache directory that has a pins file (also one it cannot parse, and one with
/// keys it does not know), finds the same objects, discards nothing for it and leaves the file.
#[tokio::test]
async fn a_release_without_pins_starts_on_a_cache_with_a_pins_file() {
    let rig = small("tscache-pin-old-release", 1000);
    let blobs = filled(&rig, 2, 100).await;
    let _ = rig.cache.pin(MOUNT, "t", &blobs).unwrap();
    rig.cache.inner.save_pins(true);
    let root = &rig.config.path;
    let written = std::fs::read(root.join("pins")).unwrap();
    for text in [
        written.clone(),
        b"not json".to_vec(),
        br#"{"trees":[],"future":1}"#.to_vec(),
    ] {
        std::fs::write(root.join("pins"), &text).unwrap();
        let scanned = layout::scan(root).unwrap();
        assert_eq!((scanned.found.len(), scanned.discarded), (2, 0));
        assert_eq!(layout::read_only_scan(root).found.len(), 2);
        assert_eq!(layout::clear_stage(root).unwrap(), 0);
        assert_eq!(std::fs::read(root.join("pins")).unwrap(), text);
    }
}

/// Asserts the writes of the file: made at a tick, by rename (no half-written file is left), at
/// most once a second however many ticks, not at all when nothing changed, and tried again at the
/// next tick when it failed.
#[tokio::test]
async fn the_pins_file_is_written_at_a_tick_at_most_once_a_second() {
    let rig = small("tscache-pin-file", 1000);
    let blobs = filled(&rig, 2, 100).await;
    let cache = rig.locked();
    let file = rig.config.path.join("pins");
    assert_eq!(cache.tick().unwrap(), Tick::Held);
    assert!(!file.exists());

    let _ = cache.pin(MOUNT, "a", &blobs[..1]).unwrap();
    assert_eq!(cache.tick().unwrap(), Tick::Held);
    assert_eq!(pins_file(&rig).trees.len(), 1);
    assert!(!rig.config.path.join("pins.new").exists());

    let _ = cache.pin(MOUNT, "b", &blobs[1..]).unwrap();
    rig.clock.advance(999);
    assert_eq!(cache.tick().unwrap(), Tick::Held);
    assert_eq!(pins_file(&rig).trees.len(), 1);
    rig.clock.advance(1);
    assert_eq!(cache.tick().unwrap(), Tick::Held);
    assert_eq!(pins_file(&rig).trees.len(), 2);

    // A write that fails leaves the old file and is tried at the next tick that is due.
    let _ = cache.unpin(MOUNT, "a").unwrap();
    std::fs::create_dir(rig.config.path.join("pins.new")).unwrap();
    rig.clock.advance(1000);
    assert_eq!(cache.tick().unwrap(), Tick::Held);
    assert_eq!(pins_file(&rig).trees.len(), 2);
    std::fs::remove_dir(rig.config.path.join("pins.new")).unwrap();
    rig.clock.advance(1000);
    assert_eq!(cache.tick().unwrap(), Tick::Held);
    assert_eq!(pins_file(&rig).trees.len(), 1);
}

/// Asserts that a read-only cache writes no pins file at a tick or when the loop ends.
#[tokio::test]
async fn a_read_only_cache_writes_no_pins_file() {
    let rig = small("tscache-pin-readonly-file", 1000);
    let _ = filled(&rig, 1, 100).await;
    let holder = rig.locked();
    let waiting = rig.locked();
    assert_eq!(waiting.tick().unwrap(), Tick::Waiting);
    waiting.inner.save_pins(true);
    assert!(!rig.config.path.join("pins").exists());
    drop(holder);
}

/// Asserts the hand-over: the holder writes the file when its loop ends, the next process over the
/// directory is read-only until the holder is gone, and at the tick where it takes the lock it
/// reads the file and has the pins of the blobs it holds, up to its own limit; a tree of another
/// mount stays until `pins_live`, and a blob the new holder lacks is dropped.
#[tokio::test]
async fn the_next_holder_reads_the_pins_when_it_takes_the_lock() {
    let rig = small("tscache-pin-handover", 1000);
    let blobs = filled(&rig, 3, 100).await;
    let first = rig.locked();
    let second = rig.locked_with(|config: &mut CacheConfig| config.pin_limit = 200);
    assert_eq!(second.mode(), Mode::ReadOnly);
    let missing = digest(b"gone before the hand-over");
    // The file names a blob nobody holds, which only a hand-made file can: the cache reads it
    // back as absent.
    let _ = first.pin(MOUNT, "a", &blobs).unwrap();
    let _ = first.pin("boot-0", "z", &blobs[..1]).unwrap();
    let halt = Arc::new(Notify::new());
    halt.notify_one();
    first.hold(Duration::from_secs(1), halt).await;
    let mut file = pins_file(&rig);
    file.trees[0].blobs.push(missing);
    std::fs::write(
        rig.config.path.join("pins"),
        serde_json::to_vec(&file).unwrap(),
    )
    .unwrap();
    drop(first);

    assert_eq!(second.pinned_bytes(), 0);
    assert_eq!(second.tick().unwrap(), Tick::Took);
    // Two of the three fit in 200; the other mount's tree shares the first blob.
    assert_eq!(second.pinned_bytes(), 200);
    assert_eq!(second.pins_live(MOUNT, &["a".to_owned()]), Ok(1));
    assert_eq!(second.pinned_bytes(), 200);
    assert_eq!(second.unpin(MOUNT, "a"), Ok(2));
    assert_eq!(second.pinned_bytes(), 0);
}

/// Asserts that a cache that opens with the lock (or without one) reads the file at open, and
/// that a file that is not there, is not valid JSON or is a directory means no pins and no
/// failure to start.
#[tokio::test]
async fn a_cache_reads_the_pins_file_at_open_and_starts_without_a_usable_one() {
    let rig = small("tscache-pin-open", 1000);
    let blobs = filled(&rig, 2, 100).await;
    let _ = rig.cache.pin(MOUNT, "t", &blobs).unwrap();
    rig.cache.inner.save_pins(true);
    assert_eq!(rig.reopen().pinned_bytes(), 200);
    assert_eq!(rig.locked().pinned_bytes(), 200);

    let file = rig.config.path.join("pins");
    std::fs::write(&file, b"{").unwrap();
    assert_eq!(rig.reopen().pinned_bytes(), 0);
    std::fs::remove_file(&file).unwrap();
    assert_eq!(rig.reopen().pinned_bytes(), 0);
    std::fs::create_dir(&file).unwrap();
    assert_eq!(rig.reopen().pinned_bytes(), 0);
}
