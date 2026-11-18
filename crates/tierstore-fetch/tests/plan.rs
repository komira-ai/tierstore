//! Proves the part plan of one fill (design 4.4): which cells each part covers, for small and
//! large packs, first touches at the start and in the middle, odd part sizes and cuts, plus the
//! size helpers around it.
//!
//! Catches: a small pack whose first piece is shorter than two or longer than four cells; a ramp
//! that does not double, ignores the P/2 cap or is not trimmed at the grid boundary; a runt after
//! the ramp; cells before the first touch left out or cut off the grid; a part size that is not
//! rounded down to cells; pieces that overlap, leave a gap or are unsorted; a cut that is
//! ignored, or applied past the pack; wrong size classes or cell counts at their boundaries; a
//! piece lookup that is off by one.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::collections::BTreeSet;

use tierstore_fetch::EngineConfig;
use tierstore_fetch::plan::{CELL_BYTES, Piece, PlanInput, cell_count, piece_of, plan, size_class};

const MIB: u64 = 1 << 20;

fn pieces(v: &[(u32, u32)]) -> Vec<Piece> {
    v.iter().map(|&(first, end)| Piece { first, end }).collect()
}

fn input(pack_cells: u64, part_cells: u64, first_touch: u32, touched: u32) -> PlanInput {
    PlanInput {
        pack_size: pack_cells * CELL_BYTES,
        part_size: part_cells * CELL_BYTES,
        first_touch,
        first_touch_cells: touched,
    }
}

fn planned(input: PlanInput) -> Vec<Piece> {
    plan(&EngineConfig::default(), input, &BTreeSet::new())
}

/// A small pack takes the touched cells as the first piece, then one piece for the rest after
/// them and one for the cells before them.
#[test]
fn a_small_pack_is_the_touched_cells_then_the_rest_after_and_the_cells_before() {
    let got = planned(input(20, 64, 5, 3));
    assert_eq!(got, pieces(&[(0, 5), (5, 8), (8, 20)]));
}

/// A first touch of one cell is widened to the minimum of two cells.
#[test]
fn a_small_pack_first_piece_is_at_least_two_cells() {
    let got = planned(input(20, 64, 5, 1));
    assert_eq!(got, pieces(&[(0, 5), (5, 7), (7, 20)]));
}

/// A first touch of ten cells is cut to the maximum of four cells.
#[test]
fn a_small_pack_first_piece_is_at_most_four_cells() {
    let got = planned(input(20, 64, 5, 10));
    assert_eq!(got, pieces(&[(0, 5), (5, 9), (9, 20)]));
}

/// A first touch at cell 0 leaves no piece before it.
#[test]
fn a_small_pack_touched_at_the_start_has_no_piece_before_it() {
    let got = planned(input(20, 64, 0, 3));
    assert_eq!(got, pieces(&[(0, 3), (3, 20)]));
}

/// A first touch whose cells run past the end leaves no piece after it.
#[test]
fn a_small_pack_touched_at_the_end_has_no_piece_after_it() {
    let got = planned(input(20, 64, 19, 3));
    assert_eq!(got, pieces(&[(0, 19), (19, 20)]));
}

/// A first touch beyond the pack is moved to the last cell.
#[test]
fn a_first_touch_past_the_end_of_the_pack_is_the_last_cell() {
    let got = planned(input(10, 64, 50, 3));
    assert_eq!(got, pieces(&[(0, 9), (9, 10)]));
}

/// A pack of exactly 32 MiB is small: no ramp.
#[test]
fn a_pack_of_exactly_the_small_pack_max_takes_no_ramp() {
    let got = planned(input(32, 64, 0, 3));
    assert_eq!(got, pieces(&[(0, 3), (3, 32)]));
}

/// One byte above 32 MiB is a large pack: the ramp starts, and the last piece is trimmed at the
/// pack's end.
#[test]
fn a_pack_one_byte_over_the_small_pack_max_takes_the_ramp_trimmed_at_the_pack_end() {
    let mut i = input(32, 64, 0, 3);
    i.pack_size += 1;
    let got = planned(i);
    assert_eq!(got, pieces(&[(0, 4), (4, 12), (12, 28), (28, 33)]));
}

/// A large pack first touched at offset 0 ramps 4, 8, 16, 32 MiB, then 4 MiB trimmed at the
/// 64 MiB boundary, then whole grid parts, the last trimmed at the pack's end.
#[test]
fn a_large_pack_touched_at_offset_zero_ramps_4_8_16_32_then_4_and_then_whole_grid_parts() {
    let got = planned(input(200, 64, 0, 3));
    assert_eq!(
        got,
        pieces(&[
            (0, 4),
            (4, 12),
            (12, 28),
            (28, 60),
            (60, 64),
            (64, 128),
            (128, 192),
            (192, 200),
        ])
    );
}

