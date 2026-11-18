//! The locks of the pins and unpins: pins of one set together, an unpin alone, and sets apart.

use super::PinLocks;
use crate::tree_manifest::TreeDigest;

/// A digest whose last byte is `last`.
fn digest(last: u8) -> TreeDigest {
    let mut bytes = [9_u8; 32];
    bytes[31] = last;
    TreeDigest::from_bytes(bytes)
}

/// Asserts that the pins of one tree are held together, an unpin of its set waits for them (it
/// cannot be taken while one is held), and a tree of another set is not held by either.
#[test]
fn pins_of_one_set_are_together_and_an_unpin_excludes_them_but_not_another_set() {
    let locks = PinLocks::new();
    let (tree, same, other) = (digest(5), digest(5), digest(6));
    assert_ne!(PinLocks::set_of(&tree), PinLocks::set_of(&other));
    let first = locks.pin(&tree);
    let second = locks.pin(&same);
    assert!(!locks.free(&tree));
    assert!(locks.free(&other));
    drop(first);
    assert!(!locks.free(&tree));
    drop(second);
    assert!(locks.free(&tree));
    let unpin = locks.unpin(&tree);
    assert_eq!(locks.unpins_asked(), 1);
    let _elsewhere = locks.pin(&other);
    assert!(!locks.free(&tree));
    drop(unpin);
    assert!(locks.free(&tree));
}

/// Asserts that every byte value is a set of its own: 256 trees whose digests end in each byte hold
/// their locks at once.
#[test]
fn every_last_byte_is_a_set_of_its_own() {
    let locks = PinLocks::new();
    let held: Vec<_> = (0..=255_u8)
        .map(|last| locks.unpin(&digest(last)))
        .collect();
    assert_eq!(held.len(), 256);
    assert_eq!(locks.unpins_asked(), 256);
}
