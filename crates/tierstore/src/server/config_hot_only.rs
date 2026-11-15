//! The keys only `[hot_store]` has, which `[cold_store]` refuses.

use super::config::{ConfigError, TierTable, refuse};
use super::config_entry::refuse_marks_keys;

/// Refuses the keys only `[hot_store]` has, in `cold`.
pub(super) fn refuse_hot_keys(cold: &TierTable) -> Result<(), ConfigError> {
    let cold_has = [
        ("marks_bucket", cold.marks_bucket.is_some()),
        ("budget", cold.budget.is_some()),
        ("high_water", cold.high_water.is_some()),
        ("low_water", cold.low_water.is_some()),
        ("critical", cold.critical.is_some()),
        ("copy_parallel", cold.copy_parallel.is_some()),
        ("lease_ttl", cold.lease_ttl.is_some()),
        ("lock_file", cold.lock_file.is_some()),
        ("usage_stale_after", cold.usage_stale_after.is_some()),
        ("journal_linger", cold.journal_linger.is_some()),
        ("journal_retention", cold.journal_retention.is_some()),
        (
            "journal_batch_entries",
            cold.journal_batch_entries.is_some(),
        ),
        ("usage_refresh", cold.usage_refresh.is_some()),
        ("journal_page_size", cold.journal_page_size.is_some()),
        ("copy_rounds", cold.copy_rounds.is_some()),
        ("compare_range", cold.compare_range.is_some()),
        ("delete_rounds", cold.delete_rounds.is_some()),
        ("listing_heads", cold.listing_heads.is_some()),
        ("upload_listing_calls", cold.upload_listing_calls.is_some()),
        (
            "abandoned_upload_after",
            cold.abandoned_upload_after.is_some(),
        ),
        ("retry_min", cold.retry_min.is_some()),
        ("retry_max", cold.retry_max.is_some()),
        ("stray_grace", cold.stray_grace.is_some()),
        ("stray_interval", cold.stray_interval.is_some()),
        ("copy_back_ttl", cold.copy_back_ttl.is_some()),
        ("usage_interval", cold.usage_interval.is_some()),
        ("usage_scan_interval", cold.usage_scan_interval.is_some()),
        (
            "proxy_usage_stale_after",
            cold.proxy_usage_stale_after.is_some(),
        ),
        ("format_refresh", cold.format_refresh.is_some()),
        ("delete_wait", cold.delete_wait.is_some()),
        ("delete_poll_min", cold.delete_poll_min.is_some()),
        ("delete_poll_max", cold.delete_poll_max.is_some()),
        ("stray_rate", cold.stray_rate.is_some()),
        ("copy_back_max", cold.copy_back_max.is_some()),
        ("scan_pages", cold.scan_pages.is_some()),
        ("candidate_limit", cold.candidate_limit.is_some()),
        ("tag_record_limit", cold.tag_record_limit.is_some()),
        ("entry_rounds", cold.entry_rounds.is_some()),
        ("heartbeat", cold.heartbeat.is_some()),
        ("departure", cold.departure.is_some()),
        ("held_ttl", cold.held_ttl.is_some()),
        (
            "emergency_cold_landing",
            cold.emergency_cold_landing.is_some(),
        ),
        ("step_attempts", cold.step_attempts.is_some()),
        ("step_retry_min", cold.step_retry_min.is_some()),
        ("step_retry_max", cold.step_retry_max.is_some()),
        ("read_window", cold.read_window.is_some()),
        ("read_retries", cold.read_retries.is_some()),
        ("list_marker_pages", cold.list_marker_pages.is_some()),
        ("want_retry", cold.want_retry.is_some()),
        ("want_recent_limit", cold.want_recent_limit.is_some()),
        ("copy_back_max_bytes", cold.copy_back_max_bytes.is_some()),
        ("deadline_margin", cold.deadline_margin.is_some()),
        ("reconcile_interval", cold.reconcile_interval.is_some()),
        (
            "registration_retention",
            cold.registration_retention.is_some(),
        ),
        ("idle_min", cold.idle_min.is_some()),
        ("idle_max", cold.idle_max.is_some()),
        ("lease_renew", cold.lease_renew.is_some()),
        ("lease_margin", cold.lease_margin.is_some()),
        ("listing_page_size", cold.listing_page_size.is_some()),
        ("stop_wait", cold.stop_wait.is_some()),
        ("lock_retry", cold.lock_retry.is_some()),
        ("first_beat_poll", cold.first_beat_poll.is_some()),
    ];
    refuse("cold_store", "only [hot_store] has it", &cold_has)?;
    refuse_marks_keys(cold)
}

/// The retired keys of `[hot_store]` that `hot` sets, as `hot_store.<key>`: the keys of the
/// journal and of the journal reconciler (`journal_linger`, `journal_retention`,
/// `reconcile_interval`, `registration_retention`, `journal_batch_entries`, `journal_page_size`,
/// `copy_rounds` and `delete_rounds`).
pub(super) fn retired_keys(hot: &TierTable) -> Vec<&'static str> {
    let set = [
        ("hot_store.journal_linger", hot.journal_linger.is_some()),
        (
            "hot_store.journal_retention",
            hot.journal_retention.is_some(),
        ),
        (
            "hot_store.reconcile_interval",
            hot.reconcile_interval.is_some(),
        ),
        (
            "hot_store.registration_retention",
            hot.registration_retention.is_some(),
        ),
        (
            "hot_store.journal_batch_entries",
            hot.journal_batch_entries.is_some(),
        ),
        (
            "hot_store.journal_page_size",
            hot.journal_page_size.is_some(),
        ),
        ("hot_store.copy_rounds", hot.copy_rounds.is_some()),
        ("hot_store.delete_rounds", hot.delete_rounds.is_some()),
    ];
    set.into_iter()
        .filter_map(|(key, given)| given.then_some(key))
        .collect()
}

/// Says on the operator's stderr, once for each key, that a retired key of `[hot_store]` is
/// accepted and ignored: a file written for the releases before the entries still starts.
pub(super) fn warn_retired_keys(hot: &TierTable) {
    for key in retired_keys(hot) {
        tracing::warn!("tierstore: {key} is ignored: the journal and its reconciler were removed");
    }
}
