//! A file's record changed after the tree was opened (the cache never does this): the read of it
//! fails by what the changed record says, and never panics.
//!
//! Proves: a file changed to a pack the tree lacks fails to read with `NotFound`, and so does one whose bytes would end past the last byte number; one whose bytes
//! lie past its pack's end, or at an offset no cell number fits, ends with the stage's own failure
//! and no stage output; no reader counter is left raised.
//!
//! Catches: an index past the packs; a cell number past the state page that panics or counts.

use crate::support::{Events, VecStage, failed, fill_failed, find, patch, section, small_tree};
use tierstore_fetch_api::list::NodeIndex;
use tierstore_tree::testing::Rig;
use tierstore_tree::{ImageOptions, ReadFailure};

fn rig(name: &str) -> Rig {
    let tree = small_tree();
    Rig::new(
        env!("CARGO_TARGET_TMPDIR").as_ref(),
        name,
        tree.lists().1,
        tree.pack_list(),
    )
}

fn patch_file_record(rig: &Rig, node: NodeIndex, at: u64, bytes: &[u8]) {
    let l2 = small_tree().lists().1;
    patch(
        &rig.dir().join("tree.l2"),
        section(&l2, 0).0 + node.0 * 32 + at,
        bytes,
    );
}

#[test]
fn a_file_changed_to_a_pack_the_tree_lacks_fails_to_read_as_not_found() {
    let rig = rig("changed-pack");
    let tree = rig.tree(ImageOptions::default());
    let a = find(tree.image(), "lib/a.txt");
    patch_file_record(&rig, a, 8, &9u32.to_le_bytes());
    let step = tree.read(a, 0, 10, VecStage::new(&Events::default()));
    assert_eq!(failed(step), ReadFailure::NotFound);
}

#[test]
fn a_file_changed_to_lie_past_its_packs_end_ends_with_the_stages_failure() {
    let rig = rig("changed-offset");
    let tree = rig.tree(ImageOptions::default());
    rig.publish_all();
    let a = find(tree.image(), "lib/a.txt");
    // Past the end of the pack, with cell numbers beyond the state page.
    patch_file_record(&rig, a, 16, &(3 * (1u64 << 20)).to_le_bytes());
    let events = Events::default();
    fill_failed(tree.read(a, 0, 10, VecStage::new(&events)));
    // At an offset no cell number fits in a u32.
    patch_file_record(&rig, a, 16, &(u64::MAX - 200).to_le_bytes());
    fill_failed(tree.read(a, 0, 10, VecStage::new(&events)));
    assert_eq!(events.all(), Vec::<&str>::new());
    assert_eq!(rig.readers(1, 0), 0);
}

#[test]
fn a_file_changed_to_end_past_the_last_byte_number_fails_to_read_as_not_found() {
    let rig = rig("changed-overflow");
    let tree = rig.tree(ImageOptions::default());
    rig.publish_all();
    let a = find(tree.image(), "lib/a.txt");
    let events = Events::default();
    patch_file_record(&rig, a, 16, &(u64::MAX - 10).to_le_bytes());
    let step = tree.read(a, 0, 10, VecStage::new(&events));
    assert_eq!(failed(step), ReadFailure::NotFound);
    assert_eq!(events.all(), Vec::<&str>::new());
    assert_eq!(rig.readers(1, 0), 0);
}
