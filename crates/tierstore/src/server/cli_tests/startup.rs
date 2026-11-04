//! The exit statuses of a failed start and the probe id of a start.

use super::*;

/// Asserts that the id of a start's cache-directory probe has the proxy id, the time and the process, with
/// every character a key cannot carry in the proxy id replaced by an underscore.
#[test]
fn the_probe_run_id_is_made_of_the_proxy_the_time_and_the_process() {
    assert_eq!(probe_run_id("p1", 1234, 77), "p1-1234-77");
    assert_eq!(probe_run_id("a/b c.d-e", 5, 6), "a_b_c_d-e-5-6");
    assert_ne!(probe_run_id("p1", 1, 7), probe_run_id("p1", 2, 7));
}

/// Asserts the exit statuses of a failed start: flags that cannot go together are status 2, a
/// cache directory whose file system answered the check of the cache wrongly is status 3, and any
/// other failure is status 1 (a tier that cannot be reached is one: a restart may mend it).
#[test]
fn the_exit_status_of_a_failed_start_is_told_by_what_failed() {
    let invalid: super::super::Error = Box::new(InvalidFlags("x".to_owned()));
    let wrong: super::super::Error = Box::new(LocalTierFailure::Failed {
        path: "/cache".to_owned(),
        check: "create-only",
    });
    let other: super::super::Error = "another reason".into();
    assert_eq!(exit_status(&invalid), 2);
    assert_eq!(exit_status(&wrong), 3);
    assert_eq!(exit_status(&other), 1);
}

/// Asserts that the report of a drain names the requests that the deadline cut, and that a drain
/// that cut none reports nothing.
#[test]
fn a_drain_that_cut_requests_reports_them_and_one_that_cut_none_says_nothing() {
    assert_eq!(cut_note(Drained { abandoned: 0 }), "");
    assert_eq!(
        cut_note(Drained { abandoned: 3 }),
        "tierstore: 3 requests were cut at the drain deadline\n"
    );
}
