//! The file of a storage node with a local cache in front of its proxy: what it makes, and what it
//! refuses.

use std::path::PathBuf;

use super::config::{PerRun, from_text, settings_from_text};
use super::config_combined::CombinedSettings;
use super::config_testing::{DEFAULTS, oci_interface};

/// The tables of a storage node: two directory tiers, and no cache.
const NODE: &str = r#"
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
    assert!(cache.scrub_rate.is_some());

    let given = text().replacen(
        "budget = \"40GiB\"",
        "budget = \"40GiB\"\nwrite_through = true",
        1,
    );
    let given = combined(&given);
    assert!(given.cache.config.write_through);
}

/// Asserts each refusal of the file of a node with a cache, by the exact message (the key and what
/// is wrong, so a value of the file that is repeated or leaked is a difference): `write_through = false`, no policy that caches, policies that nest,
/// a cache directory that is a tier's directory, inside it or around it (also written with `.` and
/// `..`, or relative, or with a relative tier directory), around the lock file or a key file of
/// the OCI front, a relative lock file or relative key files of the OCI front (which would lead
/// into the cache directory from the working directory), the keys of the hot store that are not for
/// the cold one, a mark out of order in `[local_cache]`, and an OCI front no policy keeps.
#[test]
fn a_combined_file_is_refused_when_it_cannot_be_run() {
    let in_cache =
        |extra: &str| NODE.replacen("[hot_store]\n", &format!("[hot_store]\n{extra}\n"), 1) + CACHE;
    let oci = |files: &str| format!("{NODE}{CACHE}{}", oci_interface("127.0.0.1:5000", files));
    let cache_path =
        |path: &str| text().replacen("\"/var/lib/tierstore-cache\"", &format!("\"{path}\""), 1);
    let cases = [
        (
            text().replacen(
                "budget = \"40GiB\"",
                "budget = \"40GiB\"\nwrite_through = false",
                1,
            ),
            "local_cache.write_through: a node that owns the data takes writes, and they pass through the cache so that its entries stay right: leave it out or give true",
        ),
        (
            text().replacen("cache  = true", "cache  = false", 1),
            "policy: no policy has cache = true, so nothing would be kept",
        ),
        (
            format!("{}\n[[policy]]\nprefix = \"cas/\"\ncache = false\n", text()),
            "policy[1].prefix: \"cas/\" overlaps \"cas/b/\" of policy[0]",
        ),
        (
            cache_path("/srv/hot"),
            "local_cache.path: must not be the directory of hot_store, inside it or around it: the tier would list the cache's files as objects",
        ),
        (
            cache_path("/srv/hot/cache"),
            "local_cache.path: must not be the directory of hot_store, inside it or around it: the tier would list the cache's files as objects",
        ),
        (
            cache_path("/srv/cold/deeper/cache"),
            "local_cache.path: must not be the directory of cold_store, inside it or around it: the tier would list the cache's files as objects",
        ),
        (
            cache_path("/srv"),
            "local_cache.path: must not be the directory of hot_store, inside it or around it: the tier would list the cache's files as objects",
        ),
        (
            cache_path("/srv/other/../hot/./cache"),
            "local_cache.path: must not be the directory of hot_store, inside it or around it: the tier would list the cache's files as objects",
        ),
        (
            cache_path("/../srv/hot"),
            "local_cache.path: must not be the directory of hot_store, inside it or around it: the tier would list the cache's files as objects",
        ),
        (
            cache_path("./hot/cache"),
            "local_cache.path: must be an absolute path (it is compared with the directories of the tiers)",
        ),
        (
            text().replacen("path   = \"/srv/hot\"", "path   = \"hot\"", 1),
            "local_cache.path: must be an absolute path to be compared with hot_store's directory",
        ),
        (
            in_cache("lock_file = \"/var/lib/tierstore-cache/instance.lock\""),
            "local_cache.path: must not hold hot_store.lock_file: it is not a place for the cache's files",
        ),
        (
            oci(
                ", tls_cert_file = \"/var/lib/tierstore-cache/oci.crt\", tls_key_file = \"/etc/oci.key\"",
            ),
            "local_cache.path: must not hold oci interface params.tls_cert_file: it is not a place for the cache's files",
        ),
        (
            oci(
                ", tls_cert_file = \"/etc/oci.crt\", tls_key_file = \"/var/lib/tierstore-cache/oci.key\"",
            ),
            "local_cache.path: must not hold oci interface params.tls_key_file: it is not a place for the cache's files",
        ),
        (
            in_cache("lock_file = \"instance.lock\""),
            "hot_store.lock_file: \"instance.lock\" must be an absolute path",
        ),
        (
            oci(", tls_cert_file = \".stage/oci.crt\", tls_key_file = \"/etc/oci.key\""),
            "oci interface params.tls_cert_file: must be an absolute path in a file with a local cache",
        ),
        (
            oci(", tls_cert_file = \"/etc/oci.crt\", tls_key_file = \".stage/oci.key\""),
            "oci interface params.tls_key_file: must be an absolute path in a file with a local cache",
        ),
        (
            NODE.replacen("[hot_store]\n", "[hot_store]\nskip_checks = [\"a=b\"]\n", 1) + CACHE,
            "hot_store.skip_checks: only [cold_store] has it",
        ),
        (
            NODE.replacen("[cold_store]\n", "[cold_store]\nlease_ttl = \"10s\"\n", 1) + CACHE,
            "cold_store.lease_ttl: only [hot_store] has it",
        ),
        (
            text().replacen(
                "budget = \"40GiB\"",
                "budget = \"40GiB\"\nlow_water = 95",
                1,
            ),
            "local_cache.low_water: must be below high_water",
        ),
        (
            oci(""),
            "oci interface params.prefix: no [[policy]] covers the blobs under \"oci/blobs/sha256/\", so every pull would go to the upstream; give a [[policy]] with prefix = \"oci/\" and cache = true",
        ),
    ];
    for (file, expected) in cases {
        assert_eq!(refusal(&file), expected);
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

/// Asserts that the settings of a storage node cannot hold the file of a node with a cache, and
/// say so, and that it is not the file of a storage node or of a build node.
#[test]
fn a_combined_file_is_not_the_settings_of_a_storage_node() {
    let why = from_text(&text(), DEFAULTS).unwrap_err().to_string();
    assert_eq!(
        why,
        "local_cache: is in a file with [hot_store] too, which these settings of a storage node cannot hold"
    );
    let settings = settings_from_text(&text(), DEFAULTS).unwrap();
    assert!(settings.into_storage().is_none());
    let settings = settings_from_text(&text(), DEFAULTS).unwrap();
    assert!(settings.into_cache().is_none());
    let node = settings_from_text(NODE, DEFAULTS).unwrap();
    assert!(node.into_combined().is_none());
}

/// The run of a service whose state directory is `/var/lib/tierstore`.
fn serviced() -> PerRun {
    PerRun {
        state_dir: Some(PathBuf::from("/var/lib/tierstore")),
        ..DEFAULTS
    }
}

fn with_cache_line(line: &str) -> String {
    text().replacen(
        "budget = \"40GiB\"",
        &format!("budget = \"40GiB\"\n{line}"),
        1,
    )
}

/// Asserts that a combined node has no fill socket without a key or a state directory, has
/// `fill.sock` in the state directory when it has one, and takes the path the key names over it;
/// and that `fill_socket_concurrency` and `fill_socket_wait` size its pool. A file without these
/// keys reads as it did.
#[test]
fn a_combined_node_places_and_sizes_its_fill_socket() {
    use std::time::Duration;
    assert_eq!(combined(&text()).fill_socket, None);
    let made = settings_from_text(&text(), serviced()).unwrap();
    let default = Some(PathBuf::from("/var/lib/tierstore/fill.sock"));
    assert_eq!(made.into_combined().unwrap().fill_socket, default);
    let named = with_cache_line("fill_socket = \"/run/tierstore/fill.sock\"");
    let wanted = Some(PathBuf::from("/run/tierstore/fill.sock"));
    assert_eq!(combined(&named).fill_socket, wanted);
    let made = settings_from_text(&named, serviced()).unwrap();
    assert_eq!(made.into_combined().unwrap().fill_socket, wanted);

    let sized = with_cache_line("fill_socket_concurrency = 4\nfill_socket_wait = \"5s\"");
    let cache = combined(&sized).cache;
    assert_eq!(cache.config.fill_socket_concurrency, 4);
    assert_eq!(cache.config.fill_socket_wait, Duration::from_secs(5));
}

/// Asserts what a fill socket path is refused for in a combined file: a relative path, the
/// hand-off socket's path, and a path inside the cache directory.
#[test]
fn a_combined_node_refuses_a_fill_socket_it_cannot_bind() {
    let relative = with_cache_line("fill_socket = \"fill.sock\"");
    assert!(refusal(&relative).starts_with("local_cache.fill_socket: "));
    let same = text().replacen(
        "access_key_file = \"/etc/tierstore/access.key\"",
        "access_key_file = \"/etc/tierstore/access.key\"\nhandoff_socket = \"/run/t/s.sock\"",
        1,
    );
    let same = same.replacen(
        "budget = \"40GiB\"",
        "budget = \"40GiB\"\nfill_socket = \"/run/t/s.sock\"",
        1,
    );
    let why = refusal(&same);
    assert!(why.contains("server.handoff_socket too"), "{why}");
    let inside = with_cache_line("fill_socket = \"/var/lib/tierstore-cache/obj/fill.sock\"");
    let why = refusal(&inside);
    assert!(
        why.starts_with("local_cache.path: must not hold local_cache.fill_socket"),
        "{why}"
    );
}
