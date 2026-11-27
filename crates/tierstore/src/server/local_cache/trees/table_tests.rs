//! `[trees]`: the defaults, each key and its range, the shapes of file that may have the table.

use std::time::Duration;

use super::{Prefetch, PreparePrefetch, TreesConfig, TreesTable};
use crate::server::config::settings_from_text;
use crate::server::config_cache::CacheSettings;
use crate::server::config_testing::{DEFAULTS, MINIMAL};
use crate::server::config_unknown::{fields_of, unknown_key_warnings};

/// A build node's file with `more` after its tables.
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
path   = "/var/lib/tierstore-cache"
budget = 1000

[cold_store]
kind = "memory"

[[policy]]
prefix = "cas/"
cache  = true
{more}
"#
    )
}

fn cache_of(text: &str) -> CacheSettings {
    settings_from_text(text, DEFAULTS)
        .unwrap()
        .into_cache()
        .unwrap()
}

fn refusal(text: &str) -> String {
    settings_from_text(text, DEFAULTS).unwrap_err().to_string()
}

fn trees(more: &str) -> TreesConfig {
    cache_of(&file(&format!("[trees]\n{more}"))).trees
}

/// Asserts that a build node's file with no `[trees]` has the defaults the design lists, and that
/// an empty `[trees]` table has the same.
#[test]
fn a_build_node_has_the_default_trees_with_no_table_and_with_an_empty_one() {
    let default = TreesConfig::default();
    assert_eq!(cache_of(&file("")).trees, default);
    assert_eq!(trees(""), default);
    assert!(default.enabled);
    assert_eq!(default.ttl, Duration::from_secs(4 * 3600));
    assert_eq!(default.ttl_max, Duration::from_secs(24 * 3600));
    assert_eq!((default.max_entries, default.max_depth), (16_777_215, 4096));
    assert_eq!(default.max_dir_entries, 1_000_000);
    assert_eq!(
        (default.max_request_bytes, default.max_bytes),
        (256 << 20, 32 << 30)
    );
    assert_eq!(default.max_placements, 1024);
    assert_eq!(default.fill_concurrency, 8);
    assert_eq!(default.prepare_concurrency, 8);
    assert_eq!(default.max_sources, 16_384);
    assert_eq!(default.prefetch_default, Prefetch::Wait);
    assert_eq!(default.prepare_prefetch, PreparePrefetch::Background);
    assert_eq!(default.create_timeout, Duration::from_secs(120));
    assert_eq!(default.max_failures, 20);
}

