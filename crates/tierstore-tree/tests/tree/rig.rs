//! The rig of `tierstore_tree::testing` itself: what its helpers do, and that they stop the test
//! that misuses them with a message that says what was wrong.
//!
//! Proves: an evicted cell is read as not present until it is published again; a rig of a list
//! that does not open, or of the wrong number of packs' bytes, panics saying so; an entry asked
//! for before the pack was opened, a cell published twice, a cell marked bad that is not
//! claimable, and an eviction of a cell that is not present each panic saying which.
//!
//! Catches: a rig whose helper fails silently and lets a test pass over a state it never made.

use crate::support::{Events, VecStage, find, parked, served, small_tree};
use tierstore_fetch_api::ids::CellRun;
use tierstore_tree::ImageOptions;
use tierstore_tree::testing::Rig;

fn rig(name: &str) -> Rig {
    let tree = small_tree();
    Rig::new(
        env!("CARGO_TARGET_TMPDIR").as_ref(),
        name,
        tree.lists().1,
        tree.pack_list(),
    )
}

#[test]
fn an_evicted_cell_is_not_present_until_it_is_published_again() {
    let rig = rig("rig-evict");
    let tree = rig.tree(ImageOptions::default());
    rig.publish_all();
    let events = Events::default();
    let cc = find(tree.image(), "bin/cc");
    served(tree.read(cc, 0, 10, VecStage::new(&events)));
    rig.evict(0, 0);
    let wait = parked(tree.read(cc, 0, 10, VecStage::new(&events)));
    assert_eq!(wait.pack(), 0);
    rig.publish(0, CellRun { first: 0, count: 1 });
    served(tree.read(cc, 0, 10, VecStage::new(&events)));
    assert_eq!(tree.stats().served, 2);
}

#[test]
fn the_rig_names_its_tree_dir_and_image() {
    let rig = rig("rig-facts");
    assert_eq!(
        rig.tree_id(),
        &crate::support::tree_id(&small_tree().lists().1)
    );
    assert!(rig.dir().is_dir());
    assert!(format!("{rig:?}").starts_with("Rig"));
    // The image file is written once and opened again each time.
    assert_eq!(
        rig.image(ImageOptions::default()).len(),
        rig.image(ImageOptions { lock: true }).len()
    );
    assert_ne!(rig.id(0).key, rig.id(1).key);
    assert_eq!(rig.bytes(1).len(), 100);
}

#[test]
#[should_panic(expected = "rig list")]
fn a_list_that_does_not_open_stops_the_rig() {
    let _ = Rig::new(
        env!("CARGO_TARGET_TMPDIR").as_ref(),
        "rig-bad-list",
        vec![1, 2, 3],
        vec![],
    );
}

#[test]
#[should_panic(expected = "rig: one bytes per pack")]
fn bytes_for_the_wrong_number_of_packs_stop_the_rig() {
    let tree = small_tree();
    let _ = Rig::new(
        env!("CARGO_TARGET_TMPDIR").as_ref(),
        "rig-bad-packs",
        tree.lists().1,
        vec![],
    );
}

#[test]
#[should_panic(expected = "rig: pack 0 not open")]
fn the_entry_of_a_pack_not_opened_yet_stops_the_test() {
    let _ = rig("rig-not-open").entry(0);
}

#[test]
#[should_panic(expected = "rig: cell 0 of pack 0 not claimable")]
fn publishing_a_cell_twice_stops_the_test() {
    let rig = rig("rig-twice");
    let _tree = rig.tree(ImageOptions::default());
    rig.publish(0, CellRun { first: 0, count: 1 });
    rig.publish(0, CellRun { first: 0, count: 1 });
}

#[test]
#[should_panic(expected = "rig: cell 0 of pack 0 not claimable")]
fn marking_a_present_cell_bad_stops_the_test() {
    let rig = rig("rig-bad-present");
    let _tree = rig.tree(ImageOptions::default());
    rig.publish(0, CellRun { first: 0, count: 1 });
    rig.mark_bad(0, 0);
}

#[test]
#[should_panic(expected = "rig: cell 1 not present")]
fn evicting_a_cell_that_is_not_present_stops_the_test() {
    let rig = rig("rig-evict-absent");
    let _tree = rig.tree(ImageOptions::default());
    rig.evict(0, 1);
}
