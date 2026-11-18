//! The cases of holders: registration, `open-pack`'s attach, the reader counters the evictor
//! sums, the lifeline's drop and the flapping connection (sections 5.1 and 7).
//!
//! # Panics
//!
//! Every case panics, with a message that starts `conformance:`, when the implementation breaks
//! the contract the case checks.

use std::sync::Arc;

use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::entry::{Entry, EntryError, EntryStore, EvictStep};
use tierstore_fetch_api::ids::HolderId;
use tierstore_fetch_api::page::CounterRegion;

use super::check::{ensure, ensure_eq, expect_err, expect_ok};
use super::support::{MIB, open, pack, publish, run, same, state_of};
use super::{EntryStoreFixture, MountCounters};

/// The cells of the pack `p` the cases attach to: 40 cells, 3 groups.
const P_CELLS: u64 = 40;

/// The groups of `p`.
const P_GROUPS: u32 = 3;

fn region(offset: u64, groups: u32) -> CounterRegion {
    CounterRegion { offset, groups }
}

/// The byte offset of `group`'s counter in `region` (`u64::MAX` for a group outside it, which a
/// case never asks for).
fn counter(region: CounterRegion, group: u32) -> u64 {
    region.counter_offset(group).unwrap_or(u64::MAX)
}

/// A store with `p` open, and a registered holder over `len` bytes.
struct Rig {
    store: Arc<dyn EntryStore>,
    p: Arc<dyn Entry>,
    counters: Arc<dyn tierstore_fetch_api::entry::HolderCounters>,
    mount: Arc<dyn MountCounters>,
}

impl Rig {
    fn new(f: &dyn EntryStoreFixture, len: u64) -> Self {
        let store = f.store();
        let p = open(&*store, "p", P_CELLS * MIB);
        let (counters, mount) = f.holder(len);
        expect_ok(
            store.register_holder(HolderId(1), counters.clone()),
            "registration of holder 1",
        );
        Self {
            store,
            p,
            counters,
            mount,
        }
    }

    fn attach(&self, region: CounterRegion) -> Result<(), EntryError> {
        self.store.attach(&pack("p"), HolderId(1), region)
    }
}

/// `attach` takes the holder's hold, and `group_readers` sums, per group, the counters the mount
/// writes in its region: several holders add up, a group outside the pack counts 0, and a region
/// counts only the pack it was attached to.
pub fn attach_takes_a_hold_and_group_readers_sum_the_attached_counters(f: &dyn EntryStoreFixture) {
    let rig = Rig::new(f, 64);
    let p2 = open(&*rig.store, "p2", 17 * MIB);
    let r = region(8, P_GROUPS);
    expect_ok(rig.attach(r), "attach of holder 1 to p");
    ensure_eq(rig.p.holders(), 1, "an attach takes the holder's hold");
    ensure_eq(rig.p.group_readers(0), 0, "no reader yet");

    rig.mount.add(counter(r, 1), 2);
    rig.mount.add(counter(r, 2), 1);
    ensure_eq(rig.p.group_readers(1), 2, "two readers of group 1");
    ensure_eq(rig.p.group_readers(2), 1, "one reader of group 2");
    ensure_eq(rig.p.group_readers(0), 0, "group 0 is not counted");
    rig.mount.sub(counter(r, 1), 1);
    ensure_eq(rig.p.group_readers(1), 1, "a reader left group 1");
    ensure_eq(
        rig.p.group_readers(P_GROUPS),
        0,
        "a group outside the pack counts 0",
    );
    ensure_eq(
        rig.p.group_readers(u32::MAX),
        0,
        "the last group number is outside the pack",
    );

    let (second, writer) = f.holder(16);
    expect_ok(
        rig.store.register_holder(HolderId(2), second),
        "registration of holder 2",
    );
    expect_ok(
        rig.store
            .attach(&pack("p"), HolderId(2), region(0, P_GROUPS)),
        "attach of holder 2",
    );
    writer.add(counter(region(0, P_GROUPS), 1), 4);
    ensure_eq(rig.p.group_readers(1), 5, "two holders' counters add up");
    ensure_eq(rig.p.holders(), 2, "two holders");

    ensure_eq(
        p2.group_readers(1),
        0,
        "a region counts only the entry it was attached to",
    );
    ensure_eq(p2.holders(), 0, "another entry has no hold");
}