/// Asserts that every key of the table is read, each to a value that is not its default, and that
/// the limits are the ones the builder of a tree is handed.
#[test]
fn every_key_is_read() {
    let made = trees(
        r#"
enabled = false
ttl = "30m"
ttl_max = "2h"
max_entries = 100
max_depth = 16
max_dir_entries = 50
max_request_bytes = "1MiB"
max_bytes = "2GiB"
max_placements = 7
fill_concurrency = 3
prepare_concurrency = 2
max_sources = 9
prefetch_default = "background"
prepare_prefetch = "none"
create_timeout = "30s"
max_failures = 5
"#,
    );
    assert_eq!(
        made,
        TreesConfig {
            enabled: false,
            ttl: Duration::from_secs(1800),
            ttl_max: Duration::from_secs(7200),
            max_entries: 100,
            max_depth: 16,
            max_dir_entries: 50,
            max_request_bytes: 1 << 20,
            max_bytes: 2 << 30,
            max_placements: 7,
            fill_concurrency: 3,
            prepare_concurrency: 2,
            max_sources: 9,
            prefetch_default: Prefetch::Background,
            prepare_prefetch: PreparePrefetch::None,
            create_timeout: Duration::from_secs(30),
            max_failures: 5,
        }
    );
    let limits = made.limits();
    assert_eq!(
        (limits.max_entries, limits.max_depth, limits.max_dir_entries),
        (100, 16, 50)
    );
    assert_eq!(
        trees(r#"prefetch_default = "none""#).prefetch_default,
        Prefetch::None
    );
    assert_eq!(
        trees(r#"prepare_prefetch = "background""#).prepare_prefetch,
        PreparePrefetch::Background
    );
}

/// Asserts that a key out of its range is refused naming the key and the range, at each end, and
/// that a `ttl` above `ttl_max` is refused.
#[test]
fn a_key_out_of_its_range_is_refused_by_name() {
    let cases = [
        (
            "ttl = \"0s\"",
            "trees.ttl: 0ms is not between 1000ms and 604800000ms",
        ),
        ("ttl = \"169h\"", "trees.ttl:"),
        ("ttl_max = \"0s\"", "trees.ttl_max:"),
        (
            "ttl = \"3h\"\nttl_max = \"2h\"",
            "trees.ttl: must not be above ttl_max",
        ),
        (
            "max_entries = 0",
            "trees.max_entries: 0 is not between 1 and 16777215",
        ),
        ("max_entries = 16777216", "trees.max_entries:"),
        (
            "max_depth = 0",
            "trees.max_depth: 0 is not between 1 and 4096",
        ),
        ("max_depth = 4097", "trees.max_depth:"),
        ("max_dir_entries = 0", "trees.max_dir_entries:"),
        ("max_dir_entries = 1000001", "trees.max_dir_entries:"),
        ("max_request_bytes = 1023", "trees.max_request_bytes:"),
        ("max_request_bytes = \"2GiB\"", "trees.max_request_bytes:"),
        ("max_bytes = 1", "trees.max_bytes:"),
        ("max_bytes = \"2TiB\"", "trees.max_bytes:"),
        ("max_placements = 0", "trees.max_placements:"),
        ("max_placements = 1000001", "trees.max_placements:"),
        ("fill_concurrency = 0", "trees.fill_concurrency:"),
        ("fill_concurrency = 1025", "trees.fill_concurrency:"),
        ("prepare_concurrency = 0", "trees.prepare_concurrency:"),
        ("prepare_concurrency = 1025", "trees.prepare_concurrency:"),
        ("max_sources = 0", "trees.max_sources:"),
        ("max_sources = 1000001", "trees.max_sources:"),
        ("create_timeout = \"0s\"", "trees.create_timeout:"),
        ("create_timeout = \"2h\"", "trees.create_timeout:"),
        ("max_failures = 0", "trees.max_failures:"),
        ("max_failures = 1001", "trees.max_failures:"),
    ];
    for (more, said) in cases {
        let why = refusal(&file(&format!("[trees]\n{more}")));
        assert!(why.contains(said), "{more}: {why}");
    }
    let edges =
        "ttl = \"1s\"\nttl_max = \"168h\"\nmax_entries = 1\nmax_depth = 4096\nmax_failures = 1000";
    assert_eq!(trees(edges).ttl, Duration::from_secs(1));
}

/// Asserts that a value of the wrong kind is refused without showing it, for a duration, a size
/// and a prefetch mode.
#[test]
fn a_value_of_the_wrong_kind_is_refused_and_not_shown() {
    for more in [
        "ttl = \"soon\"",
        "max_bytes = \"lots\"",
        "prefetch_default = \"sometimes\"",
        "enabled = \"yes\"",
    ] {
        let why = refusal(&file(&format!("[trees]\n{more}")));
        let value = more.split('"').nth(1).unwrap_or("");
        assert!(
            !value.is_empty() && !why.contains(value) || more.starts_with("enabled"),
            "{why}"
        );
        assert!(why.starts_with("line "), "{why}");
    }
}

/// Asserts that the table is the build node's: a storage node and a storage node with a cache are
/// refused it, and a key of the table that this release does not know is warned about, not
/// refused.
#[test]
fn only_a_build_node_has_the_table_and_an_unknown_key_is_a_warning() {
    let storage = format!("{MINIMAL}\n[trees]\nttl = \"1h\"\n");
    assert!(refusal(&storage).starts_with("trees: only a build node has trees"));
    let combined = format!(
        "{MINIMAL}\n[local_cache]\npath = \"/c\"\nbudget = 1000\n[[policy]]\nprefix = \"cas/\"\ncache = true\n[trees]\n"
    );
    assert!(refusal(&combined).starts_with("trees: only a build node has trees"));
    let known = fields_of::<TreesTable>();
    assert_eq!(known.len(), 14);
    let table: toml::Table = toml::from_str("[trees]\nttl = \"1h\"\nnew_knob = 1\n").unwrap();
    assert_eq!(
        unknown_key_warnings(&table),
        ["tierstore: config key trees.new_knob is not known to this release and is ignored"]
    );
}

/// Asserts that a file written for an earlier release, which sets the four keys that release read
/// and nothing used (`fill_rate`, `fill_queue`, `coalesce_gap`, `whole_pack_percent`), still
/// loads: each key is warned about by name and ignored, the settings are the defaults, and a
/// value that release refused as out of range is no longer refused.
#[test]
fn a_file_with_the_removed_fill_keys_loads_with_a_warning_for_each() {
    let old = "fill_rate = \"200MiB/s\"\nfill_queue = 99\ncoalesce_gap = \"64KiB\"\nwhole_pack_percent = 75\n";
    assert_eq!(trees(old), TreesConfig::default());
    assert_eq!(
        trees("whole_pack_percent = 101\nfill_queue = 0"),
        TreesConfig::default()
    );
    let table: toml::Table = toml::from_str(&format!("[trees]\n{old}")).unwrap();
    assert_eq!(
        unknown_key_warnings(&table),
        [
            "tierstore: config key trees.coalesce_gap is not known to this release and is ignored",
            "tierstore: config key trees.fill_queue is not known to this release and is ignored",
            "tierstore: config key trees.fill_rate is not known to this release and is ignored",
            "tierstore: config key trees.whole_pack_percent is not known to this release and is ignored",
        ]
    );
}

/// Asserts that `prepare_prefetch` takes `background` and `none` alone: `wait` (a value of
/// `prefetch_default`, which `Prepare` has not), a misspelling, another case, an empty string and
/// a value of another type are refused, naming the key, and for a string the values it takes,
/// without repeating the value.
#[test]
fn prepare_prefetch_refuses_a_value_it_does_not_take_by_name() {
    let said = "trees.prepare_prefetch: not a value this key takes (it is not shown); expected \"background\" or \"none\"";
    for value in ["wait", "lazy", "None", "Background", "background ", ""] {
        let why = refusal(&file(&format!("[trees]\nprepare_prefetch = \"{value}\"")));
        assert_eq!(why, said, "{value:?}");
    }
    let why = refusal(&file("[trees]\nprepare_prefetch = true"));
    assert!(why.starts_with("line "), "{why}");
}
