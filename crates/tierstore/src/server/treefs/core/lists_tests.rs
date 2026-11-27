//! The budget of mapped tree lists, `fs.tree_list_ram_bytes`, end to end through the core: the
//! node's cache holds a list of its own at each `trees/<digest>` (the upstream's objects, filled
//! by the cache), every list one page, and the rig's cap is a number of pages. The tests that look
//! at the node's holds on placements run the core over the node's real tree store, whose lists
//! the mount shares (they are mapped from the store's first look at them and counted once). The
//! use stamps are the rig's clock, advanced between uses so that they never tie.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use super::super::blob::SAY_EVERY_MS;
use super::super::placed::key_of;
use super::super::{Core, Entry, Errno, Looked};
use crate::TreeDigest;
use crate::server::local_cache::trees::testing::{fs_at, open_image};
use crate::server::local_cache::trees::{ImageRequest, Mapped, PlacementId, TreeStore, TreesConfig};
use crate::server::treefs::in_process::InProcess;
use crate::server::treefs::ino;
use crate::server::treefs::locked;
use crate::server::treefs::table::TreeTable;
use crate::server::treefs::testing::{
    Built, Logged, Rig, TREE_IDLE, image_named, logged, open_cached, sample, until,
};
use crate::tree_image_list::in_pages;

/// The bytes one list of the rigs takes mapped: a page.
fn page() -> u64 {
    in_pages(1)
}

/// A rig that keeps at most `pages` pages of lists mapped, whose upstream has a list of its own as
/// the object of each of the `digests`: the `at`-th list is `trees/<digests[at]>`.
struct Listed {
    rig: Rig,
    /// The digest of each list.
    digests: Vec<TreeDigest>,
}

impl std::ops::Deref for Listed {
    type Target = Rig;

    fn deref(&self) -> &Rig {
        &self.rig
    }
}

/// A [`Listed`] of `count` lists and no other tree.
fn listed(name: &str, pages: u64, count: usize) -> Listed {
    listed_over(name, pages, count, Built::of(Vec::new()))
}

/// [`listed`], with the trees of manifests `built` upstream.
fn listed_over(name: &str, pages: u64, count: usize, built: Built) -> Listed {
    let rig = Rig::capped(name, built, pages * page());
    let mut digests = Vec::new();
    for at in 0..count {
        let bytes = image_named(&format!("d{at}"));
        assert!(bytes.len() as u64 <= page());
        let digest = TreeDigest::of(&bytes);
        let _ = locked(&rig.upstream.built)
            .manifests
            .insert(digest.to_string(), bytes);
        digests.push(digest);
    }
    Listed { rig, digests }
}

/// The root of the list `at`, looked up as `trees/<digest>`.
fn found(rig: &Listed, at: usize) -> Entry {
    let digest = rig.digests[at].to_string();
    match rig.core.lookup(ino::TREES, &digest).unwrap() {
        Looked::Found(entry) => entry,
        other => panic!("{digest}: {other:?}"),
    }
}

/// The number of the tree of the list `at`.
fn number(rig: &Listed, at: usize) -> u64 {
    rig.core.table.find(&rig.digests[at]).unwrap()
}

/// Whether the tree `number` is in memory.
fn in_memory(rig: &Rig, number: u64) -> bool {
    rig.core.trees.get(number).is_some()
}

/// What the core counted: the releases for the cap, those when idle, and the lists mapped past
/// the cap.
fn counted(rig: &Rig) -> [u64; 3] {
    let totals = rig.core.lists.totals();
    [totals.cap, totals.idle, totals.over_cap]
}

/// Moves the clock of `rig` a second on, so that the next use is later than every use before.
fn later(rig: &Rig) {
    rig.clock.advance(Duration::from_secs(1));
}

/// What the node's cache was told of the lists of the mount `mount-1`: the lists mapped and their
/// bytes, the cap, the lists released for it and when idle, and those mapped past it.
fn told(rig: &Rig) -> [u64; 6] {
    let text = rig.node.metrics();
    let value = |series: &str| -> u64 {
        let prefix = format!("{series} ");
        let found = text.lines().find_map(|line| line.strip_prefix(&prefix));
        found
            .unwrap_or_else(|| panic!("no series {series} in {text}"))
            .parse()
            .unwrap()
    };
    [
        value("tierstore_fs_tree_lists_mapped{mount=\"mount-1\"}"),
        value("tierstore_fs_tree_list_mapped_bytes{mount=\"mount-1\"}"),
        value("tierstore_fs_tree_list_ram_bytes{mount=\"mount-1\"}"),
        value("tierstore_fs_tree_list_releases_total{mount=\"mount-1\",why=\"cap\"}"),
        value("tierstore_fs_tree_list_releases_total{mount=\"mount-1\",why=\"idle\"}"),
        value("tierstore_fs_tree_list_over_cap_total{mount=\"mount-1\"}"),
    ]
}

