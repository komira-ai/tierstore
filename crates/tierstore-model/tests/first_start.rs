//! The scenarios of the first start of a hot tier with no format object
//! (`docs/design/tierstore-design.md`, 13.1): the property each is evidence for holds in every
//! reachable state (`holds_in_*`), or the checker rejects the wrong variant with a counterexample
//! of the rule named (`rejects_*`), or states the residual the design leaves to the credential
//! fence.

use tierstore_model::scenarios::verify;

fn check(id: &str) {
    if let Err(line) = verify(id) {
        panic!("{line}");
    }
}

/// One node proves the tier new, creates active-marks and is writable.
#[test]
fn holds_in_one_node_over_a_new_s3_tier() {
    check("first-start/one-node-s3");
}

/// One node creates active at epoch 2 over a dir tier.
#[test]
fn holds_in_one_node_over_a_new_dir_tier() {
    check("first-start/one-node-dir");
}

/// The loser of the create follows the object, though the winner's markers fill the marks bucket.
#[test]
fn holds_in_two_nodes_racing_over_a_new_s3_tier() {
    check("first-start/two-nodes-race-s3");
}

/// Two processes over one dir.
#[test]
fn holds_in_two_nodes_racing_over_a_new_dir_tier() {
    check("first-start/two-nodes-race-dir");
}

/// Both writers write the same record, so a create that is not atomic does no harm.
#[test]
fn holds_in_two_nodes_whose_create_is_not_atomic() {
    check("first-start/two-nodes-race-create-not-atomic");
}

/// A crash at any step of either node.
#[test]
fn holds_in_a_crash_between_registration_and_the_put() {
    check("first-start/crash-between-registration-and-put");
}

/// A crash after the put took effect.
#[test]
fn holds_in_a_crash_between_the_put_and_its_answer() {
    check("first-start/crash-between-put-and-answer");
}

/// A restart reads the object and runs no proof.
#[test]
fn holds_in_a_restart_after_the_activation() {
    check("first-start/restart-after-activation");
}

/// A journal alone stops the activation.
#[test]
fn holds_in_a_journal_left_by_a_departed_release() {
    check("first-start/journal-left");
}

/// An entry alone stops the activation.
#[test]
fn holds_in_an_entry_left_without_a_format_object() {
    check("first-start/entries-left");
}

/// A marker alone stops the activation.
#[test]
fn holds_in_a_marks_bucket_that_holds_a_marker() {
    check("first-start/marks-bucket-used");
}

/// No epoch 3 over a missing bucket.
#[test]
fn holds_in_a_marks_bucket_that_does_not_exist() {
    check("first-start/marks-bucket-missing");
}

/// Activation over a journal.
#[test]
fn rejects_fs1_in_a_proof_without_the_journal_list() {
    check("negative/first-start-no-journal-check");
}

/// Activation beside a running old node.
#[test]
fn rejects_fs1_in_a_proof_without_the_registration_list() {
    check("negative/first-start-no-registration-check");
}

/// Activation over an entry.
#[test]
fn rejects_fs1_in_a_proof_without_the_entries_list() {
    check("negative/first-start-no-entries-check");
}

/// Activation over a marker.
#[test]
fn rejects_fs1_in_a_proof_without_the_marks_list() {
    check("negative/first-start-no-marks-check-used");
}

/// Epoch 3 over a missing bucket.
#[test]
fn rejects_fs3_in_a_proof_without_the_marks_list() {
    check("negative/first-start-no-marks-check-missing");
}

/// The window between the last list and the put, which the credential fence closes.
#[test]
fn states_the_residual_of_an_old_node_that_starts_after_the_proof() {
    check("residual/first-start-old-starts-after-the-proof");
}

/// A proof that counts the node's own registration refuses on a new tier (FS2).
#[test]
fn rejects_fs2_in_a_proof_that_counts_its_own_registration() {
    check("negative/first-start-counts-its-own-registration");
}

/// A proof that counts the registrations of peers starting now makes two nodes block each other (FS2).
#[test]
fn rejects_fs2_in_a_proof_that_counts_its_peers_registrations() {
    check("negative/first-start-counts-its-peers-registrations");
}

/// No activation beside a running node of an earlier release that writes journals (FS1).
#[test]
fn holds_in_an_old_release_node_writing_journals() {
    check("first-start/old-release-writing");
}

/// The same over a dir tier, where the old node may die (FS1).
#[test]
fn holds_in_an_old_release_node_writing_journals_over_a_dir_tier() {
    check("first-start/old-release-writing-dir");
}

/// An old node that starts once the object exists writes journals, which the fence stops.
#[test]
fn states_the_residual_of_an_old_node_that_starts_after_the_activation() {
    check("residual/first-start-old-starts-after-the-activation");
}

/// An old node that starts at any point is the same residual, over two nodes and a crash.
#[test]
fn states_the_residual_of_an_old_node_that_starts_at_any_time() {
    check("residual/first-start-old-starts-at-any-time");
}