/// A first touch mid-pack ramps to the next grid boundary with no runt after it; the cells before
/// are whole grid parts plus the head of the first touch's grid part.
#[test]
fn a_large_pack_touched_mid_pack_ramps_to_the_grid_boundary_and_has_no_runt_after() {
    let got = planned(input(300, 64, 100, 3));
    assert_eq!(
        got,
        pieces(&[
            (0, 64),
            (64, 100),
            (100, 104),
            (104, 112),
            (112, 128),
            (128, 192),
            (192, 256),
            (256, 300),
        ])
    );
}

/// The ramp is capped at P/2 and its last piece is trimmed to the grid boundary.
#[test]
fn a_ramp_is_capped_at_half_a_part_and_trimmed_to_the_boundary() {
    let got = planned(input(130, 64, 65, 3));
    assert_eq!(
        got,
        pieces(&[
            (0, 64),
            (64, 65),
            (65, 69),
            (69, 77),
            (77, 93),
            (93, 125),
            (125, 128),
            (128, 130),
        ])
    );
}

/// A first touch on a grid boundary has no head piece before it in its own grid part.
#[test]
fn a_first_touch_on_a_grid_boundary_has_no_head_piece() {
    let got = planned(input(200, 64, 64, 3));
    assert_eq!(
        got,
        pieces(&[
            (0, 64),
            (64, 68),
            (68, 76),
            (76, 92),
            (92, 124),
            (124, 128),
            (128, 192),
            (192, 200),
        ])
    );
}

/// With P of 32 MiB the ramp is capped at 16 MiB and the grid parts are 32 MiB.
#[test]
fn a_part_size_of_32_mib_ramps_to_a_16_mib_cap_and_then_has_32_mib_parts() {
    let got = planned(input(100, 32, 0, 3));
    assert_eq!(
        got,
        pieces(&[
            (0, 4),
            (4, 12),
            (12, 28),
            (28, 32),
            (32, 64),
            (64, 96),
            (96, 100),
        ])
    );
}

/// A ramp that fits in a short last grid part is trimmed at the pack's end.
#[test]
fn a_ramp_in_the_last_grid_part_is_trimmed_at_the_pack_end() {
    let got = planned(input(70, 64, 66, 3));
    assert_eq!(got, pieces(&[(0, 64), (64, 66), (66, 70)]));
}

/// A part size below one cell is one cell: every piece is a single cell.
#[test]
fn a_part_size_smaller_than_a_cell_is_one_cell() {
    let mut i = input(40, 1, 3, 3);
    i.part_size = 1000;
    let got = planned(i);
    let want: Vec<Piece> = (0..40)
        .map(|c| Piece {
            first: c,
            end: c + 1,
        })
        .collect();
    assert_eq!(got, want);
}

/// A part size that is not a whole number of cells is rounded down: 64.5 cells plans as 64.
#[test]
fn a_part_size_that_is_not_a_cell_multiple_is_rounded_down() {
    let mut odd = input(200, 64, 0, 3);
    odd.part_size += CELL_BYTES / 2;
    assert_eq!(planned(odd), planned(input(200, 64, 0, 3)));
}

/// A pack of no bytes has no piece.
#[test]
fn an_empty_pack_has_no_piece() {
    let got = planned(input(0, 64, 0, 3));
    assert_eq!(got, Vec::<Piece>::new());
}

/// A pack of one byte is one piece of one cell.
#[test]
fn a_pack_of_one_byte_is_one_piece_of_one_cell() {
    let mut i = input(0, 64, 0, 3);
    i.pack_size = 1;
    assert_eq!(planned(i), pieces(&[(0, 1)]));
}

/// Piece::cells is the number of cells of the piece.
#[test]
fn a_piece_counts_its_cells() {
    assert_eq!(Piece { first: 4, end: 12 }.cells(), 8);
    assert_eq!(Piece { first: 7, end: 8 }.cells(), 1);
}

/// For many fixed inputs, small and large, the pieces are sorted, disjoint, non-empty and cover
/// every cell exactly once.
#[test]
fn pieces_are_sorted_disjoint_and_cover_every_cell_exactly_once() {
    let cells = [1_u64, 2, 3, 31, 32, 33, 63, 64, 65, 100, 128, 129, 300, 513];
    let parts = [1_u64, 2, 8, 32, 64];
    let touches = [
        (0_u32, 1_u32),
        (0, 3),
        (1, 2),
        (31, 4),
        (64, 4),
        (100, 10),
        (5000, 3),
    ];
    let mut runs = 0;
    for &n in &cells {
        for &p in &parts {
            for &(ft, t) in &touches {
                let got = planned(input(n, p, ft, t));
                let covered: Vec<u32> = got.iter().flat_map(|x| x.first..x.end).collect();
                let want: Vec<u32> = (0..u32::try_from(n).unwrap()).collect();
                assert_eq!(covered, want, "n={n} p={p} ft={ft} t={t}");
                assert!(got.iter().all(|x| x.cells() > 0), "n={n} p={p} ft={ft}");
                runs += 1;
            }
        }
    }
    assert_eq!(runs, 490);
}

