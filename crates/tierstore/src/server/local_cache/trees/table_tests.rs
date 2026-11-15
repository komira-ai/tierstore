//! `[trees]`: the defaults, each key and its range, the shapes of file that may have the table.

use std::time::Duration;

use super::{Prefetch, TreesConfig, TreesTable};
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
    assert_eq!(
        (
            default.fill_concurrency,
            default.fill_rate,
            default.fill_queue
        ),
        (8, None, 16_384)
    );
    assert_eq!(default.coalesce_gap, 256 << 10);
    assert_eq!(default.prefetch_default, Prefetch::Wait);
    assert_eq!(default.whole_pack_percent, 50);
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
fill_rate = "200MiB/s"
fill_queue = 99
coalesce_gap = "64KiB"
prefetch_default = "background"
whole_pack_percent = 75
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
            fill_rate: Some(200 << 20),
            fill_queue: 99,
            coalesce_gap: 64 << 10,
            prefetch_default: Prefetch::Background,
            whole_pack_percent: 75,
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
    assert_eq!(trees(r#"fill_rate = "off""#).fill_rate, None);
    assert_eq!(trees("coalesce_gap = 0").coalesce_gap, 0);
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
        ("fill_queue = 0", "trees.fill_queue:"),
        ("fill_queue = 10000001", "trees.fill_queue:"),
        ("coalesce_gap = \"2GiB\"", "trees.coalesce_gap:"),
        ("whole_pack_percent = 0", "trees.whole_pack_percent:"),
        ("whole_pack_percent = 101", "trees.whole_pack_percent:"),
        ("create_timeout = \"0s\"", "trees.create_timeout:"),
        ("create_timeout = \"2h\"", "trees.create_timeout:"),
        ("max_failures = 0", "trees.max_failures:"),
        ("max_failures = 1001", "trees.max_failures:"),
    ];
    for (more, said) in cases {
        let why = refusal(&file(&format!("[trees]\n{more}")));
        assert!(why.contains(said), "{more}: {why}");
    }
    let edges = "ttl = \"1s\"\nttl_max = \"168h\"\nmax_entries = 1\nmax_depth = 4096\nmax_failures = 1000\nwhole_pack_percent = 100";
    assert_eq!(trees(edges).ttl, Duration::from_secs(1));
}

/// Asserts that a value of the wrong kind is refused without showing it, for a duration, a size,
/// a rate and a prefetch mode.
#[test]
fn a_value_of_the_wrong_kind_is_refused_and_not_shown() {
    for more in [
        "ttl = \"soon\"",
        "max_bytes = \"lots\"",
        "fill_rate = \"fast\"",
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
    assert_eq!(known.len(), 17);
    let table: toml::Table = toml::from_str("[trees]\nttl = \"1h\"\nnew_knob = 1\n").unwrap();
    assert_eq!(
        unknown_key_warnings(&table),
        ["tierstore: config key trees.new_knob is not known to this release and is ignored"]
    );
}
