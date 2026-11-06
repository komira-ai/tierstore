//! The entry keys of `[hot_store]`: their defaults, that each reaches the setting it stands for,
//! their ranges, and the relations between them.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::super::cli::{Built, Cli, build};
use super::super::config::{TierTable, from_text};
use super::super::config_testing::{DEFAULTS, MINIMAL};
use super::super::config_unknown::fields_of;
use super::EntryTuning;
use crate::{EntryReconcilerConfig, ProxyConfig, ReconcilerConfig, StepRetry, SystemClock};

/// `MINIMAL` with `lines` as more keys of `[hot_store]`.
fn with_hot(lines: &str) -> String {
    MINIMAL.replacen("budget = 1000", &format!("budget = 1000\n{lines}"), 1)
}

/// What `build` makes of `cli` with a good key file as its full key.
fn built_of(cli: &mut Cli) -> Built {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-entry-tuning");
    let path = dir.path().join("key");
    std::fs::write(&path, "AKID\nsecret-0123456789\n").unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
    cli.access_key_file = path;
    build(cli).unwrap()
}

fn refusal(text: &str) -> String {
    from_text(text, DEFAULTS).unwrap_err().to_string()
}

/// The settings of the entry reconciler that `built` makes, over the base the node has.
fn reconciler_of(built: &Built) -> EntryReconcilerConfig {
    let base = ReconcilerConfig::new("p1/reconciler", 1000, std::sync::Arc::new(SystemClock));
    let mut config = EntryReconcilerConfig::new(base);
    built.entry.apply_reconciler(&mut config);
    config
}

/// Every key of this change, each at a value that is not its default.
const ALL: &str = "retry_min = \"2s\"\nretry_max = \"90s\"\nstray_grace = \"20m\"\n\
    stray_interval = \"12h\"\nstray_rate = 7\ncopy_back_ttl = \"5m\"\ncopy_back_max = 11\n\
    usage_interval = \"4s\"\nusage_scan_interval = \"6h\"\nscan_pages = 13\n\
    candidate_limit = 17\ntag_record_limit = 19\nproxy_usage_stale_after = \"45s\"\n\
    format_refresh = \"3s\"\nentry_rounds = 5\ndelete_wait = \"40s\"\ndelete_poll_min = \"2s\"\n\
    delete_poll_max = \"8s\"\nheld_ttl = \"2h\"\nemergency_cold_landing = true\nstep_attempts = 6\n\
    step_retry_min = \"50ms\"\nstep_retry_max = \"3s\"";

/// A file that gives none of the keys has the values the code had before they were keys, in the
/// file's settings and in what `build` makes of them: the same as the defaults of the proxy and of
/// the entry reconciler.
#[test]
fn a_file_that_gives_none_of_the_keys_has_the_old_values() {
    let mut cli = from_text(MINIMAL, DEFAULTS).unwrap();
    assert_eq!(cli.entry_tuning, EntryTuning::default());
    let built = built_of(&mut cli);
    let old = ProxyConfig::new("p1", 1000, std::sync::Arc::new(SystemClock));
    assert_eq!(built.proxy.usage_stale_after, old.usage_stale_after);
    assert_eq!(built.proxy.format_refresh, old.format_refresh);
    assert_eq!(built.proxy.entry_rounds, old.entry_rounds);
    assert_eq!(built.proxy.delete_wait, old.delete_wait);
    assert_eq!(built.proxy.step_retry, old.step_retry);
    assert_eq!(built.proxy.step_retry, StepRetry::new());
    assert_eq!(
        built.proxy.emergency_cold_landing,
        old.emergency_cold_landing
    );
    assert!(!built.proxy.emergency_cold_landing);
    let base = ReconcilerConfig::new("p1/reconciler", 1000, std::sync::Arc::new(SystemClock));
    let before = EntryReconcilerConfig::new(base);
    let after = reconciler_of(&built);
    assert_eq!(after.retry_min, before.retry_min);
    assert_eq!(after.retry_max, before.retry_max);
    assert_eq!(after.stray_grace, before.stray_grace);
    assert_eq!(after.stray_interval, before.stray_interval);
    assert_eq!(after.stray_rate, before.stray_rate);
    assert_eq!(after.copy_back_ttl, before.copy_back_ttl);
    assert_eq!(after.copy_back_max, before.copy_back_max);
    assert_eq!(after.usage_interval, before.usage_interval);
    assert_eq!(after.usage_scan_interval, before.usage_scan_interval);
    assert_eq!(after.scan_pages, before.scan_pages);
    assert_eq!(after.candidate_limit, before.candidate_limit);
    assert_eq!(after.tag_record_limit, before.tag_record_limit);
    assert_eq!(after.held_ttl, before.held_ttl);
    assert_eq!(after.held_ttl, Duration::from_secs(3600));
}

