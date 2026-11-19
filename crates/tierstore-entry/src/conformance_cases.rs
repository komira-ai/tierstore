//! The `EntryStore` conformance suite of `tierstore-harness` as tests: one `#[test]` per case
//! for a fixture, and a test that every case of the suite has one here (so a case added to the
//! suite and not to this list fails). Used by the fixture of each store this crate has.

/// Expands to one `#[test]` per case of `tierstore_harness::conformance::CASES`, each running the
/// case on a fresh `$fixture`, and the test that the list is the suite's.
macro_rules! conformance_cases {
    ($fixture:expr) => {
        conformance_cases!(@cases $fixture;
            entry_store::new_entry_is_all_absent,
            entry_store::claim_wins_each_absent_cell_once_in_ascending_order,
            entry_store::abandon_returns_the_cell_to_absent,
            entry_store::publish_makes_the_cell_present_and_read_cell_returns_the_bytes,
            entry_store::publish_of_the_wrong_length_returns_the_cell_to_absent,
            entry_store::publish_of_a_cell_that_is_not_claimed_changes_nothing,
            entry_store::mark_bad_then_expire_returns_the_cell_to_absent,
            entry_store::settle_moves_recheck_cells_and_refuses_other_states,
            entry_store::evict_steps_keep_the_durable_bit_and_refuse_wrong_states,
            entry_store::punch_refuses_wrong_cells_with_no_effect,
            entry_store::punch_releases_runs_with_one_journal_sync_per_call,
            entry_store::commit_batch_makes_written_present_cells_durable_once,
            entry_store::counts_sort_cells_by_state,
            entry_store::touch_records_last_use_and_group_epochs,
            entry_store::read_cell_refuses_unreadable_cells,
            store::open_returns_the_one_entry_of_a_pack,
            store::packs_and_entries_are_in_pack_order,
            store::remove_refuses_a_held_entry_and_removes_an_unheld_one,
            store::holds_count_distinct_holders_with_a_refcount_each,
            holders::attach_takes_a_hold_and_group_readers_sum_the_attached_counters,
            holders::attach_remaps_the_counters_to_the_regions_end_before_it_returns,
            holders::attach_refuses_unknown_packs_unknown_holders_and_bad_regions,
            holders::attach_of_the_same_region_again_is_idempotent,
            holders::a_second_different_region_is_refused_and_changes_nothing,
            holders::register_holder_keeps_the_first_registration_of_a_live_holder,
            holders::drop_holder_forgets_counters_and_releases_holds_so_eviction_proceeds,
            holders::a_dropped_holder_can_register_again_with_fresh_counters,
            holders::a_flapping_connection_keeps_counters_regions_and_holds,
            holders::two_hundred_holders_on_one_pack_are_all_served,
        );
    };
    (@cases $fixture:expr; $($module:ident::$name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                tierstore_harness::conformance::$module::$name(&$fixture);
            }
        )*

        #[test]
        fn every_case_of_the_suite_has_its_own_test_and_run_all_passes() {
            let tested: Vec<&str> = vec![$(stringify!($name)),*];
            let cases: Vec<&str> = tierstore_harness::conformance::CASES
                .iter()
                .map(|(name, _)| *name)
                .collect();
            assert_eq!(tested, cases);
            tierstore_harness::conformance::run_all(&$fixture);
        }
    };
}
