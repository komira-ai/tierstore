//! `local_cache.put_ingest`: left out it is none, given it is the root (normalised), and what it
//! is refused for: a relative path, a node that does not take writes, a root that is the cache
//! directory, inside it or around it, a relative cache directory, and a storage node with a cache.

use std::path::PathBuf;

use crate::server::config::settings_from_text;
use crate::server::config_testing::DEFAULTS;

/// A build node's file with `more` lines in `[local_cache]`, whose cache is `cache`.
fn file(cache: &str, more: &str) -> String {
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
path   = "{cache}"
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

/// The root a build node's file with `more` ingests.
fn root_of(more: &str) -> Option<PathBuf> {
    let text = file("/var/lib/tierstore-cache", more);
    let settings = settings_from_text(&text, DEFAULTS).unwrap();
    settings.into_cache().unwrap().put_ingest
}

/// The refusal of a build node's file whose cache is `cache` and with `more`.
fn refusal(cache: &str, more: &str) -> String {
    settings_from_text(&file(cache, more), DEFAULTS)
        .unwrap_err()
        .to_string()
}

/// Asserts that a node whose file leaves the key out ingests nothing, and that one that gives it
/// ingests the root it names, normalised.
#[test]
fn the_key_names_the_root_and_its_absence_none() {
    assert_eq!(root_of("write_through = true"), None);
    let root = root_of("write_through = true\nput_ingest = \"/usr/local/tierstore/./put/\"");
    assert_eq!(root, Some(PathBuf::from("/usr/local/tierstore/put")));
}

/// Asserts the refusals, each by the key and its reason: a relative root, a node that does not
/// take writes (the key left out or false), and a root that is the cache directory, inside it or
/// around it.
#[test]
fn a_root_the_ingest_cannot_use_is_refused_by_name() {
    let cache = "/var/lib/tierstore-cache";
    let given = |root: &str, writes: &str| format!("{writes}\nput_ingest = \"{root}\"");
    assert_eq!(
        refusal(cache, &given("put", "write_through = true")),
        "local_cache.put_ingest: \"put\" is not an absolute path"
    );
    let needs = "local_cache.put_ingest: each object the ingest stores is a put to the upstream: it needs local_cache.write_through = true, and a key of the upstream that may write";
    assert_eq!(refusal(cache, &given("/srv/put", "")), needs);
    assert_eq!(
        refusal(cache, &given("/srv/put", "write_through = false")),
        needs
    );
    for root in [cache, "/var/lib/tierstore-cache/put", "/var/lib"] {
        assert_eq!(
            refusal(cache, &given(root, "write_through = true")),
            format!(
                "local_cache.put_ingest: {root:?} is the cache directory, inside it or around it: the jobs' files are not the cache's"
            )
        );
    }
}

/// Asserts that a relative cache directory, which cannot be compared with the root, is refused by
/// the key of the cache's path when the root is given.
#[test]
fn a_relative_cache_directory_is_refused_with_a_root() {
    let why = refusal("cache", "write_through = true\nput_ingest = \"/srv/put\"");
    assert_eq!(
        why,
        "local_cache.path: must be an absolute path to be compared with local_cache.put_ingest"
    );
}

/// Asserts that a storage node with a cache in front of its proxy refuses the key by name, and
/// takes the file without it.
#[test]
fn a_storage_node_with_a_cache_refuses_the_key() {
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
    let text = node.replace("KEY", "put_ingest = \"/srv/put\"");
    let why = settings_from_text(&text, DEFAULTS).unwrap_err().to_string();
    assert_eq!(
        why,
        "local_cache.put_ingest: only a build node ingests put directories"
    );
    let plain = node.replace("KEY", "");
    assert!(
        settings_from_text(&plain, DEFAULTS)
            .unwrap()
            .into_combined()
            .is_some()
    );
}
