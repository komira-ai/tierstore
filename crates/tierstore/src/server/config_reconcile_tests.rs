//! The reconciler-loop keys of `[hot_store]`: their defaults, that each reaches the setting it
//! stands for, their ranges, and the relations between them.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::super::cli::{Built, Cli, build};
use super::super::config::{TierTable, from_text};
use super::super::config_testing::{DEFAULTS, MINIMAL};
use super::super::config_unknown::fields_of;
use super::ReconcileTuning;
use crate::{EntryReconcilerConfig, ReconcilerConfig, SystemClock};

/// `MINIMAL` with `lines` as more keys of `[hot_store]`.
fn with_hot(lines: &str) -> String {
    MINIMAL.replacen("budget = 1000", &format!("budget = 1000\n{lines}"), 1)
}

/// What `build` makes of `cli` with a good key file as its full key.
fn built_of(cli: &mut Cli) -> Built {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-reconcile-tuning");
    let path = dir.path().join("key");
    std::fs::write(&path, "AKID\nsecret-0123456789\n").unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
    cli.access_key_file = path;
    build(cli).unwrap()
}

fn refusal(text: &str) -> String {
    from_text(text, DEFAULTS).unwrap_err().to_string()
}

/// Every key of this change, each at a value that is not its default.
const ALL: &str = "idle_min = \"2s\"\nidle_max = \"20s\"\nlease_renew = \"7s\"\n\
    lease_margin = \"9s\"\nlisting_page_size = 25\nstop_wait = \"8s\"\nlock_retry = \"3s\"\n\
    first_beat_poll = \"5ms\"";

/// A file that gives none of the keys has the values the code had before they were keys, in the
/// file's settings and in what `build` makes of them: the same as the defaults of the reconciler
/// settings.
#[test]
fn a_file_that_gives_none_of_the_keys_has_the_old_values() {
    let mut cli = from_text(MINIMAL, DEFAULTS).unwrap();
    assert_eq!(cli.reconcile_tuning, ReconcileTuning::default());
    let built = built_of(&mut cli);
    let clock = Arc::new(SystemClock);
    let old = ReconcilerConfig::new("p1/reconciler", 1000, clock);
    let new = &built.reconciler;
    assert_eq!((new.idle_min, new.idle_max), (old.idle_min, old.idle_max));
    assert_eq!((new.lease_renew, new.lease_margin), (None, None));
    assert_eq!((old.lease_renew, old.lease_margin), (None, None));
    assert_eq!(new.page_size, old.page_size);
    assert_eq!(new.stop_wait, old.stop_wait);
    let secs = Duration::from_secs;
    let tuning = cli.reconcile_tuning;
    assert_eq!((tuning.idle_min, tuning.idle_max), (secs(1), secs(10)));
    assert_eq!(tuning.listing_page_size, 100);
    assert_eq!(tuning.stop_wait, secs(5));
    assert_eq!(tuning.lock_retry, secs(1));
    assert_eq!(tuning.first_beat_poll, Duration::from_millis(1));
}

/// Every key parses, and a value that is not the default reaches the setting that it stands for.
#[test]
fn every_key_reaches_the_setting_it_stands_for() {
    let mut cli = from_text(&with_hot(ALL), DEFAULTS).unwrap();
    let built = built_of(&mut cli);
    let secs = Duration::from_secs;
    let config = &built.reconciler;
    assert_eq!((config.idle_min, config.idle_max), (secs(2), secs(20)));
    assert_eq!(config.lease_renew, Some(secs(7)));
    assert_eq!(config.lease_margin, Some(secs(9)));
    assert_eq!(config.page_size, 25);
    assert_eq!(config.stop_wait, secs(8));
    assert_eq!(cli.reconcile_tuning.lock_retry, secs(3));
    assert_eq!(
        cli.reconcile_tuning.first_beat_poll,
        Duration::from_millis(5)
    );
    // The entry reconciler starts its loop from the idle wait of the settings it is built over.
    let entry = EntryReconcilerConfig::new(built.reconciler.clone());
    assert_eq!((entry.idle_min, entry.idle_max), (secs(2), secs(20)));
}

/// A file with the keys `keys` (a later pair of one key wins), as `[hot_store]` lines.
fn with_keys(keys: &[(&str, &str)]) -> String {
    let mut given = std::collections::BTreeMap::new();
    for (key, value) in keys {
        given.insert(*key, *value);
    }
    let lines: Vec<String> = given
        .iter()
        .map(|(key, value)| format!("{key} = \"{value}\""))
        .collect();
    with_hot(&lines.join("\n"))
}

