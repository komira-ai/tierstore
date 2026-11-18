//! The `EntryStore` conformance suite on the in-memory store of `tierstore-fetch-api`
//! (`MemEntryStore` with `MemCounters`): every case is its own test, `run_all` covers every case,
//! the checks panic with the messages the cases rely on, and a store that breaks one contract is
//! caught by the case for that contract.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use tierstore_fetch_api::entry::{
    Entry, EntryError, EntryInfo, EntrySpec, EntryStore, HolderCounters,
};
use tierstore_fetch_api::fake::{MemCounters, MemEntryStore};
use tierstore_fetch_api::ids::{HolderId, PackId};
use tierstore_fetch_api::page::CounterRegion;
use tierstore_harness::conformance::check::{ensure, ensure_eq, expect_err, expect_ok};
use tierstore_harness::conformance::{
    CASES, Case, EntryStoreFixture, MountCounters, entry_store, holders, run_all, store,
};

/// The mount's side of a [`MemCounters`].
struct Mount(Arc<MemCounters>);

impl MountCounters for Mount {
    fn add(&self, offset: u64, delta: u16) {
        self.0.add(offset, delta);
    }

    fn sub(&self, offset: u64, delta: u16) {
        self.0.add(offset, delta.wrapping_neg());
    }
}

/// Fresh `MemEntryStore`s; it keeps them to reach `restore_as_recheck`.
#[derive(Default)]
struct MemFixture {
    stores: Mutex<Vec<Arc<MemEntryStore>>>,
}

impl EntryStoreFixture for MemFixture {
    fn store(&self) -> Arc<dyn EntryStore> {
        let s = Arc::new(MemEntryStore::new());
        self.stores.lock().unwrap().push(s.clone());
        s
    }

    fn holder(&self, len: u64) -> (Arc<dyn HolderCounters>, Arc<dyn MountCounters>) {
        let counters = Arc::new(MemCounters::new(len));
        (counters.clone(), Arc::new(Mount(counters)))
    }

    fn restore_as_recheck(&self, store: &dyn EntryStore, pack: &PackId, cell: u32, bytes: &[u8]) {
        let stores = self.stores.lock().unwrap();
        let mem = stores
            .iter()
            .find(|s| std::ptr::addr_eq(Arc::as_ptr(s), store))
            .expect("a store of this fixture");
        mem.mem_entry(pack)
            .expect("an open pack")
            .restore_as_recheck(cell, bytes)
            .unwrap();
    }
}

macro_rules! cases {
    ($(($module:ident, $name:ident, $doc:literal)),* $(,)?) => {
        $(
            #[doc = $doc]
            #[test]
            fn $name() {
                $module::$name(&MemFixture::default());
            }
        )*

        /// The names of the tests above.
        const TESTED: &[&str] = &[$(stringify!($name)),*];
    };
}