/// Every key parses, and a value that is not the default reaches the setting of the reconciler or
/// of the proxy that it stands for.
#[test]
fn every_key_reaches_the_setting_it_stands_for() {
    let mut cli = from_text(&with_hot(ALL), DEFAULTS).unwrap();
    let built = built_of(&mut cli);
    let secs = Duration::from_secs;
    let config = reconciler_of(&built);
    assert_eq!(config.retry_min, secs(2));
    assert_eq!(config.retry_max, secs(90));
    assert_eq!(config.stray_grace, secs(1200));
    assert_eq!(config.stray_interval, secs(12 * 3600));
    assert_eq!(config.stray_rate, 7);
    assert_eq!(config.copy_back_ttl, secs(300));
    assert_eq!(config.copy_back_max, 11);
    assert_eq!(config.usage_interval, secs(4));
    assert_eq!(config.usage_scan_interval, secs(6 * 3600));
    assert_eq!(config.scan_pages, 13);
    assert_eq!(config.candidate_limit, 17);
    assert_eq!(config.tag_record_limit, 19);
    assert_eq!(config.held_ttl, secs(2 * 3600));
    assert_eq!(built.proxy.usage_stale_after, secs(45));
    assert_eq!(built.proxy.format_refresh, secs(3));
    assert_eq!(built.proxy.entry_rounds, 5);
    assert_eq!(built.proxy.delete_wait.wait, secs(40));
    assert_eq!(built.proxy.delete_wait.poll_min, secs(2));
    assert_eq!(built.proxy.delete_wait.poll_max, secs(8));
    assert!(built.proxy.emergency_cold_landing);
    assert_eq!(built.proxy.step_retry.attempts, 6);
    assert_eq!(built.proxy.step_retry.delay, Duration::from_millis(50));
    assert_eq!(built.proxy.step_retry.delay_max, secs(3));
}

