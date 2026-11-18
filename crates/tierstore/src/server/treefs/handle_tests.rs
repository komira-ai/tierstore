//! The numbers of file handles.

use super::{MOST_GENERATION, generation_of, make, next_generation};

/// Asserts that a number gives back the generation it was made with, and keeps the process's
/// count in the bits below.
#[test]
fn a_handle_number_says_its_generation() {
    let handle = make(7, 12);
    assert_eq!(generation_of(handle), 7);
    assert_eq!(handle & 0xff_ffff_ffff, 12);
    assert_eq!(generation_of(make(MOST_GENERATION, 99)), MOST_GENERATION);
}

/// Asserts that bit 63, which an earlier release set on a passthrough open, is not read as part of
/// the generation, and that this release never sets it.
#[test]
fn the_passthrough_bit_of_an_earlier_release_is_not_the_generation() {
    let marked = (1 << 63) | make(MOST_GENERATION, 99);
    assert_eq!(generation_of(marked), MOST_GENERATION);
    assert_eq!(make(MOST_GENERATION, u64::MAX) >> 63, 0);
}

/// Asserts that two processes' first handles differ, so that a handle of the process before is
/// never taken for one of this process.
#[test]
fn the_first_handle_of_each_generation_is_its_own() {
    assert_ne!(make(1, 1), make(2, 1));
}

/// Asserts that the generation counts up and begins again at 1 after the most there is room for.
#[test]
fn the_generation_counts_up_and_wraps_to_one() {
    assert_eq!(next_generation(1), 2);
    assert_eq!(next_generation(MOST_GENERATION - 1), MOST_GENERATION);
    assert_eq!(next_generation(MOST_GENERATION), 1);
}

/// Asserts that a count past the bits it has does not reach the generation.
#[test]
fn a_count_past_its_bits_does_not_change_the_generation() {
    let handle = make(3, u64::MAX);
    assert_eq!(generation_of(handle), 3);
}