/// Asserts that a list that does not fit under the cap releases the list no request has named for
/// longest, of those with no run and no request: of `a` and `b`, `a` named again after `b`, `b`
/// is released and its implicit attachment ended; `a` and the new `c` stay mapped, two pages, and
/// one release for the cap is counted.
#[test]
fn a_list_past_the_cap_releases_the_least_recently_used_list_with_no_run_and_no_request() {
    let rig = listed("tfs-lists-lru", 2, 3);
    let a = found(&rig, 0);
    later(&rig);
    let _ = found(&rig, 1);
    later(&rig);
    rig.core.getattr(a.attr.ino).unwrap();
    later(&rig);
    let _ = found(&rig, 2);
    let [a, b, c] = [number(&rig, 0), number(&rig, 1), number(&rig, 2)];
    assert!(in_memory(&rig, a) && in_memory(&rig, c));
    assert!(!in_memory(&rig, b));
    let mut implicit = rig.core.implicit();
    implicit.sort_unstable();
    assert_eq!(implicit, [a, c]);
    assert_eq!(
        rig.core.lists.mapped.mapped(),
        Mapped {
            lists: 2,
            bytes: 2 * page()
        }
    );
    assert_eq!(counted(&rig), [1, 0, 0]);
}

/// Asserts that the line of a lookup that attached a tree list says the bytes of the list, after
/// its time, so that the time of an attach can be read by the size of its list (a lookup of a tree
/// of manifests says none: `core/implicit_attach_tests.rs`).
#[test]
fn the_line_of_a_lookup_of_a_list_says_its_bytes() {
    let rig = listed("tfs-lists-line", 2, 1);
    let log = Logged::default();
    let _logging = logged(&log, tracing::Level::INFO);
    let _ = found(&rig, 0);
    let digest = rig.digests[0];
    let bytes = image_named("d0").len();
    let lines: Vec<String> = log
        .text()
        .lines()
        .filter(|line| line.contains(": lookup of "))
        .map(str::to_owned)
        .collect();
    assert_eq!(
        lines,
        [format!(
            "tierstore fs: lookup of {digest}: ok in 0 ms; 0 manifests: 0 from the cache, 0 \
             filled, 0 not had; 1 levels; fills by level: 0, slowest ms by level: 0; fill-tree \
             ok in 0 ms, 0 ms at the node, fills by level: 1; a tree list of {bytes} bytes"
        )]
    );
}

/// Asserts that a list whose tree a run has attached is never released for the cap, even when no
/// request has named it for longest: `b` is released in its place.
#[test]
fn a_list_a_run_attached_is_never_released_for_the_cap() {
    let rig = listed("tfs-lists-run", 2, 3);
    let _ = found(&rig, 0);
    later(&rig);
    let _ = found(&rig, 1);
    later(&rig);
    let a = number(&rig, 0);
    let _ = locked(&rig.core.state)
        .attachments
        .add("run-1", a, rig.digests[0]);
    let _ = found(&rig, 2);
    assert!(in_memory(&rig, a));
    assert!(!in_memory(&rig, number(&rig, 1)));
    assert_eq!(counted(&rig), [1, 0, 0]);
}

/// Asserts that a tree of manifests, which holds no list, is never released for the cap, even
/// when no request has named it for longest and only a lookup attached it: the list is mapped
/// past the cap instead, and the tree of manifests stays attached.
#[test]
fn a_tree_of_manifests_is_never_released_for_the_cap() {
    let rig = listed_over("tfs-lists-manifests", 1, 2, sample());
    let root = locked(&rig.upstream.built).root.clone();
    let tree = match rig.core.lookup(ino::TREES, &root).unwrap() {
        Looked::Found(entry) => ino::split(entry.attr.ino).0,
        other => panic!("{other:?}"),
    };
    later(&rig);
    let _ = found(&rig, 0);
    let held = rig.core.tree(number(&rig, 0)).unwrap();
    later(&rig);
    let _ = found(&rig, 1);
    assert!(in_memory(&rig, tree));
    assert!(rig.core.implicit().contains(&tree));
    assert_eq!(counted(&rig), [0, 0, 1]);
    drop(held);
}

