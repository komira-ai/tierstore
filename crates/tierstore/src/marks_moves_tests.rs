//! The transition table of the marker (`docs/design/put-without-lock.md`, 4.3): what each move
//! makes of a marker, and from what shapes it is refused. No store is involved.

use crate::marks::{MarkBirth, MarkMove, Next};
use crate::{MARKER_VERSION, Marker, MarkerState, MarkerWhy};

const ME: &str = "me";
const THEM: &str = "them";

fn marker(
    st: MarkerState,
    gone: bool,
    dirty: bool,
    why: Option<MarkerWhy>,
    owner: Option<&str>,
) -> Marker {
    Marker {
        v: MARKER_VERSION,
        key: "k".to_owned(),
        st,
        gone,
        dirty,
        why,
        owner: owner.map(str::to_owned),
        nonce: "n".to_owned(),
        extra: serde_json::Map::new(),
    }
}

/// A marker that nobody holds: `DIRTY` or `WANT`.
fn free(st: MarkerState, gone: bool) -> Marker {
    let dirty = st == MarkerState::Dirty;
    marker(st, gone, dirty, None, None)
}

/// A lock of `owner` for `why`, as a creation makes it, with the `gone` given.
fn lock(why: MarkerWhy, gone: bool, owner: &str) -> Marker {
    let dirty = why.owes_cold_a_copy();
    marker(MarkerState::Lock, gone, dirty, Some(why), Some(owner))
}

fn shape(
    st: MarkerState,
    gone: bool,
    dirty: bool,
    why: Option<MarkerWhy>,
    owner: Option<&str>,
) -> Next {
    Next {
        st,
        gone,
        dirty,
        why,
        owner: owner.map(str::to_owned),
    }
}

/// The marker a shape makes, as the stored body would read.
fn made(next: Next) -> Marker {
    marker(
        next.st,
        next.gone,
        next.dirty,
        next.why,
        next.owner.as_deref(),
    )
}

/// Takes `mv` on `from` as `ME` and gives the marker it makes.
fn took(from: &Marker, mv: MarkMove) -> Marker {
    made(mv.apply(from, ME).expect("the move applies"))
}

/// Asserts the creations of the table: M1 and M12 make `DIRTY` with `gone = false`; M10 makes `WANT`
/// with nothing owed; M3 makes a lock owned by the writer with `gone = false`, owing cold a copy
/// for every reason but an eviction and a copy-back.
#[test]
fn a_creation_makes_the_marker_the_table_names() {
    let dirty = MarkBirth::Dirty.shape(ME);
    assert_eq!(dirty, shape(MarkerState::Dirty, false, true, None, None));
    let want = MarkBirth::Want.shape(ME);
    assert_eq!(want, shape(MarkerState::Want, false, false, None, None));
    let reasons = [
        (MarkerWhy::Overwrite, true),
        (MarkerWhy::Delete, true),
        (MarkerWhy::ColdLand, true),
        (MarkerWhy::Evict, false),
        (MarkerWhy::CopyBack, false),
    ];
    for (why, dirty) in reasons {
        let locked = MarkBirth::Lock(why.clone()).shape(ME);
        let want = shape(MarkerState::Lock, false, dirty, Some(why), Some(ME));
        assert_eq!(locked, want);
    }
}

/// Asserts M2: a put whose object landed makes `DIRTY` or `WANT` `DIRTY` with something owed, and
/// keeps `gone`, so that a reader that missed hot before the object landed still finds a deleted
/// key deleted.
#[test]
fn a_bump_makes_dirty_and_keeps_gone() {
    for (from, gone) in [
        (MarkerState::Dirty, false),
        (MarkerState::Dirty, true),
        (MarkerState::Want, false),
        (MarkerState::Want, true),
    ] {
        let bumped = took(&free(from.clone(), gone), MarkMove::Bump);
        let want = made(shape(MarkerState::Dirty, gone, true, None, None));
        assert_eq!(bumped, want, "{from:?} gone={gone}");
    }
}

