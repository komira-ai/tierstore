//! The budget of mapped tree lists, `fs.tree_list_ram_bytes`, end to end through the core: the
//! fake fill socket places a list of its own at each location, every list one page, and the rig's
//! cap is a number of pages. The use stamps are the rig's clock, advanced between uses so that
//! they never tie.

use std::time::Duration;

use super::super::blob::SAY_EVERY_MS;
use super::super::placed::key_of;
use super::super::{Entry, Errno, Looked};
use crate::TreeDigest;
use crate::server::local_cache::trees::{Mapped, image_path};
use crate::server::treefs::ino;
use crate::server::treefs::locked;
use crate::server::treefs::testing::{
    Built, Logged, Rig, TREE_IDLE, image_named, logged, open_cached, sample, until,
};
use crate::tree_image_list::in_pages;

/// The bytes one list of the rigs takes mapped: a page.
fn page() -> u64 {
    in_pages(1)
}

/// The placement id of the `at`-th location.
fn id(at: usize) -> String {
    format!("{:032x}", at + 1)
}

/// A rig that keeps at most `pages` pages of lists mapped, whose node places a list of its own at
/// each of `locations`, the `at`-th as placement [`id`]`(at)`; the digest of each list.
fn placed(name: &str, pages: u64, locations: &[&str]) -> (Rig, Vec<TreeDigest>) {
    placed_over(name, pages, locations, Built::of(Vec::new()))
}

/// [`placed`], with the trees of manifests `built` upstream.
fn placed_over(name: &str, pages: u64, locations: &[&str], built: Built) -> (Rig, Vec<TreeDigest>) {
    let rig = Rig::capped(name, built, pages * page());
    let mut trees = Vec::new();
    for (at, location) in locations.iter().enumerate() {
        let bytes = image_named(&format!("d{at}"));
        assert!(bytes.len() as u64 <= page());
        let tree = TreeDigest::of(&bytes);
        let file = image_path(&rig.cache, &tree);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, &bytes).unwrap();
        answer(
            &rig,
            &format!("place mount-1 {location}"),
            &format!("tree {}", id(at)),
        );
        let image = format!(r#"image {tree} ["{location}"]"#);
        answer(&rig, &format!("image {}", id(at)), &image);
        trees.push(tree);
    }
    (rig, trees)
}

/// Answers the requests `request` (a whole line, or a verb) of the fake fill socket of `rig` with
/// `answer`.
fn answer(rig: &Rig, request: &str, answer: &str) {
    let _ = locked(&rig.upstream.answers).insert(request.to_owned(), answer.to_owned());
}

/// The root of the placement at `location`, looked up.
fn found(rig: &Rig, location: &str) -> Entry {
    match rig.core.lookup(ino::ROOT, location).unwrap() {
        Looked::Found(entry) => entry,
        other => panic!("{location}: {other:?}"),
    }
}

/// The number of the tree of the `at`-th placement at `location`.
fn number(rig: &Rig, at: usize, location: &str) -> u64 {
    let key = key_of(&id(at), location).unwrap();
    rig.core.table.find(&key).unwrap()
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

/// Asserts that a list that does not fit under the cap releases the list no request has named for
/// longest, of those with no run and no request: of `a` and `b`, `a` named again after `b`, `b`
/// is released, its implicit attachment ended and the node's hold on its placement given back;
/// `a` and the new `c` stay mapped, two pages, and one release for the cap is counted.
#[test]
fn a_list_past_the_cap_releases_the_least_recently_used_list_with_no_run_and_no_request() {
    let (rig, _) = placed("tfs-lists-lru", 2, &["a", "b", "c"]);
    let a = found(&rig, "a");
    later(&rig);
    let _ = found(&rig, "b");
    later(&rig);
    rig.core.getattr(a.attr.ino).unwrap();
    later(&rig);
    let _ = found(&rig, "c");
    let [a, b, c] = [
        number(&rig, 0, "a"),
        number(&rig, 1, "b"),
        number(&rig, 2, "c"),
    ];
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
    let unplaced = format!("unplace mount-1 {}", id(1));
    assert_eq!(rig.upstream.asked("unplace"), [unplaced]);
}

/// Asserts that the line of a lookup that attached a tree list says the bytes of the list, after
/// its time, so that the time of an attach can be read by the size of its list (a lookup of a tree
/// of manifests says none: `core/implicit_attach_tests.rs`).
#[test]
fn the_line_of_a_lookup_of_a_list_says_its_bytes() {
    let (rig, _) = placed("tfs-lists-line", 2, &["a"]);
    let log = Logged::default();
    let _logging = logged(&log, tracing::Level::INFO);
    let _ = found(&rig, "a");
    let key = key_of(&id(0), "a").unwrap();
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
            "tierstore fs: lookup of {key}: ok in 0 ms; 0 manifests: 0 from the cache, 0 filled, \
             0 not had; a tree list of {bytes} bytes"
        )]
    );
}