/// Asserts that a list a lookup attaches again while it is being released (the release has ended
/// its attachment and waits to give the tree's pins back, which another request holds the lock of)
/// is not released: it stays in memory, attached, and is not counted as released; the new list,
/// finding no other to release, is mapped past the cap.
#[test]
fn a_list_attached_again_while_it_is_released_stays() {
    let rig = listed("tfs-lists-again-attached", 1, 2);
    let _ = found(&rig, 0);
    let a = number(&rig, 0);
    later(&rig);
    let core = Arc::clone(&rig.core);
    let digest = rig.digests[0];
    let held = core.pins.unpin(&digest);
    let asked = core.pins.unpins_asked();
    let (second, first) = (rig.digests[1].to_string(), rig.digests[0].to_string());
    let releasing = {
        let core = Arc::clone(&rig.core);
        std::thread::spawn(move || {
            let _ = core.lookup(ino::TREES, &second);
        })
    };
    // The lookup of `b` ended the implicit attachment of `a` for room, and its unpin of the tree
    // waits for the lock the test holds.
    until(|| core.pins.unpins_asked() == asked + 1 && !core.implicit().contains(&a));
    let again = {
        let core = Arc::clone(&rig.core);
        std::thread::spawn(move || {
            let _ = core.lookup(ino::TREES, &first);
        })
    };
    until(|| core.implicit().contains(&a));
    drop(held);
    releasing.join().unwrap();
    again.join().unwrap();
    assert!(in_memory(&rig, a));
    assert!(rig.core.implicit().contains(&a));
    assert_eq!(counted(&rig), [0, 0, 1]);
}

/// Asserts that a list with a request in flight on it (the request holds its tree in memory) is
/// never released for the cap, even when no request has named it for longest: `b` is released in
/// its place; and once the request has ended, `a` is the next released.
#[test]
fn a_list_with_a_request_in_flight_is_never_released_for_the_cap() {
    let rig = listed("tfs-lists-flight", 2, 4);
    let _ = found(&rig, 0);
    later(&rig);
    let _ = found(&rig, 1);
    later(&rig);
    let a = number(&rig, 0);
    let request = rig.core.tree(a).unwrap();
    let _ = found(&rig, 2);
    assert!(in_memory(&rig, a));
    assert!(!in_memory(&rig, number(&rig, 1)));
    drop(request);
    later(&rig);
    let _ = found(&rig, 3);
    assert!(!in_memory(&rig, a));
    assert_eq!(counted(&rig), [2, 0, 0]);
}

/// Asserts that a mount past the cap with every mapped list held still attaches the new list: it
/// is found and mapped past the cap, counted, and said at warn once in `SAY_EVERY_MS` with the
/// count of those not said; the held lists stay.
#[test]
fn a_mount_past_the_cap_with_every_list_held_still_attaches_and_is_counted() {
    let rig = listed("tfs-lists-over", 1, 4);
    let log = Logged::default();
    let _logging = logged(&log, tracing::Level::WARN);
    let _ = found(&rig, 0);
    let held: Vec<_> = (0..3)
        .map(|at| {
            if at > 0 {
                let _ = found(&rig, at);
            }
            rig.core.tree(number(&rig, at)).unwrap()
        })
        .collect();
    rig.clock.advance(Duration::from_millis(SAY_EVERY_MS));
    let _ = found(&rig, 3);
    assert_eq!(rig.core.lists.mapped.mapped().lists, 4);
    assert_eq!(counted(&rig), [0, 0, 3]);
    let line = |tree: &TreeDigest, mapped: u64| {
        format!(
            "tierstore fs: tree list {tree} mapped past fs.tree_list_ram_bytes: {mapped} of {} \
             mapped, every mapped list held",
            page()
        )
    };
    let said: Vec<String> = log.text().lines().map(str::to_owned).collect();
    assert_eq!(
        said,
        [
            line(&rig.digests[1], 2 * page()),
            format!(
                "{}; 1 more tree list mapped past the cap since the last line",
                line(&rig.digests[3], 4 * page())
            ),
        ]
    );
    drop(held);
}

