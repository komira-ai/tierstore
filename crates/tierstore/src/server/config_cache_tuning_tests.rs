//! The loop and limit keys of `[local_cache]`: their defaults, that each reaches the setting it
//! stands for, their ranges, and the relation of `scrub_tick` to `scrub_min_round`.

use std::time::Duration;

use super::super::config::settings_from_text;
use super::super::config_cache::{CacheSettings, LocalCacheTable};
use super::super::config_testing::DEFAULTS;
use super::super::config_unknown::fields_of;
use super::super::local_cache::{
    CacheConfig, FILL_CHUNK, FORGET_LIMIT, LOCK_INTERVAL, PASS_INTERVAL, SCRUB_TICK,
};

/// The least a build node's file can say, with `more` lines in `[local_cache]`.
fn file(more: &str) -> String {
    format!(
        r#"
[[interface]]
type   = "s3"
params = {{ addresses = ["127.0.0.1:0"] }}

[server]
proxy_id        = "c1"
bucket          = "tier-bucket"
access_key_file = "key"

[local_cache]
path   = "/c"
budget = 1000
{more}

[cold_store]
kind = "memory"

[[policy]]
prefix = "cas/"
cache  = true
"#
    )
}

fn cache_of(more: &str) -> CacheSettings {
    settings_from_text(&file(more), DEFAULTS)
        .unwrap()
        .into_cache()
        .unwrap()
}

fn refusal(more: &str) -> String {
    settings_from_text(&file(more), DEFAULTS)
        .unwrap_err()
        .to_string()
}

/// Every key of this change, each at a value that is not its default.
const ALL: &str = "pass_interval = \"20s\"\nlock_interval = \"3s\"\nscrub_tick = \"2s\"\n\
    listing_calls = 7\nforget_limit = 11\nfill_chunk = \"1MiB\"\nkeep_queue_bytes = \"2MiB\"";

/// A file that gives none of the keys has the values the code had before they were keys: the
/// defaults of the cache, which are the constants the loops used.
#[test]
fn a_file_that_gives_none_of_the_keys_has_the_old_values() {
    let cache = cache_of("").cache;
    let old = CacheConfig::new("/c", 1000, Vec::new());
    assert_eq!(cache.pass_interval, old.pass_interval);
    assert_eq!(cache.lock_interval, old.lock_interval);
    assert_eq!(cache.scrub_tick, old.scrub_tick);
    assert_eq!(cache.listing_calls, old.listing_calls);
    assert_eq!(cache.forget_limit, old.forget_limit);
    assert_eq!(cache.fill_chunk, old.fill_chunk);
    assert_eq!(cache.keep_queue_bytes, old.keep_queue_bytes);
    let secs = Duration::from_secs;
    assert_eq!(
        (PASS_INTERVAL, LOCK_INTERVAL, SCRUB_TICK),
        (secs(10), secs(1), secs(1))
    );
    assert_eq!(old.pass_interval, PASS_INTERVAL);
    assert_eq!(old.lock_interval, LOCK_INTERVAL);
    assert_eq!(old.scrub_tick, SCRUB_TICK);
    assert_eq!(old.listing_calls, 100);
    assert_eq!((old.forget_limit, FORGET_LIMIT), (65_536, 65_536));
    assert_eq!((old.fill_chunk, FILL_CHUNK), (8 << 20, 8 << 20));
    assert_eq!(old.keep_queue_bytes, 4 << 20);
}

/// Every key parses, and a value that is not the default reaches the setting of the cache that it
/// stands for.
#[test]
fn every_key_reaches_the_setting_it_stands_for() {
    let cache = cache_of(ALL).cache;
    let secs = Duration::from_secs;
    assert_eq!(cache.pass_interval, secs(20));
    assert_eq!(cache.lock_interval, secs(3));
    assert_eq!(cache.scrub_tick, secs(2));
    assert_eq!(cache.listing_calls, 7);
    assert_eq!(cache.forget_limit, 11);
    assert_eq!(cache.fill_chunk, 1 << 20);
    assert_eq!(cache.keep_queue_bytes, 2 << 20);
}

