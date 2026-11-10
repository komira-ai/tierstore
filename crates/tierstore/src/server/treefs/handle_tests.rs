//! The numbers of file handles.

use super::{MOST_GENERATION, generation_of, is_backed, make, next_generation};

/// Asserts that a number gives back the generation and the kind it was made with, and keeps the
/// process's count in the bits below.
#[test]
fn a_handle_number_says_its_generation_and_whether_it_is_a_passthrough_open() {
    let cached = make(7, 12, false);
    assert_eq!(generation_of(cached), 7);
    assert!(!is_backed(cached));
    assert_eq!(cached & 0xff_ffff_ffff, 12);
    let backed = make(MOST_GENERATION, 99, true);
    assert_eq!(generation_of(backed), MOST_GENERATION);
    assert!(is_backed(backed));
}

/// Asserts that two processes' first handles differ, so that a handle of the process before is
/// never taken for one of this process.
#[test]
fn the_first_handle_of_each_generation_is_its_own() {
    assert_ne!(make(1, 1, false), make(2, 1, false));
    assert_ne!(make(1, 1, true), make(1, 1, false));
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
    let handle = make(3, u64::MAX, false);
    assert_eq!(generation_of(handle), 3);
    assert!(!is_backed(handle));
}