/// Asserts that a released list is read again by the next request that names its tree's number,
/// under the same number and with the same answers, and mapped again, under the cap like any
/// other: the list released in its turn is the one it released.
#[test]
fn a_released_list_is_read_again_by_the_next_request_under_its_tree_number() {
    let rig = listed("tfs-lists-again", 1, 2);
    let a = found(&rig, 0);
    let number_a = number(&rig, 0);
    later(&rig);
    let _ = found(&rig, 1);
    assert!(!in_memory(&rig, number_a));
    let again = rig.core.getattr(a.attr.ino).unwrap();
    assert_eq!(again.attr, a.attr);
    assert!(in_memory(&rig, number_a));
    assert_eq!(number(&rig, 0), number_a);
    assert!(!in_memory(&rig, number(&rig, 1)));
    assert_eq!(rig.core.lists.mapped.mapped().lists, 1);
    assert_eq!(counted(&rig), [2, 0, 0]);
}

/// Asserts that the sweep for idle trees counts the lists it releases, and a tree of manifests
/// it releases not; and that the report tells the cache the lists mapped and their bytes, the cap,
/// and the running totals: the lists mapped and their bytes, the cap, those released for the cap
/// and when idle, and those mapped past the cap.
#[test]
fn the_sweep_counts_the_lists_it_releases_and_the_report_tells_the_node() {
    let rig = listed("tfs-lists-idle", 2, 3);
    let _ = found(&rig, 0);
    later(&rig);
    let _ = found(&rig, 1);
    later(&rig);
    let _ = found(&rig, 2);
    assert_eq!(counted(&rig), [1, 0, 0]);
    rig.clock.advance(TREE_IDLE);
    assert_eq!(rig.core.detach_idle(), 2);
    assert_eq!(counted(&rig), [1, 2, 0]);
    rig.core.look_around();
    assert_eq!(told(&rig), [0, 0, 2 * page(), 1, 2, 0]);
}

/// Asserts that no list is released while a successor takes over: the room is not made, and the
/// list in memory stays; once the hold ends, it is released.
#[test]
fn no_list_is_released_while_a_successor_takes_over() {
    let rig = listed("tfs-lists-held", 1, 1);
    let _ = found(&rig, 0);
    let a = number(&rig, 0);
    rig.core.hold(true);
    assert!(!rig.core.make_room(page()));
    assert!(in_memory(&rig, a));
    rig.core.hold(false);
    assert!(rig.core.make_room(page()));
    assert!(!in_memory(&rig, a));
    assert_eq!(counted(&rig), [1, 0, 0]);
}

/// Asserts that a list whose lookups the kernel has all forgotten leaves memory as it is released
/// for the cap, and is counted as released for the cap all the same.
#[test]
fn a_forgotten_list_leaves_memory_when_it_is_released_and_is_counted_released() {
    let rig = listed("tfs-lists-forgotten", 1, 2);
    let a = found(&rig, 0);
    rig.core.forget(a.attr.ino, 1);
    assert!(in_memory(&rig, number(&rig, 0)));
    later(&rig);
    let _ = found(&rig, 1);
    assert!(!in_memory(&rig, number(&rig, 0)));
    assert_eq!(counted(&rig), [1, 0, 0]);
}