/// A mount whose counter memfd is bigger than the server's mapping: `attach` makes the mapping
/// reach the region's end before it returns, and the evictor then reads the new region's counters;
/// a region inside the mapping does not shrink it.
pub fn attach_remaps_the_counters_to_the_regions_end_before_it_returns(f: &dyn EntryStoreFixture) {
    let rig = Rig::new(f, 8);
    let r = region(4096, P_GROUPS);
    let end = r.end().unwrap_or(0);
    ensure(
        rig.counters.len() < end,
        "the server's mapping ends before the region",
    );
    expect_ok(rig.attach(r), "attach past the mapping");
    ensure(
        rig.counters.len() >= end,
        "attach returned before the mapping reached the region's end",
    );
    ensure_eq(
        rig.counters.load(counter(r, 2)),
        Some(0),
        "the new region's counters are readable",
    );
    rig.mount.add(counter(r, 2), 3);
    ensure_eq(
        rig.p.group_readers(2),
        3,
        "the evictor reads the counters of the new region",
    );

    let before = rig.counters.len();
    let inside = region(0, P_GROUPS);
    let other = open(&*rig.store, "q", P_CELLS * MIB);
    expect_ok(
        rig.store.attach(&pack("q"), HolderId(1), inside),
        "attach inside the mapping",
    );
    ensure(
        rig.counters.len() >= before,
        "an attach inside the mapping does not shrink it",
    );
    ensure_eq(
        other.group_readers(0),
        0,
        "the second region counts its own pack",
    );
}

/// `attach` errors are `UnknownPack`, `UnknownHolder` and `BadRegion` (misaligned, overflowing,
/// or not counting the pack's groups); each attaches nothing and an invalid region is refused
/// before the mapping is touched.
pub fn attach_refuses_unknown_packs_unknown_holders_and_bad_regions(f: &dyn EntryStoreFixture) {
    let rig = Rig::new(f, 64);
    let good = region(0, P_GROUPS);
    expect_err(
        rig.store.attach(&pack("none"), HolderId(1), good),
        |e| matches!(e, EntryError::UnknownPack { .. }),
        "attach to a pack with no entry",
    );
    expect_err(
        rig.store.attach(&pack("p"), HolderId(9), good),
        |e| {
            matches!(
                e,
                EntryError::UnknownHolder {
                    holder: HolderId(9)
                }
            )
        },
        "attach by a holder that never registered",
    );
    let len = rig.counters.len();
    for bad in [
        region(4, P_GROUPS),
        region(u64::MAX - 7, P_GROUPS),
        region(0, P_GROUPS - 1),
        region(0, P_GROUPS + 1),
        region(0, 0),
    ] {
        expect_err(
            rig.attach(bad),
            |e| {
                same(
                    e,
                    &EntryError::BadRegion {
                        pack: pack("p"),
                        region: bad,
                    },
                )
            },
            format_args!("attach of {bad:?}"),
        );
    }
    ensure_eq(
        rig.counters.len(),
        len,
        "a bad region does not touch the mapping",
    );
    ensure_eq(rig.p.holders(), 0, "a refused attach takes no hold");
    ensure_eq(
        rig.p.group_readers(0),
        0,
        "a refused attach adds no counters",
    );
}

/// Attaching the same region again is idempotent: no second hold (one release frees the holder)
/// and the counters keep counting.
pub fn attach_of_the_same_region_again_is_idempotent(f: &dyn EntryStoreFixture) {
    let rig = Rig::new(f, 64);
    let r = region(8, P_GROUPS);
    expect_ok(rig.attach(r), "the first attach");
    rig.mount.add(counter(r, 0), 1);
    expect_ok(rig.attach(r), "the second attach of the same region");
    ensure_eq(rig.p.holders(), 1, "a second attach takes no second hold");
    ensure_eq(rig.p.group_readers(0), 1, "the counters still count");
    ensure(rig.p.release(HolderId(1)), "one release");
    ensure(!rig.p.release(HolderId(1)), "the holder held once");
    ensure_eq(rig.p.holders(), 0, "one release frees the holder");
}