cases! {
    (entry_store, new_entry_is_all_absent,
        "A new entry has every cell absent, counted so, unused and unheld; an empty pack has no cell."),
    (entry_store, claim_wins_each_absent_cell_once_in_ascending_order,
        "Claim wins absent cells once, ascending, and skips every other state and range."),
    (entry_store, abandon_returns_the_cell_to_absent,
        "Abandon returns a claimed cell to absent and it is claimable again."),
    (entry_store, publish_makes_the_cell_present_and_read_cell_returns_the_bytes,
        "Publish makes a cell present and read_cell returns its bytes, the short last cell included."),
    (entry_store, publish_of_the_wrong_length_returns_the_cell_to_absent,
        "A wrong-length publish errors and returns the cell to absent."),
    (entry_store, publish_of_a_cell_that_is_not_claimed_changes_nothing,
        "A publish of an unclaimed cell is NotClaimed and changes nothing."),
    (entry_store, mark_bad_then_expire_returns_the_cell_to_absent,
        "A bad cell is unclaimable and unreadable and expires to absent once."),
    (entry_store, settle_moves_recheck_cells_and_refuses_other_states,
        "Recheck cells settle to present or absent; no settle wins from another state."),
    (entry_store, evict_steps_keep_the_durable_bit_and_refuse_wrong_states,
        "The evictor's steps keep the durable bit and refuse wrong states; claimed cells are never changed."),
    (entry_store, punch_refuses_wrong_cells_with_no_effect,
        "Punch refuses non-evicting and out-of-range cells with no effect; empty runs report zeros."),
    (entry_store, punch_releases_runs_with_one_journal_sync_per_call,
        "Punch journals one record per run, syncs once per call, and Finish then leaves absent cells."),
    (entry_store, commit_batch_makes_written_present_cells_durable_once,
        "Commit makes only the present cells written since the last commit durable, one sync."),
    (entry_store, counts_sort_cells_by_state,
        "Counts sort cells by state and used is present + claimed + recheck."),
    (entry_store, touch_records_last_use_and_group_epochs,
        "Touch records the last use and group epochs, and a group outside the pack moves last use only."),
    (entry_store, read_cell_refuses_unreadable_cells,
        "read_cell refuses absent, claimed and out-of-range cells."),
    (store, open_returns_the_one_entry_of_a_pack,
        "Open gives one entry per pack, refuses another size and an oversized pack."),
    (store, packs_and_entries_are_in_pack_order,
        "packs and entries list in pack order with each entry's info."),
    (store, remove_refuses_a_held_entry_and_removes_an_unheld_one,
        "Remove refuses a held entry and removes an unheld one."),
    (store, holds_count_distinct_holders_with_a_refcount_each,
        "Holds are a refcount per holder and holders counts distinct holders."),
    (holders, attach_takes_a_hold_and_group_readers_sum_the_attached_counters,
        "Attach takes a hold and group_readers sums the attached holders' counters per group."),
    (holders, attach_remaps_the_counters_to_the_regions_end_before_it_returns,
        "Attach remaps the server's mapping to the region's end before it returns."),
    (holders, attach_refuses_unknown_packs_unknown_holders_and_bad_regions,
        "Attach refuses unknown packs, unknown holders and bad regions, attaching nothing."),
    (holders, attach_of_the_same_region_again_is_idempotent,
        "A second attach of the same region takes no second hold."),
    (holders, a_second_different_region_is_refused_and_changes_nothing,
        "A different region for the same pack and holder is refused and changes nothing."),
    (holders, register_holder_keeps_the_first_registration_of_a_live_holder,
        "A second registration of a live holder keeps the first counters."),
    (holders, drop_holder_forgets_counters_and_releases_holds_so_eviction_proceeds,
        "drop_holder forgets counters and regions, releases holds, and eviction proceeds."),
    (holders, a_dropped_holder_can_register_again_with_fresh_counters,
        "A dropped holder registers again with fresh counters."),
    (holders, a_flapping_connection_keeps_counters_regions_and_holds,
        "A flapping connection keeps counters, regions and the single hold."),
    (holders, two_hundred_holders_on_one_pack_are_all_served,
        "More than 128 holders on one pack are all attached and summed."),
}

/// `run_all` passes on the in-memory store, and every case of `CASES` has a test of its own here
/// (and every test here names a case), so a case added to the suite without a test is caught.
#[test]
fn run_all_runs_every_case_and_every_case_has_its_own_test() {
    run_all(&MemFixture::default());
    let names: Vec<&str> = CASES.iter().map(|(n, _)| *n).collect();
    assert_eq!(
        names, TESTED,
        "CASES and the tests here list the same cases"
    );
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "case names are unique");
}

fn message(payload: Box<dyn Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(s) => *s,
        Err(p) => (*p.downcast::<&str>().expect("a string panic")).to_owned(),
    }
}

fn panic_of(f: impl FnOnce()) -> Option<String> {
    catch_unwind(AssertUnwindSafe(f)).err().map(message)
}

/// The checks panic with `conformance:` and what broke, and pass silently when the contract
/// holds.
#[test]
fn the_checks_panic_with_the_clause_that_broke() {
    ensure(true, "fine");
    ensure_eq(1, 1, "fine");
    assert_eq!(expect_ok(Ok(5), "fine"), 5);
    expect_err(
        Err::<(), _>(EntryError::UnknownHolder {
            holder: HolderId(3),
        }),
        |e| matches!(e, EntryError::UnknownHolder { .. }),
        "fine",
    );
    assert_eq!(
        panic_of(|| ensure(false, "the claim")).as_deref(),
        Some("conformance: the claim")
    );
    assert_eq!(
        panic_of(|| ensure_eq(1, 2, "a count")).as_deref(),
        Some("conformance: a count: got 1, want 2")
    );
    assert_eq!(
        panic_of(|| {
            expect_ok::<()>(
                Err(EntryError::UnknownHolder {
                    holder: HolderId(3),
                }),
                "an open",
            );
        })
        .as_deref(),
        Some("conformance: an open: unexpected error: holder HolderId(3) is not registered")
    );
    assert_eq!(
        panic_of(|| expect_err(Ok(5), |_| true, "a refusal")).as_deref(),
        Some("conformance: a refusal: expected an error, got a success")
    );
    let wrong = panic_of(|| {
        expect_err(
            Err::<(), _>(EntryError::UnknownHolder {
                holder: HolderId(3),
            }),
            |e| matches!(e, EntryError::UnknownPack { .. }),
            "a kind",
        );
    });
    assert_eq!(
        wrong.as_deref(),
        Some("conformance: a kind: wrong error: UnknownHolder { holder: HolderId(3) }")
    );
}