/// Asserts that a list released for the cap says the opens that failed under it (here, of a file
/// whose blob the upstream does not have), as the sweep for idle trees does when that attachment
/// ends.
#[test]
fn a_list_released_for_the_cap_says_the_opens_that_failed_under_it() {
    let rig = listed("tfs-lists-failed", 1, 2);
    let a = found(&rig, 0);
    let tool = match rig.core.lookup(a.attr.ino, "bin").unwrap() {
        Looked::Found(bin) => match rig.core.lookup(bin.attr.ino, "tool").unwrap() {
            Looked::Found(tool) => tool,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    assert!(open_cached(&rig.core, tool.attr.ino, false).is_err());
    let log = Logged::default();
    let _logging = logged(&log, tracing::Level::WARN);
    later(&rig);
    let _ = found(&rig, 1);
    let key = rig.digests[0];
    let number = number(&rig, 0);
    let hello = TreeDigest::of(b"hello");
    assert_eq!(
        log.text().lines().collect::<Vec<_>>(),
        [format!(
            "tierstore fs: the attachment of the tree {key} (number {number}) by a lookup ended \
             with 1 failed open: /bin/tool ({hello}: upstream 404)"
        )]
    );
}

/// Asserts that a cap lowered while the mount serves releases at once the lists over it that
/// nothing holds, the one no request has named for longest first (`a`, then `b`), counts each as
/// released for the cap, and tells the cache the new cap with what stays mapped, `c` alone.
/// Catches a lowered cap that waits for the next list to be mapped, or that the cache is not told.
#[test]
fn a_lowered_cap_releases_at_once_the_lists_over_it_and_tells_the_node() {
    let rig = listed("tfs-lists-lowered", 3, 3);
    for at in 0..3 {
        let _ = found(&rig, at);
        later(&rig);
    }
    rig.core.set_tree_list_ram_bytes(page());
    assert!(!in_memory(&rig, number(&rig, 0)));
    assert!(!in_memory(&rig, number(&rig, 1)));
    assert!(in_memory(&rig, number(&rig, 2)));
    assert_eq!(counted(&rig), [2, 0, 0]);
    assert_eq!(told(&rig), [1, page(), page(), 2, 0, 0]);
}

/// Asserts that a cap raised while the mount serves releases nothing and lets in a list the cap
/// before had no room for: `a` stays mapped beside `b`, nothing is counted, and the cache is told
/// the new cap.
#[test]
fn a_raised_cap_releases_nothing_and_lets_another_list_in() {
    let rig = listed("tfs-lists-raised", 1, 2);
    let _ = found(&rig, 0);
    later(&rig);
    rig.core.set_tree_list_ram_bytes(2 * page());
    let _ = found(&rig, 1);
    assert!(in_memory(&rig, number(&rig, 0)));
    assert!(in_memory(&rig, number(&rig, 1)));
    assert_eq!(counted(&rig), [0, 0, 0]);
    assert_eq!(told(&rig), [1, page(), 2 * page(), 0, 0, 0]);
}

/// Asserts that a lowered cap leaves a list a run attached mapped, however long no request has
/// named it, and releases the one nothing holds in its place; and that a cap under the one held
/// list (a byte: the core takes any cap, its caller holds it to the file's bound) releases
/// nothing more, and counts nothing mapped past the cap, as no list was mapped.
#[test]
fn a_lowered_cap_leaves_the_lists_that_are_held() {
    let rig = listed("tfs-lists-lowered-held", 2, 2);
    let _ = found(&rig, 0);
    later(&rig);
    let _ = found(&rig, 1);
    let a = number(&rig, 0);
    let _ = locked(&rig.core.state)
        .attachments
        .add("run-1", a, rig.digests[0]);
    rig.core.set_tree_list_ram_bytes(page());
    assert!(in_memory(&rig, a));
    assert!(!in_memory(&rig, number(&rig, 1)));
    assert_eq!(counted(&rig), [1, 0, 0]);
    rig.core.set_tree_list_ram_bytes(1);
    assert!(in_memory(&rig, a));
    assert_eq!(counted(&rig), [1, 0, 0]);
}

/// A rig over the node's real tree store, which has a placement of an image of its own at each of
/// the `at`-th group of fs locations; the core of the rig calls the store in process, and shares
/// its mapped lists.
struct Placed {
    rig: Rig,
    store: TreeStore,
    /// The placement of each image.
    ids: Vec<PlacementId>,
}

impl std::ops::Deref for Placed {
    type Target = Rig;

    fn deref(&self) -> &Rig {
        &self.rig
    }
}

/// A [`Placed`] whose core keeps at most `pages` pages of lists mapped, with an image placed at
/// each group of `locations`, the `at`-th of the list `d<at>` (`image_named`).
fn placed(name: &str, pages: u64, locations: &[&[&str]]) -> Placed {
    let mut rig = Rig::capped(name, Built::of(Vec::new()), pages * page());
    let store = {
        let handle = rig.work.handle();
        let _entered = handle.enter();
        TreeStore::open(rig.node.clone(), TreesConfig::default()).expect("a store")
    };
    let mut ids = Vec::new();
    for (at, group) in locations.iter().enumerate() {
        let bytes = Bytes::from(image_named(&format!("d{at}")));
        let request = ImageRequest {
            digest: TreeDigest::of(&bytes),
            image: open_image(bytes),
            at: group.iter().map(|path| fs_at(path)).collect(),
            ttl: None,
            ready: false,
            staged: None,
        };
        ids.push(store.create_image(request).expect("a placement").placement);
    }
    let runtime = rig.dir.path().join("run-placed");
    std::fs::create_dir_all(&runtime).unwrap();
    let asks = Arc::new(InProcess::new(
        (rig.node.clone(), Some(store.clone())),
        rig.work.handle(),
        Duration::from_secs(30),
        Arc::new(|_| {}),
    ));
    rig.core = Arc::new(Core::new(
        rig.core.settings().clone(),
        asks,
        TreeTable::create(&runtime).unwrap(),
        &runtime,
    ));
    Placed { rig, store, ids }
}

/// The root of the placement at `location`, looked up.
fn placed_at(rig: &Placed, location: &str) -> Entry {
    match rig.core.lookup(ino::ROOT, location).unwrap() {
        Looked::Found(entry) => entry,
        other => panic!("{location}: {other:?}"),
    }
}

/// The number of the tree of the `at`-th placement at `location`.
fn placed_number(rig: &Placed, at: usize, location: &str) -> u64 {
    let key = key_of(&rig.ids[at].to_string(), location).unwrap();
    rig.core.table.find(&key).unwrap()
}

/// The runs of the mount that hold the `at`-th placement in the node's tree store.
fn holds(rig: &Placed, at: usize) -> usize {
    rig.store
        .placement(&rig.ids[at])
        .map_or(0, |placement| placement.attachments)
}

/// Asserts that a list released for the cap gives the node's hold on its placement back, and the
/// placement of the list that takes its room is held: of `a` and `b`, `a`, named first, is released
/// when `b` is looked up, which the store's tree keeps mapped (the store's two lists are over the
/// cap of one page, whichever the mount holds).
#[test]
fn a_list_released_for_the_cap_gives_the_hold_on_its_placement_back() {
    let rig = placed("tfs-lists-unplace", 1, &[&["a"], &["b"]]);
    let _ = placed_at(&rig, "a");
    assert_eq!(holds(&rig, 0), 1);
    later(&rig);
    let _ = placed_at(&rig, "b");
    assert!(rig.core.trees.get(placed_number(&rig, 0, "a")).is_none());
    assert_eq!(holds(&rig, 0), 0);
    assert_eq!(holds(&rig, 1), 1);
    assert_eq!(counted(&rig)[0], 1);
}

/// Asserts that a second location of a list mapped past the cap, which another tree of the mount
/// shows already, adds nothing: no list is released for it (not even one no request holds), and it
/// is not counted or said as mapped past the cap. The store keeps both lists mapped, two pages
/// over the cap of one, so each first lookup counts a list past the cap.
#[test]
fn a_second_location_of_a_list_the_mount_shows_releases_nothing_and_is_not_counted() {
    let rig = placed("tfs-lists-second", 1, &[&["a"], &["b", "c"]]);
    let _ = placed_at(&rig, "a");
    let held = rig.core.tree(placed_number(&rig, 0, "a")).unwrap();
    later(&rig);
    let _ = placed_at(&rig, "b");
    assert_eq!(counted(&rig), [0, 0, 2]);
    drop(held);
    later(&rig);
    let log = Logged::default();
    let _logging = logged(&log, tracing::Level::WARN);
    rig.clock.advance(Duration::from_millis(SAY_EVERY_MS));
    let _ = placed_at(&rig, "c");
    assert!(rig.core.trees.get(placed_number(&rig, 0, "a")).is_some());
    assert_eq!(counted(&rig), [0, 0, 2]);
    assert_eq!(rig.core.lists.mapped.mapped().lists, 2);
    assert_eq!(log.text(), "");
}

/// Asserts that a released tree of a placement that has ended since is `ENOENT`, as after the
/// sweep for idle trees, and stays out of memory.
#[test]
fn a_released_placement_tree_whose_placement_ended_is_enoent() {
    let rig = placed("tfs-lists-ended", 1, &[&["a"], &["b"]]);
    let a = placed_at(&rig, "a");
    later(&rig);
    let _ = placed_at(&rig, "b");
    assert_eq!(holds(&rig, 0), 0);
    rig.store.release(&rig.ids[0]).unwrap();
    let _ = rig.store.sweep();
    assert_eq!(rig.core.getattr(a.attr.ino), Err(Errno::NoEnt));
    assert!(rig.core.trees.get(placed_number(&rig, 0, "a")).is_none());
}
