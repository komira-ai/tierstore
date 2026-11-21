use super::super::super::ino;
use super::{Failing, Lines};
use crate::tree_manifest::TreeDigest;

fn digest(byte: u8) -> TreeDigest {
    TreeDigest::from_hex(&format!("{byte:02x}").repeat(32)).unwrap()
}

/// Asserts that the first failure of a tree and kind is said, those within ten seconds are counted
/// and not said, and the count rides on the next line said.
#[test]
fn a_failure_is_said_once_in_ten_seconds_and_the_next_line_carries_the_count() {
    let lines = Lines::default();
    let tree = digest(0xab);
    assert_eq!(
        lines.failed((7, Failing::Open), Some(&tree), 1_000),
        Some(0)
    );
    assert_eq!(lines.failed((7, Failing::Open), Some(&tree), 1_001), None);
    assert_eq!(lines.failed((7, Failing::Open), Some(&tree), 10_999), None);
    assert_eq!(
        lines.failed((7, Failing::Open), Some(&tree), 11_000),
        Some(2)
    );
    // Another kind, and another tree, are said at once.
    assert_eq!(
        lines.failed((7, Failing::Read), Some(&tree), 11_001),
        Some(0)
    );
    assert_eq!(lines.failed((8, Failing::Open), None, 11_001), Some(0));
}

/// Asserts that the counts not said are said, whole, once their last line is ten seconds old, and
/// not before; that a tree said this way has its next failure said at once; and that a tree with
/// nothing left to say is forgotten then.
#[test]
fn the_counts_not_said_are_said_by_the_sweep_once_the_last_line_is_ten_seconds_old() {
    let lines = Lines::default();
    let tree = digest(0x01);
    let _ = lines.failed((3, Failing::Open), Some(&tree), 0);
    let _ = lines.failed((3, Failing::Open), Some(&tree), 5);
    let _ = lines.failed((3, Failing::Open), Some(&tree), 6);
    let _ = lines.failed((4, Failing::Read), None, 0);
    let _ = lines.failed((4, Failing::Read), None, 1);
    let _ = lines.failed((0, Failing::Other), None, 0);
    let _ = lines.failed((0, Failing::Other), None, 2);
    let _ = lines.failed((9, Failing::Tree), Some(&tree), 0);
    assert_eq!(lines.unsaid(9_999, None), Vec::<String>::new());
    assert_eq!(lines.kept(), 4);
    let hex = tree.to_string();
    assert_eq!(
        lines.unsaid(10_000, None),
        vec![
            "tierstore fs: 1 more failed read of an open file of the tree number 4 since the last \
             line, not said"
                .to_owned(),
            "tierstore fs: 1 more other failed request since the last line, not said".to_owned(),
            format!(
                "tierstore fs: 2 more failed opens of the tree {hex} (number 3) since the last \
                 line, not said"
            ),
        ]
    );
    assert_eq!(lines.kept(), 0);
    assert_eq!(
        lines.failed((3, Failing::Open), Some(&tree), 10_001),
        Some(0)
    );
}

/// Asserts that a tree that leaves memory says its counts at once, whatever the time, and only
/// its own.
#[test]
fn a_tree_that_leaves_memory_says_its_counts_at_once_and_only_its_own() {
    let lines = Lines::default();
    let tree = digest(0x02);
    for now in 0..4 {
        let _ = lines.failed((5, Failing::Tree), Some(&tree), now);
        let _ = lines.failed((6, Failing::Tree), Some(&tree), now);
    }
    let _ = lines.failed((5, Failing::Open), Some(&tree), 0);
    assert_eq!(
        lines.unsaid(4, Some(5)),
        vec![format!(
            "tierstore fs: 3 more failed reads of the tree {tree} (number 5) since the last line, \
             not said"
        )]
    );
    assert_eq!(lines.kept(), 1);
    assert_eq!(lines.unsaid(4, Some(5)), Vec::<String>::new());
    // The count of the tree that stayed is kept.
    assert_eq!(
        lines.failed((6, Failing::Tree), Some(&tree), 10_003),
        Some(3)
    );
}

/// Asserts the words of one failure not said, of each kind.
#[test]
fn one_failure_not_said_is_in_the_singular_for_each_kind() {
    let lines = Lines::default();
    let number = ino::split(ino::of(2, 0)).0;
    for what in [Failing::Open, Failing::Tree, Failing::Other] {
        let _ = lines.failed((number, what), None, 0);
        let _ = lines.failed((number, what), None, 1);
    }
    assert_eq!(
        lines.unsaid(20_000, None),
        vec![
            "tierstore fs: 1 more failed open of the tree number 2 since the last line, not said",
            "tierstore fs: 1 more failed read of the tree number 2 since the last line, not said",
            "tierstore fs: 1 more other failed request of the tree number 2 since the last line, \
             not said",
        ]
    );
}
