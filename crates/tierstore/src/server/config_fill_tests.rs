//! The place of the fill socket: its default in the state directory, the key that names another
//! path, what it is refused for, the two keys that size its pool, and the shape of node that does
//! not take them.

use std::path::PathBuf;
use std::time::Duration;

use super::super::config::{PerRun, settings_from_text};
use super::super::config_cache::CacheSettings;
use super::super::config_testing::DEFAULTS;
use super::super::local_cache::{FILL_SOCKET_CONCURRENCY, FILL_SOCKET_WAIT};
use super::DEFAULT_FILL_SOCKET_NAME;

/// A build node's file with `more` lines in `[local_cache]` and `server` in `[server]`.
fn file(server: &str, more: &str) -> String {
    format!(
        r#"
[[interface]]
type   = "s3"
params = {{ addresses = ["127.0.0.1:0"] }}

[server]
proxy_id        = "c1"
bucket          = "tier-bucket"
access_key_file = "key"
{server}

[local_cache]
path   = "/var/lib/tierstore-cache"
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

/// The run of a service whose state directory is `/var/lib/tierstore`.
fn serviced() -> PerRun {
    PerRun {
        state_dir: Some(PathBuf::from("/var/lib/tierstore")),
        ..DEFAULTS
    }
}

fn cache_of(text: &str, run: PerRun) -> CacheSettings {
    settings_from_text(text, run).unwrap().into_cache().unwrap()
}

fn refusal(text: &str, run: PerRun) -> String {
    settings_from_text(text, run).unwrap_err().to_string()
}

/// Asserts that a node with no key and no state directory has no fill socket, that one with a state
/// directory has `fill.sock` in it, and that the key names another path and wins.
#[test]
fn the_socket_is_in_the_state_directory_unless_a_key_names_another_path() {
    assert_eq!(DEFAULT_FILL_SOCKET_NAME, "fill.sock");
    assert_eq!(cache_of(&file("", ""), DEFAULTS).fill_socket, None);
    let default = Some(PathBuf::from("/var/lib/tierstore/fill.sock"));
    assert_eq!(cache_of(&file("", ""), serviced()).fill_socket, default);
    let named = file("", "fill_socket = \"/run/tierstore/fill.sock\"");
    let wanted = Some(PathBuf::from("/run/tierstore/fill.sock"));
    assert_eq!(cache_of(&named, serviced()).fill_socket, wanted);
    assert_eq!(cache_of(&named, DEFAULTS).fill_socket, wanted);
}

/// Asserts what a path is refused for: a relative path, a directory, a path too long for a Unix
/// socket (the last length that fits is taken), the hand-off socket's path, a path inside the cache
/// directory, and a cache directory that is relative and so cannot be compared with it.
#[test]
fn a_path_that_cannot_be_a_socket_is_refused_by_name() {
    let refused = |value: &str| refusal(&file("", &format!("fill_socket = \"{value}\"")), DEFAULTS);
    assert!(refused("fill.sock").starts_with("local_cache.fill_socket: "));
    assert!(refused("fill.sock").contains("must be an absolute path"));
    assert!(refused("/").contains("must name a file"));
    let long = format!("/{}", "a".repeat(103));
    assert!(refused(&long).contains("is 104 bytes; the path of a Unix socket is at most 103"));
    let longest = format!("/{}", "a".repeat(102));
    let fits = file("", &format!("fill_socket = \"{longest}\""));
    assert_eq!(
        cache_of(&fits, DEFAULTS).fill_socket,
        Some(PathBuf::from(&longest))
    );

    let same = file(
        "handoff_socket = \"/run/t/s.sock\"",
        "fill_socket = \"/run/t/s.sock\"",
    );
    let why = refusal(&same, DEFAULTS);
    assert!(why.starts_with("local_cache.fill_socket: "), "{why}");
    assert!(why.contains("server.handoff_socket too"), "{why}");

    let inside = file(
        "",
        "fill_socket = \"/var/lib/tierstore-cache/obj/fill.sock\"",
    );
    let why = refusal(&inside, DEFAULTS);
    assert!(
        why.starts_with("local_cache.path: must not hold local_cache.fill_socket"),
        "{why}"
    );

    let relative = file("", "fill_socket = \"/run/t/f.sock\"").replace(
        "path   = \"/var/lib/tierstore-cache\"",
        "path   = \"cache\"",
    );
    let why = refusal(&relative, DEFAULTS);
    assert!(
        why.starts_with("local_cache.path: must be an absolute path"),
        "{why}"
    );
}

/// Asserts the pool's keys: their defaults are the constants, a value reaches the cache, and a
/// value out of range is refused naming the key and the range, the ends being taken.
#[test]
fn the_pool_of_the_socket_has_defaults_and_ranges() {
    let cache = cache_of(&file("", ""), DEFAULTS).cache;
    assert_eq!(cache.fill_socket_concurrency, FILL_SOCKET_CONCURRENCY);
    assert_eq!(cache.fill_socket_wait, FILL_SOCKET_WAIT);
    assert_eq!(
        (FILL_SOCKET_CONCURRENCY, FILL_SOCKET_WAIT),
        (16, Duration::from_secs(60))
    );
    let given = "fill_socket_concurrency = 3\nfill_socket_wait = \"7s\"";
    let cache = cache_of(&file("", given), DEFAULTS).cache;
    assert_eq!(cache.fill_socket_concurrency, 3);
    assert_eq!(cache.fill_socket_wait, Duration::from_secs(7));
    for (value, bad) in [(0, "0"), (1025, "1025")] {
        let why = refusal(
            &file("", &format!("fill_socket_concurrency = {value}")),
            DEFAULTS,
        );
        assert_eq!(
            why,
            format!("local_cache.fill_socket_concurrency: {bad} is not between 1 and 1024")
        );
    }
    for value in [1, 1024] {
        let cache = cache_of(
            &file("", &format!("fill_socket_concurrency = {value}")),
            DEFAULTS,
        );
        assert_eq!(cache.cache.fill_socket_concurrency, value);
    }
    let why = refusal(&file("", "fill_socket_wait = \"0s\""), DEFAULTS);
    assert_eq!(
        why,
        "local_cache.fill_socket_wait: 0ms is not between 1ms and 3600000ms"
    );
}

/// Asserts that a storage node with a cache in front of its proxy has no fill socket and no
/// prefetch queue and refuses the keys of them by name.
#[test]
fn a_storage_node_with_a_cache_refuses_the_keys_of_the_socket() {
    let node = r#"
[[interface]]
type   = "s3"
params = { addresses = ["192.0.2.10:9400"] }

[server]
proxy_id        = "tierstore-1"
bucket          = "tierstore"
access_key_file = "/etc/tierstore/access.key"

[hot_store]
kind   = "dir"
path   = "/srv/hot"
budget = "500GiB"

[cold_store]
kind = "dir"
path = "/srv/cold"

[local_cache]
path   = "/var/lib/tierstore-cache"
budget = "40GiB"
KEY

[[policy]]
prefix = "cas/b/"
cache  = true
"#;
    for line in [
        "fill_socket = \"/run/t/f.sock\"",
        "fill_socket_concurrency = 4",
        "fill_socket_wait = \"5s\"",
    ] {
        let text = node.replace("KEY", line);
        let key = line.split(' ').next().unwrap();
        let why = refusal(&text, DEFAULTS);
        assert_eq!(
            why,
            format!("local_cache.{key}: only a build node has a fill socket")
        );
    }
    for line in [
        "prefetch_concurrency = 1",
        "prefetch_queue = 10",
        "prefetch_queue_ttl = \"5s\"",
        "hydrate_max_trees = 3",
        "hydrate_max_blobs = 3",
        "hydrate_max_bytes = \"2MiB\"",
    ] {
        let text = node.replace("KEY", line);
        let key = line.split(' ').next().unwrap();
        let why = refusal(&text, DEFAULTS);
        assert_eq!(
            why,
            format!("local_cache.{key}: only a build node has a prefetch queue")
        );
    }
    let plain = node.replace("KEY", "");
    let settings = settings_from_text(&plain, serviced()).unwrap();
    assert!(settings.into_combined().is_some());
}
