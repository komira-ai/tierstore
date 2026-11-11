//! The build-node shape of the configuration file.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::config::{from_text, settings_from_text};
use super::config_cache::{CacheSettings, LocalCacheTable, Rate};
use super::config_testing::{DEFAULT_DRAIN, DEFAULTS, GOLDEN, MINIMAL, oci_interface};
use super::config_unknown::fields_of;
use super::local_cache::{FILL_CHUNK, SCRUB_MIN_ROUND, SCRUB_RATE, WHOLE_FILL_LIMIT};

/// A build node's file with every key given a value that is not the default.
const CACHE: &str = r#"
[[interface]]
type   = "s3"
params = { addresses = ["192.0.2.10:9400"] }

[[interface]]
type   = "oci"
params = { addresses = ["192.0.2.10:5000"], tls_cert_file = "/etc/tierstore-cache/oci.crt", tls_key_file = "/etc/tierstore-cache/oci.key", prefix = "images/", max_connections = 16, connection_lifetime = "30m" }

[server]
proxy_id              = "tierstore-cache-1"
bucket                = "tierstore"
region                = "eu-west-1"
max_request_bytes     = 134217728
access_key_file       = "/etc/tierstore-cache/access.key"
read_only_key_files   = ["/etc/tierstore-cache/clients.key"]
presign_max_expires   = "10m"
max_concurrent_puts   = 8
domains               = ["s3.example.com"]
delete_concurrency    = 4
put_permit_wait       = "25s"
put_min_bytes_per_sec = 2048
put_rate_grace        = "45s"
body_idle_timeout     = "20s"
aws_chunked_max_line_bytes = 2048
s3_request_timeout    = "2m"

[local_cache]
path       = "/var/lib/tierstore-cache"
layout     = "digest-v1"
budget     = "40GiB"
high_water = 85
low_water  = 60
critical   = 97
free_floor = 15
max_concurrent_fills = 6
max_concurrent_keeps = 3
keep_wait  = "45s"
keep_deadline = "5m"
write_through = true
whole_fill_limit = "128MiB"
cache      = true
scrub_rate = "50MiB/s"
scrub_min_round = "30m"

[cold_store]
kind             = "s3"
bucket           = "tierstore"
endpoint         = "http://192.0.2.20:9300"
region           = "us-west-2"
credentials_file = "/etc/tierstore-cache/upstream-read.key"

[[policy]]
prefix = "cas/b/"
cache  = true

[[policy]]
prefix = "cas/d/"
cache  = true

[[policy]]
prefix = "images/"
cache  = true

[[policy]]
prefix = ""
cache  = false
"#;

