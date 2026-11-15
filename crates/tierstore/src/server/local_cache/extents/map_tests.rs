//! The ranges of an entry by extents: what is held, what is missing, and how ranges join.

use super::map::ExtentMap;

/// The map of the `[start, end)` ranges, as a sidecar's pairs would build it.
fn map_of(runs: &[(u64, u64)]) -> ExtentMap {
    let mut map = ExtentMap::default();
    for (start, end) in runs {
        let _ = map.insert(*start, *end);
    }
    map
}

/// Asserts that a range that overlaps or touches the ranges held joins them into one, and that
/// the answer is the bytes that were not held: 0 for a range inside one, the gap and the
/// extension for one that spans two.
#[test]
fn ranges_that_overlap_or_touch_join_and_the_answer_is_the_new_bytes() {
    let mut map = map_of(&[(10, 20), (30, 40)]);
    assert_eq!(map.pairs(), [[10, 10], [30, 10]]);
    assert_eq!(map.insert(12, 18), 0);
    assert_eq!(map.insert(20, 30), 10);
    assert_eq!(map.pairs(), [[10, 30]]);
    assert_eq!(map.insert(5, 12), 5);
    assert_eq!(map.insert(38, 50), 10);
    assert_eq!(map.pairs(), [[5, 45]]);
    assert_eq!(map.bytes(), 45);
    assert_eq!(map.insert(60, 60), 0);
    assert_eq!(map.insert(70, 60), 0);
    assert_eq!(map.pairs(), [[5, 45]]);
}

/// Asserts that a range is covered only when one held range holds all of it, that an empty
/// range always is, and that a range that begins in a gap or ends past a run is not.
#[test]
fn a_range_is_covered_only_by_one_held_range() {
    let map = map_of(&[(10, 20), (25, 30)]);
    assert!(map.covers(10, 20));
    assert!(map.covers(12, 15));
    assert!(map.covers(27, 27));
    assert!(map.covers(100, 50));
    assert!(!map.covers(5, 15));
    assert!(!map.covers(15, 26));
    assert!(!map.covers(20, 25));
    assert!(!map.covers(0, 5));
    assert!(!ExtentMap::default().covers(0, 1));
    assert!(map_of(&[(0, 9)]).is_whole(9));
    assert!(!map.is_whole(30));
}

/// Asserts that the parts of a range that are not held come in order: before, between and after
/// the held ranges, none when it is held, the whole range when nothing is.
#[test]
fn the_missing_parts_of_a_range_are_its_gaps_in_order() {
    let map = map_of(&[(10, 20), (30, 40)]);
    assert_eq!(map.missing(0, 50), [(0, 10), (20, 30), (40, 50)]);
    assert_eq!(map.missing(15, 35), [(20, 30)]);
    assert_eq!(map.missing(10, 20), []);
    assert_eq!(map.missing(12, 18), []);
    assert_eq!(map.missing(0, 5), [(0, 5)]);
    assert_eq!(map.missing(5, 10), [(5, 10)]);
    assert_eq!(map.missing(45, 50), [(45, 50)]);
    assert_eq!(ExtentMap::default().missing(3, 9), [(3, 9)]);
    assert_eq!(map.missing(9, 9), []);
    assert!(ExtentMap::default().is_empty());
    assert!(!map.is_empty());
}

/// Asserts that the pairs of a sidecar are read back into the same ranges, and that a pair that
/// is empty, passes the size, overflows, is out of order, overlaps the one before or touches it
/// is refused: this release writes none of them, so the sidecar is damaged.
#[test]
fn pairs_read_back_and_a_damaged_list_is_refused() {
    let map = map_of(&[(10, 20), (30, 40)]);
    assert_eq!(ExtentMap::from_pairs(&map.pairs(), 100), Some(map));
    assert_eq!(ExtentMap::from_pairs(&[], 100), Some(ExtentMap::default()));
    assert_eq!(
        ExtentMap::from_pairs(&[[0, 100]], 100).map(|m| m.bytes()),
        Some(100)
    );
    assert_eq!(ExtentMap::from_pairs(&[[10, 0]], 100), None);
    assert_eq!(ExtentMap::from_pairs(&[[10, 91]], 100), None);
    assert_eq!(ExtentMap::from_pairs(&[[u64::MAX, 2]], u64::MAX), None);
    assert_eq!(ExtentMap::from_pairs(&[[30, 5], [10, 5]], 100), None);
    assert_eq!(ExtentMap::from_pairs(&[[10, 10], [15, 10]], 100), None);
    assert_eq!(ExtentMap::from_pairs(&[[10, 10], [20, 10]], 100), None);
    assert_eq!(
        ExtentMap::from_pairs(&[[0, 10], [20, 10]], 100).map(|m| m.bytes()),
        Some(20)
    );
}
