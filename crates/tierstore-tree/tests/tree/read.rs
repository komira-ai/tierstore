//! `Tree::read`: the reader protocol of 5.1 around a caller's stage.
//!
//! Proves, against the real mount server over a real entry store (the rig):
//! - the bytes served after publish equal the pack's, for a range inside a cell, one straddling
//!   two cells, one straddling the boundary of two 16-cell groups, one cut short by the end of the
//!   file, and a whole file;
//! - a read at or past the end of the file, of 0 bytes, of an empty file, never touches the stage;
//! - a directory or link is `NotAFile`, an index past the end `NotFound`;
//! - a read over a cell not `present` parks with the pack and range to wait for; over a `bad` cell
//!   it fails with the cell; and the reader counters of every group are raised while the stage
//!   fills and back to 0 after every outcome;
//! - a cell that leaves `present` while the stage fills makes the bytes be discarded, not
//!   committed, and the read park; the read is served once the cell is back;
//! - a stage that cannot fill ends the read with its own error;
//! - the first read of each cell is counted once per session, as ready or not.
//!
//! Catches: bytes served over a cell evicted during the read; a counter left raised after a
//! park, a failure or a discard; a read past the end of a file that reads another file's bytes.

use crate::support::{
    Events, MIB, Tree, VecStage, empty, failed, fill_failed, find, pack_bytes, parked, served,
    small_tree,
};
use tierstore_fetch_api::entry::EvictStep;
use tierstore_fetch_api::ids::{ByteRange, CellRun};
use tierstore_fetch_api::list::NodeIndex;
use tierstore_tree::testing::Rig;
use tierstore_tree::{ImageOptions, ReadFailure, TreeStats};

fn rig_of(name: &str, tree: &Tree) -> Rig {
    Rig::new(
        env!("CARGO_TARGET_TMPDIR").as_ref(),
        name,
        tree.lists().1,
        tree.pack_list(),
    )
}

fn small(name: &str) -> Rig {
    rig_of(name, &small_tree())
}

fn cells(first: u32, count: u32) -> CellRun {
    CellRun { first, count }
}

#[test]
fn a_range_inside_one_cell_is_served_with_the_packs_bytes() {
    let rig = small("read-inside");
    let tree = rig.tree(ImageOptions::default());
    rig.publish(0, cells(0, 3));
    let cc = find(tree.image(), "bin/cc");
    let events = Events::default();
    let bytes = served(tree.read(cc, 100, 1000, VecStage::new(&events)));
    assert_eq!(bytes, rig.bytes(0)[100..1100]);
    assert_eq!(events.all(), ["fill", "commit"]);
    assert_eq!(rig.readers(0, 0), 0);
    let stats = tree.stats();
    assert_eq!((stats.served, stats.parked, stats.waits), (1, 0, 0));
}

#[test]
fn a_range_straddling_two_cells_is_served_whole() {
    let rig = small("read-straddle");
    let tree = rig.tree(ImageOptions::default());
    rig.publish(0, cells(0, 2));
    let cc = find(tree.image(), "bin/cc");
    let at = MIB - 10;
    let events = Events::default();
    let bytes = served(tree.read(cc, at, 20, VecStage::new(&events)));
    assert_eq!(bytes, rig.bytes(0)[at as usize..at as usize + 20]);
}

#[test]
fn a_file_not_at_the_start_of_its_pack_reads_from_its_own_offset() {
    let rig = small("read-offset-in-pack");
    let tree = rig.tree(ImageOptions::default());
    rig.publish(0, cells(1, 2));
    let ld = find(tree.image(), "bin/ld");
    let events = Events::default();
    let bytes = served(tree.read(ld, 5, 100, VecStage::new(&events)));
    let from = (MIB + 4096 + 5) as usize;
    assert_eq!(bytes, rig.bytes(0)[from..from + 100]);
}

#[test]
fn a_read_longer_than_the_rest_of_the_file_is_cut_at_its_end() {
    let rig = small("read-cut");
    let tree = rig.tree(ImageOptions::default());
    rig.publish(1, cells(0, 1));
    let a = find(tree.image(), "lib/a.txt");
    let events = Events::default();
    let bytes = served(tree.read(a, 50, 100, VecStage::new(&events)));
    assert_eq!(
        bytes,
        rig.bytes(1)[50..60],
        "a.txt ends at 60, b.txt's bytes follow"
    );
    let b = find(tree.image(), "lib/b.txt");
    let bytes = served(tree.read(b, 0, 1 << 20, VecStage::new(&events)));
    assert_eq!(bytes, rig.bytes(1)[60..100]);
}

