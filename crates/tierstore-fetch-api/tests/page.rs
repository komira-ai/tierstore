//! The state page layout, its header, the mount seals and the counter regions (`page`).
//!
//! Proves the page geometry at every boundary (0 bytes, 1 byte, exact cell and group multiples,
//! the page-size rounding step, the largest pack, one cell past it), that the header round trips
//! and that any changed byte or a short slice is refused, that a memfd is read-only for mounts only
//! when every seal is present, how a cell run maps to counter groups, and the arithmetic of a
//! counter region including a region that overflows.
//!
//! Catches:
//! - a layout whose epochs or length are misaligned, or that rounds the length down;
//! - a pack of more than `u32::MAX` cells that is given a layout;
//! - a header that does not round trip, or that is accepted after one byte changed;
//! - a state memfd accepted with a seal missing;
//! - a group range off by one at a group boundary, or one that wraps on overflow;
//! - a counter offset or region end that wraps, or a misaligned region reported valid.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use tierstore_fetch_api::ids::CellRun;
use tierstore_fetch_api::page::{
    CELL_SHIFT, CounterRegion, F_SEAL_FUTURE_WRITE, F_SEAL_GROW, F_SEAL_SEAL, F_SEAL_SHRINK,
    GROUP_CELLS, MOUNT_SEALS, PAGE_BYTES, STATE_PAGE_HEADER_LEN, STATE_PAGE_MAGIC,
    STATE_PAGE_VERSION, StatePageLayout, groups_of, sealed_for_mounts,
};

const CELL: u64 = 1 << 20;

fn layout(cells: u64) -> StatePageLayout {
    StatePageLayout::for_pack(cells * CELL).expect("fits")
}

#[test]
fn the_constants_are_the_documented_values() {
    assert_eq!(STATE_PAGE_MAGIC.to_le_bytes(), *b"TSSP");
    assert_eq!(STATE_PAGE_VERSION, 1);
    assert_eq!(STATE_PAGE_HEADER_LEN, 64);
    assert_eq!(GROUP_CELLS, 16);
    assert_eq!(CELL_SHIFT, 20);
    assert_eq!(PAGE_BYTES, 4096);
}

#[test]
fn a_pack_of_zero_bytes_has_no_cell_and_one_page() {
    let l = StatePageLayout::for_pack(0).expect("fits");
    assert_eq!(l.cell_count, 0);
    assert_eq!(l.group_count(), 0);
    assert!(l.is_empty());
    assert_eq!(l.cells_offset(), 64);
    assert_eq!(l.epochs_offset(), 64);
    assert_eq!(l.len(), 4096);
}

#[test]
fn a_pack_of_one_byte_has_one_cell_one_group_and_epochs_at_the_next_multiple_of_eight() {
    let l = StatePageLayout::for_pack(1).expect("fits");
    assert_eq!(l.pack_size, 1);
    assert_eq!(l.cell_count, 1);
    assert_eq!(l.group_count(), 1);
    assert!(!l.is_empty());
    assert_eq!(l.epochs_offset(), 72);
    assert_eq!(l.len(), 4096);
}

#[test]
fn cells_round_up_at_the_one_mebibyte_boundary() {
    assert_eq!(
        StatePageLayout::for_pack(CELL - 1)
            .expect("fits")
            .cell_count,
        1
    );
    assert_eq!(StatePageLayout::for_pack(CELL).expect("fits").cell_count, 1);
    assert_eq!(
        StatePageLayout::for_pack(CELL + 1)
            .expect("fits")
            .cell_count,
        2
    );
}

#[test]
fn groups_round_up_at_sixteen_cells() {
    assert_eq!(layout(16).group_count(), 1);
    assert_eq!(layout(17).group_count(), 2);
    assert_eq!(layout(32).group_count(), 2);
    assert_eq!(layout(33).group_count(), 3);
}

