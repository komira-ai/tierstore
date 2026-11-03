//! The settings of the start-up self-test that earlier releases ran, which are still accepted.
//!
//! tierstore no longer tests the tiers it is given, so `--skip-backend-check` (`skip_checks` of
//! `[cold_store]`) and `--backend-check-wait` (`backend_check_wait` of `[server]`) have nothing to
//! act on. They stay valid so that a unit or a file written for an earlier release still starts
//! (formats change additively only): the values are not used (`--backend-check-wait` is still
//! parsed as a duration, as it always was, so a value that is not one is still refused), and each
//! setting given is said on stderr to be ignored.

use std::time::Duration;

/// Why the settings of the removed self-test are ignored; the end of the operator's warning.
const IGNORED: &str =
    "is deprecated and ignored: tierstore no longer tests its stores at start-up; remove it";

/// The warnings for the settings of the removed self-test that were given, one line for each, in
/// the order of the settings. The text of a skipped check's reason is never repeated: it is free
/// text.
pub(super) fn warnings(skipped: &[String], wait: Option<Duration>) -> Vec<String> {
    let mut lines = Vec::new();
    if !skipped.is_empty() {
        lines.push(format!(
            "tierstore: --skip-backend-check (cold_store.skip_checks) {IGNORED}"
        ));
    }
    if wait.is_some() {
        lines.push(format!(
            "tierstore: --backend-check-wait (server.backend_check_wait) {IGNORED}"
        ));
    }
    lines
}

/// Says each of [`warnings`] on the operator's stderr.
pub(super) fn warn(skipped: &[String], wait: Option<Duration>) {
    for line in warnings(skipped, wait) {
        tracing::warn!("{line}");
    }
}

#[cfg(test)]
#[path = "deprecated_tests.rs"]
mod tests;
