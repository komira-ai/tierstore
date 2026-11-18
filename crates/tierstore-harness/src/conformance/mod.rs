//! The `EntryStore` conformance suite: every documented contract of `Entry`, `EntryStore`,
//! `StateView` and the holders' counters of `tierstore-fetch-api`, as cases that run on any
//! implementation (section 14: "`EntryStore` conformance suite passes on the memfd implementation
//! (Linux) and the in-process implementation (macOS path), loom covers both").
//!
//! An implementation provides an [`EntryStoreFixture`]: a fresh empty store, a holder (the
//! server's view of a mount's counters and the [`MountCounters`] a test plays the mount with), and
//! a way to put a cell in `recheck`, which no trait method does (a restart in another boot does).
//! Each case is a `pub fn(&dyn EntryStoreFixture)` that opens what it needs in a fresh store and
//! panics with a message that starts `conformance:` when the implementation breaks its contract;
//! [`CASES`] lists them by name and [`run_all`] runs them in order. This crate's
//! `tests/conformance_entry_store.rs` runs them on the in-memory store of `tierstore-fetch-api`;
//! the Linux implementation (T1) and the macOS one (T-M1) are meant to each run them on their own
//! fixture.
//!
//! Section 14 rows the cases prove, in-process: the suite row above;
//! "Mount killed (lifeline hangup): counters dropped, eviction proceeds; mount whose protocol
//! connection flaps keeps counters, regions and holds"
//! ([`holders::drop_holder_forgets_counters_and_releases_holds_so_eviction_proceeds`],
//! [`holders::a_flapping_connection_keeps_counters_regions_and_holds`]); "A mount grows its counter
//! memfd past the server's mapping: the server remaps before acknowledging open-pack"
//! ([`holders::attach_remaps_the_counters_to_the_regions_end_before_it_returns`]); "More than 128
//! holders on one pack are all served"
//! ([`holders::two_hundred_holders_on_one_pack_are_all_served`]); "Eviction removes an unheld
//! entry's path while a mount holds the data fd" (the in-process part: a held entry is not
//! removed, an unheld one is, [`store::remove_refuses_a_held_entry_and_removes_an_unheld_one`]);
//! "Crash after a batched PUNCH fsync ... one fsync per batch" (the punch contract,
//! [`entry_store::punch_releases_runs_with_one_journal_sync_per_call`]); "Journal batching ...
//! durable set only after it" (the commit contract,
//! [`entry_store::commit_batch_makes_written_present_cells_durable_once`]).
//!
//! The cases use packs of up to 40 cells and 200 holders; a pack past `u32::MAX` cells is only
//! named, never allocated by a conforming store.

use std::sync::Arc;

use tierstore_fetch_api::entry::{EntryStore, HolderCounters};
use tierstore_fetch_api::ids::PackId;

pub mod check;
pub mod entry_store;
pub mod holders;
pub mod store;
mod support;

/// The mount's side of its counter memfd: what a test plays the reader count with. A write at
/// an offset past the memfd's size first grows the memfd (a mount grows its memfd past the
/// server's mapping; the server maps more only when `attach` asks it to).
pub trait MountCounters: Send + Sync {
    /// `fetch_add` of `delta` (wrapping) on the counter at byte `offset`.
    fn add(&self, offset: u64, delta: u16);

    /// `fetch_sub` of `delta` (wrapping) on the counter at byte `offset`.
    fn sub(&self, offset: u64, delta: u16);
}

/// What an implementation gives the cases.
pub trait EntryStoreFixture {
    /// A new, empty store. Every call is independent of the others: a case never sees another's
    /// entries.
    fn store(&self) -> Arc<dyn EntryStore>;

    /// A new holder: the server's view of a counter memfd that is `len` bytes (all counters 0) and
    /// the mount's side of the same memfd.
    fn holder(&self, len: u64) -> (Arc<dyn HolderCounters>, Arc<dyn MountCounters>);

    /// What a restart in another boot does to `cell` of `pack` in `store` (a store this fixture
    /// made): its `bytes` (the cell's length) are on disk and the cell is `recheck`.
    fn restore_as_recheck(&self, store: &dyn EntryStore, pack: &PackId, cell: u32, bytes: &[u8]);
}

/// A case: runs against the fixture and panics when the contract is broken.
pub type Case = fn(&dyn EntryStoreFixture);