#[test]
fn a_read_at_or_past_the_end_or_of_nothing_is_empty_and_leaves_the_stage_unused() {
    let rig = small("read-empty");
    let tree = rig.tree(ImageOptions::default());
    let a = find(tree.image(), "lib/a.txt");
    let empty_file = find(tree.image(), "lib/empty");
    let events = Events::default();
    empty(tree.read(a, 60, 10, VecStage::new(&events)));
    empty(tree.read(a, 61, 10, VecStage::new(&events)));
    empty(tree.read(a, u64::MAX, 10, VecStage::new(&events)));
    empty(tree.read(a, 0, 0, VecStage::new(&events)));
    empty(tree.read(empty_file, 0, 100, VecStage::new(&events)));
    assert_eq!(events.all(), Vec::<&str>::new());
    assert_eq!(tree.stats(), TreeStats::default());
}

#[test]
fn a_directory_a_link_and_an_index_past_the_end_are_not_files() {
    let rig = small("read-not-file");
    let tree = rig.tree(ImageOptions::default());
    let events = Events::default();
    for path in ["lib", "lib/self", "tmp"] {
        let node = find(tree.image(), path);
        assert_eq!(
            failed(tree.read(node, 0, 10, VecStage::new(&events))),
            ReadFailure::NotAFile,
            "{path}"
        );
    }
    let past = NodeIndex(tree.image().len());
    assert_eq!(
        failed(tree.read(past, 0, 10, VecStage::new(&events))),
        ReadFailure::NotFound
    );
    assert_eq!(events.all(), Vec::<&str>::new());
}

#[test]
fn a_read_over_a_cell_not_present_parks_with_what_to_wait_for_and_gives_the_counters_back() {
    let rig = small("read-park");
    let tree = rig.tree(ImageOptions::default());
    let cc = find(tree.image(), "bin/cc");
    let events = Events::default();
    let wait = parked(tree.read(cc, 100, 1000, VecStage::new(&events)));
    assert_eq!(wait.pack(), 0);
    assert_eq!(
        wait.range(),
        ByteRange {
            offset: 100,
            len: 1000
        }
    );
    assert_eq!(events.all(), Vec::<&str>::new());
    assert_eq!(rig.readers(0, 0), 0);
    let stats = tree.stats();
    assert_eq!(
        (stats.parked, stats.served, stats.recheck_discarded),
        (1, 0, 0)
    );

    // Cell 0 present, cell 1 not: a read over both parks too, still holding no counter.
    rig.publish(0, cells(0, 1));
    let wait = parked(tree.read(cc, MIB - 10, 20, VecStage::new(&events)));
    assert_eq!(
        wait.range(),
        ByteRange {
            offset: MIB - 10,
            len: 20
        }
    );
    assert_eq!(rig.readers(0, 0), 0);
    assert_eq!(tree.stats().parked, 2);
}

#[test]
fn the_counters_of_every_group_the_read_covers_are_raised_while_the_stage_fills() {
    // 18 cells: the groups are cells 0-15 and 16-17.
    let big = Tree::default()
        .pack("big", pack_bytes(9, (17 * MIB + 5) as usize))
        .file("f", "big", 15 * MIB + MIB / 2, MIB);
    let rig = rig_of("read-groups", &big);
    let tree = rig.tree(ImageOptions::default());
    rig.publish(0, cells(15, 2));
    let f = find(tree.image(), "f");
    let seen = std::cell::Cell::new((u64::MAX, u64::MAX));
    let hook = || seen.set((rig.readers(0, 0), rig.readers(0, 1)));
    let events = Events::default();
    let bytes = served(tree.read(f, 0, 1 << 20, VecStage::during_fill(&events, &hook)));
    let from = (15 * MIB + MIB / 2) as usize;
    assert_eq!(bytes, rig.bytes(0)[from..from + (1 << 20)]);
    assert_eq!(seen.get(), (1, 1));
    assert_eq!((rig.readers(0, 0), rig.readers(0, 1)), (0, 0));
}