/// A duration outside the range of its key is refused, naming the key and the range, and the ends
/// of the range are taken (the idle wait with its two ends moved together).
#[test]
fn a_duration_out_of_range_is_refused_by_name_and_the_ends_are_taken() {
    let keys = [
        "idle_min",
        "idle_max",
        "stop_wait",
        "lock_retry",
        "first_beat_poll",
    ];
    for key in keys {
        assert_eq!(
            refusal(&with_keys(&[(key, "0s")])),
            format!("hot_store.{key}: 0ms is not between 1ms and 3600000ms")
        );
        assert_eq!(
            refusal(&with_keys(&[(key, "61m")])),
            format!("hot_store.{key}: 3660000ms is not between 1ms and 3600000ms")
        );
    }
    for key in ["stop_wait", "lock_retry", "first_beat_poll"] {
        from_text(&with_keys(&[(key, "1ms")]), DEFAULTS).unwrap();
        from_text(&with_keys(&[(key, "1h")]), DEFAULTS).unwrap();
    }
    from_text(&with_keys(&[("idle_min", "1ms")]), DEFAULTS).unwrap();
    from_text(&with_keys(&[("idle_max", "1h")]), DEFAULTS).unwrap();
    let low = [("idle_min", "1ms"), ("idle_max", "1ms")];
    from_text(&with_keys(&low), DEFAULTS).unwrap();
    let high = [("idle_min", "1h"), ("idle_max", "1h")];
    from_text(&with_keys(&high), DEFAULTS).unwrap();
    assert_eq!(
        refusal(&with_keys(&[("lease_renew", "0s")])),
        "hot_store.lease_renew: 0ms is not between 1ms and 3600000ms"
    );
    assert_eq!(
        refusal(&with_keys(&[("lease_renew", "61m")])),
        "hot_store.lease_renew: 3660000ms is not between 1ms and 3600000ms"
    );
    assert_eq!(
        refusal(&with_keys(&[("lease_margin", "61m")])),
        "hot_store.lease_margin: 3660000ms is not between 0ms and 3600000ms"
    );
}

/// The listing page size is refused outside 1 to 100000, naming the key, and the ends are taken.
#[test]
fn the_listing_page_size_is_refused_out_of_range_and_the_ends_are_taken() {
    assert_eq!(
        refusal(&with_hot("listing_page_size = 0")),
        "hot_store.listing_page_size: 0 is not between 1 and 100000"
    );
    assert_eq!(
        refusal(&with_hot("listing_page_size = 100001")),
        "hot_store.listing_page_size: 100001 is not between 1 and 100000"
    );
    for value in [1, 100_000] {
        let cli = from_text(&with_hot(&format!("listing_page_size = {value}")), DEFAULTS);
        assert_eq!(cli.unwrap().reconcile_tuning.listing_page_size, value);
    }
}

/// The relations between the keys are checked once, when the file is read, and the message names
/// the key to change: the idle wait must not end below where it starts, and the renewal and the
/// margin of the lease must each be below the ttl and together not above it.
#[test]
fn the_relations_between_keys_are_refused_by_name() {
    assert_eq!(
        refusal(&with_hot("idle_min = \"20s\"")),
        "hot_store.idle_max: 10000ms is below idle_min, which is 20000ms"
    );
    // Equal ends are taken.
    assert!(from_text(&with_hot("idle_min = \"10s\""), DEFAULTS).is_ok());
    assert_eq!(
        refusal(&with_hot("lease_renew = \"30s\"")),
        "hot_store.lease_renew: 30000ms must be below lease_ttl, which is 30000ms"
    );
    assert_eq!(
        refusal(&with_hot("lease_margin = \"31s\"")),
        "hot_store.lease_margin: 31000ms must be below lease_ttl, which is 30000ms"
    );
    // The key left out is a third of the ttl (10 s): 25 s and 10 s are 35 s of a 30 s lease.
    let together = "lease_renew = \"25s\"";
    let why = "hot_store.lease_renew: lease_renew (25000ms) and lease_margin (10000ms) together \
               must not be longer than lease_ttl, which is 30000ms, or the holder counts its \
               lease as lost before it renews it";
    assert_eq!(refusal(&with_hot(together)), why);
    let why = "hot_store.lease_margin: lease_renew (10000ms) and lease_margin (25000ms) together \
               must not be longer than lease_ttl, which is 30000ms, or the holder counts its \
               lease as lost before it renews it";
    assert_eq!(refusal(&with_hot("lease_margin = \"25s\"")), why);
    // Together exactly the ttl is taken, and so is a ttl the file sets.
    assert!(from_text(&with_hot("lease_renew = \"20s\""), DEFAULTS).is_ok());
    let longer = "lease_ttl = \"90s\"\nlease_renew = \"60s\"\nlease_margin = \"30s\"";
    assert!(from_text(&with_hot(longer), DEFAULTS).is_ok());
    let zero = "lease_margin = \"0s\"\nlease_renew = \"29s\"";
    assert!(from_text(&with_hot(zero), DEFAULTS).is_ok());
}

/// Every key is a key of `[hot_store]` only: `[cold_store]` refuses it, naming it.
#[test]
fn the_cold_store_refuses_each_reconciler_key() {
    for pair in ALL.split('\n').filter(|part| part.contains('=')) {
        let key = pair.split('=').next().unwrap().trim();
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

/// The keys are known to this release (a file that sets them has no unknown-key warning), and a
/// file that sets none of them loads to the settings it had before the keys existed. The retired
/// `journal_page_size` is still not a live key: the page size is `listing_page_size`.
#[test]
fn the_keys_are_known_and_a_file_without_them_is_unchanged() {
    let fields = fields_of::<TierTable>();
    for pair in ALL.split('\n').filter(|part| part.contains('=')) {
        let key = pair.split('=').next().unwrap().trim();
        assert!(fields.contains(&key), "{key}");
    }
    let plain = from_text(MINIMAL, DEFAULTS).unwrap();
    assert_eq!(plain.reconcile_tuning, ReconcileTuning::default());
    let set = from_text(&with_hot(ALL), DEFAULTS).unwrap();
    assert_ne!(set.reconcile_tuning, plain.reconcile_tuning);
    assert_eq!(set.hot_tuning, plain.hot_tuning);
    let retired = from_text(&with_hot("journal_page_size = 7"), DEFAULTS).unwrap();
    assert_eq!(retired.reconcile_tuning.listing_page_size, 100);
}