#[test]
fn epochs_start_at_a_multiple_of_eight_after_the_cell_bytes() {
    for cells in 0..=40u64 {
        let l = layout(cells);
        let e = l.epochs_offset();
        assert_eq!(e % 8, 0, "{cells}");
        assert!(e >= 64 + cells, "{cells}");
        assert!(e < 64 + cells + 8, "{cells}");
    }
    assert_eq!(layout(8).epochs_offset(), 72);
    assert_eq!(layout(9).epochs_offset(), 80);
}

#[test]
fn the_length_is_a_page_multiple_that_holds_the_epochs_and_rounds_up_exactly_at_the_page() {
    // 3224 cells: 64 + 3224 = 3288 (a multiple of 8), 202 groups of 4 bytes: 4096 exactly.
    let exact = layout(3224);
    assert_eq!(exact.epochs_offset(), 3288);
    assert_eq!(exact.group_count(), 202);
    assert_eq!(exact.len(), 4096);
    // One cell more pushes the epochs past the page.
    assert_eq!(layout(3225).len(), 8192);
    assert_eq!(layout(4096).len(), 8192);
    for cells in [0, 1, 16, 17, 1000, 3224, 3225, 5000, 100_000] {
        let l = layout(cells);
        assert_eq!(l.len() % PAGE_BYTES, 0, "{cells}");
        assert!(
            l.len() >= l.epochs_offset() + 4 * u64::from(l.group_count()),
            "{cells}"
        );
        assert!(l.len() < l.epochs_offset() + 4 * u64::from(l.group_count()) + PAGE_BYTES);
    }
}

#[test]
fn the_largest_pack_has_u32_max_cells_and_one_cell_more_has_no_layout() {
    let big = StatePageLayout::for_pack(u64::from(u32::MAX) << CELL_SHIFT).expect("fits");
    assert_eq!(big.cell_count, u32::MAX);
    assert_eq!(big.group_count(), 1 << 28);
    assert_eq!(big.len() % PAGE_BYTES, 0);
    assert_eq!(StatePageLayout::from_header(&big.header()), Some(big));
    let past = (u64::from(u32::MAX) + 1) << CELL_SHIFT;
    assert_eq!(StatePageLayout::for_pack(past), None);
    assert_eq!(
        StatePageLayout::for_pack((u64::from(u32::MAX) << CELL_SHIFT) + 1),
        None
    );
    assert_eq!(StatePageLayout::for_pack(u64::MAX), None);
}

#[test]
fn the_header_holds_each_field_at_its_documented_offset() {
    let l = StatePageLayout::for_pack(3 * CELL + 5).expect("fits");
    let h = l.header();
    assert_eq!(h.len(), 64);
    assert_eq!(&h[0..4], b"TSSP");
    assert_eq!(&h[4..6], &1u16.to_le_bytes());
    assert_eq!(h[6], 20);
    assert_eq!(h[7], 16);
    assert_eq!(&h[8..12], &4u32.to_le_bytes());
    assert_eq!(&h[12..16], &1u32.to_le_bytes());
    assert_eq!(&h[16..24], &(3 * CELL + 5).to_le_bytes());
    assert_eq!(&h[24..32], &64u64.to_le_bytes());
    assert_eq!(&h[32..40], &72u64.to_le_bytes());
    assert_eq!(&h[40..48], &4096u64.to_le_bytes());
    assert_eq!(&h[48..64], &[0u8; 16]);
}

#[test]
fn a_header_round_trips_for_every_size_class() {
    for size in [
        0,
        1,
        CELL,
        CELL + 1,
        16 * CELL,
        17 * CELL + 3,
        3225 * CELL,
        5000 * CELL - 1,
    ] {
        let l = StatePageLayout::for_pack(size).expect("fits");
        assert_eq!(StatePageLayout::from_header(&l.header()), Some(l), "{size}");
    }
}