/// Asserts that a list whose tree a run has attached is never released for the cap, even when no
/// request has named it for longest: `b` is released in its place.
#[test]
fn a_list_a_run_attached_is_never_released_for_the_cap() {
    let (rig, _) = placed("tfs-lists-run", 2, &["a", "b", "c"]);
    let _ = found(&rig, "a");
    later(&rig);
    let _ = found(&rig, "b");
    later(&rig);
    let a = number(&rig, 0, "a");
    let key = key_of(&id(0), "a").unwrap();
    let _ = locked(&rig.core.state).attachments.add("run-1", a, key);
    let _ = found(&rig, "c");
    assert!(in_memory(&rig, a));
    assert!(!in_memory(&rig, number(&rig, 1, "b")));
    assert_eq!(counted(&rig), [1, 0, 0]);
}

/// Asserts that a tree of manifests, which holds no list, is never released for the cap, even
/// when no request has named it for longest and only a lookup attached it: the list is mapped
/// past the cap instead, and the tree of manifests stays attached.
#[test]
fn a_tree_of_manifests_is_never_released_for_the_cap() {
    let (rig, _) = placed_over("tfs-lists-manifests", 1, &["a", "b"], sample());
    let root = locked(&rig.upstream.built).root.clone();
    let tree = match rig.core.lookup(ino::TREES, &root).unwrap() {
        Looked::Found(entry) => ino::split(entry.attr.ino).0,
        other => panic!("{other:?}"),
    };
    later(&rig);
    let _ = found(&rig, "a");
    let held = rig.core.tree(number(&rig, 0, "a")).unwrap();
    later(&rig);
    let _ = found(&rig, "b");
    assert!(in_memory(&rig, tree));
    assert!(rig.core.implicit().contains(&tree));
    assert_eq!(counted(&rig), [0, 0, 1]);
    drop(held);
}

/// Asserts that a list a lookup attaches again while it is being released (the release has ended
/// its attachment and is giving its hold back) is not released: it stays in memory, attached,
/// and is not counted as released; the new list, finding no other to release, is mapped past the
/// cap.
#[test]
fn a_list_attached_again_while_it_is_released_stays() {
    let (rig, _) = placed("tfs-lists-again-attached", 1, &["a", "b"]);
    let _ = found(&rig, "a");
    let a = number(&rig, 0, "a");
    later(&rig);
    rig.upstream.gate_verb("unplace");
    let core = std::sync::Arc::clone(&rig.core);
    let releasing = std::thread::spawn(move || {
        let _ = core.lookup(ino::ROOT, "b");
    });
    rig.upstream.wait_for_verb("unplace", 1);
    let core = std::sync::Arc::clone(&rig.core);
    let again = std::thread::spawn(move || {
        let _ = core.lookup(ino::ROOT, "a");
    });
    until(|| rig.core.implicit().contains(&a));
    rig.upstream.permit_verb("unplace", 1);
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
    let (rig, _) = placed("tfs-lists-flight", 2, &["a", "b", "c", "d"]);
    let _ = found(&rig, "a");
    later(&rig);
    let _ = found(&rig, "b");
    later(&rig);
    let a = number(&rig, 0, "a");
    let request = rig.core.tree(a).unwrap();
    let _ = found(&rig, "c");
    assert!(in_memory(&rig, a));
    assert!(!in_memory(&rig, number(&rig, 1, "b")));
    drop(request);
    later(&rig);
    let _ = found(&rig, "d");
    assert!(!in_memory(&rig, a));
    assert_eq!(counted(&rig), [2, 0, 0]);
}

