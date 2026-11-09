//! The scenarios of the first start of a hot tier with no format object
//! (`docs/design/tierstore-design.md`, 13.1): the property each is evidence for holds in every
//! reachable state (`holds_in_*`), or the checker rejects the wrong variant with a counterexample
//! of the rule named (`rejects_*`), or states the residual the design accepts (`residual_*`).

use tierstore_model::scenarios::verify;

fn check(id: &str) {
    if let Err(line) = verify(id) {
        panic!("{line}");
    }
}

/// One node writes the format object and is writable.
#[test]
fn holds_in_one_node_over_a_tier_with_no_format() {
    check("first-start/one-node");
}

/// The loser of the create-only put follows the object of the winner.
#[test]
fn holds_in_two_nodes_racing() {
    check("first-start/two-nodes-race");
}

/// No start replaces a pause the operator wrote over the record.
#[test]
fn holds_in_two_nodes_racing_with_a_pause() {
    check("first-start/two-nodes-race-with-a-pause");
}

/// A crash at any step of either node.
#[test]
fn holds_in_a_crash_at_any_step() {
    check("first-start/crash-at-any-step");
}

/// A crash after the put took effect.
#[test]
fn holds_in_a_crash_between_the_put_and_its_answer() {
    check("first-start/crash-between-put-and-answer");
}

/// An ambiguous answer is not retried and is followed on read.
#[test]
fn holds_in_an_ambiguous_answer() {
    check("first-start/ambiguous-answer");
}

/// A restart reads the object and writes nothing, whether it is the record or a pause.
#[test]
fn holds_in_a_restart_over_an_object() {
    check("first-start/restart-after-activation");
    check("first-start/restart-over-a-pause");
}

/// A create that is not atomic can replace a pause.
#[test]
fn rejects_a_create_that_is_not_atomic() {
    check("negative/first-start-create-not-atomic");
}

/// A node that stops following after an ambiguous answer stays 503.
#[test]
fn rejects_a_node_that_does_not_follow_an_ambiguous_answer() {
    check("negative/first-start-ambiguous-answer-not-followed");
}

/// A release before the format object beside a bucket this rule has used is unsupported.
#[test]
fn residual_an_earlier_release_beside_the_format_object() {
    check("residual/first-start-old-journal-left");
    check("residual/first-start-old-release-running");
}