#[test]
fn a_header_with_any_one_byte_changed_is_refused() {
    let h = StatePageLayout::for_pack(3 * CELL + 5)
        .expect("fits")
        .header();
    // The pack size is the one field a change can leave consistent; it has a test of its own.
    for i in (0..h.len()).filter(|i| !(16..24).contains(i)) {
        for delta in [1u8, 0x80, 0xFF] {
            let mut bad = h;
            bad[i] ^= delta;
            assert_eq!(
                StatePageLayout::from_header(&bad),
                None,
                "byte {i} ^ {delta:#x}"
            );
        }
    }
}

#[test]
fn a_header_with_another_pack_size_is_accepted_only_if_all_its_other_fields_agree() {
    let h = StatePageLayout::for_pack(3 * CELL + 5)
        .expect("fits")
        .header();
    for i in 16..24 {
        let mut bad = h;
        bad[i] ^= 1;
        // The low three bytes keep the cell count, so the header still describes a pack (of a
        // slightly different size); a higher byte changes the cell count and is refused.
        let got = StatePageLayout::from_header(&bad);
        assert_eq!(got.is_some(), i < 19, "byte {i}");
        if let Some(l) = got {
            assert_eq!(l.header(), bad, "byte {i}");
        }
    }
}

#[test]
fn a_short_or_empty_header_is_refused_and_a_longer_slice_reads_its_first_64_bytes() {
    let l = StatePageLayout::for_pack(3 * CELL + 5).expect("fits");
    let h = l.header();
    assert_eq!(StatePageLayout::from_header(&[]), None);
    assert_eq!(StatePageLayout::from_header(&h[..63]), None);
    let mut page = h.to_vec();
    page.extend_from_slice(&[2; 100]);
    assert_eq!(StatePageLayout::from_header(&page), Some(l));
}

#[test]
fn a_header_naming_more_cells_than_a_u32_is_refused() {
    let mut h = StatePageLayout::for_pack(CELL).expect("fits").header();
    h[16..24].copy_from_slice(&((u64::from(u32::MAX) + 1) << CELL_SHIFT).to_le_bytes());
    assert_eq!(StatePageLayout::from_header(&h), None);
}

#[test]
fn a_memfd_is_read_only_for_mounts_only_with_every_seal() {
    assert_eq!(MOUNT_SEALS, 0x17);
    assert!(sealed_for_mounts(MOUNT_SEALS));
    assert!(!sealed_for_mounts(0));
    for missing in [F_SEAL_SEAL, F_SEAL_SHRINK, F_SEAL_GROW, F_SEAL_FUTURE_WRITE] {
        assert!(!sealed_for_mounts(MOUNT_SEALS & !missing), "{missing:#x}");
        assert!(
            sealed_for_mounts(MOUNT_SEALS | 0x08 | 0x20),
            "extra seals do not matter"
        );
    }
    // F_SEAL_WRITE (8) is not a substitute for F_SEAL_FUTURE_WRITE.
    assert!(!sealed_for_mounts(
        F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | 0x08
    ));
}

fn groups(first: u32, count: u32) -> Option<(u32, u32)> {
    groups_of(CellRun { first, count }).map(|r| (r.first, r.count))
}

#[test]
fn an_empty_run_covers_no_group() {
    assert_eq!(groups(0, 0), None);
    assert_eq!(groups(u32::MAX, 0), None);
}

#[test]
fn a_run_maps_to_the_groups_that_cover_it_at_every_boundary() {
    assert_eq!(groups(0, 1), Some((0, 1)));
    assert_eq!(groups(15, 1), Some((0, 1)));
    assert_eq!(groups(16, 1), Some((1, 1)));
    assert_eq!(groups(0, 16), Some((0, 1)));
    assert_eq!(groups(0, 17), Some((0, 2)));
    assert_eq!(groups(15, 2), Some((0, 2)));
    assert_eq!(groups(16, 16), Some((1, 1)));
    assert_eq!(groups(1, 31), Some((0, 2)));
    assert_eq!(groups(1, 32), Some((0, 3)));
}