/// The key `usage_stale_after`, which sets the limit of `/usagez`, is not the proxy's limit: setting
/// it leaves the proxy's at its default, and `proxy_usage_stale_after` leaves `/usagez`'s alone.
#[test]
fn the_usage_limit_of_the_proxy_is_not_the_one_of_usagez() {
    let mut cli = from_text(&with_hot("usage_stale_after = \"5m\""), DEFAULTS).unwrap();
    let built = built_of(&mut cli);
    assert_eq!(built.proxy.usage_stale_after, Duration::from_secs(30));
    assert_eq!(built.server.usage_stale_after, Duration::from_secs(300));
    let mut cli = from_text(&with_hot("proxy_usage_stale_after = \"50s\""), DEFAULTS).unwrap();
    let built = built_of(&mut cli);
    assert_eq!(built.proxy.usage_stale_after, Duration::from_secs(50));
    assert_eq!(built.server.usage_stale_after, Duration::from_secs(120));
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

/// A duration outside the range of its key is refused, naming the key and the range, and the ends of
/// the range are taken (with the keys they are ordered against moved with them).
#[test]
fn a_duration_out_of_range_is_refused_by_name_and_the_ends_are_taken() {
    const HOURS: &str = "3600000";
    const YEAR: &str = "31536000000";
    let keys = [
        ("retry_min", HOURS, "61m"),
        ("retry_max", HOURS, "61m"),
        ("stray_grace", YEAR, "8761h"),
        ("stray_interval", YEAR, "8761h"),
        ("copy_back_ttl", YEAR, "8761h"),
        ("usage_interval", HOURS, "61m"),
        ("usage_scan_interval", YEAR, "8761h"),
        ("proxy_usage_stale_after", HOURS, "61m"),
        ("format_refresh", HOURS, "61m"),
        ("delete_wait", HOURS, "61m"),
        ("delete_poll_min", HOURS, "61m"),
        ("delete_poll_max", HOURS, "61m"),
        ("held_ttl", YEAR, "8761h"),
        ("step_retry_min", HOURS, "61m"),
        ("step_retry_max", HOURS, "61m"),
    ];
    for (key, high, over) in keys {
        assert_eq!(
            refusal(&with_keys(&[(key, "0s")])),
            format!("hot_store.{key}: 0ms is not between 1ms and {high}ms")
        );
        let over_ms = if over == "61m" {
            "3660000"
        } else {
            "31539600000"
        };
        assert_eq!(
            refusal(&with_keys(&[(key, over)])),
            format!("hot_store.{key}: {over_ms}ms is not between 1ms and {high}ms")
        );
    }
    let low = [("retry_min", "1ms"), ("delete_poll_min", "1ms")];
    let hour = [
        ("retry_max", "1h"),
        ("delete_poll_max", "1h"),
        ("delete_wait", "1h"),
        ("proxy_usage_stale_after", "1h"),
    ];
    let year = [
        "stray_grace",
        "stray_interval",
        "copy_back_ttl",
        "usage_scan_interval",
    ];
    let one_ms = [
        "retry_min",
        "retry_max",
        "delete_wait",
        "delete_poll_min",
        "delete_poll_max",
    ];
    for (key, value) in hour {
        let kept: Vec<_> = low.iter().chain(&[(key, value)]).copied().collect();
        from_text(&with_keys(&kept), DEFAULTS).unwrap();
    }
    for key in year {
        from_text(&with_keys(&[(key, "8760h")]), DEFAULTS).unwrap();
        from_text(&with_keys(&[(key, "1ms")]), DEFAULTS).unwrap();
    }
    for key in ["usage_interval", "format_refresh"] {
        from_text(&with_keys(&[(key, "1ms")]), DEFAULTS).unwrap();
    }
    from_text(&with_keys(&[("format_refresh", "1h")]), DEFAULTS).unwrap();
    for key in one_ms {
        from_text(&with_keys(&[(key, "1ms"), low[0], low[1]]), DEFAULTS).unwrap();
    }
    from_text(&with_keys(&[("proxy_usage_stale_after", "1h")]), DEFAULTS).unwrap();
}

/// A count outside the range of its key is refused, naming the key, the value and the range, and
/// the ends of the range are taken.
#[test]
fn a_count_out_of_range_is_refused_by_name_and_the_ends_are_taken() {
    let counts = [
        ("stray_rate", 1_000_000_u32),
        ("copy_back_max", 1_000_000),
        ("scan_pages", 100_000),
        ("candidate_limit", 10_000_000),
        ("tag_record_limit", 100_000_000),
        ("entry_rounds", 100),
        ("step_attempts", 100),
    ];
    for (key, high) in counts {
        let zero = refusal(&with_hot(&format!("{key} = 0")));
        assert_eq!(
            zero,
            format!("hot_store.{key}: 0 is not between 1 and {high}")
        );
        let over = refusal(&with_hot(&format!("{key} = {}", high + 1)));
        assert_eq!(
            over,
            format!("hot_store.{key}: {} is not between 1 and {high}", high + 1)
        );
        for value in [1, high] {
            let cli = from_text(&with_hot(&format!("{key} = {value}")), DEFAULTS);
            assert!(cli.is_ok(), "{key} at {value}");
        }
    }
}

/// The relations between the keys are checked once, when the file is read, and the message names
/// the key that is too small.
#[test]
fn the_relations_between_keys_are_refused_by_name() {
    assert_eq!(
        refusal(&with_hot("retry_min = \"2m\"")),
        "hot_store.retry_max: 60000ms is below retry_min, which is 120000ms"
    );
    assert_eq!(
        refusal(&with_hot("delete_poll_min = \"20s\"")),
        "hot_store.delete_poll_max: 10000ms is below delete_poll_min, which is 20000ms"
    );
    assert_eq!(
        refusal(&with_hot(
            "delete_poll_min = \"20s\"\ndelete_poll_max = \"25s\"\ndelete_wait = \"10s\""
        )),
        "hot_store.delete_wait: 10000ms is below delete_poll_min, which is 20000ms"
    );
    assert_eq!(
        refusal(&with_hot("usage_interval = \"30s\"")),
        "hot_store.proxy_usage_stale_after: 30000ms must be longer than usage_interval, which is \
         30000ms, or the proxy ignores a usage that is current"
    );
    assert_eq!(
        refusal(&with_hot("proxy_usage_stale_after = \"5s\"")),
        "hot_store.proxy_usage_stale_after: 5000ms must be longer than usage_interval, which is \
         10000ms, or the proxy ignores a usage that is current"
    );
    assert_eq!(
        refusal(&with_hot("step_retry_min = \"3s\"")),
        "hot_store.step_retry_max: 2000ms is below step_retry_min, which is 3000ms"
    );
    // Equal retry sleeps are taken, and so are the ends of their range.
    let same = "step_retry_min = \"1ms\"\nstep_retry_max = \"1ms\"";
    assert!(from_text(&with_hot(same), DEFAULTS).is_ok());
    let hour = "step_retry_min = \"1h\"\nstep_retry_max = \"1h\"";
    assert!(from_text(&with_hot(hour), DEFAULTS).is_ok());
    // Equal polls and a wait equal to a poll are taken.
    let equal = "retry_min = \"60s\"\ndelete_poll_min = \"10s\"\ndelete_wait = \"10s\"";
    assert!(from_text(&with_hot(equal), DEFAULTS).is_ok());
}

/// A `held_ttl` the file sets must be longer than `lease_ttl` and than `departure` (the longest a
/// delete holds its entry), or an entry of a live owner could be reaped while its operation runs:
/// a value at or below either is refused naming the key, the value and the one it must exceed, one
/// millisecond over is taken, and a file that leaves it out is not checked against them (the
/// default of an hour is below a `departure` of two hours, which loads as before).
#[test]
fn a_held_ttl_not_longer_than_the_lease_or_the_departure_is_refused() {
    let lease = "lease_ttl = \"90s\"\nheld_ttl = \"90s\"";
    assert_eq!(
        refusal(&with_hot(lease)),
        "hot_store.held_ttl: 90000ms must be longer than lease_ttl, which is 90000ms, or an \
         entry of a live owner is reaped while its operation runs"
    );
    assert_eq!(
        refusal(&with_hot("held_ttl = \"30s\"")),
        "hot_store.held_ttl: 30000ms must be longer than lease_ttl, which is 30000ms, or an \
         entry of a live owner is reaped while its operation runs"
    );
    let departure = "departure = \"120s\"\nheld_ttl = \"119s\"";
    assert_eq!(
        refusal(&with_hot(&format!("lease_ttl = \"10s\"\n{departure}"))),
        "hot_store.held_ttl: 119000ms must be longer than departure, which is 120000ms, or an \
         entry of a live owner is reaped while its operation runs"
    );
    let default_departure = "lease_ttl = \"10s\"\nheld_ttl = \"60s\"";
    assert!(refusal(&with_hot(default_departure)).contains("departure, which is 60000ms"));
    let just_over = "lease_ttl = \"10s\"\nheld_ttl = \"60001ms\"";
    let cli = from_text(&with_hot(just_over), DEFAULTS).unwrap();
    assert_eq!(cli.entry_tuning.held_ttl, Duration::from_millis(60_001));
    let long_departure = "heartbeat = \"10s\"\ndeparture = \"2h\"";
    let cli = from_text(&with_hot(long_departure), DEFAULTS).unwrap();
    assert_eq!(cli.entry_tuning.held_ttl, Duration::from_secs(3600));
}

/// Every key is a key of `[hot_store]` only: `[cold_store]` refuses it, naming it.
#[test]
fn the_cold_store_refuses_each_entry_key() {
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
/// file that sets none of them loads to the settings it had before the keys existed.
#[test]
fn the_keys_are_known_and_a_file_without_them_is_unchanged() {
    let fields = fields_of::<TierTable>();
    for pair in ALL.split('\n').filter(|part| part.contains('=')) {
        let key = pair.split('=').next().unwrap().trim();
        assert!(fields.contains(&key), "{key}");
    }
    let plain = from_text(MINIMAL, DEFAULTS).unwrap();
    assert_eq!(plain.entry_tuning, EntryTuning::default());
    let set = from_text(&with_hot(ALL), DEFAULTS).unwrap();
    assert_ne!(set.entry_tuning, plain.entry_tuning);
    assert_eq!(set.hot_tuning, plain.hot_tuning);
}