/// Every case, with its name (the name of its function).
pub const CASES: &[(&str, Case)] = &[
    (
        "new_entry_is_all_absent",
        entry_store::new_entry_is_all_absent,
    ),
    (
        "claim_wins_each_absent_cell_once_in_ascending_order",
        entry_store::claim_wins_each_absent_cell_once_in_ascending_order,
    ),
    (
        "abandon_returns_the_cell_to_absent",
        entry_store::abandon_returns_the_cell_to_absent,
    ),
    (
        "publish_makes_the_cell_present_and_read_cell_returns_the_bytes",
        entry_store::publish_makes_the_cell_present_and_read_cell_returns_the_bytes,
    ),
    (
        "publish_of_the_wrong_length_returns_the_cell_to_absent",
        entry_store::publish_of_the_wrong_length_returns_the_cell_to_absent,
    ),
    (
        "publish_of_a_cell_that_is_not_claimed_changes_nothing",
        entry_store::publish_of_a_cell_that_is_not_claimed_changes_nothing,
    ),
    (
        "mark_bad_then_expire_returns_the_cell_to_absent",
        entry_store::mark_bad_then_expire_returns_the_cell_to_absent,
    ),
    (
        "settle_moves_recheck_cells_and_refuses_other_states",
        entry_store::settle_moves_recheck_cells_and_refuses_other_states,
    ),
    (
        "evict_steps_keep_the_durable_bit_and_refuse_wrong_states",
        entry_store::evict_steps_keep_the_durable_bit_and_refuse_wrong_states,
    ),
    (
        "punch_refuses_wrong_cells_with_no_effect",
        entry_store::punch_refuses_wrong_cells_with_no_effect,
    ),
    (
        "punch_releases_runs_with_one_journal_sync_per_call",
        entry_store::punch_releases_runs_with_one_journal_sync_per_call,
    ),
    (
        "commit_batch_makes_written_present_cells_durable_once",
        entry_store::commit_batch_makes_written_present_cells_durable_once,
    ),
    (
        "counts_sort_cells_by_state",
        entry_store::counts_sort_cells_by_state,
    ),
    (
        "touch_records_last_use_and_group_epochs",
        entry_store::touch_records_last_use_and_group_epochs,
    ),
    (
        "read_cell_refuses_unreadable_cells",
        entry_store::read_cell_refuses_unreadable_cells,
    ),
    (
        "open_returns_the_one_entry_of_a_pack",
        store::open_returns_the_one_entry_of_a_pack,
    ),
    (
        "packs_and_entries_are_in_pack_order",
        store::packs_and_entries_are_in_pack_order,
    ),
    (
        "remove_refuses_a_held_entry_and_removes_an_unheld_one",
        store::remove_refuses_a_held_entry_and_removes_an_unheld_one,
    ),
    (
        "holds_count_distinct_holders_with_a_refcount_each",
        store::holds_count_distinct_holders_with_a_refcount_each,
    ),
    (
        "attach_takes_a_hold_and_group_readers_sum_the_attached_counters",
        holders::attach_takes_a_hold_and_group_readers_sum_the_attached_counters,
    ),
    (
        "attach_remaps_the_counters_to_the_regions_end_before_it_returns",
        holders::attach_remaps_the_counters_to_the_regions_end_before_it_returns,
    ),
    (
        "attach_refuses_unknown_packs_unknown_holders_and_bad_regions",
        holders::attach_refuses_unknown_packs_unknown_holders_and_bad_regions,
    ),
    (
        "attach_of_the_same_region_again_is_idempotent",
        holders::attach_of_the_same_region_again_is_idempotent,
    ),
    (
        "a_second_different_region_is_refused_and_changes_nothing",
        holders::a_second_different_region_is_refused_and_changes_nothing,
    ),
    (
        "register_holder_keeps_the_first_registration_of_a_live_holder",
        holders::register_holder_keeps_the_first_registration_of_a_live_holder,
    ),
    (
        "drop_holder_forgets_counters_and_releases_holds_so_eviction_proceeds",
        holders::drop_holder_forgets_counters_and_releases_holds_so_eviction_proceeds,
    ),
    (
        "a_dropped_holder_can_register_again_with_fresh_counters",
        holders::a_dropped_holder_can_register_again_with_fresh_counters,
    ),
    (
        "a_flapping_connection_keeps_counters_regions_and_holds",
        holders::a_flapping_connection_keeps_counters_regions_and_holds,
    ),
    (
        "two_hundred_holders_on_one_pack_are_all_served",
        holders::two_hundred_holders_on_one_pack_are_all_served,
    ),
];

/// Runs every case of [`CASES`] against `fixture`, in order.
///
/// # Panics
///
/// Panics at the first case that breaks its contract.
pub fn run_all(fixture: &dyn EntryStoreFixture) {
    for (_, case) in CASES {
        case(fixture);
    }
}