#[test]
fn a_read_over_a_bad_cell_fails_with_the_cell() {
    let rig = small("read-bad");
    let tree = rig.tree(ImageOptions::default());
    rig.publish(0, cells(0, 1));
    rig.mark_bad(0, 1);
    let cc = find(tree.image(), "bin/cc");
    let events = Events::default();
    assert_eq!(
        failed(tree.read(cc, MIB - 10, 20, VecStage::new(&events))),
        ReadFailure::Bad { cell: 1 }
    );
    assert_eq!(rig.readers(0, 0), 0);
    assert_eq!(events.all(), Vec::<&str>::new());
    // The cells before it still serve.
    assert_eq!(
        served(tree.read(cc, 0, 10, VecStage::new(&events))),
        rig.bytes(0)[..10]
    );
}

#[test]
fn bytes_read_over_a_cell_that_stopped_being_present_are_discarded_and_the_read_parks() {
    let rig = small("read-recheck");
    let tree = rig.tree(ImageOptions::default());
    rig.publish(0, cells(0, 2));
    let cc = find(tree.image(), "bin/cc");
    let evict = || assert!(rig.evict_step(0, 1, EvictStep::Begin));
    let events = Events::default();
    let wait = parked(tree.read(cc, MIB - 10, 20, VecStage::during_fill(&events, &evict)));
    assert_eq!(events.all(), ["fill", "discard"]);
    assert_eq!(wait.pack(), 0);
    assert_eq!(rig.readers(0, 0), 0);
    let stats = tree.stats();
    assert_eq!(
        (stats.recheck_discarded, stats.parked, stats.served),
        (1, 1, 0)
    );

    // The evictor backs out: the cell is present again and the same read is served.
    assert!(rig.evict_step(0, 1, EvictStep::Abort));
    let events = Events::default();
    let bytes = served(tree.read(cc, MIB - 10, 20, VecStage::new(&events)));
    assert_eq!(
        bytes,
        rig.bytes(0)[(MIB - 10) as usize..(MIB + 10) as usize]
    );
    assert_eq!(events.all(), ["fill", "commit"]);
    assert_eq!(tree.stats().served, 1);
}

#[test]
fn a_stage_that_cannot_fill_ends_the_read_with_its_error_and_gives_the_counters_back() {
    let rig = small("read-fill-failed");
    let tree = rig.tree(ImageOptions::default());
    rig.publish(0, cells(0, 1));
    let cc = find(tree.image(), "bin/cc");
    let events = Events::default();
    let error = fill_failed(tree.read(cc, 0, 10, VecStage::failing(&events)));
    assert_eq!(error.to_string(), "fill failed");
    assert_eq!(rig.readers(0, 0), 0);
    assert_eq!(events.all(), Vec::<&str>::new());
    assert_eq!(tree.stats().served, 0);
}

#[test]
fn the_first_read_of_each_cell_is_counted_once_as_ready_or_not() {
    let rig = small("read-first-touch");
    let tree = rig.tree(ImageOptions::default());
    rig.publish(0, cells(0, 1));
    let cc = find(tree.image(), "bin/cc");
    let events = Events::default();
    let first = |tree: &tierstore_tree::Tree| {
        let s = tree.stats();
        (s.first_touch_ready, s.first_touch_unready)
    };
    served(tree.read(cc, 0, 100, VecStage::new(&events)));
    served(tree.read(cc, 50, 100, VecStage::new(&events)));
    assert_eq!(first(&tree), (1, 0), "cell 0 twice is one first touch");
    // Cells 0 and 1: only 1 is new, and it is not present.
    parked(tree.read(cc, MIB - 10, 20, VecStage::new(&events)));
    parked(tree.read(cc, MIB - 10, 20, VecStage::new(&events)));
    assert_eq!(first(&tree), (1, 1));
    // Once present, cell 1 is no first touch any more, whichever way it was first read.
    rig.publish(0, cells(1, 1));
    served(tree.read(cc, MIB, 10, VecStage::new(&events)));
    assert_eq!(first(&tree), (1, 1));
    // A cell of the other pack is counted on its own.
    rig.publish(1, cells(0, 1));
    let a = find(tree.image(), "lib/a.txt");
    served(tree.read(a, 0, 10, VecStage::new(&events)));
    assert_eq!(first(&tree), (2, 1));
}