/// Counters whose `ensure_len` does nothing: a store over them cannot make the mapping reach a
/// region.
#[derive(Debug)]
struct NoRemap(Arc<dyn HolderCounters>);

impl HolderCounters for NoRemap {
    fn len(&self) -> u64 {
        self.0.len()
    }

    fn ensure_len(&self, _len: u64) -> Result<(), EntryError> {
        Ok(())
    }

    fn load(&self, offset: u64) -> Option<u16> {
        self.0.load(offset)
    }
}

/// How a [`Faulty`] store breaks its contract.
#[derive(Clone, Copy)]
enum Fault {
    /// `attach` returns without the mapping reaching the region's end.
    NoRemap,
    /// `drop_holder` forgets nothing and reports nothing.
    DropForgetsNothing,
    /// `packs` and `entries` list in reverse pack order.
    ReversedOrder,
}

/// A `MemEntryStore` with one fault.
struct Faulty {
    inner: MemEntryStore,
    fault: Fault,
}

impl EntryStore for Faulty {
    fn open(&self, spec: &EntrySpec) -> Result<Arc<dyn Entry>, EntryError> {
        self.inner.open(spec)
    }

    fn get(&self, pack: &PackId) -> Option<Arc<dyn Entry>> {
        self.inner.get(pack)
    }

    fn remove(&self, pack: &PackId) -> Result<bool, EntryError> {
        self.inner.remove(pack)
    }

    fn packs(&self) -> Vec<PackId> {
        let mut packs = self.inner.packs();
        if matches!(self.fault, Fault::ReversedOrder) {
            packs.reverse();
        }
        packs
    }

    fn entries(&self) -> Vec<EntryInfo> {
        let mut entries = self.inner.entries();
        if matches!(self.fault, Fault::ReversedOrder) {
            entries.reverse();
        }
        entries
    }

    fn register_holder(
        &self,
        holder: HolderId,
        counters: Arc<dyn HolderCounters>,
    ) -> Result<(), EntryError> {
        let counters: Arc<dyn HolderCounters> = if matches!(self.fault, Fault::NoRemap) {
            Arc::new(NoRemap(counters))
        } else {
            counters
        };
        self.inner.register_holder(holder, counters)
    }

    fn attach(
        &self,
        pack: &PackId,
        holder: HolderId,
        region: CounterRegion,
    ) -> Result<(), EntryError> {
        self.inner.attach(pack, holder, region)
    }

    fn drop_holder(&self, holder: HolderId) -> u32 {
        if matches!(self.fault, Fault::DropForgetsNothing) {
            return 0;
        }
        self.inner.drop_holder(holder)
    }
}

/// The fixture of the in-memory store with a fault.
struct FaultyFixture(Fault);

impl EntryStoreFixture for FaultyFixture {
    fn store(&self) -> Arc<dyn EntryStore> {
        Arc::new(Faulty {
            inner: MemEntryStore::new(),
            fault: self.0,
        })
    }

    fn holder(&self, len: u64) -> (Arc<dyn HolderCounters>, Arc<dyn MountCounters>) {
        MemFixture::default().holder(len)
    }

    fn restore_as_recheck(&self, _: &dyn EntryStore, _: &PackId, _: u32, _: &[u8]) {}
}

fn caught(fault: Fault, case: Case) -> Option<String> {
    panic_of(|| case(&FaultyFixture(fault)))
}

/// A store whose `attach` does not remap before it returns is caught by the remap case, with the
/// clause named.
#[test]
fn a_store_that_does_not_remap_before_attach_returns_is_caught() {
    assert_eq!(
        caught(
            Fault::NoRemap,
            holders::attach_remaps_the_counters_to_the_regions_end_before_it_returns
        )
        .as_deref(),
        Some("conformance: attach returned before the mapping reached the region's end")
    );
}

/// A store whose `drop_holder` forgets nothing is caught by the lifeline case: it reports no
/// released entry.
#[test]
fn a_store_whose_drop_holder_forgets_nothing_is_caught() {
    assert_eq!(
        caught(
            Fault::DropForgetsNothing,
            holders::drop_holder_forgets_counters_and_releases_holds_so_eviction_proceeds,
        )
        .as_deref(),
        Some("conformance: three entries released the holder: got 0, want 3")
    );
}

/// A store that lists entries out of pack order is caught by the listing case.
#[test]
fn a_store_that_lists_packs_out_of_order_is_caught() {
    let message = caught(
        Fault::ReversedOrder,
        store::packs_and_entries_are_in_pack_order,
    );
    assert!(
        message.is_some_and(|m| m.starts_with("conformance: packs in pack order: got ")),
        "the listing case names the clause"
    );
}
