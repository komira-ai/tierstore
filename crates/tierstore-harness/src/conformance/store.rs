//! The cases of the store's entries: open, get, remove, listing order and holds (section 7).
//!
//! # Panics
//!
//! Every case panics, with a message that starts `conformance:`, when the implementation breaks
//! the contract the case checks.

use tierstore_fetch_api::entry::{CellCounts, EntryError, EntryInfo, EntrySpec};
use tierstore_fetch_api::ids::HolderId;

use super::EntryStoreFixture;
use super::check::{ensure, ensure_eq, expect_err};
use super::support::{MIB, open, pack, publish, run, same};

/// Opening a pack twice gives the same entry (a claim through one is seen through the other),
/// `get` finds it and nothing else, another size is `SizeMismatch` and changes nothing, and a pack
/// of more than `u32::MAX` cells is `TooLarge` and opens nothing.
pub fn open_returns_the_one_entry_of_a_pack(f: &dyn EntryStoreFixture) {
    let store = f.store();
    ensure(
        store.get(&pack("p")).is_none(),
        "an empty store has no entry",
    );
    let first = open(&*store, "p", 3 * MIB);
    let second = open(&*store, "p", 3 * MIB);
    ensure_eq(
        first.claim(run(0, 1)).len(),
        1,
        "a claim through the first handle",
    );
    ensure_eq(
        second.counts().claimed,
        1,
        "the second handle sees the claim",
    );
    let got = store.get(&pack("p"));
    ensure_eq(
        got.map(|e| e.counts().claimed),
        Some(1),
        "get returns the entry",
    );
    ensure(
        store.get(&pack("q")).is_none(),
        "get of another pack is None",
    );

    expect_err(
        store.open(&EntrySpec {
            pack: pack("p"),
            size: 4 * MIB,
        }),
        |e| {
            same(
                e,
                &EntryError::SizeMismatch {
                    pack: pack("p"),
                    size: 4 * MIB,
                    existing: 3 * MIB,
                },
            )
        },
        "an open with another size",
    );
    ensure_eq(first.size(), 3 * MIB, "a refused open leaves the size");

    expect_err(
        store.open(&EntrySpec {
            pack: pack("big"),
            size: (u64::from(u32::MAX) + 1) << 20,
        }),
        |e| matches!(e, EntryError::TooLarge { .. }),
        "a pack of 2^32 cells",
    );
    ensure(
        store.get(&pack("big")).is_none(),
        "a refused open creates no entry",
    );
    ensure_eq(
        store.packs(),
        vec![pack("p")],
        "only the opened pack is listed",
    );
}

/// `packs` and `entries` list every entry in pack order, whatever the order of opening, and
/// `entries` reports each pack's size, last use, holders and cell counts.
pub fn packs_and_entries_are_in_pack_order(f: &dyn EntryStoreFixture) {
    let store = f.store();
    open(&*store, "b", 2 * MIB);
    let a = open(&*store, "a", MIB);
    open(&*store, "c", 3 * MIB);
    publish(&*a, 0, 1);
    a.touch(4_000, 0);
    a.hold(HolderId(1));
    ensure_eq(
        store.packs(),
        vec![pack("a"), pack("b"), pack("c")],
        "packs in pack order",
    );
    ensure_eq(
        store.entries(),
        vec![
            EntryInfo {
                pack: pack("a"),
                size: MIB,
                last_use: 4_000,
                holders: 1,
                counts: CellCounts {
                    present: 1,
                    ..CellCounts::default()
                },
            },
            EntryInfo {
                pack: pack("b"),
                size: 2 * MIB,
                last_use: 0,
                holders: 0,
                counts: CellCounts {
                    absent: 2,
                    ..CellCounts::default()
                },
            },
            EntryInfo {
                pack: pack("c"),
                size: 3 * MIB,
                last_use: 0,
                holders: 0,
                counts: CellCounts {
                    absent: 3,
                    ..CellCounts::default()
                },
            },
        ],
        "entries in pack order with their info",
    );
}

/// `remove` refuses a held entry and removes an unheld one (it is gone from `get` and `packs`),
/// reports `false` for a pack that is not there, and a released entry can be removed.
pub fn remove_refuses_a_held_entry_and_removes_an_unheld_one(f: &dyn EntryStoreFixture) {
    let store = f.store();
    let held = open(&*store, "held", MIB);
    open(&*store, "free", MIB);
    held.hold(HolderId(1));
    ensure_eq(
        store.remove(&pack("held")).ok(),
        Some(false),
        "a held entry is not removed",
    );
    ensure(
        store.get(&pack("held")).is_some(),
        "a held entry is still there",
    );
    ensure_eq(
        store.remove(&pack("free")).ok(),
        Some(true),
        "an unheld entry is removed",
    );
    ensure(
        store.get(&pack("free")).is_none(),
        "a removed entry is gone",
    );
    ensure_eq(
        store.remove(&pack("free")).ok(),
        Some(false),
        "a removed entry is removed once",
    );
    ensure_eq(
        store.remove(&pack("none")).ok(),
        Some(false),
        "an unknown pack is not removed",
    );
    ensure_eq(
        store.packs(),
        vec![pack("held")],
        "only the held entry is listed",
    );
    ensure(held.release(HolderId(1)), "the hold is released");
    ensure_eq(
        store.remove(&pack("held")).ok(),
        Some(true),
        "a released entry is removed",
    );
    ensure(store.packs().is_empty(), "no entry is left");
}

/// Holds are counted per holder: `holders` is the number of distinct holders, a holder that holds
/// twice needs two releases, and a release of a hold that is not held is `false`.
pub fn holds_count_distinct_holders_with_a_refcount_each(f: &dyn EntryStoreFixture) {
    let store = f.store();
    let e = open(&*store, "p", MIB);
    e.hold(HolderId(1));
    e.hold(HolderId(1));
    ensure_eq(e.holders(), 1, "two holds of one holder are one holder");
    e.hold(HolderId(2));
    ensure_eq(e.holders(), 2, "two holders");
    ensure(e.release(HolderId(1)), "the first release");
    ensure_eq(e.holders(), 2, "one hold of holder 1 is left");
    ensure(e.release(HolderId(1)), "the second release");
    ensure_eq(e.holders(), 1, "holder 1 is gone");
    ensure(!e.release(HolderId(1)), "a third release holds nothing");
    ensure(e.release(HolderId(2)), "holder 2 releases");
    ensure_eq(e.holders(), 0, "no holder is left");
}
