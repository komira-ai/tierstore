//! Pack assembly: exact block counts for first-fit decreasing, windows and chains.

use super::*;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// Sizes round up to the next 4 KiB and stay when already a multiple.
#[test]
fn records_round_up_to_four_kib() {
    assert_eq!(align(0), 0);
    assert_eq!(align(1), 4096);
    assert_eq!(align(4096), 4096);
    assert_eq!(align(4097), 8192);
}

/// Largest first into the first block with room: 6, 5, 5, 4 in blocks of 10 take two blocks (6
/// and 4, then 5 and 5); three 7s take three; nothing takes none.
#[test]
fn first_fit_decreasing_counts_blocks() {
    assert_eq!(first_fit_decreasing(&[4, 6, 5, 5], 10), 2);
    assert_eq!(first_fit_decreasing(&[7, 7, 7], 10), 3);
    assert_eq!(first_fit_decreasing(&[], 10), 0);
}

/// A 600 MiB and a 400 MiB pack share one 1 GiB block (1000 MiB fit in 1023 MiB); a 700 MiB
/// and a 400 MiB pack do not (1100 MiB do not).
#[test]
fn a_600_and_a_400_mib_pack_share_a_gib_block_and_a_700_and_a_400_do_not() {
    let share = fit(&[400 * MIB, 600 * MIB], 64, GIB);
    assert_eq!((share.blocks, share.chain_blocks, share.chains), (1, 0, 0));
    assert_eq!(share.data_bytes, 1000 * MIB);
    assert!((share.mean_fill - 1000.0 / 1024.0).abs() < 1e-12);
    assert!((share.packed_fill - share.mean_fill).abs() < 1e-12);
    let apart = fit(&[400 * MIB, 700 * MIB], 64, GIB);
    assert_eq!(apart.blocks, 2);
    assert!((apart.mean_fill - 1100.0 / 2048.0).abs() < 1e-12);
}

/// Only objects of the same window are placed together: with a window of one, the 600 and the
/// 400 MiB packs each take a block; with a window of two they share one.
#[test]
fn objects_in_different_windows_do_not_share_a_block() {
    let sizes = [600 * MIB, 400 * MIB];
    assert_eq!(fit(&sizes, 1, GIB).blocks, 2);
    assert_eq!(fit(&sizes, 2, GIB).blocks, 1);
    // Three objects with a window of two: the first two share, the third is alone.
    assert_eq!(fit(&[600 * MIB, 400 * MIB, 5 * MIB], 2, GIB).blocks, 2);
}

/// An object of exactly `block - 1 MiB` fits a block and fills all but the reserve; one byte
/// more is a chain of two blocks, its record rounded up to 4 KiB.
#[test]
fn an_object_of_block_minus_reserve_fits_and_one_byte_more_is_a_chain() {
    let block = 256 * MIB;
    let capacity = block - BLOCK_RESERVE;
    let fits = fit(&[capacity], 64, block);
    assert_eq!((fits.blocks, fits.chain_blocks, fits.chains), (1, 0, 0));
    assert_eq!(fits.data_bytes, capacity);
    assert!((fits.mean_fill - 255.0 / 256.0).abs() < 1e-12);
    let chain = fit(&[capacity + 1], 64, block);
    assert_eq!((chain.blocks, chain.chain_blocks, chain.chains), (2, 2, 1));
    assert_eq!(chain.data_bytes, capacity + 4096);
    assert!((chain.mean_fill - (capacity + 4096) as f64 / (2 * block) as f64).abs() < 1e-12);
    assert!(chain.packed_fill.abs() < 1e-12);
}

/// A 600 MiB object in 256 MiB blocks is a chain of ceil(600 / 255) = 3 blocks and leaves the
/// small objects of its window to their own blocks.
#[test]
fn a_chain_takes_ceil_of_size_over_capacity_blocks() {
    let f = fit(&[600 * MIB, 100 * MIB, 100 * MIB, 100 * MIB], 64, 256 * MIB);
    assert_eq!((f.chains, f.chain_blocks), (1, 3));
    // 100 MiB x 3 do not fit in one block of 255 MiB: two blocks (200 + 100).
    assert_eq!(f.blocks, 3 + 2);
    assert_eq!(f.data_bytes, 900 * MIB);
    assert!((f.packed_fill - 300.0 / 512.0).abs() < 1e-12);
}

/// No objects: no blocks and a fill of zero, not a division by zero.
#[test]
fn no_objects_fill_no_blocks() {
    let f = fit(&[], 64, GIB);
    assert_eq!((f.blocks, f.data_bytes), (0, 0));
    assert!(f.mean_fill.abs() < 1e-12 && f.packed_fill.abs() < 1e-12);
}

/// The packing counts the objects above `large_min` and those of 400 to 700 MiB (bounds
/// inclusive), and holds one fit per block size and window in order.
#[test]
fn the_packing_counts_mid_size_packs_and_runs_every_block_size_and_window() {
    let sizes = [
        5 * MIB,
        400 * MIB - 1,
        400 * MIB,
        700 * MIB,
        700 * MIB + 1,
        900 * MIB,
    ];
    let p = packing(&sizes);
    assert_eq!(p.large_min, 4 * MIB);
    assert_eq!(p.objects.objects, 6);
    assert_eq!(p.objects.bytes, sizes.iter().sum::<u64>());
    assert_eq!((p.mid.objects, p.mid.bytes), (2, 1100 * MIB));
    assert_eq!(p.fits.len(), 12);
    let keys: Vec<(u64, usize)> = p.fits.iter().map(|f| (f.block_bytes, f.window)).collect();
    assert_eq!(keys[0], (256 * MIB, 16));
    assert_eq!(keys[1], (256 * MIB, 64));
    assert_eq!(keys[3], (512 * MIB, 16));
    assert_eq!(keys[11], (2 * GIB, 256));
    for f in &p.fits {
        assert_eq!(f.data_bytes, sizes.iter().map(|s| align(*s)).sum::<u64>());
    }
}
