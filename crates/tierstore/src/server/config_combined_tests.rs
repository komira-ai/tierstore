//! The file of a storage node with a local cache in front of its proxy: what it makes, and what it
//! refuses.

use super::config::{from_text, settings_from_text};
use super::config_combined::CombinedSettings;
use super::config_testing::DEFAULTS;

/// The tables of a storage node: two directory tiers, and no cache.
const NODE: &str = r#"
[server]
listen          = "192.0.2.10:9400"
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
"#;

/// The tables that give that node a cache.
const CACHE: &str = r#"
[local_cache]
path   = "/var/lib/tierstore-cache"
budget = "40GiB"

[[policy]]
prefix = "cas/b/"
cache  = true
"#;

fn text() -> String {
    format!("{NODE}{CACHE}")
}

fn combined(text: &str) -> CombinedSettings {
    let settings = settings_from_text(text, DEFAULTS).unwrap();
    settings.into_combined().unwrap()
}

fn refusal(text: &str) -> String {
    settings_from_text(text, DEFAULTS).unwrap_err().to_string()
}

/// Asserts what a file with the three tables makes: the node is what the file without
/// `[local_cache]` makes, the cache is the one its table gives (with the defaults of the cache),
/// writes pass through it whether the key is left out or true, and versions are compared by size.
#[test]
fn a_file_with_the_three_tables_makes_a_combined_node() {
    let made = combined(&text());
    assert_eq!(made.node, from_text(NODE, DEFAULTS).unwrap());
    let cache = &made.cache;
    assert_eq!(cache.config.path.to_str(), Some("/var/lib/tierstore-cache"));
    assert_eq!(cache.config.budget, 40 << 30);
    assert_eq!(cache.config.policies, [("cas/b/".to_owned(), true)]);
    assert_eq!(cache.config.high_water, 90);
    assert!(cache.config.write_through);
    assert!(cache.config.tiered_upstream);
    assert!(cache.scrub_rate.is_some());

    let given = text().replacen(
        "budget = \"40GiB\"",
        "budget = \"40GiB\"\nwrite_through = true",
        1,
    );
    let given = combined(&given);
    assert!(given.cache.config.write_through);
    assert!(given.cache.config.tiered_upstream);
}

