//! The loom model of the 5.1 reader/evictor protocol and of the claim and publish of a cell.

use tierstore_harness::models::evict::{
    EvictorSpec, Protocol, ReaderSpec, Report, Scenario, Violation, check,
};

/// Reader over cells 1..=2 of groups of 2 (cells 0,1 | 2,3): it straddles two groups; cells 0
/// and 3 are absent neighbours that fillers claim and publish during the scan.
fn straddle() -> Scenario {
    let mut s = Scenario::new(4, 2, 1);
    s.present = vec![1, 2];
    s.readers = vec![ReaderSpec {
        holder: 0,
        first: 1,
        len: 2,
    }];
    s.fillers = vec![3];
    s.evictor = Some(EvictorSpec {
        candidates: vec![0, 1, 2, 3],
        wait_polls: 1,
    });
    s
}

/// The straddling scenario with a filler on each absent neighbour (cells 0 and 3), explored
/// with preemption bound 2.
fn straddle_both_neighbours() -> Scenario {
    let mut s = straddle();
    s.fillers = vec![0, 3];
    s.preemption_bound = 2;
    s
}

/// One cell, one reader, a forced loss of its counter, an evictor on the cell.
fn forced_loss() -> Scenario {
    let mut s = Scenario::new(1, 1, 1);
    s.present = vec![0];
    s.readers = vec![ReaderSpec {
        holder: 0,
        first: 0,
        len: 1,
    }];
    s.forced_clear = Some((0, 0));
    s.evictor = Some(EvictorSpec {
        candidates: vec![0],
        wait_polls: 1,
    });
    s
}

/// A holder that never releases, an evictor on both cells.
fn stuck(durable: bool) -> Scenario {
    let mut s = Scenario::new(2, 2, 1);
    s.present = vec![0, 1];
    s.durable = durable;
    s.stuck = vec![(0, 0)];
    s.evictor = Some(EvictorSpec {
        candidates: vec![0, 1],
        wait_polls: 2,
    });
    s
}

/// A hedged cell next to a present cell, with the evictor scanning both.
fn hedge() -> Scenario {
    let mut s = Scenario::new(2, 1, 1);
    s.present = vec![1];
    s.hedge = Some(0);
    s.evictor = Some(EvictorSpec {
        candidates: vec![0, 1],
        wait_polls: 1,
    });
    s
}

/// Two holders each reading the one present cell of a group, and the evictor.
fn two_holders() -> Scenario {
    let mut s = Scenario::new(2, 2, 2);
    s.present = vec![0];
    s.readers = vec![
        ReaderSpec {
            holder: 0,
            first: 0,
            len: 1,
        },
        ReaderSpec {
            holder: 1,
            first: 0,
            len: 1,
        },
    ];
    s.evictor = Some(EvictorSpec {
        candidates: vec![0, 1],
        wait_polls: 1,
    });
    s
}

fn clean(r: &Report) {
    assert!(r.executions >= 1, "{r:?}");
    assert_eq!(r.violations, vec![], "{r:?}");
}

/// The design's protocol has no violation when a reader straddles two groups while fillers claim
/// and publish the neighbouring cells and the evictor scans all four: no reader commits bytes of
/// a punched cell and the evictor never changes a claimed cell.
#[test]
fn design_protocol_survives_a_straddling_reader_and_neighbour_fills() {
    for s in [straddle(), straddle_both_neighbours()] {
        let r = check(&s, &Protocol::design());
        clean(&r);
        assert!(
            r.commits > 0 && r.waits > 0,
            "readers committed and waited: {r:?}"
        );
    }
}

/// With a forced loss of the reader's counter the evictor can punch under a live reader, and the
/// post-read recheck discards the bytes: no commit of zeros, and some execution discards.
/// Catches: a recheck that does not reload the states.
#[test]
fn forced_counter_loss_is_caught_by_the_post_read_recheck() {
    let r = check(&forced_loss(), &Protocol::design());
    clean(&r);
    assert!(
        r.discards > 0,
        "some interleaving discards the bytes: {r:?}"
    );
    assert!(r.commits > 0, "some interleaving commits real bytes: {r:?}");
}

