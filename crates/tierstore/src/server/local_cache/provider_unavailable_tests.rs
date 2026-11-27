//! The provider's answers `UNAVAILABLE` on `/metricz`: every reason shown at zero before any
//! answer, and each answer counted once under its own reason and no other.

use super::{ProviderUnavailable, REASONS, UNAVAILABLE, Unavailable};

/// The text `render` gives for the counts of `no_fs`, `not_ready`, `not_prepared_here`,
/// `tree_gone`, `read_failed`, `store_failed` and `not_released`, in that order.
fn expected(counts: [u64; 7]) -> String {
    let [no_fs, not_ready, not_here, gone, read, store, released] = counts;
    format!(
        "# TYPE {UNAVAILABLE} counter\n\
         {UNAVAILABLE}{{reason=\"no_fs\"}} {no_fs}\n\
         {UNAVAILABLE}{{reason=\"not_ready\"}} {not_ready}\n\
         {UNAVAILABLE}{{reason=\"not_prepared_here\"}} {not_here}\n\
         {UNAVAILABLE}{{reason=\"tree_gone\"}} {gone}\n\
         {UNAVAILABLE}{{reason=\"read_failed\"}} {read}\n\
         {UNAVAILABLE}{{reason=\"store_failed\"}} {store}\n\
         {UNAVAILABLE}{{reason=\"not_released\"}} {released}\n"
    )
}

/// Asserts the family before any answer: typed, and every reason at zero.
#[test]
fn every_reason_is_shown_at_zero_before_an_answer() {
    assert_eq!(UNAVAILABLE, "tierstore_provider_unavailable_total");
    assert_eq!(ProviderUnavailable::default().render(), expected([0; 7]));
}

/// Asserts that the answers of each reason count under that reason alone: reason `i` answered
/// `i + 1` times gives the counts 1 to 7 in the order of the family. Catches two reasons that
/// share a counter, and a label written for the wrong counter.
#[test]
fn each_answer_counts_under_its_reason_alone() {
    let counts = ProviderUnavailable::default();
    for (times, reason) in (1..).zip(REASONS) {
        for _ in 0..times {
            counts.count(reason);
        }
    }
    assert_eq!(counts.render(), expected([1, 2, 3, 4, 5, 6, 7]));
    let labels: Vec<&str> = REASONS.iter().map(|reason| reason.label()).collect();
    assert_eq!(
        labels,
        [
            "no_fs",
            "not_ready",
            "not_prepared_here",
            "tree_gone",
            "read_failed",
            "store_failed",
            "not_released"
        ]
    );
    assert_eq!(Unavailable::NoFs.label(), "no_fs");
}