/// Another region for the same pack and holder is `BadRegion` and changes nothing: the first
/// region still counts, the second does not, and the hold is unchanged.
pub fn a_second_different_region_is_refused_and_changes_nothing(f: &dyn EntryStoreFixture) {
    let rig = Rig::new(f, 64);
    let first = region(0, P_GROUPS);
    let second = region(16, P_GROUPS);
    expect_ok(rig.attach(first), "the first attach");
    expect_err(
        rig.attach(second),
        |e| {
            same(
                e,
                &EntryError::BadRegion {
                    pack: pack("p"),
                    region: second,
                },
            )
        },
        "a second region for the same pack and holder",
    );
    rig.mount.add(counter(first, 1), 1);
    rig.mount.add(counter(second, 1), 10);
    ensure_eq(rig.p.group_readers(1), 1, "only the first region counts");
    ensure_eq(rig.p.holders(), 1, "the hold is unchanged");
}

/// A second registration of a live holder keeps the first counters: the second counters are not
/// summed, and a re-registration after an attach changes nothing.
pub fn register_holder_keeps_the_first_registration_of_a_live_holder(f: &dyn EntryStoreFixture) {
    let rig = Rig::new(f, 64);
    let (other, other_mount) = f.holder(64);
    expect_ok(
        rig.store.register_holder(HolderId(1), other),
        "a second registration",
    );
    let r = region(0, P_GROUPS);
    expect_ok(rig.attach(r), "attach");
    rig.mount.add(counter(r, 0), 1);
    other_mount.add(counter(r, 0), 100);
    ensure_eq(
        rig.p.group_readers(0),
        1,
        "the first registration's counters are the ones summed",
    );
    let (again, _) = f.holder(64);
    expect_ok(
        rig.store.register_holder(HolderId(1), again),
        "a re-registration after an attach",
    );
    ensure_eq(
        rig.p.group_readers(0),
        1,
        "a re-registration changes nothing",
    );
}

/// The lifeline's `drop_holder` forgets the holder's counters and regions and releases its holds
/// on every entry (the number of entries released is returned), so the evictor's group sum
/// drops to 0 and eviction proceeds; the holder is unknown afterwards and dropping again
/// releases nothing.
pub fn drop_holder_forgets_counters_and_releases_holds_so_eviction_proceeds(
    f: &dyn EntryStoreFixture,
) {
    let rig = Rig::new(f, 64);
    let p2 = open(&*rig.store, "p2", 17 * MIB);
    let direct = open(&*rig.store, "direct", MIB);
    let r = region(0, P_GROUPS);
    expect_ok(rig.attach(r), "attach to p");
    expect_ok(
        rig.store.attach(&pack("p2"), HolderId(1), region(8, 2)),
        "attach to p2",
    );
    direct.hold(HolderId(1));
    publish(&*rig.p, 0, 5);
    rig.mount.add(counter(r, 0), 1);

    ensure(
        rig.p.evict_cell_cas(0, EvictStep::Begin),
        "the evictor begins",
    );
    ensure_eq(rig.p.group_readers(0), 1, "a mount is reading the group");
    ensure(
        rig.p.evict_cell_cas(0, EvictStep::Abort),
        "the evictor aborts behind the reader",
    );

    ensure_eq(
        rig.store.drop_holder(HolderId(1)),
        3,
        "three entries released the holder",
    );
    ensure_eq(rig.p.holders(), 0, "p has no holder");
    ensure_eq(p2.holders(), 0, "p2 has no holder");
    ensure_eq(
        direct.holders(),
        0,
        "a hold taken on the entry directly is released too",
    );
    ensure_eq(
        rig.p.group_readers(0),
        0,
        "a dropped holder's counters are not summed",
    );
    ensure(
        rig.p.evict_cell_cas(0, EvictStep::Begin),
        "the evictor begins again",
    );
    ensure_eq(rig.p.group_readers(0), 0, "no reader is left");
    expect_ok(rig.p.punch(&[run(0, 1)]), "punch");
    ensure(
        rig.p.evict_cell_cas(0, EvictStep::Finish),
        "eviction proceeds",
    );
    ensure_eq(
        state_of(&*rig.p, 0),
        Some(CellState::Absent),
        "the cell is evicted",
    );
    ensure_eq(
        rig.store.remove(&pack("direct")).ok(),
        Some(true),
        "an entry nobody holds is removable",
    );

    expect_err(
        rig.attach(r),
        |e| matches!(e, EntryError::UnknownHolder { .. }),
        "attach by a dropped holder",
    );
    ensure_eq(
        rig.store.drop_holder(HolderId(1)),
        0,
        "a second drop releases nothing",
    );
    ensure_eq(
        rig.store.drop_holder(HolderId(9)),
        0,
        "a holder that never registered releases nothing",
    );
}

