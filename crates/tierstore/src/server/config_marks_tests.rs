//! The keys of `[hot_store]` for the leader's pass over the markers: `copy_concurrency`,
//! `marks_page`, `orphan_after`, `tombstone_hold` and `lock_lease`. Their defaults, that each
//! reaches the setting it stands for, their ranges, their relations to the lease and to the
//! refreshes of a lock, and that `[cold_store]` refuses them.

use std::time::Duration;

use super::super::config::{TierTable, from_text};
use super::super::config_testing::{DEFAULTS, MINIMAL};
use super::super::config_unknown::fields_of;
use super::EntryTuning;
use crate::{EntryReconcilerConfig, MarkersConfig, ReconcilerConfig, SystemClock};

/// `MINIMAL` with `lines` as more keys of `[hot_store]`.
fn with_hot(lines: &str) -> String {
    MINIMAL.replacen("budget = 1000", &format!("budget = 1000\n{lines}"), 1)
}

fn refusal(text: &str) -> String {
    from_text(text, DEFAULTS).unwrap_err().to_string()
}

fn reconciler_with(tuning: &EntryTuning) -> EntryReconcilerConfig {
    let base = ReconcilerConfig::new("p1/reconciler", 1000, std::sync::Arc::new(SystemClock));
    let mut config = EntryReconcilerConfig::new(base);
    tuning.apply_reconciler(&mut config);
    config
}

const ALL: &str = "copy_concurrency = 7\nmarks_page = 11\norphan_after = \"3m\"\n\
    tombstone_hold = \"4m\"\nlock_lease = \"5m\"";

/// A file that gives none of the keys has the values of the design (section 9), and they are the
/// defaults the reconciler's settings start from.
#[test]
fn a_file_that_gives_none_of_the_keys_has_the_defaults_of_the_design() {
    let tuning = from_text(MINIMAL, DEFAULTS).unwrap().entry_tuning;
    assert_eq!(tuning.copy_concurrency, 128);
    assert_eq!(tuning.marks_page, 1000);
    assert_eq!(tuning.orphan_after, Duration::from_secs(120));
    assert_eq!(tuning.tombstone_hold, Duration::from_secs(120));
    assert_eq!(tuning.lock_lease, Duration::from_secs(120));
    let base = ReconcilerConfig::new("p1/reconciler", 1000, std::sync::Arc::new(SystemClock));
    let before = EntryReconcilerConfig::new(base);
    let after = reconciler_with(&tuning);
    assert_eq!(after.copy_concurrency, before.copy_concurrency);
    assert_eq!(after.marks_page, before.marks_page);
    assert_eq!(after.orphan_after, before.orphan_after);
    assert_eq!(after.tombstone_hold, before.tombstone_hold);
    assert_eq!(after.markers, MarkersConfig::default());
}

/// Every key parses, and a value that is not the default reaches the setting of the reconciler it
/// stands for.
#[test]
fn every_key_reaches_the_setting_it_stands_for() {
    let tuning = from_text(&with_hot(ALL), DEFAULTS).unwrap().entry_tuning;
    let config = reconciler_with(&tuning);
    assert_eq!(config.copy_concurrency, 7);
    assert_eq!(config.marks_page, 11);
    assert_eq!(config.orphan_after, Duration::from_secs(180));
    assert_eq!(config.tombstone_hold, Duration::from_secs(240));
    assert_eq!(config.markers.lock_lease, Duration::from_secs(300));
    assert_eq!(
        config.markers.lock_refresh,
        MarkersConfig::default().lock_refresh
    );
}

/// A count is from 1 to 100000 and a duration from 1 ms to a year, and the refusal names the key.
#[test]
fn each_key_is_refused_outside_its_range() {
    let counts = ["copy_concurrency", "marks_page"];
    for key in counts {
        let zero = format!("{key} = 0");
        let big = format!("{key} = 100001");
        let message = |value: &str| format!("hot_store.{key}: {value} is not between 1 and 100000");
        assert_eq!(refusal(&with_hot(&zero)), message("0"), "{key}");
        assert_eq!(refusal(&with_hot(&big)), message("100001"), "{key}");
    }
    for key in ["orphan_after", "tombstone_hold", "lock_lease"] {
        let zero = format!("{key} = \"0s\"");
        let over = format!("{key} = \"8761h\"");
        let message = |value: &str, high: &str| {
            format!("hot_store.{key}: {value} is not between 1ms and {high}ms")
        };
        assert_eq!(
            refusal(&with_hot(&zero)),
            message("0ms", "31536000000"),
            "{key}"
        );
        assert_eq!(
            refusal(&with_hot(&over)),
            message("31539600000ms", "31536000000"),
            "{key}"
        );
    }
}

/// A `tombstone_hold` below `lease_ttl` plus `lease_margin` is refused (a stalled leader's late
/// cold write could land after the hold): with the defaults (30 s and a third of it) 40 s is the
/// least, with a margin of the file's the sum of the two, and a longer lease raises it.
#[test]
fn a_config_with_a_tombstone_hold_below_the_lease_plus_margin_is_refused() {
    let text = |lines: &str| with_hot(lines);
    let message = |hold: u64, least: u64| {
        format!(
            "hot_store.tombstone_hold: {hold}ms must be at least lease_ttl plus lease_margin, \
             which is {least}ms, or a late cold write of a stalled leader can land after the hold"
        )
    };
    assert_eq!(
        refusal(&text("tombstone_hold = \"39s\"")),
        message(39_000, 40_000)
    );
    let at_least = from_text(&text("tombstone_hold = \"40s\""), DEFAULTS).unwrap();
    assert_eq!(
        at_least.entry_tuning.tombstone_hold,
        Duration::from_secs(40)
    );
    let margin = "lease_margin = \"20s\"\ntombstone_hold = \"49s\"";
    assert_eq!(refusal(&text(margin)), message(49_000, 50_000));
    let long = "lease_ttl = \"90s\"\ntombstone_hold = \"119s\"";
    assert_eq!(refusal(&text(long)), message(119_000, 120_000));
    let enough = "lease_ttl = \"90s\"\ntombstone_hold = \"120s\"";
    assert!(from_text(&text(enough), DEFAULTS).is_ok());
}

/// A `lock_lease` below two refreshes plus the margin of a queued call is refused with the
/// markers' own message; the least is 120 s with the defaults of the other two.
#[test]
fn a_config_with_a_lock_lease_below_its_refreshes_plus_the_queued_call_margin_is_refused() {
    assert_eq!(
        refusal(&with_hot("lock_lease = \"119s\"")),
        "hot_store.lock_lease: lock_lease (119s) is below 2 x lock_refresh (30s) + \
         queued_call_margin (60s)"
    );
    let least = from_text(&with_hot("lock_lease = \"120s\""), DEFAULTS).unwrap();
    assert_eq!(least.entry_tuning.lock_lease, Duration::from_secs(120));
}

/// Every key is a key of `[hot_store]` only: `[cold_store]` refuses it, naming it. And the keys
/// are known to this release.
#[test]
fn the_cold_store_refuses_each_key_and_the_keys_are_known() {
    let fields = fields_of::<TierTable>();
    for pair in ALL.split('\n') {
        let key = pair.split('=').next().unwrap().trim();
        assert!(fields.contains(&key), "{key}");
        let text = MINIMAL.replacen(
            "[cold_store]\nkind = \"memory\"",
            &format!("[cold_store]\nkind = \"memory\"\n{pair}"),
            1,
        );
        assert_eq!(
            refusal(&text),
            format!("cold_store.{key}: only [hot_store] has it"),
            "{key}"
        );
    }
}