/// Asserts M4 and M11: a lock taken over `DIRTY` or `WANT` is owned by the taker, keeps `gone` as
/// found, and owes cold a copy if the marker did or the reason is one that changes hot; a copy-back
/// over `WANT` owes nothing.
#[test]
fn taking_the_lock_keeps_gone_and_keeps_what_was_owed() {
    let cases = [
        // (found, gone, reason, dirty of the lock)
        (MarkerState::Dirty, true, MarkerWhy::Overwrite, true),
        (MarkerState::Want, false, MarkerWhy::Overwrite, true),
        (MarkerState::Want, false, MarkerWhy::Delete, true),
        (MarkerState::Want, false, MarkerWhy::ColdLand, true),
        (MarkerState::Dirty, false, MarkerWhy::Evict, true),
        (MarkerState::Dirty, true, MarkerWhy::CopyBack, true),
        (MarkerState::Want, false, MarkerWhy::CopyBack, false),
        (MarkerState::Want, false, MarkerWhy::Evict, false),
    ];
    for (st, gone, why, dirty) in cases {
        let from = free(st.clone(), gone);
        let locked = took(&from, MarkMove::Lock(why.clone()));
        let want = shape(MarkerState::Lock, gone, dirty, Some(why.clone()), Some(ME));
        assert_eq!(locked, made(want), "{st:?} gone={gone} {why:?}");
    }
}

/// Asserts M4': a deleter that found hot under its own lock writes `gone = true` into the lock,
/// and only a `LOCK(DELETE)` of its own takes it.
#[test]
fn a_deleter_writes_gone_into_its_own_delete_lock_and_nothing_else_takes_it() {
    let own = lock(MarkerWhy::Delete, false, ME);
    let decided = took(&own, MarkMove::Gone);
    assert_eq!(decided, lock(MarkerWhy::Delete, true, ME));
    let refused = [
        lock(MarkerWhy::Overwrite, false, ME),
        lock(MarkerWhy::Delete, false, THEM),
        free(MarkerState::Dirty, false),
        marker(MarkerState::Clearing, false, false, None, Some(ME)),
    ];
    for from in refused {
        assert_eq!(MarkMove::Gone.apply(&from, ME), None, "{from:?}");
    }
}

/// Asserts M5 and M6: a lock ends `DIRTY` when the key owes cold a copy and `CLEARING`, owned by
/// the holder, when it does not, `gone` kept either way; a holder that changed hot though the lock
/// did not say it would (`changed`) ends `DIRTY` whatever it was.
#[test]
fn a_lock_ends_dirty_if_the_key_owes_cold_a_copy_and_clearing_if_not() {
    let dirty = |gone| made(shape(MarkerState::Dirty, gone, true, None, None));
    let clearing = |gone| made(shape(MarkerState::Clearing, gone, false, None, Some(ME)));
    let end = |from: &Marker, changed| took(from, MarkMove::Release { changed });

    assert_eq!(
        end(&lock(MarkerWhy::Overwrite, false, ME), false),
        dirty(false)
    );
    assert_eq!(
        end(&lock(MarkerWhy::Overwrite, true, ME), false),
        dirty(true)
    );
    assert_eq!(end(&lock(MarkerWhy::Delete, true, ME), false), dirty(true));
    assert_eq!(
        end(&lock(MarkerWhy::ColdLand, false, ME), false),
        dirty(false)
    );
    assert_eq!(
        end(&lock(MarkerWhy::Evict, false, ME), false),
        clearing(false)
    );
    assert_eq!(
        end(&lock(MarkerWhy::CopyBack, false, ME), false),
        clearing(false)
    );
    assert_eq!(end(&lock(MarkerWhy::Evict, false, ME), true), dirty(false));
    assert_eq!(
        end(&lock(MarkerWhy::CopyBack, false, ME), true),
        dirty(false)
    );

    // An eviction that took the lock over a marker that owed a copy still owes it.
    let over_dirty = took(
        &free(MarkerState::Dirty, true),
        MarkMove::Lock(MarkerWhy::Evict),
    );
    assert_eq!(end(&over_dirty, false), dirty(true));
}

/// Asserts M14: a refresh writes the holder's own lock again, every field the same.
#[test]
fn a_refresh_writes_the_same_lock() {
    for why in [MarkerWhy::Overwrite, MarkerWhy::Delete, MarkerWhy::Evict] {
        let own = lock(why, true, ME);
        assert_eq!(took(&own, MarkMove::Refresh), own);
    }
}