/// The least a build node's file can say.
const LEAST: &str = r#"
[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:0"] }

[server]
proxy_id        = "c1"
bucket          = "tier-bucket"
access_key_file = "key"

[local_cache]
path   = "/c"
budget = 1000

[cold_store]
kind = "memory"

[[policy]]
prefix = "cas/"
cache  = true
"#;

/// `LEAST` with a policy that keeps the keys of the default prefix of an `oci` interface, `oci/`.
const KEEPS_OCI: &str = r#"
[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:0"] }

[server]
proxy_id        = "c1"
bucket          = "tier-bucket"
access_key_file = "key"

[local_cache]
path   = "/c"
budget = 1000

[cold_store]
kind = "memory"

[[policy]]
prefix = "oci/"
cache  = true
"#;

/// The settings of a build node's file.
fn cache_of(text: &str) -> CacheSettings {
    settings_from_text(text, DEFAULTS)
        .unwrap()
        .into_cache()
        .unwrap()
}

/// The message of the refusal of this file text.
fn refusal(text: &str) -> String {
    settings_from_text(text, DEFAULTS).unwrap_err().to_string()
}

/// `message` without the leading `line N, column M: ` of a refusal of the syntax, if it has one.
fn without_position(message: &str) -> &str {
    match message.split_once(": ") {
        Some((position, rest)) if position.starts_with("line ") => rest,
        _ => message,
    }
}

/// `CACHE` with the text `from` replaced by `to`, which must be in it.
fn cache_with(from: &str, to: &str) -> String {
    assert!(CACHE.contains(from), "{from}");
    CACHE.replacen(from, to, 1)
}

/// Asserts that a build node's `server.slot` reaches the settings of its front, as a storage
/// node's does, with none when the key is left out, and that zero is refused naming the key: the
/// hand-over's slot units run both shapes.
#[test]
fn a_build_node_file_takes_a_slot() {
    let with = |value: &str| cache_with("[server]", &format!("[server]\nslot = {value}"));
    assert_eq!(cache_of(&with("9302")).front.slot, Some(9302));
    assert_eq!(cache_of(CACHE).front.slot, None);
    assert!(refusal(&with("0")).contains("server.slot"));
}

/// Asserts that a build node's file makes the settings it says, every key to its field, and that
/// a storage node's loader (`from_text`) says the file is a build node's.
#[test]
fn a_build_node_file_makes_a_cache() {
    let cache = cache_of(CACHE);
    let front = &cache.front;
    assert_eq!(front.listen, vec!["192.0.2.10:9400".parse().unwrap()]);
    assert_eq!(front.proxy_id, "tierstore-cache-1");
    assert_eq!(
        (front.bucket.as_str(), front.region.as_str()),
        ("tierstore", "eu-west-1")
    );
    assert_eq!(front.max_request_bytes, 134_217_728);
    assert_eq!(
        front.access_key_file,
        Path::new("/etc/tierstore-cache/access.key")
    );
    assert_eq!(
        front.read_only_key_file,
        [PathBuf::from("/etc/tierstore-cache/clients.key")]
    );
    assert_eq!(front.presign_max_expires, Duration::from_secs(600));
    assert_eq!(front.max_concurrent_puts, 8);
    assert_eq!(front.domains, ["s3.example.com"]);
    assert_eq!(front.delete_concurrency, 4);
    assert_eq!(front.put_permit_wait, Duration::from_secs(25));
    assert_eq!(front.put_min_bytes_per_sec, 2048);
    assert_eq!(front.put_rate_grace, Duration::from_secs(45));
    assert_eq!(front.body_idle_timeout, Duration::from_secs(20));
    assert_eq!(front.aws_chunked_max_line_bytes, 2048);
    assert_eq!(cache.s3_request_timeout, Duration::from_secs(120));
    assert_eq!(front.drain_lb_wait, DEFAULT_DRAIN.0);

    let config = &cache.cache;
    assert_eq!(config.path, Path::new("/var/lib/tierstore-cache"));
    assert_eq!(config.budget, 40 << 30);
    assert_eq!(
        (
            config.high_water,
            config.low_water,
            config.critical,
            config.free_floor
        ),
        (85, 60, 97, 15)
    );
    let policies = [
        ("cas/b/", true),
        ("cas/d/", true),
        ("images/", true),
        ("", false),
    ];
    let policies: Vec<(String, bool)> = policies
        .iter()
        .map(|(p, c)| ((*p).to_owned(), *c))
        .collect();
    assert_eq!(config.policies, policies);
    assert_eq!(
        (config.fill_chunk, config.whole_limit),
        (8 << 20, 128 << 20)
    );
    assert_eq!((FILL_CHUNK, WHOLE_FILL_LIMIT), (8 << 20, 64 << 20));
    assert_eq!(config.max_concurrent_fills, 6);
    assert_eq!(config.max_concurrent_keeps, 3);
    assert_eq!(config.keep_wait, Duration::from_secs(45));
    assert_eq!(config.keep_deadline, Duration::from_secs(300));
    assert!(config.write_through);
    let upstream = &cache.upstream;
    assert_eq!(upstream.spec, "s3:tierstore");
    assert_eq!(upstream.endpoint.as_deref(), Some("http://192.0.2.20:9300"));
    assert_eq!(upstream.region.as_deref(), Some("us-west-2"));
    assert_eq!(
        upstream.credentials_file.as_deref(),
        Some(Path::new("/etc/tierstore-cache/upstream-read.key"))
    );
    assert_eq!(cache.scrub_rate, Some(50 << 20));
    assert_eq!(cache.scrub_min_round, Duration::from_secs(30 * 60));

    let why = from_text(CACHE, DEFAULTS).unwrap_err().to_string();
    assert!(why.contains("the file of a build node"), "{why}");
    let settings = settings_from_text(CACHE, DEFAULTS).unwrap();
    assert!(settings.into_storage().is_none());
    let storage = settings_from_text(GOLDEN, DEFAULTS).unwrap();
    assert!(storage.into_cache().is_none());
}

/// Asserts the defaults of a build node's file: marks at 90, 75 and 95 percent, a floor of 10
/// percent, the region of the flags, the scrub at its default rate, and the limits of the flags.
#[test]
fn a_build_node_file_has_the_defaults_of_the_flags_and_of_the_cache() {
    let cache = cache_of(LEAST);
    let config = &cache.cache;
    assert_eq!(
        (
            config.high_water,
            config.low_water,
            config.critical,
            config.free_floor
        ),
        (90, 75, 95, 10)
    );
    assert_eq!(config.max_concurrent_fills, 4);
    assert_eq!(config.max_concurrent_keeps, 8);
    assert_eq!(config.keep_wait, Duration::from_secs(5));
    assert_eq!(config.keep_deadline, Duration::from_secs(60));
    assert!(
        !config.write_through,
        "a file that leaves it out is a read cache"
    );
    assert_eq!(config.whole_limit, WHOLE_FILL_LIMIT);
    assert_eq!(config.budget, 1000);
    assert_eq!(cache.front.region, "us-east-1");
    assert_eq!(cache.front.max_request_bytes, 256 << 20);
    assert_eq!(cache.front.max_concurrent_puts, 32);
    assert!(cache.front.domains.is_empty());
    assert_eq!(cache.front.delete_concurrency, 16);
    assert_eq!(cache.front.put_permit_wait, Duration::from_secs(15));
    assert_eq!(cache.s3_request_timeout, Duration::from_secs(60));
    assert_eq!(cache.scrub_rate, Some(SCRUB_RATE));
    assert_eq!(cache.scrub_min_round, SCRUB_MIN_ROUND);
    assert_eq!(SCRUB_MIN_ROUND, Duration::from_secs(3600));
    assert_eq!(SCRUB_RATE, 16 << 20);
    assert_eq!(cache.upstream.spec, "memory");
}

/// Asserts that `scrub_rate` of `"off"` or of zero bytes turns the scrub off.
#[test]
fn a_scrub_rate_of_off_or_zero_turns_the_scrub_off() {
    for off in ["\"off\"", "\"0B/s\""] {
        let text = cache_with("\"50MiB/s\"", off);
        assert_eq!(cache_of(&text).scrub_rate, None, "{off}");
    }
    assert_eq!(serde_json::from_str::<Rate>("\"off\"").unwrap(), Rate(0));
}

/// Asserts that a single `endpoints` entry is the upstream, as `endpoint` is.
#[test]
fn a_list_of_one_endpoint_is_the_upstream() {
    let one = cache_with(
        "endpoint         = \"http://192.0.2.20:9300\"",
        "endpoints        = [\"http://192.0.2.21:9300\"]",
    );
    let cache = cache_of(&one);
    assert_eq!(
        cache.upstream.endpoint.as_deref(),
        Some("http://192.0.2.21:9300")
    );
}

/// Asserts that several `endpoints` are the upstream's endpoints in order.
#[test]
fn a_list_of_endpoints_is_the_upstreams_endpoints_in_turn() {
    let several = cache_with(
        "endpoint         = \"http://192.0.2.20:9300\"",
        "endpoints        = [\"http://192.0.2.21:9300\", \"http://192.0.2.22:9300\"]",
    );
    let upstream = cache_of(&several).upstream;
    assert_eq!(upstream.endpoint.as_deref(), Some("http://192.0.2.21:9300"));
    assert_eq!(upstream.more_endpoints, ["http://192.0.2.22:9300"]);
}

/// Asserts each refusal of a build node's file, each naming the key and what is wrong, and none
/// repeating a value: a hot store with a cache that does not take writes, a layout that is not the only one, a cache that
/// is turned off, marks out of order or out of range, a budget that is missing or zero, a rate
/// that is not a rate, no policy that caches, more than one upstream or two ways to give it,
/// and the keys of a storage node that do nothing here.
#[test]
fn a_build_node_file_is_refused_when_it_cannot_be_run() {
    let no_cache_policy = cache_with(
        "prefix = \"cas/b/\"\ncache  = true",
        "prefix = \"cas/b/\"\ncache  = false",
    )
    .replacen(
        "prefix = \"cas/d/\"\ncache  = true",
        "prefix = \"cas/d/\"\ncache  = false",
        1,
    )
    .replacen(
        "prefix = \"images/\"\ncache  = true",
        "prefix = \"images/\"\ncache  = false",
        1,
    );
    let cases = [
        (
            cache_with(
                "[local_cache]",
                "[hot_store]\nkind = \"memory\"\nbudget = 1\n\n[local_cache]",
            )
            .replace("write_through = true", "write_through = false"),
            "local_cache.write_through",
        ),
        (
            cache_with("\"digest-v1\"", "\"digest-v2\""),
            "local_cache.layout",
        ),
        (
            cache_with("cache      = true", "cache      = false"),
            "local_cache.cache",
        ),
        (
            cache_with("budget     = \"40GiB\"\n", ""),
            "local_cache.budget",
        ),
        (cache_with("\"40GiB\"", "0"), "local_cache.budget"),
        (
            cache_with("low_water  = 60", "low_water  = 90"),
            "local_cache.low_water",
        ),
        (
            cache_with("critical   = 97", "critical   = 80"),
            "must not be above critical",
        ),
        (
            cache_with("free_floor = 15", "free_floor = 100"),
            "local_cache.free_floor",
        ),
        (
            cache_with("high_water = 85", "high_water = 101"),
            "local_cache.high_water",
        ),
        (
            cache_with("\"50MiB/s\"", "\"fast\""),
            "expected a rate such as",
        ),
        (cache_with("\"50MiB/s\"", "50"), "scrub_rate"),
        (no_cache_policy, "no policy has cache = true"),
        (
            cache_with(
                "[[policy]]\nprefix = \"cas/d/\"",
                "[[policy]]\nprefix = \"cas/b/\"",
            ),
            "also the prefix of policy[0]",
        ),
        (
            cache_with("prefix = \"cas/d/\"", "prefix = \"_journal/\""),
            "keeps for itself",
        ),
        (
            cache_with(
                "credentials_file = ",
                "skip_checks = []\ncredentials_file = ",
            ),
            "cold_store.skip_checks",
        ),
        (
            cache_with(
                "credentials_file = ",
                "copy_parallel = 4\ncredentials_file = ",
            ),
            "cold_store.copy_parallel",
        ),
        (
            cache_with(
                "s3_request_timeout    = \"2m\"",
                "backend_check_wait = \"1m\"",
            ),
            "server.backend_check_wait",
        ),
        (
            cache_with(
                "endpoint         = \"http://192.0.2.20:9300\"",
                "endpoints = [\"http://a:1\", \"http://a:1\"]",
            ),
            "cold_store.endpoints: \"http://a:1\" is given twice",
        ),
        (
            cache_with(
                "endpoint         = \"http://192.0.2.20:9300\"",
                "endpoints = []",
            ),
            "cold_store.endpoints: is empty; give at least one endpoint",
        ),
        (
            cache_with(
                "credentials_file = ",
                "endpoints = [\"http://a:1\"]\ncredentials_file = ",
            ),
            "not both",
        ),
        (
            cache_with("endpoint         = \"http://192.0.2.20:9300\"\n", ""),
            "cold_store.endpoint",
        ),
        (
            cache_with(
                "credentials_file = ",
                "secret_access_key = \"x\"\ncredentials_file = ",
            ),
            "never in the file",
        ),
    ];
    for (text, want) in cases {
        let why = refusal(&text);
        assert!(why.contains(want), "{want}: {why}");
    }
    // A rate that does not parse is refused naming its key, and the value is not shown.
    let why = refusal(&cache_with("\"50MiB/s\"", "\"fast\""));
    assert!(why.contains("scrub_rate"), "{why}");
    assert!(!why.contains("fast"), "{why}");
    // The same for a minimum between rounds that is not a duration.
    let text = cache_with("scrub_min_round = \"30m\"", "scrub_min_round = \"soon\"");
    let why = refusal(&text);
    assert!(why.contains("scrub_min_round"), "{why}");
    assert!(!why.contains("soon"), "{why}");
}

/// Asserts that a minimum between rounds under one second is refused naming its key (it would
/// round down to no minimum), and that exactly `0s` and one second are accepted.
#[test]
fn a_scrub_min_round_under_a_second_is_refused_except_zero() {
    let with = |value: &str| {
        cache_with(
            "scrub_min_round = \"30m\"",
            &format!("scrub_min_round = \"{value}\""),
        )
    };
    let why = refusal(&with("500ms"));
    assert!(why.contains("local_cache.scrub_min_round"), "{why}");
    assert!(why.contains("at least 1s"), "{why}");
    assert_eq!(cache_of(&with("0s")).scrub_min_round, Duration::ZERO);
    assert_eq!(
        cache_of(&with("1s")).scrub_min_round,
        Duration::from_secs(1)
    );
}

/// Asserts that a `whole_fill_limit` that is not a size is refused naming its key and without the
/// value, as a `budget` is: the key is one a refusal may name.
#[test]
fn a_whole_fill_limit_that_is_not_a_size_is_refused_naming_the_key_and_not_the_value() {
    for bad in ["\"huge\"", "-5"] {
        let text = LEAST.replacen(
            "budget = 1000",
            &format!("budget = 1000\nwhole_fill_limit = {bad}"),
            1,
        );
        let why = refusal(&text);
        assert!(why.contains("whole_fill_limit"), "{bad}: {why}");
        assert!(!why.contains("huge"), "{bad}: {why}");
    }
}

/// Asserts the edges of the water marks: a low water mark one under the high one, a high one
/// equal to the critical one and a free floor of 99 are accepted; a low water mark equal to the
/// high one, a high one over the critical one, a high or low water mark of 0 and a floor of 100
/// are refused naming the key.
#[test]
fn the_water_marks_are_judged_at_their_edges() {
    let accepted = [
        cache_with("low_water  = 60", "low_water  = 84"),
        cache_with("high_water = 85", "high_water = 97"),
        cache_with("free_floor = 15", "free_floor = 99"),
        cache_with("free_floor = 15", "free_floor = 0"),
        cache_with("max_concurrent_fills = 6", "max_concurrent_fills = 1"),
        cache_with("max_concurrent_fills = 6", "max_concurrent_fills = 64"),
        cache_with("max_concurrent_keeps = 3", "max_concurrent_keeps = 1"),
        cache_with("max_concurrent_keeps = 3", "max_concurrent_keeps = 64"),
        cache_with("keep_wait  = \"45s\"", "keep_wait  = \"1ms\""),
        cache_with("keep_wait  = \"45s\"", "keep_wait  = \"5m\""),
        cache_with("keep_deadline = \"5m\"", "keep_deadline = \"45s\""),
        cache_with("keep_deadline = \"5m\"", "keep_deadline = \"2h\""),
    ];
    for text in accepted {
        let made = settings_from_text(&text, DEFAULTS);
        assert!(made.is_ok(), "{text}");
    }
    let refused = [
        (
            cache_with("low_water  = 60", "low_water  = 85"),
            "local_cache.low_water",
        ),
        (
            cache_with("high_water = 85", "high_water = 98"),
            "local_cache.high_water",
        ),
        (
            cache_with("high_water = 85", "high_water = 0"),
            "local_cache.high_water",
        ),
        (
            cache_with("low_water  = 60", "low_water  = 0"),
            "local_cache.low_water",
        ),
        (
            cache_with("free_floor = 15", "free_floor = 100"),
            "local_cache.free_floor",
        ),
        (
            cache_with("max_concurrent_fills = 6", "max_concurrent_fills = 0"),
            "local_cache.max_concurrent_fills",
        ),
        (
            cache_with("max_concurrent_fills = 6", "max_concurrent_fills = 65"),
            "local_cache.max_concurrent_fills",
        ),
        (
            cache_with("max_concurrent_keeps = 3", "max_concurrent_keeps = 0"),
            "local_cache.max_concurrent_keeps",
        ),
        (
            cache_with("max_concurrent_keeps = 3", "max_concurrent_keeps = 65"),
            "local_cache.max_concurrent_keeps",
        ),
        (
            cache_with("keep_wait  = \"45s\"", "keep_wait  = \"0s\""),
            "local_cache.keep_wait",
        ),
        (
            // A value that is not a duration is refused naming its key and the kind of value, and
            // without the value (the loop below checks that "soon" is not shown).
            cache_with("keep_wait  = \"45s\"", "keep_wait  = \"soon\""),
            "keep_wait: not a value this key takes (it is not shown); expected a duration such as",
        ),
        (
            cache_with("keep_deadline = \"5m\"", "keep_deadline = \"later\""),
            "keep_deadline: not a value this key takes (it is not shown); expected a duration such as",
        ),
        (
            cache_with("keep_deadline = \"5m\"", "keep_deadline = \"0s\""),
            "local_cache.keep_deadline",
        ),
        (
            cache_with("keep_deadline = \"5m\"", "keep_deadline = \"44s\""),
            "local_cache.keep_deadline",
        ),
    ];
    for (text, key) in refused {
        let why = refusal(&text);
        assert!(why.contains(key), "{key}: {why}");
        assert!(!why.contains("soon") && !why.contains("later"), "{why}");
    }
}

/// Asserts that a storage node refuses a policy that asks for a cache, with the reason.
#[test]
fn a_storage_node_refuses_a_cache_policy() {
    let policy = format!("{GOLDEN}\n[[policy]]\nprefix = \"a/\"\ncache = true\n");
    let why = refusal(&policy);
    assert!(why.contains("policy[0].cache"), "{why}");
    assert!(why.contains("keeps no cache"), "{why}");
}

/// Asserts that a build node's file takes an `oci` interface with the params of a storage node's,
/// each to the field it has, and that a file without one has no front.
#[test]
fn a_build_node_file_with_an_oci_interface_makes_the_front() {
    let oci = cache_of(CACHE).oci;
    assert_eq!(oci.listen, vec!["192.0.2.10:5000".parse().unwrap()]);
    assert_eq!(
        oci.tls_cert.as_deref(),
        Some(Path::new("/etc/tierstore-cache/oci.crt"))
    );
    assert_eq!(
        oci.tls_key.as_deref(),
        Some(Path::new("/etc/tierstore-cache/oci.key"))
    );
    assert_eq!(oci.prefix.as_deref(), Some("images/"));
    assert_eq!(oci.max_connections, Some(16));
    assert_eq!(oci.connection_lifetime, Some(Duration::from_secs(1800)));

    let plain = format!("{KEEPS_OCI}{}", oci_interface("127.0.0.1:5000", ""));
    let oci = cache_of(&plain).oci;
    assert_eq!(oci.listen, vec!["127.0.0.1:5000".parse().unwrap()]);
    assert_eq!((oci.tls_cert, oci.tls_key, oci.prefix), (None, None, None));
    assert_eq!((oci.max_connections, oci.connection_lifetime), (None, None));

    assert!(cache_of(LEAST).oci.listen.is_empty());
}

/// Asserts that a build node's interfaces take lists of addresses as a storage node's do, in the
/// `s3` and the `oci` entries, and are refused with the same messages when an address is twice.
#[test]
fn a_build_node_takes_lists_of_addresses() {
    let server = "[\"192.0.2.10:9400\", \"[::]:9400\"]";
    let oci = "[\"192.0.2.10:5000\", \"[::]:5000\"]";
    let text = CACHE.replacen("[\"192.0.2.10:9400\"]", server, 1).replacen(
        "[\"192.0.2.10:5000\"]",
        oci,
        1,
    );
    let cache = cache_of(&text);
    let wanted = |first: &str, second: &str| vec![first.parse().unwrap(), second.parse().unwrap()];
    assert_eq!(cache.front.listen, wanted("192.0.2.10:9400", "[::]:9400"));
    assert_eq!(cache.oci.listen, wanted("192.0.2.10:5000", "[::]:5000"));

    let twice = "[\"192.0.2.10:9400\", \"192.0.2.10:9400\"]";
    let why = refusal(&CACHE.replacen("[\"192.0.2.10:9400\"]", twice, 1));
    assert_eq!(
        why,
        "interface[0].params: the address 192.0.2.10:9400 is listed twice"
    );
    let twice = "[\"192.0.2.10:5000\", \"192.0.2.10:5000\"]";
    let why = refusal(&CACHE.replacen("[\"192.0.2.10:5000\"]", twice, 1));
    assert_eq!(
        why,
        "interface[1].params: the address 192.0.2.10:5000 is listed twice"
    );
}

/// Asserts that a build node's `oci` interface is refused exactly as a storage node's is: for each
/// case below the build node's message equals the storage node's and names the key, so a
/// certificate without a key, a key without a certificate, a count of zero and a key the entry
/// does not have are refused by the one set of rules.
#[test]
fn a_build_node_oci_interface_is_refused_as_a_storage_nodes_is() {
    let cases = [
        (
            ", tls_cert_file = \"/c\"",
            "interface[1].params.tls_key_file: it is needed with tls_cert_file",
        ),
        (
            ", tls_key_file = \"/k\"",
            "interface[1].params.tls_cert_file: it is needed with tls_key_file",
        ),
        (
            ", max_connections = 0",
            "interface[1].params.max_connections",
        ),
        (", bogus = 1", "bogus"),
    ];
    for (more, want) in cases {
        let entry = oci_interface("127.0.0.1:5000", more);
        let build = refusal(&format!("{LEAST}{entry}"));
        let storage = refusal(&format!("{MINIMAL}{entry}"));
        assert!(build.contains(want), "{more}: {build}");
        // A refusal of the syntax says where it is, and the two files are not laid out alike.
        assert_eq!(
            without_position(&build),
            without_position(&storage),
            "{more}"
        );
    }
}

/// Asserts that a rate reads as a size and `/s`, and is refused otherwise.
#[test]
fn a_rate_is_a_size_and_a_per_second() {
    let rate = |text: &str| serde_json::from_str::<Rate>(&format!("{text:?}"));
    assert_eq!(rate("50MiB/s").unwrap(), Rate(50 << 20));
    assert_eq!(rate("7B/s").unwrap(), Rate(7));
    for bad in ["50MiB", "x/s", "-1B/s", "/s", "1 B/s"] {
        let why = rate(bad).unwrap_err().to_string();
        assert!(why.contains("a rate such as"), "{bad}: {why}");
    }
    let why = serde_json::from_str::<Rate>("5").unwrap_err().to_string();
    assert!(why.contains("a rate such as"), "{why}");
}

/// Asserts that an `oci` front whose blobs no policy keeps is refused at load, with a message
/// that names `params.prefix` and says which policy decides: none covers the default prefix, the
/// policy that covers it has `cache = false`, and a given prefix is checked, not the default. A
/// policy that keeps the prefix (the default, and a given one) loads.
#[test]
fn an_oci_front_over_keys_the_cache_does_not_keep_is_refused() {
    let front = oci_interface("127.0.0.1:5000", "");
    let front = front.as_str();
    let none = refusal(&format!("{LEAST}\n{front}"));
    assert!(none.contains("params.prefix"), "{none}");
    assert!(none.contains("no [[policy]] covers"), "{none}");
    assert!(none.contains("\"oci/blobs/sha256/\""), "{none}");

    let off = "[[policy]]\nprefix = \"\"\ncache = false\n";
    let why = refusal(&format!("{LEAST}\n{off}\n{front}"));
    assert!(why.contains("params.prefix"), "{why}");
    assert!(why.contains("cache = false"), "{why}");

    let exact = "[[policy]]\nprefix = \"oci/\"\ncache = false\n";
    let why = refusal(&format!("{LEAST}\n{exact}\n{front}"));
    assert!(why.contains("params.prefix"), "{why}");
    assert!(why.contains("cache = false"), "{why}");

    // A policy prefix must end in `/` and the blobs are flat under `blobs/sha256/`, so no policy
    // can cover some of the blobs: the one that covers them all decides.
    let rest = "[[policy]]\nprefix = \"\"\ncache = true\n";
    let inside = "[[policy]]\nprefix = \"oci/blobs/sha256/0\"\ncache = false\n";
    let why = refusal(&format!("{LEAST}\n{rest}\n{inside}\n{front}"));
    assert!(why.contains("does not end in /"), "{why}");
    let outside = "[[policy]]\nprefix = \"other/\"\ncache = false\n";
    let loaded = cache_of(&format!("{LEAST}\n{rest}\n{outside}\n{front}"));
    assert!(!loaded.oci.listen.is_empty());

    let given = oci_interface("127.0.0.1:5000", ", prefix = \"layers/\"");
    let why = refusal(&format!("{KEEPS_OCI}\n{given}"));
    assert!(why.contains("\"layers/blobs/sha256/\""), "{why}");
    let layers = "[[policy]]\nprefix = \"layers/\"\ncache = true\n";
    let kept = cache_of(&format!("{KEEPS_OCI}\n{layers}\n{given}"));
    assert!(!kept.oci.listen.is_empty());
    let default = cache_of(&format!("{KEEPS_OCI}\n{front}"));
    assert!(!default.oci.listen.is_empty());
    // No front, no check: a file whose policies keep nothing under `oci/` loads as before.
    assert!(cache_of(LEAST).oci.listen.is_empty());
}

/// Asserts the `whole_fill_limit` key: a size that reaches `whole_limit`, and refused at zero and
/// above the budget, naming the key.
#[test]
fn the_whole_fill_limit_is_a_size_between_one_byte_and_the_budget() {
    let with = |limit: &str| {
        let key = format!("budget = 1000\nwhole_fill_limit = {limit}");
        LEAST.replacen("budget = 1000", &key, 1)
    };
    assert_eq!(cache_of(&with("1000")).cache.whole_limit, 1000);
    assert_eq!(cache_of(&with("1")).cache.whole_limit, 1);
    for bad in ["0", "1001"] {
        let why = refusal(&with(bad));
        assert!(why.contains("local_cache.whole_fill_limit"), "{bad}: {why}");
    }
}

/// The keys of `[local_cache]`, which the README of the packaging must document each in a row.
const LOCAL_CACHE_KEYS: [&str; 37] = [
    "fill_socket",
    "fill_socket_concurrency",
    "fill_socket_wait",
    "prefetch_concurrency",
    "prefetch_queue",
    "prefetch_queue_ttl",
    "hold_max",
    "hold_max_count",
    "hydrate_max_trees",
    "hydrate_max_blobs",
    "hydrate_max_bytes",
    "path",
    "layout",
    "budget",
    "high_water",
    "low_water",
    "critical",
    "free_floor",
    "max_concurrent_fills",
    "max_concurrent_keeps",
    "keep_wait",
    "keep_deadline",
    "write_through",
    "whole_fill_limit",
    "pin_limit",
    "cache",
    "scrub_rate",
    "scrub_min_round",
    "pass_interval",
    "lock_interval",
    "scrub_tick",
    "listing_calls",
    "forget_limit",
    "fill_chunk",
    "keep_queue_bytes",
    "revalidate_after",
    "head_before_put",
];

/// Asserts that the README of the packaging documents the build-node shape: its example loads,
/// every key of `[local_cache]` has a row of the table, and the list above is every key the table
/// has.
#[test]
fn the_readme_documents_the_build_node_file() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let readme = std::fs::read_to_string(root.join("../../packaging/README.md")).unwrap();
    let blocks = readme.split("```toml\n").skip(1);
    let example = blocks
        .into_iter()
        .find(|block| block.contains("[local_cache]"));
    let example = example.unwrap().split_once("\n```").unwrap().0;
    let cache = cache_of(example);
    assert_eq!(cache.cache.policies.len(), 3);
    for key in LOCAL_CACHE_KEYS {
        let wanted = format!("`local_cache.{key}`");
        let row = readme
            .lines()
            .find(|line| line.starts_with('|') && line.contains(&wanted));
        assert!(row.is_some(), "no row for {key}");
    }
    // The keys the table takes are the fields of its struct.
    let keys: BTreeSet<&str> = fields_of::<LocalCacheTable>().iter().copied().collect();
    let listed: BTreeSet<&str> = LOCAL_CACHE_KEYS.iter().copied().collect();
    assert_eq!(keys, listed);
}

/// Asserts which defect a build-node file with two is refused for: an interface's is named before
/// a table's (the interfaces are read first), and with the interface mended the `[local_cache]`
/// one is named.
#[test]
fn an_interface_defect_is_named_before_a_local_cache_one() {
    let oci = "\n[[interface]]\ntype = \"oci\"\nparams = { prefix = \"oci/\" }\n";
    let broken = LEAST.replacen(
        "budget = 1000",
        "budget = 1000\nmax_concurrent_fills = 0",
        1,
    );
    let why = refusal(&format!("{broken}{oci}"));
    assert_eq!(why, "interface[1].params: it needs port or addresses");
    let why = refusal(&format!("{broken}{}", oci_interface("127.0.0.1:5000", "")));
    assert!(why.contains("local_cache.max_concurrent_fills"), "{why}");
}

/// Asserts that the example in the header of `config_cache.rs` is a file the loader reads as a
/// build node's, serving the one loopback S3 address the header names and the registry front.
#[test]
fn the_example_in_the_module_header_loads() {
    let source = include_str!("config_cache.rs");
    let example: String = source
        .lines()
        .skip_while(|line| *line != "//! ```toml")
        .skip(1)
        .take_while(|line| *line != "//! ```")
        .map(|line| {
            line.strip_prefix("//!")
                .unwrap_or(line)
                .trim_start_matches(' ')
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(example.contains("[[interface]]"), "{example}");
    let cache = cache_of(&example);
    assert_eq!(cache.front.listen, vec!["127.0.0.1:9400".parse().unwrap()]);
    assert_eq!(cache.oci.listen, vec!["127.0.0.1:5000".parse().unwrap()]);
}