#[test]
fn a_run_at_the_end_of_the_cell_space_maps_and_one_past_it_is_none() {
    let last_group = u32::MAX / GROUP_CELLS;
    assert_eq!(groups(u32::MAX, 1), Some((last_group, 1)));
    assert_eq!(groups(u32::MAX - 1, 2), Some((last_group, 1)));
    assert_eq!(groups(u32::MAX, 2), None);
    assert_eq!(groups(1, u32::MAX), Some((0, last_group + 1)));
    assert_eq!(groups(2, u32::MAX), None);
}

#[test]
fn a_counter_region_takes_two_bytes_a_group_rounded_up_to_eight() {
    let len = |groups| CounterRegion { offset: 0, groups }.len();
    assert_eq!(len(0), 0);
    assert_eq!(len(1), 8);
    assert_eq!(len(4), 8);
    assert_eq!(len(5), 16);
    assert_eq!(len(8), 16);
    assert_eq!(len(9), 24);
    assert_eq!(len(u32::MAX), 8_589_934_592);
}

#[test]
fn a_counter_region_is_empty_only_with_no_group() {
    assert!(
        CounterRegion {
            offset: 8,
            groups: 0
        }
        .is_empty()
    );
    assert!(
        !CounterRegion {
            offset: 8,
            groups: 1
        }
        .is_empty()
    );
}

#[test]
fn a_counter_region_ends_after_its_length_and_overflow_is_none() {
    assert_eq!(
        CounterRegion {
            offset: 16,
            groups: 5
        }
        .end(),
        Some(32)
    );
    assert_eq!(
        CounterRegion {
            offset: 16,
            groups: 0
        }
        .end(),
        Some(16)
    );
    assert_eq!(
        CounterRegion {
            offset: u64::MAX - 8,
            groups: 4
        }
        .end(),
        Some(u64::MAX)
    );
    assert_eq!(
        CounterRegion {
            offset: u64::MAX - 8,
            groups: 5
        }
        .end(),
        None
    );
}

#[test]
fn a_counter_offset_is_two_bytes_per_group_inside_the_region_only() {
    let r = CounterRegion {
        offset: 64,
        groups: 3,
    };
    assert_eq!(r.counter_offset(0), Some(64));
    assert_eq!(r.counter_offset(1), Some(66));
    assert_eq!(r.counter_offset(2), Some(68));
    assert_eq!(r.counter_offset(3), None);
    assert_eq!(r.counter_offset(u32::MAX), None);
    assert_eq!(
        CounterRegion {
            offset: 0,
            groups: 0
        }
        .counter_offset(0),
        None
    );
}

#[test]
fn a_counter_offset_that_would_overflow_is_none_instead_of_wrapping() {
    let r = CounterRegion {
        offset: u64::MAX - 1,
        groups: 2,
    };
    assert_eq!(r.counter_offset(0), Some(u64::MAX - 1));
    assert_eq!(r.counter_offset(1), None);
}

#[test]
fn a_counter_region_is_valid_when_aligned_and_not_overflowing() {
    assert!(
        CounterRegion {
            offset: 0,
            groups: 1
        }
        .is_valid()
    );
    assert!(
        CounterRegion {
            offset: 4096,
            groups: 100
        }
        .is_valid()
    );
    assert!(
        CounterRegion {
            offset: u64::MAX - 15,
            groups: 4
        }
        .is_valid()
    );
    assert!(
        !CounterRegion {
            offset: 4,
            groups: 1
        }
        .is_valid()
    );
    assert!(
        !CounterRegion {
            offset: 1,
            groups: 0
        }
        .is_valid()
    );
    assert!(
        !CounterRegion {
            offset: u64::MAX - 15,
            groups: 5
        }
        .is_valid()
    );
}