/// Asserts M7: the copier is done with a `DIRTY` marker, `gone` or not, and makes it `CLEARING`,
/// owned by the copier with nothing owed; a marker of any other state is not its to clear.
#[test]
fn the_copier_clears_a_dirty_marker_and_no_other() {
    for gone in [false, true] {
        let cleared = took(&free(MarkerState::Dirty, gone), MarkMove::Clear);
        let want = marker(MarkerState::Clearing, gone, false, None, Some(ME));
        assert_eq!(cleared, want);
    }
    let refused = [
        free(MarkerState::Want, false),
        lock(MarkerWhy::Overwrite, false, ME),
        marker(MarkerState::Clearing, false, false, None, Some(THEM)),
    ];
    for from in refused {
        assert_eq!(MarkMove::Clear.apply(&from, ME), None, "{from:?}");
    }
}

/// Asserts M13: the leader clears an orphan, a `DIRTY` marker with `gone = false`, and never a
/// `gone` one, which only the tombstone hold may clear.
#[test]
fn the_leader_clears_an_orphan_that_is_not_gone() {
    let cleared = took(&free(MarkerState::Dirty, false), MarkMove::ClearOrphan);
    assert_eq!(
        cleared,
        marker(MarkerState::Clearing, false, false, None, Some(ME))
    );
    let gone = free(MarkerState::Dirty, true);
    assert_eq!(MarkMove::ClearOrphan.apply(&gone, ME), None);
}

/// Asserts M9, one case per row of 5.13: a lock the leader reaps ends as its reason requires, and
/// keeps `gone` (a reaped overwrite over a `gone` marker must not clear it, or a deleted cold copy
/// would be served again).
#[test]
fn a_lock_is_reaped_to_the_state_its_reason_requires() {
    let dirty = |gone| made(shape(MarkerState::Dirty, gone, true, None, None));
    let clearing = |gone| made(shape(MarkerState::Clearing, gone, false, None, Some(ME)));
    let reap = |from: &Marker| took(from, MarkMove::Reap);

    assert_eq!(reap(&lock(MarkerWhy::Overwrite, false, THEM)), dirty(false));
    assert_eq!(reap(&lock(MarkerWhy::Overwrite, true, THEM)), dirty(true));
    assert_eq!(reap(&lock(MarkerWhy::Delete, true, THEM)), dirty(true));
    assert_eq!(reap(&lock(MarkerWhy::Delete, false, THEM)), dirty(false));
    assert_eq!(reap(&lock(MarkerWhy::ColdLand, true, THEM)), dirty(true));
    assert_eq!(reap(&lock(MarkerWhy::Evict, false, THEM)), clearing(false));
    assert_eq!(
        reap(&lock(MarkerWhy::CopyBack, false, THEM)),
        clearing(false)
    );

    // A lock taken over a marker that owed a copy gives it back to the copier.
    let over_dirty = took(
        &free(MarkerState::Dirty, false),
        MarkMove::Lock(MarkerWhy::CopyBack),
    );
    assert_eq!(reap(&over_dirty), dirty(false));
}

/// Asserts that the leader reaps a state it does not know as the design says to treat it, as a
/// lock, to `DIRTY` with `gone` kept, so that nothing owed is dropped, and that a `CLEARING` is not
/// reaped by a move (the leader removes it).
#[test]
fn a_state_the_leader_does_not_know_is_reaped_to_dirty_and_a_clearing_is_not_reaped() {
    let unknown = marker(
        MarkerState::Other("FROZEN".to_owned()),
        true,
        false,
        None,
        Some(THEM),
    );
    let reaped = took(&unknown, MarkMove::Reap);
    assert_eq!(
        reaped,
        made(shape(MarkerState::Dirty, true, true, None, None))
    );
    let clearing = marker(MarkerState::Clearing, false, false, None, Some(THEM));
    assert_eq!(MarkMove::Reap.apply(&clearing, ME), None);
}

/// What the design says of who may write which state, written out once more, apart from `apply`:
/// `none`, `DIRTY` and `WANT` are written by anyone; `LOCK` and `CLEARING` only by their owner, or
/// by the leader reaping. For each move, whether it applies to a marker of this shape.
fn the_table_allows(mv: &MarkMove, from: &Marker) -> bool {
    let nobody_holds = matches!(from.st, MarkerState::Dirty | MarkerState::Want);
    let own_lock = matches!(
        (&from.st, from.owner.as_deref()),
        (MarkerState::Lock, Some(ME))
    );
    match mv {
        MarkMove::Bump | MarkMove::Lock(_) => nobody_holds,
        MarkMove::Gone => own_lock && from.why == Some(MarkerWhy::Delete),
        MarkMove::Release { .. } | MarkMove::Refresh => own_lock,
        MarkMove::Clear => from.st == MarkerState::Dirty,
        MarkMove::ClearOrphan => matches!((&from.st, from.gone), (MarkerState::Dirty, false)),
        MarkMove::Reap => matches!(from.st, MarkerState::Lock | MarkerState::Other(_)),
    }
}