/// A holder that registered and never attached is dropped with nothing to release, and it can
/// register again afterwards and attach with fresh counters.
pub fn a_dropped_holder_can_register_again_with_fresh_counters(f: &dyn EntryStoreFixture) {
    let rig = Rig::new(f, 64);
    ensure_eq(rig.store.drop_holder(HolderId(1)), 0, "no hold to release");
    let (fresh, mount) = f.holder(64);
    expect_ok(
        rig.store.register_holder(HolderId(1), fresh),
        "registration after the drop",
    );
    let r = region(0, P_GROUPS);
    expect_ok(rig.attach(r), "attach after the drop");
    rig.mount.add(counter(r, 0), 7);
    mount.add(counter(r, 0), 2);
    ensure_eq(
        rig.p.group_readers(0),
        2,
        "only the fresh counters are summed",
    );
    ensure_eq(rig.p.holders(), 1, "one hold");
}

/// A mount whose protocol connection flaps (re-sends `open-pack` and registers again, never
/// hangs up its lifeline) keeps its counters, its region and its single hold.
pub fn a_flapping_connection_keeps_counters_regions_and_holds(f: &dyn EntryStoreFixture) {
    let rig = Rig::new(f, 64);
    let r = region(8, P_GROUPS);
    expect_ok(rig.attach(r), "attach");
    rig.mount.add(counter(r, 1), 2);
    for round in 0..3 {
        expect_ok(
            rig.store.register_holder(HolderId(1), rig.counters.clone()),
            format_args!("re-registration {round}"),
        );
        expect_ok(rig.attach(r), format_args!("re-attach {round}"));
        ensure_eq(
            rig.p.group_readers(1),
            2,
            format_args!("counters after flap {round}"),
        );
        ensure_eq(rig.p.holders(), 1, format_args!("holds after flap {round}"));
    }
    rig.mount.sub(counter(r, 1), 2);
    ensure_eq(rig.p.group_readers(1), 0, "the region still counts");
    expect_err(
        rig.attach(region(24, P_GROUPS)),
        |e| matches!(e, EntryError::BadRegion { .. }),
        "the region is remembered across flaps",
    );
}

/// More than 128 holders on one pack are all attached, held and summed, and dropping half of
/// them halves the sum.
pub fn two_hundred_holders_on_one_pack_are_all_served(f: &dyn EntryStoreFixture) {
    const HOLDERS: u64 = 200;
    let store = f.store();
    let p = open(&*store, "p", P_CELLS * MIB);
    let r = region(0, P_GROUPS);
    for id in 0..HOLDERS {
        let (counters, mount) = f.holder(16);
        expect_ok(
            store.register_holder(HolderId(id), counters),
            format_args!("register {id}"),
        );
        expect_ok(
            store.attach(&pack("p"), HolderId(id), r),
            format_args!("attach {id}"),
        );
        mount.add(counter(r, 1), 1);
        // The id fits a u16: the counter of group 0 is the holder's number.
        mount.add(counter(r, 0), id as u16);
    }
    ensure_eq(p.holders(), HOLDERS as u32, "every holder holds");
    ensure_eq(
        p.group_readers(1),
        HOLDERS,
        "every holder's counter is summed",
    );
    ensure_eq(
        p.group_readers(0),
        HOLDERS * (HOLDERS - 1) / 2,
        "the counters of group 0 add up",
    );
    ensure_eq(p.group_readers(2), 0, "an untouched group counts 0");
    for id in (0..HOLDERS).step_by(2) {
        ensure_eq(
            store.drop_holder(HolderId(id)),
            1,
            format_args!("drop {id}"),
        );
    }
    ensure_eq(p.holders(), HOLDERS as u32 / 2, "half the holders are left");
    ensure_eq(
        p.group_readers(1),
        HOLDERS / 2,
        "the dropped holders are not summed",
    );
}
