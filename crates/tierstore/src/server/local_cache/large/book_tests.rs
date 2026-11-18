//! Unit tests of the book of large objects: what it knows, its definitive misses and its bounds.

use super::book::{Book, KNOWN_LIMIT, Known};
use super::testing::generation;

fn known(g: u8, size: u64) -> Known {
    Known {
        generation: generation(g),
        size,
        modified: 7,
    }
}

/// Asserts the default limit is 65536 objects and as many misses.
#[test]
fn the_default_limit_is_sixty_five_thousand_five_hundred_thirty_six() {
    assert_eq!(KNOWN_LIMIT, 65_536);
}

/// Asserts an object that was learned is known again with all its fields.
#[test]
fn a_learned_object_is_known() {
    let mut book = Book::new(4);
    assert!(book.learn_at("a", known(1, 100), book.epoch()));
    assert_eq!(book.known("a"), Some(known(1, 100)));
}

/// Asserts a key never learned is not known.
#[test]
fn an_unknown_key_is_none() {
    let mut book = Book::new(4);
    assert!(book.learn_at("a", known(1, 100), book.epoch()));
    assert_eq!(book.known("b"), None);
}

/// Asserts learning a key again replaces what was known of it.
#[test]
fn learning_again_replaces_what_was_known() {
    let mut book = Book::new(4);
    assert!(book.learn_at("a", known(1, 100), book.epoch()));
    assert!(book.learn_at("a", known(2, 200), book.epoch()));
    assert_eq!(book.known("a"), Some(known(2, 200)));
    assert_eq!(book.len(), (1, 0));
}

/// Asserts a miss answers true before its deadline and false at it and after.
#[test]
fn a_miss_holds_before_its_deadline_and_not_at_or_after_it() {
    let mut book = Book::new(4);
    assert!(book.miss_at("a", 100, book.epoch()));
    assert!(book.missed("a", 0));
    assert!(book.missed("a", 99));
    assert!(!book.missed("a", 100));
    assert!(!book.missed("a", 101));
}

/// Asserts a miss that ran out is dropped from the table, not only answered false.
#[test]
fn an_expired_miss_is_dropped() {
    let mut book = Book::new(4);
    assert!(book.miss_at("a", 100, book.epoch()));
    assert_eq!(book.len(), (0, 1));
    assert!(!book.missed("a", 100));
    assert_eq!(book.len(), (0, 0));
    assert!(!book.missed("a", 0));
}

/// Asserts a miss that still holds stays in the table when asked.
#[test]
fn a_live_miss_stays_when_asked() {
    let mut book = Book::new(4);
    assert!(book.miss_at("a", 100, book.epoch()));
    assert!(book.missed("a", 50));
    assert_eq!(book.len(), (0, 1));
}

/// Asserts a key never missed is not missed.
#[test]
fn an_unmissed_key_is_not_missed() {
    let mut book = Book::new(4);
    assert!(book.miss_at("a", 100, book.epoch()));
    assert!(!book.missed("b", 0));
}

/// Asserts learning an object forgets a miss of it.
#[test]
fn learning_forgets_a_miss() {
    let mut book = Book::new(4);
    assert!(book.miss_at("a", 100, book.epoch()));
    assert!(book.learn_at("a", known(1, 10), book.epoch()));
    assert!(!book.missed("a", 0));
    assert_eq!(book.known("a"), Some(known(1, 10)));
    assert_eq!(book.len(), (1, 0));
}

/// Asserts a miss forgets what was known of the object.
#[test]
fn a_miss_forgets_what_was_known() {
    let mut book = Book::new(4);
    assert!(book.learn_at("a", known(1, 10), book.epoch()));
    assert!(book.miss_at("a", 100, book.epoch()));
    assert_eq!(book.known("a"), None);
    assert!(book.missed("a", 0));
    assert_eq!(book.len(), (0, 1));
}

/// Asserts forget removes both what was known and a miss, and only of that key.
#[test]
fn forget_removes_known_and_miss_of_that_key_only() {
    let mut book = Book::new(4);
    assert!(book.learn_at("a", known(1, 10), book.epoch()));
    assert!(book.learn_at("b", known(2, 20), book.epoch()));
    assert!(book.miss_at("c", 100, book.epoch()));
    book.forget("a");
    book.forget("c");
    assert_eq!(book.known("a"), None);
    assert!(!book.missed("c", 0));
    assert_eq!(book.known("b"), Some(known(2, 20)));
    assert_eq!(book.len(), (1, 0));
    book.forget("never-seen");
    assert_eq!(book.len(), (1, 0));
}

/// Asserts that at the limit the least recently used object goes, and a lookup counts as a use.
#[test]
fn the_least_recently_used_object_is_forgotten_past_the_limit() {
    let mut book = Book::new(2);
    assert!(book.learn_at("a", known(1, 1), book.epoch()));
    assert!(book.learn_at("b", known(2, 2), book.epoch()));
    assert_eq!(book.known("a"), Some(known(1, 1)));
    assert!(book.learn_at("c", known(3, 3), book.epoch()));
    assert_eq!(book.known("b"), None);
    assert_eq!(book.known("a"), Some(known(1, 1)));
    assert_eq!(book.known("c"), Some(known(3, 3)));
    assert_eq!(book.len(), (2, 0));
}

/// Asserts that without a lookup the oldest learned object goes first.
#[test]
fn the_oldest_learned_object_goes_first_without_lookups() {
    let mut book = Book::new(2);
    assert!(book.learn_at("a", known(1, 1), book.epoch()));
    assert!(book.learn_at("b", known(2, 2), book.epoch()));
    assert!(book.learn_at("c", known(3, 3), book.epoch()));
    assert_eq!(book.known("a"), None);
    assert_eq!(book.known("b"), Some(known(2, 2)));
    assert_eq!(book.known("c"), Some(known(3, 3)));
}