/// Asserts, for every move against every shape of marker (each state, each owner, each reason,
/// `gone` and `dirty` both ways), that it applies exactly where the table says it does: a writer
/// that finds a lock or a clearing it does not own writes nothing, a stale owner's refresh or
/// release of a lock someone else holds is refused, and a copier cannot clear a lock.
#[test]
fn a_move_applies_exactly_where_the_table_says() {
    let moves = [
        MarkMove::Bump,
        MarkMove::Lock(MarkerWhy::Overwrite),
        MarkMove::Lock(MarkerWhy::CopyBack),
        MarkMove::Gone,
        MarkMove::Release { changed: false },
        MarkMove::Release { changed: true },
        MarkMove::Refresh,
        MarkMove::Clear,
        MarkMove::ClearOrphan,
        MarkMove::Reap,
    ];
    let states = [
        MarkerState::Dirty,
        MarkerState::Lock,
        MarkerState::Clearing,
        MarkerState::Want,
        MarkerState::Other("FROZEN".to_owned()),
    ];
    let owners = [None, Some(ME), Some(THEM)];
    let reasons = [
        None,
        Some(MarkerWhy::Overwrite),
        Some(MarkerWhy::Delete),
        Some(MarkerWhy::Evict),
        Some(MarkerWhy::Other("REBALANCE".to_owned())),
    ];
    let mut checked = 0;
    for mv in &moves {
        for st in &states {
            for owner in owners {
                for why in &reasons {
                    for (gone, dirty) in
                        [(false, false), (false, true), (true, false), (true, true)]
                    {
                        let from = marker(st.clone(), gone, dirty, why.clone(), owner);
                        let applies = mv.clone().apply(&from, ME).is_some();
                        assert_eq!(applies, the_table_allows(mv, &from), "{mv:?} on {from:?}");
                        checked += 1;
                    }
                }
            }
        }
    }
    assert_eq!(checked, 10 * 5 * 3 * 5 * 4);
}

/// Asserts the delete of the design as a walk through the table (5.6): lock over nothing, decide
/// `gone` after the HEAD, release to `DIRTY(gone)`, the copier clears after it deleted cold.
#[test]
fn a_delete_walks_lock_gone_dirty_clearing() {
    let locked = made(MarkBirth::Lock(MarkerWhy::Delete).shape(ME));
    assert_eq!(locked, lock(MarkerWhy::Delete, false, ME));
    let decided = took(&locked, MarkMove::Gone);
    let released = took(&decided, MarkMove::Release { changed: false });
    assert_eq!(released, free(MarkerState::Dirty, true));
    let cleared = took(&released, MarkMove::Clear);
    assert_eq!(
        cleared,
        marker(MarkerState::Clearing, true, false, None, Some(ME))
    );
}

/// Asserts the put over a deleted key of 5.1: the bump keeps `gone` while cold still holds the old
/// object, so a reader that misses hot finds `gone` and answers absent.
#[test]
fn a_put_over_a_deleted_key_keeps_gone_through_the_bump_and_a_later_overwrite() {
    let deleted = free(MarkerState::Dirty, true);
    let bumped = took(&deleted, MarkMove::Bump);
    assert!(bumped.gone);
    let overwrite = took(&bumped, MarkMove::Lock(MarkerWhy::Overwrite));
    assert!(overwrite.gone);
    let released = took(&overwrite, MarkMove::Release { changed: false });
    assert_eq!(released, free(MarkerState::Dirty, true));
}

/// Asserts the copy-back of 5.11 as a walk: `WANT` is locked by the leader, nothing is owed, and
/// the release clears it.
#[test]
fn a_copy_back_walks_want_lock_clearing() {
    let locked = took(
        &free(MarkerState::Want, false),
        MarkMove::Lock(MarkerWhy::CopyBack),
    );
    assert!(!locked.dirty);
    let released = took(&locked, MarkMove::Release { changed: false });
    assert_eq!(
        released,
        marker(MarkerState::Clearing, false, false, None, Some(ME))
    );
}
