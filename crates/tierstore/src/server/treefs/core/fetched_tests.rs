//! The fetches of one read of a tree, as they are said and reported: nothing for a read that
//! fetched no manifest; the levels in order, with the fills and the slowest fill of each, and a
//! level met only by reads from the cache; and each answer of a `fill-tree`, with the rounds it
//! adds.

use super::{Fetched, TreeAsked};
use crate::server::local_cache::TreeFilled;
use crate::server::local_cache::attaches::Levels;
use crate::server::treefs::fills::TreeAnswer;

/// Asserts that a read that fetched no manifest (a placement's tree) says nothing and reports no
/// level and no round.
#[test]
fn a_read_with_no_manifest_says_nothing() {
    let fetched = Fetched::default();
    assert_eq!(fetched.by_level(), "");
    assert_eq!(fetched.levels(), Levels::default());
}

/// Asserts that the levels are said in order, each with its fills and its slowest fill, a level
/// read only from the cache with none, and that the rounds are the levels with a fill: fills of 4
/// and 9 ms at level 0, none at level 1, one of 2 ms at level 2 that was not had.
#[test]
fn the_levels_are_said_in_order_with_their_fills_and_slowest() {
    let fetched = Fetched::default();
    fetched.filled(0, 4, true);
    fetched.filled(0, 9, true);
    fetched.cached(1);
    fetched.filled(2, 2, false);
    assert_eq!(
        fetched.by_level(),
        "; 3 levels; fills by level: 2 0 1, slowest ms by level: 9 0 2"
    );
    assert_eq!(
        fetched.levels(),
        Levels {
            levels: 3,
            rounds: 2,
        }
    );
    let counts = fetched.counts();
    assert_eq!((counts.cache, counts.filled, counts.failed), (1, 2, 1));
    assert_eq!(counts.fill_ms, 15);
}

/// Asserts that each answer of a `fill-tree` is said after the levels, in its own words, and adds
/// one round: filled, with the node's time and its fills by level; failed, with the words of the
/// failure.
#[test]
fn each_answer_of_a_fill_tree_is_said_and_adds_a_round() {
    let said = |answer: TreeAnswer| {
        let fetched = Fetched::default();
        fetched.cached(0);
        fetched.asked_tree(TreeAsked { answer, ms: 7 });
        assert_eq!(
            fetched.levels(),
            Levels {
                levels: 1,
                rounds: 1,
            }
        );
        fetched.by_level()
    };
    let levels = "; 1 levels; fills by level: 0, slowest ms by level: 0";
    let filled = TreeAnswer::Filled(TreeFilled {
        ms: 5,
        levels: vec![1, 2],
    });
    assert_eq!(
        said(filled),
        format!("{levels}; fill-tree ok in 7 ms, 5 ms at the node, fills by level: 1 2")
    );
    assert_eq!(
        said(TreeAnswer::Failed("read-only".to_owned())),
        format!("{levels}; fill-tree failed in 7 ms: read-only")
    );
}