/// Asserts the miss table is bounded the same way: the oldest miss goes past the limit.
#[test]
fn the_miss_table_forgets_the_oldest_miss_past_the_limit() {
    let mut book = Book::new(2);
    assert!(book.miss_at("a", 100, book.epoch()));
    assert!(book.miss_at("b", 100, book.epoch()));
    assert!(book.miss_at("c", 100, book.epoch()));
    assert!(!book.missed("a", 0));
    assert!(book.missed("b", 0));
    assert!(book.missed("c", 0));
    assert_eq!(book.len(), (0, 2));
}

/// Asserts a lookup of a miss counts as a use, so the other miss goes first.
#[test]
fn asking_for_a_miss_counts_as_a_use() {
    let mut book = Book::new(2);
    assert!(book.miss_at("a", 100, book.epoch()));
    assert!(book.miss_at("b", 100, book.epoch()));
    assert!(book.missed("a", 0));
    assert!(book.miss_at("c", 100, book.epoch()));
    assert!(book.missed("a", 0));
    assert!(!book.missed("b", 0));
    assert!(book.missed("c", 0));
}

/// Asserts the two tables are bounded independently: full misses do not push out objects.
#[test]
fn the_tables_are_bounded_independently() {
    let mut book = Book::new(2);
    assert!(book.learn_at("a", known(1, 1), book.epoch()));
    assert!(book.learn_at("b", known(2, 2), book.epoch()));
    assert!(book.miss_at("x", 100, book.epoch()));
    assert!(book.miss_at("y", 100, book.epoch()));
    assert_eq!(book.len(), (2, 2));
    assert!(book.miss_at("z", 100, book.epoch()));
    assert_eq!(book.len(), (2, 2));
    assert_eq!(book.known("a"), Some(known(1, 1)));
    assert_eq!(book.known("b"), Some(known(2, 2)));
    assert!(book.learn_at("c", known(3, 3), book.epoch()));
    assert_eq!(book.len(), (2, 2));
    assert!(book.missed("y", 0));
    assert!(book.missed("z", 0));
}

/// Asserts a limit of 0 is taken as 1: only the last object and the last miss are kept.
#[test]
fn a_limit_of_zero_is_taken_as_one() {
    let mut book = Book::new(0);
    assert!(book.learn_at("a", known(1, 1), book.epoch()));
    assert!(book.learn_at("b", known(2, 2), book.epoch()));
    assert_eq!(book.known("a"), None);
    assert_eq!(book.known("b"), Some(known(2, 2)));
    assert!(book.miss_at("x", 100, book.epoch()));
    assert!(book.miss_at("y", 100, book.epoch()));
    assert!(!book.missed("x", 0));
    assert!(book.missed("y", 0));
    assert_eq!(book.len(), (1, 1));
}

/// Asserts learning a key already held does not grow the book and counts as a use.
#[test]
fn relearning_does_not_grow_and_refreshes_use() {
    let mut book = Book::new(2);
    assert!(book.learn_at("a", known(1, 1), book.epoch()));
    assert!(book.learn_at("b", known(2, 2), book.epoch()));
    assert!(book.learn_at("a", known(3, 3), book.epoch()));
    assert_eq!(book.len(), (2, 0));
    assert!(book.learn_at("c", known(4, 4), book.epoch()));
    assert_eq!(book.known("b"), None);
    assert_eq!(book.known("a"), Some(known(3, 3)));
    assert_eq!(book.known("c"), Some(known(4, 4)));
}

/// Asserts re-missing a key already held does not grow the misses and replaces its deadline.
#[test]
fn remissing_does_not_grow_and_replaces_the_deadline() {
    let mut book = Book::new(2);
    assert!(book.miss_at("a", 10, book.epoch()));
    assert!(book.miss_at("b", 100, book.epoch()));
    assert!(book.miss_at("a", 200, book.epoch()));
    assert_eq!(book.len(), (0, 2));
    assert!(book.miss_at("c", 100, book.epoch()));
    assert!(!book.missed("b", 0));
    assert!(book.missed("a", 150));
    assert!(book.missed("c", 0));
}

/// Asserts len counts both tables.
#[test]
fn len_counts_objects_and_misses() {
    let mut book = Book::new(8);
    assert_eq!(book.len(), (0, 0));
    assert!(book.learn_at("a", known(1, 1), book.epoch()));
    assert!(book.learn_at("b", known(2, 2), book.epoch()));
    assert!(book.miss_at("c", 100, book.epoch()));
    assert_eq!(book.len(), (2, 1));
}

/// Asserts that a forget, of any key, moves the epoch, and that an answer asked for at the epoch
/// before it is not learned and not remembered as a miss, while one at the new epoch is.
#[test]
fn an_answer_from_before_a_forget_is_not_put_in_the_book() {
    let mut book = Book::new(8);
    let before = book.epoch();
    book.forget("other");
    assert_eq!(book.epoch(), before + 1);
    assert!(!book.learn_at("a", known(1, 10), before));
    assert!(!book.miss_at("b", 100, before));
    assert_eq!(book.known("a"), None);
    assert!(!book.missed("b", 0));
    assert_eq!(book.len(), (0, 0));
    let now = book.epoch();
    assert!(book.learn_at("a", known(1, 10), now));
    assert!(book.miss_at("b", 100, now));
    assert_eq!(book.len(), (1, 1));
}