/// For large packs, no piece is longer than one grid part.
#[test]
fn no_piece_of_a_large_pack_is_longer_than_the_part_size() {
    let touches = [
        (0_u32, 3_u32),
        (1, 2),
        (31, 4),
        (64, 4),
        (100, 10),
        (299, 1),
    ];
    for p in [2_u64, 8, 32, 64] {
        for &(ft, t) in &touches {
            let got = planned(input(300, p, ft, t));
            let longest = got.iter().map(|x| x.cells()).max();
            assert!(longest.map(u64::from) <= Some(p), "p={p} ft={ft}");
        }
    }
}

/// Pieces are split at the cut cells only: a cut inside a piece splits it.
#[test]
fn a_cut_inside_a_piece_splits_it_there() {
    let cuts = BTreeSet::from([5, 10, 15]);
    let got = plan(&EngineConfig::default(), input(20, 64, 0, 4), &cuts);
    assert_eq!(got, pieces(&[(0, 4), (4, 5), (5, 10), (10, 15), (15, 20)]));
}

/// A cut on a piece boundary, one past the pack, or at cell 0 changes nothing.
#[test]
fn cuts_on_piece_boundaries_or_outside_the_pack_change_nothing() {
    let cuts = BTreeSet::from([0, 4, 20, 99]);
    let got = plan(&EngineConfig::default(), input(20, 64, 0, 4), &cuts);
    assert_eq!(got, pieces(&[(0, 4), (4, 20)]));
}

/// Cuts split the pieces of the ramp and of the grid parts of a large pack, and the result stays
/// sorted and covering.
#[test]
fn cuts_split_the_ramp_and_the_grid_parts_of_a_large_pack() {
    let cuts = BTreeSet::from([6, 100]);
    let got = plan(&EngineConfig::default(), input(130, 64, 0, 3), &cuts);
    assert_eq!(
        got,
        pieces(&[
            (0, 4),
            (4, 6),
            (6, 12),
            (12, 28),
            (28, 60),
            (60, 64),
            (64, 100),
            (100, 128),
            (128, 130),
        ])
    );
}

/// With a cut every 3, 5 or 8 cells, every cut cell starts a piece, so no piece spans one.
#[test]
fn no_piece_spans_a_cut_cell() {
    for every in [3_u32, 5, 8] {
        let cuts: BTreeSet<u32> = (1..150).filter(|c| c % every == 0).collect();
        let got = plan(&EngineConfig::default(), input(150, 64, 70, 3), &cuts);
        let covered: Vec<u32> = got.iter().flat_map(|x| x.first..x.end).collect();
        assert_eq!(covered, (0..150).collect::<Vec<u32>>());
        for &c in &cuts {
            let i = piece_of(&got, c).unwrap();
            assert_eq!(got[i].first, c, "every={every} cut={c}");
        }
    }
}

/// cell_count rounds up to whole cells and is 0 for 0 bytes.
#[test]
fn cell_count_rounds_up_to_whole_cells() {
    assert_eq!(cell_count(0), Some(0));
    assert_eq!(cell_count(1), Some(1));
    assert_eq!(cell_count(CELL_BYTES), Some(1));
    assert_eq!(cell_count(CELL_BYTES + 1), Some(2));
}

/// cell_count is None above u32::MAX cells and exact at u32::MAX cells.
#[test]
fn cell_count_beyond_u32_cells_is_none() {
    assert_eq!(cell_count(u64::from(u32::MAX) * CELL_BYTES), Some(u32::MAX));
    assert_eq!(cell_count(u64::from(u32::MAX) * CELL_BYTES + 1), None);
    assert_eq!(cell_count(u64::MAX), None);
}

/// The size class is 0 up to 32 MiB, 1 up to 512 MiB and 2 above, exact at each boundary.
#[test]
fn size_class_changes_at_32_mib_and_512_mib_exactly() {
    assert_eq!(size_class(0), 0);
    assert_eq!(size_class(32 * MIB), 0);
    assert_eq!(size_class(32 * MIB + 1), 1);
    assert_eq!(size_class(512 * MIB), 1);
    assert_eq!(size_class(512 * MIB + 1), 2);
    assert_eq!(size_class(u64::MAX), 2);
}

/// piece_of finds the piece holding a cell, at first and last cells of each piece.
#[test]
fn piece_of_finds_the_piece_of_first_and_last_cells() {
    let ps = pieces(&[(0, 4), (4, 12), (12, 20)]);
    assert_eq!(piece_of(&ps, 0), Some(0));
    assert_eq!(piece_of(&ps, 3), Some(0));
    assert_eq!(piece_of(&ps, 4), Some(1));
    assert_eq!(piece_of(&ps, 11), Some(1));
    assert_eq!(piece_of(&ps, 12), Some(2));
    assert_eq!(piece_of(&ps, 19), Some(2));
}

/// piece_of is None past the end of the pack and for an empty plan.
#[test]
fn piece_of_past_the_end_or_in_an_empty_plan_is_none() {
    let ps = pieces(&[(0, 4), (4, 12), (12, 20)]);
    assert_eq!(piece_of(&ps, 20), None);
    assert_eq!(piece_of(&ps, u32::MAX), None);
    assert_eq!(piece_of(&[], 0), None);
}