/// A duration outside 1 ms to 1 h is refused, naming the key and the range, and the ends are taken.
#[test]
fn a_duration_out_of_range_is_refused_by_name_and_the_ends_are_taken() {
    for key in ["pass_interval", "lock_interval", "scrub_tick"] {
        assert_eq!(
            refusal(&format!("{key} = \"0s\"")),
            format!("local_cache.{key}: 0ms is not between 1ms and 3600000ms")
        );
        assert_eq!(
            refusal(&format!("{key} = \"61m\"")),
            format!("local_cache.{key}: 3660000ms is not between 1ms and 3600000ms")
        );
    }
    let ends = |key: &str, value: &str| {
        cache_of(&format!("{key} = \"{value}\"\nscrub_min_round = \"0s\""))
    };
    assert_eq!(
        ends("pass_interval", "1ms").cache.pass_interval,
        Duration::from_millis(1)
    );
    assert_eq!(
        ends("lock_interval", "1h").cache.lock_interval,
        Duration::from_secs(3600)
    );
    assert_eq!(
        ends("scrub_tick", "1ms").cache.scrub_tick,
        Duration::from_millis(1)
    );
    assert_eq!(
        ends("scrub_tick", "1h").cache.scrub_tick,
        Duration::from_secs(3600)
    );
}

/// A count or a size outside the range of its key is refused, naming the key, the value and the
/// range, and the ends of the range are taken.
#[test]
fn a_count_or_size_out_of_range_is_refused_by_name_and_the_ends_are_taken() {
    let counts = [
        ("listing_calls", 1_000_000_u64),
        ("forget_limit", 100_000_000),
    ];
    for (key, high) in counts {
        assert_eq!(
            refusal(&format!("{key} = 0")),
            format!("local_cache.{key}: 0 is not between 1 and {high}")
        );
        let over = high + 1;
        assert_eq!(
            refusal(&format!("{key} = {over}")),
            format!("local_cache.{key}: {over} is not between 1 and {high}")
        );
        cache_of(&format!("{key} = 1"));
        cache_of(&format!("{key} = {high}"));
    }
    let gib = 1_u64 << 30;
    assert_eq!(
        refusal("fill_chunk = \"4095B\""),
        format!("local_cache.fill_chunk: 4095 is not between 4096 and {gib}")
    );
    assert_eq!(
        refusal("fill_chunk = \"1025MiB\""),
        format!(
            "local_cache.fill_chunk: {} is not between 4096 and {gib}",
            1025_u64 << 20
        )
    );
    assert_eq!(cache_of("fill_chunk = \"4KiB\"").cache.fill_chunk, 4096);
    assert_eq!(cache_of("fill_chunk = \"1GiB\"").cache.fill_chunk, gib);
    assert_eq!(
        refusal("keep_queue_bytes = \"0B\""),
        format!("local_cache.keep_queue_bytes: 0 is not between 1 and {gib}")
    );
    assert_eq!(
        refusal("keep_queue_bytes = \"1025MiB\""),
        format!(
            "local_cache.keep_queue_bytes: {} is not between 1 and {gib}",
            1025_u64 << 20
        )
    );
    assert_eq!(
        cache_of("keep_queue_bytes = \"1B\"").cache.keep_queue_bytes,
        1
    );
    assert_eq!(
        cache_of("keep_queue_bytes = \"1GiB\"")
            .cache
            .keep_queue_bytes,
        gib
    );
}