/// Asserts each refusal of the file of a node with a cache, each naming the key and what is wrong,
/// and none repeating a value: `write_through = false`, no policy that caches, policies that nest,
/// a cache directory that is a tier's directory, inside it or around it (and around the lock file
/// or a key file of the OCI front), a list of endpoints, the keys of the hot store that are not for
/// the cold one, a mark out of order in `[local_cache]`, and an OCI front no policy keeps.
#[test]
fn a_combined_file_is_refused_when_it_cannot_be_run() {
    let in_cache =
        |extra: &str| NODE.replacen("[hot_store]\n", &format!("[hot_store]\n{extra}\n"), 1) + CACHE;
    let oci = |files: &str| {
        let table = format!("\n[oci]\nlisten = \"127.0.0.1:5000\"\n{files}\n");
        format!("{NODE}{CACHE}{table}")
    };
    let cache_path =
        |path: &str| text().replacen("\"/var/lib/tierstore-cache\"", &format!("\"{path}\""), 1);
    let cases = [
        (
            text().replacen(
                "budget = \"40GiB\"",
                "budget = \"40GiB\"\nwrite_through = false",
                1,
            ),
            "local_cache.write_through",
            "takes writes",
        ),
        (
            text().replacen("cache  = true", "cache  = false", 1),
            "policy",
            "no policy has cache = true",
        ),
        (
            format!("{}\n[[policy]]\nprefix = \"cas/\"\ncache = false\n", text()),
            "policy[1].prefix",
            "overlaps",
        ),
        (
            cache_path("/srv/hot"),
            "local_cache.path",
            "must not be the directory of hot_store",
        ),
        (
            cache_path("/srv/hot/cache"),
            "local_cache.path",
            "must not be the directory of hot_store",
        ),
        (
            cache_path("/srv/cold/deeper/cache"),
            "local_cache.path",
            "must not be the directory of cold_store",
        ),
        (
            cache_path("/srv"),
            "local_cache.path",
            "must not be the directory of hot_store",
        ),
        (
            in_cache("lock_file = \"/var/lib/tierstore-cache/instance.lock\""),
            "local_cache.path",
            "must not hold hot_store.lock_file",
        ),
        (
            oci(
                "tls_cert_file = \"/var/lib/tierstore-cache/oci.crt\"\ntls_key_file = \"/etc/oci.key\"",
            ),
            "local_cache.path",
            "must not hold oci.tls_cert_file",
        ),
        (
            oci(
                "tls_cert_file = \"/etc/oci.crt\"\ntls_key_file = \"/var/lib/tierstore-cache/oci.key\"",
            ),
            "local_cache.path",
            "must not hold oci.tls_key_file",
        ),
        (
            NODE.replacen(
                "[cold_store]\n",
                "[cold_store]\nendpoints = [\"http://c:1\"]\n",
                1,
            ) + CACHE,
            "cold_store.endpoints",
            "build node",
        ),
        (
            NODE.replacen("[hot_store]\n", "[hot_store]\nskip_checks = [\"a=b\"]\n", 1) + CACHE,
            "hot_store.skip_checks",
            "only [cold_store] has it",
        ),
        (
            NODE.replacen("[cold_store]\n", "[cold_store]\nlease_ttl = \"10s\"\n", 1) + CACHE,
            "cold_store.lease_ttl",
            "only [hot_store] has it",
        ),
        (
            text().replacen(
                "budget = \"40GiB\"",
                "budget = \"40GiB\"\nlow_water = 95",
                1,
            ),
            "local_cache.low_water",
            "below high_water",
        ),
        (oci(""), "oci.prefix", "no [[policy]]"),
    ];
    for (file, key, fragment) in cases {
        let why = refusal(&file);
        assert!(why.starts_with(&format!("{key}: ")), "{key}: {why}");
        assert!(why.contains(fragment), "{key}: {why}");
        for value in ["/srv", "/var/lib", "/etc/oci", "http://c:1"] {
            assert!(!why.contains(value), "{key}: {why}");
        }
    }
}

/// Asserts that the cache directory is compared with the tiers' by component and only with the
/// tiers that are directories: a directory beside the hot one whose name starts the same is fine,
/// and a memory hot tier and an S3 cold tier leave the cache anywhere.
#[test]
fn the_cache_path_is_compared_by_component_and_only_against_directory_tiers() {
    let beside = text().replacen("\"/var/lib/tierstore-cache\"", "\"/srv/hot2\"", 1);
    assert_eq!(
        combined(&beside).cache.config.path.to_str(),
        Some("/srv/hot2")
    );

    let memory = NODE.replacen(
        "kind   = \"dir\"\npath   = \"/srv/hot\"",
        "kind   = \"memory\"",
        1,
    );
    let s3 = memory.replacen(
        "kind = \"dir\"\npath = \"/srv/cold\"",
        "kind = \"s3\"\nbucket = \"cold\"\nendpoint = \"http://127.0.0.1:1\"\ncredentials_file = \"/etc/cold.key\"",
        1,
    );
    for tiers in [memory, s3] {
        let inside =
            format!("{tiers}{CACHE}").replacen("\"/var/lib/tierstore-cache\"", "\"/srv/hot\"", 1);
        combined(&inside);
    }
}

/// Asserts that the file of a node with a cache has no flag form, and says so, and that it is
/// not the file of a storage node or of a build node.
#[test]
fn a_combined_file_has_no_flag_form() {
    let why = from_text(&text(), DEFAULTS).unwrap_err().to_string();
    assert_eq!(
        why,
        "local_cache: is in a file with [hot_store] too, which has no flag form"
    );
    let settings = settings_from_text(&text(), DEFAULTS).unwrap();
    assert!(settings.into_storage().is_none());
    let settings = settings_from_text(&text(), DEFAULTS).unwrap();
    assert!(settings.into_cache().is_none());
    let node = settings_from_text(NODE, DEFAULTS).unwrap();
    assert!(node.into_combined().is_none());
}