/// A live reader stuck beyond the wait: the evictor times out in every execution, counts one
/// timeout, journals nothing, and the won cells are present again (durable ones as durable).
#[test]
fn stuck_reader_times_out_restores_cells_and_journals_nothing() {
    for durable in [false, true] {
        let r = check(&stuck(durable), &Protocol::design());
        clean(&r);
        assert_eq!(r.timeouts, r.executions, "one timeout per execution: {r:?}");
        assert_eq!(r.journal_records, 0, "{r:?}");
    }
}

/// Whichever deliverer of a hedged cell takes the shared token writes and publishes; the other
/// writes nothing; the evictor leaves the claimed cell alone and punches its present neighbour.
#[test]
fn hedge_and_original_publish_once_with_a_shared_token() {
    let r = check(&hedge(), &Protocol::design());
    clean(&r);
    assert!(
        r.journal_records > 0,
        "the neighbour is punched in some interleaving: {r:?}"
    );
}

/// Two holders each hold a counter on the group; the evictor sums both, and no reader commits
/// bytes of a punched cell.
#[test]
fn design_protocol_survives_two_holders() {
    clean(&check(&two_holders(), &Protocol::design()));
}

/// Without the evictor's wait the post-read recheck alone still keeps zeros from being served,
/// and without the recheck the wait alone does too: the two guards are independent.
#[test]
fn either_guard_alone_keeps_zeros_from_being_served() {
    let mut no_wait = Protocol::design();
    no_wait.evictor_waits_for_readers = false;
    clean(&check(&straddle(), &no_wait));
    let mut no_recheck = Protocol::design();
    no_recheck.post_read_recheck = false;
    clean(&check(&straddle(), &no_recheck));
}

/// The recheck covers a reader that loads the states before it counts: it never commits a
/// punched cell.
#[test]
fn recheck_covers_a_reader_that_loads_before_counting() {
    let mut p = Protocol::design();
    p.reader_counts_before_load = false;
    clean(&check(&two_holders(), &p));
}

/// A protocol with neither the wait nor the recheck is caught: the explorer reports a reader
/// committing bytes of a punched cell.
#[test]
fn a_protocol_without_wait_and_recheck_is_caught() {
    let mut p = Protocol::design();
    p.evictor_waits_for_readers = false;
    p.post_read_recheck = false;
    let r = check(&forced_loss(), &p);
    assert_eq!(r.violations, vec![Violation::StaleBytesCommitted], "{r:?}");
}

/// A protocol without the recheck is caught when the counter is lost: the explorer reports a
/// reader committing bytes of a punched cell.
#[test]
fn a_protocol_without_the_recheck_is_caught_under_a_forced_loss() {
    let mut p = Protocol::design();
    p.post_read_recheck = false;
    let r = check(&forced_loss(), &p);
    assert_eq!(r.violations, vec![Violation::StaleBytesCommitted], "{r:?}");
}

/// A reader that counts after it loads, with no recheck, is caught: the evictor sums zero
/// between the reader's load and its count.
#[test]
fn a_reader_that_counts_after_loading_without_recheck_is_caught() {
    let mut p = Protocol::design();
    p.reader_counts_before_load = false;
    p.post_read_recheck = false;
    let r = check(&two_holders(), &p);
    assert_eq!(r.violations, vec![Violation::StaleBytesCommitted], "{r:?}");
}

/// An evictor that stores `evicting` over its whole range instead of a CAS from `present` is
/// caught: the explorer reports a claimed cell overwritten.
#[test]
fn an_evictor_without_per_cell_cas_is_caught() {
    let mut p = Protocol::design();
    p.evictor_per_cell_cas = false;
    let r = check(&straddle(), &p);
    assert!(
        r.violations.contains(&Violation::ClaimedCellChanged),
        "{r:?}"
    );
}

/// An evictor that journals the batch before it waits is caught on the timeout path: the
/// explorer reports a journal record before a timeout.
#[test]
fn an_evictor_that_journals_before_waiting_is_caught() {
    let mut p = Protocol::design();
    p.wait_before_journal = false;
    let r = check(&stuck(false), &p);
    assert_eq!(
        r.violations,
        vec![Violation::JournaledBeforeTimeout],
        "{r:?}"
    );
}

/// A hedge whose two deliverers each own a token is caught: the explorer reports the cell's
/// data written twice.
#[test]
fn a_hedge_with_two_tokens_is_caught() {
    let mut p = Protocol::design();
    p.hedge_shares_token = false;
    let r = check(&hedge(), &p);
    assert_eq!(r.violations, vec![Violation::CellWrittenNotOnce], "{r:?}");
}