/// `scrub_min_round` may not be shorter than a tick of the scrub (it is counted in ticks), unless
/// it is zero; the message names the tick, in seconds when it is whole and else in milliseconds.
#[test]
fn a_scrub_min_round_shorter_than_the_scrub_tick_is_refused() {
    let why = refusal("scrub_tick = \"10s\"\nscrub_min_round = \"5s\"");
    assert_eq!(
        why,
        "local_cache.scrub_min_round: must be at least 10s (scrub_tick: the scrub ticks that \
         often), or 0s for no minimum"
    );
    let why = refusal("scrub_tick = \"1500ms\"\nscrub_min_round = \"1s\"");
    assert_eq!(
        why,
        "local_cache.scrub_min_round: must be at least 1500ms (scrub_tick: the scrub ticks that \
         often), or 0s for no minimum"
    );
    // A tick of the same length, and no minimum at all, are taken.
    let equal = cache_of("scrub_tick = \"10s\"\nscrub_min_round = \"10s\"");
    assert_eq!(equal.scrub_min_round, Duration::from_secs(10));
    let none = cache_of("scrub_tick = \"10s\"\nscrub_min_round = \"0s\"");
    assert_eq!(none.scrub_min_round, Duration::ZERO);
}

/// The keys are fields of `[local_cache]`, so a file that sets them has no unknown-key warning.
#[test]
fn the_keys_are_known_to_the_table() {
    let fields = fields_of::<LocalCacheTable>();
    for pair in ALL.split('\n') {
        let key = pair.split('=').next().unwrap().trim();
        assert!(fields.contains(&key), "{key}");
    }
}

/// Asserts that `revalidate_after` is never when the file leaves it out (a hit is served from the
/// disk until the entry is evicted, as before the key), is the duration the file gives, and may
/// be `0s` (check every hit); that it is a field of the table; and that a value that is not a
/// duration is refused naming the key and not showing the value.
#[test]
fn revalidate_after_is_never_unless_the_file_gives_a_duration() {
    assert_eq!(cache_of("").cache.revalidate_after, None);
    assert_eq!(
        CacheConfig::new("/c", 1000, Vec::new()).revalidate_after,
        None
    );
    let hour = cache_of("revalidate_after = \"1h\"").cache.revalidate_after;
    assert_eq!(hour, Some(Duration::from_secs(3600)));
    let every = cache_of("revalidate_after = \"0s\"").cache.revalidate_after;
    assert_eq!(every, Some(Duration::ZERO));
    assert!(fields_of::<LocalCacheTable>().contains(&"revalidate_after"));
    let why = refusal("revalidate_after = \"sometimes\"");
    assert!(why.contains("revalidate_after"), "{why}");
    assert!(!why.contains("sometimes"), "{why}");
}

/// Asserts that `head_before_put` is true when the file leaves it out (a put asks the upstream for
/// the key before it reads the body, as before the key), is what the file gives, is a field of the
/// table, and that a value that is not a boolean is refused naming the key and not showing the
/// value.
#[test]
fn head_before_put_is_true_unless_the_file_says_false() {
    assert!(cache_of("").cache.head_before_put);
    assert!(CacheConfig::new("/c", 1000, Vec::new()).head_before_put);
    assert!(cache_of("head_before_put = true").cache.head_before_put);
    assert!(!cache_of("head_before_put = false").cache.head_before_put);
    assert!(fields_of::<LocalCacheTable>().contains(&"head_before_put"));
    let why = refusal("head_before_put = \"sometimes\"");
    assert!(why.contains("head_before_put"), "{why}");
    assert!(!why.contains("sometimes"), "{why}");
}

/// Asserts `local_cache.pin_limit`: left out it is half the budget (the file's budget is 1000, so
/// 500, as `CacheConfig::new` has it), a size or a number given reaches the setting, both ends of
/// the range, 0 and the budget, are taken, and a value past the budget is refused naming the key.
#[test]
fn pin_limit_is_half_the_budget_unless_given_and_is_held_to_the_budget() {
    assert_eq!(CacheConfig::new("/c", 1000, Vec::new()).pin_limit, 500);
    assert_eq!(cache_of("").cache.pin_limit, 500);
    assert_eq!(cache_of("pin_limit = 300").cache.pin_limit, 300);
    assert_eq!(cache_of("pin_limit = \"1000B\"").cache.pin_limit, 1000);
    assert_eq!(cache_of("pin_limit = 0").cache.pin_limit, 0);
    assert_eq!(
        refusal("pin_limit = 1001"),
        "local_cache.pin_limit: 1001 is not between 0 and 1000"
    );
}
