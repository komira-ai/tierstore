//! The bytes a copy moves.

use crate::{ByteRange, CopyTarget, SourceConditions, StoreCopy};

/// Asserts that a copy of an object and of a whole part move the source's bytes, and the copy
/// of a range of it the range's.
#[test]
fn a_copy_moves_the_source_or_the_range_of_its_part() {
    let conditions = SourceConditions::default();
    let copy = |target| StoreCopy {
        source: "a",
        source_size: 10,
        conditions: &conditions,
        key: "b",
        target,
    };
    let part = |range| CopyTarget::Part {
        upload_id: "u",
        number: 1,
        range,
    };

    assert_eq!(copy(CopyTarget::Object).moved(), 10);
    assert_eq!(copy(part(None)).moved(), 10);
    assert_eq!(copy(part(Some(ByteRange::new(2, 3)))).moved(), 3);
}