/// Asserts that a mount past the cap with every mapped list held still attaches the new list: it
/// is found and mapped past the cap, counted, and said at warn once in `SAY_EVERY_MS` with the
/// count of those not said; the held lists stay.
#[test]
fn a_mount_past_the_cap_with_every_list_held_still_attaches_and_is_counted() {
    let (rig, trees) = placed("tfs-lists-over", 1, &["a", "b", "c", "d"]);
    let log = Logged::default();
    let _logging = logged(&log, tracing::Level::WARN);
    let _ = found(&rig, "a");
    let held: Vec<_> = (0..3)
        .map(|at| {
            let location = ["a", "b", "c"][at];
            if at > 0 {
                let _ = found(&rig, location);
            }
            rig.core.tree(number(&rig, at, location)).unwrap()
        })
        .collect();
    rig.clock.advance(Duration::from_millis(SAY_EVERY_MS));
    let _ = found(&rig, "d");
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
            line(&trees[1], 2 * page()),
            format!(
                "{}; 1 more tree list mapped past the cap since the last line",
                line(&trees[3], 4 * page())
            ),
        ]
    );
    drop(held);
}

/// Asserts that a second location of a list mapped past the cap, which another tree of the mount
/// shows already, adds nothing: no list is released for it (not even one no request holds), and
/// it is not counted or said as mapped past the cap.
#[test]
fn a_second_location_of_a_list_the_mount_shows_releases_nothing_and_is_not_counted() {
    let (rig, trees) = placed("tfs-lists-second", 1, &["a", "b"]);
    let image = format!(r#"image {} ["b", "c"]"#, trees[1]);
    answer(&rig, &format!("image {}", id(1)), &image);
    answer(&rig, "place mount-1 c", &format!("tree {}", id(1)));
    let _ = found(&rig, "a");
    let held = rig.core.tree(number(&rig, 0, "a")).unwrap();
    later(&rig);
    let _ = found(&rig, "b");
    assert_eq!(counted(&rig), [0, 0, 1]);
    drop(held);
    later(&rig);
    let log = Logged::default();
    let _logging = logged(&log, tracing::Level::WARN);
    rig.clock.advance(Duration::from_millis(SAY_EVERY_MS));
    let _ = found(&rig, "c");
    assert!(in_memory(&rig, number(&rig, 0, "a")));
    assert_eq!(counted(&rig), [0, 0, 1]);
    assert_eq!(rig.core.lists.mapped.mapped().lists, 2);
    assert_eq!(log.text(), "");
}

/// Asserts that a released list is read again by the next request that names its tree's number,
/// under the same number and with the same answers, and mapped again, under the cap like any
/// other: the list released in its turn is the one it released.
#[test]
fn a_released_list_is_read_again_by_the_next_request_under_its_tree_number() {
    let (rig, _) = placed("tfs-lists-again", 1, &["a", "b"]);
    let a = found(&rig, "a");
    let number_a = number(&rig, 0, "a");
    later(&rig);
    let _ = found(&rig, "b");
    assert!(!in_memory(&rig, number_a));
    let again = rig.core.getattr(a.attr.ino).unwrap();
    assert_eq!(again.attr, a.attr);
    assert!(in_memory(&rig, number_a));
    assert_eq!(number(&rig, 0, "a"), number_a);
    assert!(!in_memory(&rig, number(&rig, 1, "b")));
    assert_eq!(rig.core.lists.mapped.mapped().lists, 1);
    assert_eq!(counted(&rig), [2, 0, 0]);
}

/// Asserts that a released tree of a placement that has ended since is `ENOENT`, as after the
/// sweep for idle trees, and stays out of memory.
#[test]
fn a_released_placement_tree_whose_placement_ended_is_enoent() {
    let (rig, _) = placed("tfs-lists-ended", 1, &["a", "b"]);
    let a = found(&rig, "a");
    later(&rig);
    let _ = found(&rig, "b");
    answer(&rig, &format!("image {}", id(0)), "none");
    assert_eq!(rig.core.getattr(a.attr.ino), Err(Errno::NoEnt));
    assert!(!in_memory(&rig, number(&rig, 0, "a")));
}

/// Asserts that the sweep for idle trees counts the lists it releases, and a tree of manifests
/// it releases not; and that the report tells the node the lists mapped and their bytes, the cap,
/// and the running totals: `treelists <mount> <process> <mapped> <bytes> <cap> <cap> <idle>
/// <over>`.
#[test]
fn the_sweep_counts_the_lists_it_releases_and_the_report_tells_the_node() {
    let (rig, _) = placed("tfs-lists-idle", 2, &["a", "b", "c"]);
    let _ = found(&rig, "a");
    later(&rig);
    let _ = found(&rig, "b");
    later(&rig);
    let _ = found(&rig, "c");
    assert_eq!(counted(&rig), [1, 0, 0]);
    rig.clock.advance(TREE_IDLE);
    assert_eq!(rig.core.detach_idle(), 2);
    assert_eq!(counted(&rig), [1, 2, 0]);
    rig.core.look_around();
    until(|| !rig.upstream.asked("treelists").is_empty());
    let process = std::process::id();
    assert_eq!(
        rig.upstream.asked("treelists"),
        [format!(
            "treelists mount-1 {process} 0 0 {} 1 2 0",
            2 * page()
        )]
    );
}

/// Asserts that no list is released while a successor takes over: the room is not made, and the
/// list in memory stays; once the hold ends, it is released.
#[test]
fn no_list_is_released_while_a_successor_takes_over() {
    let (rig, _) = placed("tfs-lists-held", 1, &["a"]);
    let _ = found(&rig, "a");
    let a = number(&rig, 0, "a");
    rig.core.hold(true);
    assert!(!rig.core.make_room(page()));
    assert!(in_memory(&rig, a));
    rig.core.hold(false);
    assert!(rig.core.make_room(page()));
    assert!(!in_memory(&rig, a));
    assert_eq!(counted(&rig), [1, 0, 0]);
}

/// Asserts that a list whose lookups the kernel has all forgotten leaves memory as its hold is
/// given back, and is counted as released for the cap all the same.
#[test]
fn a_forgotten_list_leaves_at_its_unplace_and_is_counted_released() {
    let (rig, _) = placed("tfs-lists-forgotten", 1, &["a", "b"]);
    let a = found(&rig, "a");
    rig.core.forget(a.attr.ino, 1);
    assert!(in_memory(&rig, number(&rig, 0, "a")));
    later(&rig);
    let _ = found(&rig, "b");
    assert!(!in_memory(&rig, number(&rig, 0, "a")));
    assert_eq!(counted(&rig), [1, 0, 0]);
}

/// Asserts that a list released for the cap says the opens that failed under its implicit
/// attachment, as the sweep for idle trees does when that attachment ends.
#[test]
fn a_list_released_for_the_cap_says_the_opens_that_failed_under_it() {
    let (rig, _) = placed("tfs-lists-failed", 1, &["a", "b"]);
    let a = found(&rig, "a");
    answer(&rig, "span", "upstream 404");
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
    let _ = found(&rig, "b");
    let key = key_of(&id(0), "a").unwrap();
    let number = number(&rig, 0, "a");
    let hello = TreeDigest::of(b"hello");
    assert_eq!(
        log.text().lines().collect::<Vec<_>>(),
        [format!(
            "tierstore fs: the attachment of the tree {key} (number {number}) by a lookup ended \
             with 1 failed open: /bin/tool ({hello}: upstream 404)"
        )]
    );
}

/// The `treelists` reports the node was told, once there is one.
fn reports(rig: &Rig) -> Vec<String> {
    until(|| !rig.upstream.asked("treelists").is_empty());
    rig.upstream.asked("treelists")
}

/// Asserts that a cap lowered while the mount serves releases at once the lists over it that
/// nothing holds, the one no request has named for longest first (`a`, then `b`), counts each as
/// released for the cap, and tells the node the new cap with what stays mapped, `c` alone.
/// Catches a lowered cap that waits for the next list to be mapped, or that the node is not told.
#[test]
fn a_lowered_cap_releases_at_once_the_lists_over_it_and_tells_the_node() {
    let (rig, _) = placed("tfs-lists-lowered", 3, &["a", "b", "c"]);
    for location in ["a", "b", "c"] {
        let _ = found(&rig, location);
        later(&rig);
    }
    rig.core.set_tree_list_ram_bytes(page());
    assert!(!in_memory(&rig, number(&rig, 0, "a")));
    assert!(!in_memory(&rig, number(&rig, 1, "b")));
    assert!(in_memory(&rig, number(&rig, 2, "c")));
    assert_eq!(counted(&rig), [2, 0, 0]);
    let process = std::process::id();
    let cap = page();
    assert_eq!(
        reports(&rig),
        [format!("treelists mount-1 {process} 1 {cap} {cap} 2 0 0")]
    );
}

/// Asserts that a cap raised while the mount serves releases nothing and lets in a list the cap
/// before had no room for: `a` stays mapped beside `b`, nothing is counted, and the node is told
/// the new cap.
#[test]
fn a_raised_cap_releases_nothing_and_lets_another_list_in() {
    let (rig, _) = placed("tfs-lists-raised", 1, &["a", "b"]);
    let _ = found(&rig, "a");
    later(&rig);
    rig.core.set_tree_list_ram_bytes(2 * page());
    let _ = found(&rig, "b");
    assert!(in_memory(&rig, number(&rig, 0, "a")));
    assert!(in_memory(&rig, number(&rig, 1, "b")));
    assert_eq!(counted(&rig), [0, 0, 0]);
    let process = std::process::id();
    let page = page();
    assert_eq!(
        reports(&rig),
        [format!(
            "treelists mount-1 {process} 1 {page} {} 0 0 0",
            2 * page
        )]
    );
}

/// Asserts that a lowered cap leaves a list a run attached mapped, however long no request has
/// named it, and releases the one nothing holds in its place; and that a cap under the one held
/// list (a byte: the core takes any cap, its caller holds it to the file's bound) releases
/// nothing more, and counts nothing mapped past the cap, as no list was mapped.
#[test]
fn a_lowered_cap_leaves_the_lists_that_are_held() {
    let (rig, _) = placed("tfs-lists-lowered-held", 2, &["a", "b"]);
    let _ = found(&rig, "a");
    later(&rig);
    let _ = found(&rig, "b");
    let a = number(&rig, 0, "a");
    let key = key_of(&id(0), "a").unwrap();
    let _ = locked(&rig.core.state).attachments.add("run-1", a, key);
    rig.core.set_tree_list_ram_bytes(page());
    assert!(in_memory(&rig, a));
    assert!(!in_memory(&rig, number(&rig, 1, "b")));
    assert_eq!(counted(&rig), [1, 0, 0]);
    rig.core.set_tree_list_ram_bytes(1);
    assert!(in_memory(&rig, a));
    assert_eq!(counted(&rig), [1, 0, 0]);
}
