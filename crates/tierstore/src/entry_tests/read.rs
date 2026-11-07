//! The read of an entry: one tagged get, and the head and the get of a store without it.

use super::*;

/// Asserts that a failed get of a read, and a failed head or get of a read from a store without
/// the tagged get, are the backend's errors, not an absent entry.
#[tokio::test]
async fn a_read_whose_head_or_get_fails_gives_that_error() {
    for (op, untagged) in [(Op::Get, false), (Op::Head, true), (Op::Get, true)] {
        let rig = Rig::new();
        let a = rig.actor("a");
        take(&a, "k", Take::Put).await;
        if untagged {
            rig.store.fail_next_with(
                Op::Get,
                Error::Unsupported("get_tagged: not here".to_owned()),
            );
        }
        rig.store
            .fail_next_with(op, Error::Unavailable("down".to_owned()));
        let error = a.entries.read("k").await.unwrap_err();
        assert_eq!(
            error,
            Error::Unavailable("down".to_owned()),
            "{op:?} {untagged}"
        );
    }
}

/// Asserts that an entry is read with one get, which carries its tag, and no head: the store
/// sees one request for it, and the tag is the one a head gives.
#[tokio::test]
async fn an_entry_is_read_with_one_get_and_no_head() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let taken = take(&a, "k", Take::Put).await;
    let path = entry_key("k");
    let (heads, gets) = (
        rig.store.calls_on(Op::Head, &path),
        rig.store.calls_on(Op::Get, &path),
    );
    let seen = read(&a, "k").await;
    assert_eq!(rig.store.calls_on(Op::Head, &path), heads);
    assert_eq!(rig.store.calls_on(Op::Get, &path), gets + 1);
    assert_eq!(seen, taken);
    assert_eq!(seen.tag, rig.memory.head(&path).await.unwrap().tag);
}

/// Asserts that a store without the tagged get is read with a head and a get, and gives the same
/// entry.
#[tokio::test]
async fn an_entry_of_a_store_without_the_tagged_get_is_read_with_a_head_and_a_get() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let taken = take(&a, "k", Take::Put).await;
    let path = entry_key("k");
    let heads = rig.store.calls_on(Op::Head, &path);
    rig.store.fail_next_with(
        Op::Get,
        Error::Unsupported("get_tagged: not here".to_owned()),
    );
    assert_eq!(read(&a, "k").await, taken);
    assert_eq!(rig.store.calls_on(Op::Head, &path), heads + 1);
}

/// Asserts that an entry that is gone when the tagged get asks for it reads as none.
#[tokio::test]
async fn an_entry_that_the_tagged_get_does_not_find_reads_as_none() {
    let rig = Rig::new();
    let a = rig.actor("a");
    take(&a, "k", Take::Put).await;
    rig.store
        .fail_next_with(Op::Get, Error::NotFound(entry_key("k")));
    assert_eq!(a.entries.read("k").await.unwrap(), None);
}

/// Asserts that an entry that is deleted between the head and the get of a read of a store without
/// the tagged get reads as none.
#[tokio::test]
async fn an_entry_deleted_between_the_head_and_the_get_of_a_read_reads_as_none() {
    let rig = Rig::new();
    let a = rig.actor("a");
    take(&a, "k", Take::Put).await;
    rig.store.fail_next_with(
        Op::Get,
        Error::Unsupported("get_tagged: not here".to_owned()),
    );
    let gate = a.gated.gate(Op::Head, "", Phase::Post);
    let mut reading = Box::pin(a.entries.read("k"));
    until_parked(&gate, &mut reading).await;
    rig.memory.delete(&entry_key("k")).await.unwrap();
    assert!(gate.release());
    assert_eq!(reading.await.unwrap(), None);
}

/// Asserts that when the entry is replaced between the head and the get of a read of a store
/// without the tagged get, the read has the newer body with the older tag, and a step taken on it
/// is lost and changes nothing.
#[tokio::test]
async fn a_read_with_a_newer_body_than_its_tag_is_lost_when_it_acts() {
    let rig = Rig::new();
    let (a, b) = (rig.actor("a"), rig.actor("b"));
    let held = take(&b, "k", Take::Put).await;
    rig.store.fail_next_with(
        Op::Get,
        Error::Unsupported("get_tagged: not here".to_owned()),
    );
    let gate = a.gated.gate(Op::Head, "", Phase::Post);
    let mut reading = Box::pin(a.entries.read("k"));
    until_parked(&gate, &mut reading).await;
    let landed = step(&b, &held, Step::LandPut).await;
    assert!(gate.release());
    let seen = reading.await.unwrap().unwrap();
    assert_eq!((seen.entry.st, &seen.tag), (EntryState::Pending, &held.tag));

    let lost = a.entries.advance(&seen, Step::Seal).await.unwrap();
    assert_eq!(lost, Outcome::Lost);
    assert_eq!(read(&a, "k").await, landed);
}

/// Asserts that a key with no entry reads as none from a store without the tagged get, whose head
/// does not find it.
#[tokio::test]
async fn a_key_with_no_entry_reads_as_none_from_a_store_without_the_tagged_get() {
    let rig = Rig::new();
    rig.store.fail_next_with(
        Op::Get,
        Error::Unsupported("get_tagged: not here".to_owned()),
    );
    assert_eq!(rig.actor("a").entries.read("k").await.unwrap(), None);
}
