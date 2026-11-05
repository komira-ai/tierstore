//! The configuration file: what a file makes, what it refuses, and that every flag that went has
//! a refusal that names its key.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::super::config_units::parse_size;
use super::{ConfigError, KEYS, PerRun, from_text, keys_for_flags, load, removed};
use crate::server::cli::{Cli, parse_duration};
use crate::server::config_testing::{DEFAULTS, GOLDEN, MINIMAL};

/// The message of the refusal of this file text.
fn refusal(text: &str) -> String {
    from_text(text, DEFAULTS).unwrap_err().to_string()
}

/// `GOLDEN` with `change` made in it: the text `from` replaced by `to`, which must be in it.
fn golden_with(from: &str, to: &str) -> String {
    assert!(GOLDEN.contains(from), "{from}");
    GOLDEN.replacen(from, to, 1)
}

/// `MINIMAL` with `from` replaced by `to`.
fn minimal_with(from: &str, to: &str) -> String {
    assert!(MINIMAL.contains(from), "{from}");
    MINIMAL.replacen(from, to, 1)
}

/// Asserts that the golden storage-node file makes the settings it says, key by key: the
/// addresses of its interfaces, the tiers, the key files and the front's params.
#[test]
fn a_storage_node_file_makes_the_settings_it_says() {
    let cli = from_text(GOLDEN, DEFAULTS).unwrap();
    assert_eq!(cli.listen, vec!["192.0.2.10:9300".parse().unwrap()]);
    assert_eq!(cli.proxy_id, "tierstore-1");
    assert_eq!(cli.bucket, "tierstore");
    assert_eq!(cli.region, "eu-west-1");
    assert_eq!(cli.hot, "s3:tierstore-hot");
    assert_eq!(cli.cold, "s3:tierstore-cold");
    assert_eq!(cli.hot_bytes, 500 << 30);
    assert_eq!((cli.high_water, cli.low_water, cli.critical), (85, 60, 97));
    assert_eq!(cli.hot_conditional_delete, Some(true));
    assert_eq!(cli.cold_conditional_delete, Some(true));
    assert_eq!(cli.read_only_key_file.len(), 2);
    assert_eq!(cli.max_request_bytes, 134_217_728);
    assert_eq!(cli.delete_concurrency, 4);
    assert_eq!(cli.put_permit_wait, Duration::from_secs(25));
    assert_eq!(cli.s3_request_timeout, Duration::from_secs(120));
    assert_eq!(cli.lease_ttl, Duration::from_secs(40));
    assert_eq!(cli.slot, None);
}

/// Asserts that a file written for the release before, which sets the two keys of the removed read
/// grant, still loads: the settings keep both keys (to be ignored with a warning at the start),
/// every other key is read as it was, and a file without them has none.
#[test]
fn a_file_with_the_keys_of_the_removed_read_grant_still_loads() {
    let old = golden_with(
        "presign_max_expires   = \"10m\"",
        "presign_max_expires   = \"10m\"\n\
         grant_key_files       = [\"/etc/tierstore/grant.key\"]\n\
         grant_max_lifetime    = \"5m\"",
    );
    let from_file = from_text(&old, DEFAULTS).unwrap();
    assert_eq!(
        from_file.grant_key_file,
        vec![PathBuf::from("/etc/tierstore/grant.key")]
    );
    assert_eq!(from_file.grant_max_lifetime, Some(Duration::from_secs(300)));
    assert_eq!(
        from_file.access_key_file,
        PathBuf::from("/etc/tierstore/access.key")
    );
    let without = from_text(GOLDEN, DEFAULTS).unwrap();
    assert!(without.grant_key_file.is_empty());
    assert_eq!(without.grant_max_lifetime, None);
}

/// Asserts that a file that gives the least a start needs gets, for every key it leaves out, the
/// documented default, and that the knobs of the run come from the run.
#[test]
fn a_key_that_is_left_out_has_its_default() {
    let cli = from_text(MINIMAL, DEFAULTS).unwrap();
    assert_eq!(cli.region, "us-east-1");
    assert_eq!((cli.high_water, cli.low_water, cli.critical), (80, 70, 95));
    assert_eq!(cli.copy_parallel, 16);
    assert_eq!(cli.lease_ttl, Duration::from_secs(30));
    assert_eq!(cli.usage_stale_after, Duration::from_secs(120));
    assert_eq!(cli.max_request_bytes, 256 << 20);
    assert_eq!(cli.max_concurrent_puts, 32);
    assert_eq!(cli.delete_concurrency, 16);
    assert_eq!(cli.put_permit_wait, Duration::from_secs(15));
    assert_eq!(cli.put_min_bytes_per_sec, 1024);
    assert_eq!(cli.put_rate_grace, Duration::from_secs(30));
    assert_eq!(cli.body_idle_timeout, Duration::from_secs(30));
    assert_eq!(cli.aws_chunked_max_line_bytes, 1024);
    assert_eq!(cli.s3_request_timeout, Duration::from_secs(60));
    assert_eq!(cli.backend_check_wait, None);
    assert_eq!(cli.hot_conditional_delete, None);
    assert_eq!(cli.cold_conditional_delete, None);
    assert_eq!(cli.presign_max_expires, Duration::from_secs(900));
    assert_eq!(cli.grant_max_lifetime, None);
    assert!(cli.grant_key_file.is_empty());
    assert!(cli.oci_listen.is_empty());
    assert!(cli.skip_backend_check.is_empty());
    assert_eq!(DEFAULTS.drain_lb_wait, Duration::from_secs(10));
    assert_eq!(DEFAULTS.drain_deadline, Duration::from_secs(900));
    assert_eq!(cli.drain_lb_wait, DEFAULTS.drain_lb_wait);
    assert_eq!(cli.drain_deadline, DEFAULTS.drain_deadline);
    let slow = PerRun {
        drain_lb_wait: Duration::from_secs(1),
        drain_deadline: Duration::from_secs(2),
    };
    let with_run = from_text(MINIMAL, slow).unwrap();
    assert_eq!(with_run.drain_lb_wait, Duration::from_secs(1));
    assert_eq!(with_run.drain_deadline, Duration::from_secs(2));
}

/// Asserts that the tiers of the other kinds are made as the specs `dir:<path>` and `memory`, and
/// that a size is a number or a string with a unit.
#[test]
fn a_directory_tier_and_a_size_in_bytes_are_read() {
    let text = minimal_with(
        "kind   = \"memory\"\nbudget = 1000",
        "kind = \"dir\"\npath = \"/var/hot\"\nbudget = \"2MiB\"",
    );
    let cli = from_text(&text, DEFAULTS).unwrap();
    assert_eq!(cli.hot, "dir:/var/hot");
    assert_eq!(cli.hot_bytes, 2 << 20);
    assert_eq!(cli.cold, "memory");
}

/// Asserts that a file is read from disk: a file that is not there is a refusal that names it,
/// and a file with a fault is a refusal that names the file and the fault.
#[test]
fn a_file_is_read_from_its_path_and_its_refusals_name_it() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-config-load");
    let good = dir.path().join("tierstore.toml");
    std::fs::write(&good, GOLDEN).unwrap();
    assert_eq!(
        load(&good, DEFAULTS).unwrap().into_storage(),
        Some(from_text(GOLDEN, DEFAULTS).unwrap())
    );
    let missing = load(&dir.path().join("none.toml"), DEFAULTS).unwrap_err();
    assert!(matches!(missing, ConfigError::Read { .. }), "{missing}");
    assert!(missing.to_string().contains("none.toml"), "{missing}");
    let bad = dir.path().join("bad.toml");
    std::fs::write(&bad, golden_with("[server]", "[server]\nslot = \"x\"")).unwrap();
    let why = load(&bad, DEFAULTS).unwrap_err().to_string();
    assert!(why.contains("bad.toml"), "{why}");
    assert!(why.contains("slot"), "{why}");
}

/// Asserts that a key or a table this release does not have, in any table of the file, does not
/// refuse the file: it loads to the settings the file makes without the key, so that a file
/// written by a later release starts here. A key it has, given a value of the wrong type, is still
/// refused with its name, and so are a file that is not TOML and one that lacks a needed key.
#[test]
fn an_unknown_key_or_table_is_ignored_and_a_known_one_is_still_checked() {
    let cases = [
        golden_with("[server]", "[server]\nbogus = 1"),
        golden_with("[hot_store]", "[hot_store]\nbogus = 1"),
        golden_with("[cold_store]", "[cold_store]\nbogus = 1"),
        format!("{GOLDEN}\n[bogus]\nx = 1\n"),
        format!("{GOLDEN}\n[[policy]]\nprefix = \"a/\"\nbogus = 1\n"),
        format!("bogus = 1\n{GOLDEN}"),
    ];
    let plain = from_text(GOLDEN, DEFAULTS).unwrap();
    for text in cases {
        assert_eq!(from_text(&text, DEFAULTS).unwrap(), plain, "{text}");
    }
    let why = refusal(&golden_with(
        "addresses = [\"192.0.2.10:9300\"]",
        "addresses = [\"nowhere\"]",
    ));
    assert!(
        why.contains("params: invalid socket address syntax"),
        "{why}"
    );
    let syntax = from_text("this is = not toml [", DEFAULTS);
    assert!(matches!(syntax, Err(ConfigError::Syntax { .. })));
    let lines: Vec<&str> = GOLDEN
        .lines()
        .filter(|line| !line.starts_with("access_key_file"))
        .collect();
    let why = refusal(&lines.join("\n"));
    assert!(why.contains("access_key_file"), "{why}");
}

/// Asserts that a key that holds a secret is refused with its table and name, wherever it is
/// (in a table, in an array of tables), while a key that names a file is not: a key file is a
/// path.
#[test]
fn a_secret_in_the_file_is_refused_and_a_path_to_one_is_not() {
    let cases = [
        (
            "[server]",
            "[server]\nsecret_access_key = \"x\"",
            "server.secret_access_key",
        ),
        (
            "[server]",
            "[server]\naccess_key_id = \"x\"",
            "server.access_key_id",
        ),
        (
            "[hot_store]",
            "[hot_store]\npassword = \"x\"",
            "hot_store.password",
        ),
        (
            "[cold_store]",
            "[cold_store]\nsession_token = \"x\"",
            "cold_store.session_token",
        ),
        (
            "[cold_store]",
            "[cold_store]\nsigning_key = \"x\"",
            "cold_store.signing_key",
        ),
        (
            "[cold_store]",
            "[cold_store]\naccess_key = \"x\"",
            "cold_store.access_key",
        ),
    ];
    for (from, to, key) in cases {
        let why = refusal(&golden_with(from, to));
        assert!(why.contains(key), "{why}");
        assert!(why.contains("never in the file"), "{why}");
    }
    let policy = format!("{GOLDEN}\n[[policy]]\nprefix = \"a/\"\ntoken = \"x\"\n");
    assert!(refusal(&policy).contains("policy.token"));
    assert!(from_text(GOLDEN, DEFAULTS).is_ok());
}

/// Asserts that a key whose name is a secret by another spelling is refused as one (any case, and
/// the words `key`, `credentials`, `passphrase`, `apikey`, `psk`), without its value.
#[test]
fn a_secret_by_another_name_is_refused_without_its_value() {
    let names = [
        "Secret_Key",
        "key",
        "credentials",
        "passphrase",
        "ApiKey",
        "psk",
    ];
    for name in names {
        let why = refusal(&golden_with(
            "[server]",
            &format!("[server]\n{name} = \"SENTINEL\""),
        ));
        assert!(why.contains(name), "{why}");
        assert!(why.contains("never in the file"), "{why}");
        assert!(!why.contains("SENTINEL"), "{why}");
    }
}

/// Asserts that no message about a fault in the file repeats the line or the value of the key:
/// a line that is not TOML, a
/// value of the wrong type, a size, a duration, a kind and a skipped check that are not valid. Each
/// still says which key (and, but for a skipped check, where).
#[test]
fn a_fault_in_the_file_is_reported_without_the_value() {
    let cases = [
        (
            golden_with("[server]", "[server]\nbroken = SENTINEL"),
            "line ",
        ),
        (
            golden_with(
                "max_concurrent_puts   = 8",
                "max_concurrent_puts = \"SENTINEL\"",
            ),
            "max_concurrent_puts",
        ),
        (
            golden_with("budget             = \"500GiB\"", "budget = \"SENTINEL\""),
            "budget",
        ),
        (
            golden_with(
                "presign_max_expires   = \"10m\"",
                "presign_max_expires = \"SENTINEL\"",
            ),
            "presign_max_expires",
        ),
        (
            golden_with("presign_max_expires   = \"10m\"", "presign_max_expires = 5"),
            "presign_max_expires",
        ),
        (
            golden_with("kind               = \"s3\"", "kind = \"SENTINEL\""),
            "kind",
        ),
    ];
    for (text, want) in cases {
        let why = refusal(&text);
        assert!(why.contains(want), "{want}: {why}");
        assert!(!why.contains("SENTINEL"), "{want}: {why}");
    }
}

/// Asserts that a fault inside a multi-line string names no "key" taken from the string's text:
/// the text before an `=` on such a line is part of a value (perhaps a pasted secret), so it is
/// neither shown as the key nor repeated.
#[test]
fn a_fault_inside_a_multi_line_string_shows_nothing_of_the_string() {
    let text = golden_with(
        "[server]",
        "[server]\nnote = \"\"\"\nabcdef=SENTINEL \\q\n\"\"\"",
    );
    let why = refusal(&text);
    assert!(why.starts_with("line "), "{why}");
    assert!(!why.contains("abcdef"), "{why}");
    assert!(!why.contains("SENTINEL"), "{why}");
}

/// Asserts that `[local_cache]` along with `[hot_store]` is read as a node with a cache, which
/// needs what a cache needs (here a policy that keeps something, the first thing it checks), and
/// that a file with none of the three shapes' tables (`[local_cache]` alone, or a storage node's
/// file missing `[hot_store]` or `[cold_store]`) is refused for the shape.
#[test]
fn a_hot_store_with_a_local_cache_needs_what_a_cache_needs_and_other_shapes_are_refused() {
    let cache = format!("{GOLDEN}\n[local_cache]\npath = \"/var/cache\"\n");
    assert_eq!(
        refusal(&cache),
        "policy: no policy has cache = true, so nothing would be kept"
    );
    let only_cache = minimal_with(
        "[hot_store]\nkind   = \"memory\"\nbudget = 1000\n\n[cold_store]\nkind = \"memory\"\n",
        "[local_cache]\npath = \"/c\"\n",
    );
    assert!(refusal(&only_cache).contains("[local_cache] and [cold_store]"));
    let no_cold = minimal_with("[cold_store]\nkind = \"memory\"\n", "");
    let no_hot = minimal_with("[hot_store]\nkind   = \"memory\"\nbudget = 1000\n", "");
    for text in [no_cold, no_hot] {
        let why = refusal(&text);
        assert!(why.contains("both [hot_store] and [cold_store]"), "{why}");
    }
}

/// Asserts that a key of a tier is refused when it does not fit the tier's kind (a path for a
/// memory tier, an endpoint for a directory, a path for an S3 tier), and when the kind needs a
/// key that is not there.
#[test]
fn the_keys_of_a_tier_must_fit_its_kind() {
    let hot = "kind   = \"memory\"\nbudget = 1000";
    let cases = [
        (
            "kind = \"memory\"\nendpoint = \"http://x\"\nbudget = 1",
            "hot_store.endpoint",
        ),
        (
            "kind = \"memory\"\npath = \"/x\"\nbudget = 1",
            "hot_store.path",
        ),
        (
            "kind = \"memory\"\nbucket = \"b\"\nbudget = 1",
            "hot_store.bucket",
        ),
        ("kind = \"dir\"\nbudget = 1", "hot_store.path"),
        (
            "kind = \"dir\"\npath = \"/x\"\nregion = \"r\"\nbudget = 1",
            "hot_store.region",
        ),
        (
            "kind = \"dir\"\npath = \"/x\"\nbucket = \"b\"\nbudget = 1",
            "hot_store.bucket",
        ),
        (
            "kind = \"s3\"\nendpoint = \"http://x\"\ncredentials_file = \"/k\"\nbudget = 1",
            "hot_store.bucket",
        ),
        (
            "kind = \"s3\"\nbucket = \"b\"\ncredentials_file = \"/k\"\nbudget = 1",
            "hot_store.endpoint",
        ),
        (
            "kind = \"s3\"\nbucket = \"b\"\nendpoint = \"http://x\"\nbudget = 1",
            "hot_store.credentials_file",
        ),
        (
            "kind = \"s3\"\nbucket = \"b\"\npath = \"/x\"\nbudget = 1",
            "hot_store.path",
        ),
        ("kind = \"memory\"", "hot_store.budget"),
        // The refused value is never shown; the key and the values it takes are.
        (
            "kind = \"tape\"\nbudget = 1",
            "kind: not a value this key takes",
        ),
    ];
    for (table, key) in cases {
        let why = refusal(&minimal_with(hot, table));
        assert!(why.contains(key), "{table}: {why}");
    }
}

/// Asserts that the keys only one tier has are refused in the other, and that a check to skip is
/// `CHECK=REASON` with a check that can be skipped.
#[test]
fn the_keys_only_one_tier_has_are_refused_in_the_other() {
    let cold = minimal_with(
        "[cold_store]\nkind = \"memory\"",
        "[cold_store]\nkind = \"memory\"\nlease_ttl = \"1s\"",
    );
    assert!(refusal(&cold).contains("cold_store.lease_ttl"));
    let every = [
        "budget = 1",
        "high_water = 1",
        "low_water = 1",
        "critical = 1",
        "copy_parallel = 1",
        "lock_file = \"/var/lib/tierstore/instance.lock\"",
        "usage_stale_after = \"1s\"",
        "heartbeat = \"1s\"",
        "departure = \"1s\"",
        "deadline_margin = \"1s\"",
        "journal_linger = \"1s\"",
        "journal_retention = \"1s\"",
        "reconcile_interval = \"1s\"",
        "registration_retention = \"1s\"",
    ];
    for line in every {
        let text = minimal_with(
            "[cold_store]\nkind = \"memory\"",
            &format!("[cold_store]\nkind = \"memory\"\n{line}"),
        );
        let key = line.split(' ').next().unwrap();
        assert!(
            refusal(&text).contains(&format!("cold_store.{key}")),
            "{line}"
        );
    }
    let hot = minimal_with("budget = 1000", "budget = 1000\nskip_checks = []");
    assert!(refusal(&hot).contains("hot_store.skip_checks"));
}

/// Asserts that the values the flags bound are bound in the file too: a water mark outside 1 to
/// 100, a budget, a count or a limit of zero, a duration that is not one, a size that is not
/// one, and a negative size.
#[test]
fn a_value_out_of_range_is_refused_with_its_key() {
    let cases = [
        ("budget = 1000", "budget = 0", "hot_store.budget"),
        ("budget = 1000", "budget = -1", "negative"),
        ("budget = 1000", "budget = true", "a number of bytes"),
        ("budget = 1000", "budget = \"lots\"", "not a size"),
        ("budget = 1000", "budget = \"99999999999TiB\"", "not a size"),
        ("budget = 1000", "budget = \"5\"", "not a size"),
        ("budget = 1000", "budget = \"5PiB\"", "not a size"),
        (
            "budget = 1000",
            "budget = 1000\nhigh_water = 0",
            "hot_store.high_water",
        ),
        (
            "budget = 1000",
            "budget = 1000\nlow_water = 101",
            "hot_store.low_water",
        ),
        (
            "budget = 1000",
            "budget = 1000\ncritical = 0",
            "hot_store.critical",
        ),
        (
            "budget = 1000",
            "budget = 1000\ncopy_parallel = 0",
            "hot_store.copy_parallel",
        ),
        (
            "budget = 1000",
            "budget = 1000\nlease_ttl = \"soon\"",
            "lease_ttl: not a value this key takes (it is not shown); expected a duration such as",
        ),
        // A lock file is an absolute path that names a file: two processes of one node must
        // lock one file, whatever their working directories are.
        (
            "budget = 1000",
            "budget = 1000\nlock_file = \"instance.lock\"",
            "hot_store.lock_file: \"instance.lock\" must be an absolute path",
        ),
        (
            "budget = 1000",
            "budget = 1000\nlock_file = \"\"",
            "hot_store.lock_file: \"\" must be an absolute path",
        ),
        (
            "budget = 1000",
            "budget = 1000\nlock_file = \"/\"",
            "hot_store.lock_file: \"/\" must name a file",
        ),
        (
            "\"tier-bucket\"",
            "\"tier-bucket\"\nmax_request_bytes = 0",
            "server.max_request_bytes",
        ),
        (
            "\"tier-bucket\"",
            "\"tier-bucket\"\nmax_concurrent_puts = 0",
            "server.max_concurrent_puts",
        ),
        (
            "\"tier-bucket\"",
            "\"tier-bucket\"\ndelete_concurrency = 0",
            "server.delete_concurrency",
        ),
        (
            "\"tier-bucket\"",
            "\"tier-bucket\"\nbody_idle_timeout = \"30\"",
            "body_idle_timeout: not a value this key takes (it is not shown); expected a duration such as",
        ),
    ];
    for (from, to, want) in cases {
        let why = refusal(&minimal_with(from, to));
        assert!(why.contains(want), "{to}: {why}");
    }
}

/// `MINIMAL` with an `oci` interface that has `params` after it.
fn minimal_with_oci(params: &str) -> String {
    format!("{MINIMAL}\n[[interface]]\ntype = \"oci\"\nparams = {{ {params} }}\n")
}

/// Asserts that the `oci` interface of the golden file makes the front's settings, every param
/// given; that an interface with an address alone leaves the other settings unset so that the
/// front's own defaults apply; and that a file without the interface opens no front.
#[test]
fn an_oci_interface_makes_the_front_it_says() {
    let all = from_text(GOLDEN, DEFAULTS).unwrap();
    assert_eq!(all.oci_listen, vec!["192.0.2.10:5000".parse().unwrap()]);
    assert_eq!(
        all.oci_tls_cert.as_deref(),
        Some(Path::new("/etc/tierstore/oci.crt"))
    );
    assert_eq!(
        all.oci_tls_key.as_deref(),
        Some(Path::new("/etc/tierstore/oci.key"))
    );
    assert_eq!(all.oci_prefix.as_deref(), Some("images/"));
    assert_eq!(all.oci_max_connections, Some(16));
    assert_eq!(all.oci_connection_lifetime, Some(Duration::from_secs(1800)));

    let alone = from_text(&minimal_with_oci("port = 5000"), DEFAULTS).unwrap();
    assert_eq!(alone.oci_listen, vec!["0.0.0.0:5000".parse().unwrap()]);
    assert_eq!(alone.oci_tls_cert, None);
    assert_eq!(alone.oci_prefix, None);
    assert_eq!(alone.oci_max_connections, None);
    assert_eq!(alone.oci_connection_lifetime, None);

    let none = from_text(MINIMAL, DEFAULTS).unwrap();
    assert!(none.oci_listen.is_empty());
}

/// Asserts that a refusal the start makes about an `--oci-*` flag names the key of the file.
#[test]
fn a_message_about_the_oci_flags_names_their_keys() {
    let message = "--oci-tls-cert and --oci-tls-key go together; --oci-prefix, \
                   --oci-max-connections, --oci-connection-lifetime";
    assert_eq!(
        keys_for_flags(message),
        "params.tls_cert_file and params.tls_key_file go together; params.prefix, \
         params.max_connections, params.connection_lifetime"
    );
}

/// Asserts that on a storage node a policy is read and checked and has no effect: the settings
/// with policies are the settings without; a prefix twice, a prefix inside another, a prefix that
/// does not end in `/`, one under `_journal/` or `_tiering/`, and one that asks for a cache are
/// refused.
#[test]
fn a_policy_is_checked_and_has_no_effect_on_a_storage_node() {
    let policies = "\n[[policy]]\nprefix = \"cas/b/\"\n\n[[policy]]\nprefix = \"cas/d/\"\ncache = false\n\n[[policy]]\nprefix = \"\"\n";
    let with = from_text(&format!("{GOLDEN}{policies}"), DEFAULTS).unwrap();
    assert_eq!(with, from_text(GOLDEN, DEFAULTS).unwrap());
    // An empty prefix (all the rest) with another after it, in either order, is no overlap.
    let rest_first =
        format!("{GOLDEN}\n[[policy]]\nprefix = \"\"\n\n[[policy]]\nprefix = \"a/\"\n");
    assert_eq!(
        from_text(&rest_first, DEFAULTS).unwrap(),
        from_text(GOLDEN, DEFAULTS).unwrap()
    );
    let refused = [
        (
            "prefix = \"a/\"\n\n[[policy]]\nprefix = \"a/\"",
            "also the prefix of policy[0]",
        ),
        (
            "prefix = \"a/\"\n\n[[policy]]\nprefix = \"a/b/\"",
            "overlaps",
        ),
        (
            "prefix = \"a/b/\"\n\n[[policy]]\nprefix = \"a/\"",
            "overlaps",
        ),
        (
            "prefix = \"\"\n\n[[policy]]\nprefix = \"\"",
            "also the prefix",
        ),
        ("prefix = \"cas\"", "does not end in /"),
        ("prefix = \"_journal/\"", "keeps for itself"),
        ("prefix = \"_tiering/selftest/\"", "keeps for itself"),
        ("prefix = \"a/\"\ncache = true", "policy[0].cache"),
    ];
    for (body, want) in refused {
        let why = refusal(&format!("{GOLDEN}\n[[policy]]\n{body}\n"));
        assert!(why.contains(want), "{body}: {why}");
    }
    let why = refusal(&format!(
        "{GOLDEN}\n[[policy]]\nprefix = \"a/\"\n\n[[policy]]\nprefix = \"_journal/\"\n"
    ));
    assert!(why.contains("policy[1].prefix"), "{why}");
}

/// Asserts that sizes and durations read as the docs say.
#[test]
fn sizes_and_durations_are_read() {
    assert_eq!(parse_size("500GiB"), Ok(500 << 30));
    assert_eq!(parse_size("3B"), Ok(3));
    assert_eq!(parse_size("2KiB"), Ok(2048));
    assert_eq!(parse_size("1TiB"), Ok(1 << 40));
    assert!(parse_size("").is_err());
    assert_eq!(parse_duration("0s"), Ok(Duration::ZERO));
    assert_eq!(parse_duration("15m"), Ok(Duration::from_secs(900)));
    assert_eq!(parse_duration("2h"), Ok(Duration::from_secs(7200)));
    assert_eq!(parse_duration("1500ms"), Ok(Duration::from_millis(1500)));
}

/// Asserts that the key table is well formed: a flag at most once, every flag with at least one
/// key, each key of a table of the file (or a param of an interface) and one name deep.
#[test]
fn the_key_table_is_well_formed() {
    let keyed: Vec<&str> = KEYS.iter().map(|(flag, _)| *flag).collect();
    let unique: BTreeSet<&str> = keyed.iter().copied().collect();
    assert_eq!(unique.len(), keyed.len(), "a flag twice in the key table");
    for (flag, keys) in KEYS {
        assert!(!keys.is_empty(), "--{flag}");
        for key in *keys {
            let (table, name) = key.split_once('.').unwrap();
            assert!(
                ["server", "hot_store", "cold_store", "params"].contains(&table),
                "{key}"
            );
            assert!(!name.is_empty(), "{key}");
            assert!(!name.contains('.'), "{key}");
        }
    }
}

/// Asserts that each flag the data path had is refused with a message that names it and the key
/// that replaces it: the flags of the key table by their first key, `--listen` and `--oci-listen`
/// by the interface that replaces them, `--print-config` by the file; and that a name the data
/// path never had is none of this.
#[test]
fn a_flag_that_went_is_refused_naming_its_key() {
    for (flag, keys) in KEYS {
        let why = removed(flag).unwrap();
        assert!(why.starts_with(&format!("--{flag} was removed:")), "{why}");
        assert!(why.contains(keys.first().unwrap()), "{flag}: {why}");
    }
    let hot = removed("hot-bytes").unwrap();
    assert_eq!(
        hot,
        "--hot-bytes was removed: the file given with --config holds it; set hot_store.budget"
    );
    let oci = removed("oci-prefix").unwrap();
    assert!(
        oci.ends_with("set params.prefix of an oci interface"),
        "{oci}"
    );
    let listen = removed("listen").unwrap();
    assert!(
        listen.contains("[[interface]] with type = \"s3\""),
        "{listen}"
    );
    let oci_listen = removed("oci-listen").unwrap();
    assert!(oci_listen.contains("type = \"oci\""), "{oci_listen}");
    let print = removed("print-config").unwrap();
    assert!(print.starts_with("--print-config was removed:"), "{print}");
    for kept in [
        "config",
        "check-config",
        "capabilities",
        "drain-lb-wait",
        "no-such",
    ] {
        assert_eq!(removed(kept), None, "{kept}");
    }
}

/// Asserts that a refusal the flags' checks make, in a start from a file, names the keys: the
/// longer flag name is replaced before a shorter one it begins with.
#[test]
fn a_message_about_flags_names_keys_for_a_file() {
    let flags = "--low-water must be below --high-water";
    assert_eq!(
        keys_for_flags(flags),
        "hot_store.low_water must be below hot_store.high_water"
    );
    let message = "--hot-credentials-file: --hot \"s3:b\" needs --hot-endpoint, not --hot-bytes";
    let keys = "hot_store.credentials_file: hot_store.kind \"s3:b\" needs hot_store.endpoint, not hot_store.budget";
    assert_eq!(keys_for_flags(message), keys);
    assert_eq!(
        keys_for_flags("opening the proxy: no flags here"),
        "opening the proxy: no flags here"
    );
}

/// Asserts that `server.slot` makes the slot (a port, 1 to 65535), that a file without it has
/// none, and that zero, a number past a port, a string and a negative number are refused with
/// the key named.
#[test]
fn the_slot_key_is_a_port() {
    let with = |value: &str| minimal_with("[server]", &format!("[server]\nslot = {value}"));
    let cli = from_text(&with("9301"), DEFAULTS).unwrap();
    assert_eq!(cli.slot, Some(9301));
    assert_eq!(from_text(MINIMAL, DEFAULTS).unwrap().slot, None);
    assert_eq!(
        from_text(&with("65535"), DEFAULTS).unwrap().slot,
        Some(65535)
    );
    for bad in ["0", "65536", "-1", "\"9301\"", "true"] {
        let why = refusal(&with(bad));
        assert!(why.contains("slot"), "{bad}: {why}");
    }
    assert!(refusal(&with("0")).contains("server.slot"));
}

/// Asserts that a file written for the releases before the entries, which sets the eight retired
/// keys of `[hot_store]`, still loads and makes the settings the file without them makes: the
/// keys are accepted and their values are not used.
#[test]
fn a_file_with_the_retired_journal_keys_still_loads_and_means_what_it_meant_without_them() {
    let old = golden_with(
        "lease_ttl          = \"40s\"",
        "lease_ttl          = \"40s\"\n\
         journal_linger     = \"50ms\"\n\
         journal_retention  = \"12h\"\n\
         reconcile_interval = \"4s\"\n\
         registration_retention = \"20m\"\n\
         journal_batch_entries = 8\n\
         journal_page_size = 20\n\
         copy_rounds = 5\n\
         delete_rounds = 6",
    );
    let with_keys = from_text(&old, DEFAULTS).unwrap();
    let without = from_text(GOLDEN, DEFAULTS).unwrap();
    assert_eq!(with_keys, without);
}

/// Asserts that `retired_keys` names, in file order of the table, each retired key a `[hot_store]`
/// sets and no other key.
#[test]
fn retired_keys_names_each_retired_key_a_hot_store_sets() {
    let table: super::TierTable = toml::from_str(
        "kind = \"memory\"\nlease_ttl = \"1s\"\njournal_linger = \"1s\"\n\
         journal_retention = \"1s\"\nreconcile_interval = \"1s\"\n\
         registration_retention = \"1s\"\njournal_batch_entries = 1\njournal_page_size = 1\n\
         copy_rounds = 1\ndelete_rounds = 1",
    )
    .unwrap();
    assert_eq!(
        super::retired_keys(&table),
        [
            "hot_store.journal_linger",
            "hot_store.journal_retention",
            "hot_store.reconcile_interval",
            "hot_store.registration_retention",
            "hot_store.journal_batch_entries",
            "hot_store.journal_page_size",
            "hot_store.copy_rounds",
            "hot_store.delete_rounds",
        ]
    );
    let none: super::TierTable = toml::from_str("kind = \"memory\"\nlease_ttl = \"1s\"").unwrap();
    assert!(super::retired_keys(&none).is_empty());
}

/// Asserts that a storage node's settings are made of the text with `Cli::from_config_text`, and
/// that the file of a build node is refused there with the shape named.
#[test]
fn the_settings_of_a_storage_node_are_made_of_the_text() {
    assert_eq!(
        Cli::from_config_text(MINIMAL).unwrap(),
        from_text(MINIMAL, DEFAULTS).unwrap()
    );
    let cache = "[[interface]]\ntype = \"s3\"\nparams = { port = 9300 }\n\n[server]\nproxy_id = \"c\"\nbucket = \"b\"\naccess_key_file = \"k\"\n\n[local_cache]\npath = \"/c\"\nbudget = 1\n\n[cold_store]\nkind = \"s3\"\nbucket = \"b\"\nendpoint = \"http://x\"\ncredentials_file = \"/k\"\n\n[[policy]]\nprefix = \"a/\"\ncache = true\n";
    let why = Cli::from_config_text(cache).unwrap_err().to_string();
    assert!(why.contains("the file of a build node"), "{why}");
}

/// Asserts that `server.min_secret_bytes`, a number whose name holds the word `secret`, is read
/// as a setting and not refused as an inline secret, while the same name in any other table, or
/// at the top, still is, and a value that is not a count of bytes is refused by its key.
#[test]
fn the_minimum_secret_length_is_a_setting_and_no_other_key_of_that_name_is() {
    let loaded = from_text(
        &golden_with("min_secret_bytes      = 24", "min_secret_bytes = 7"),
        DEFAULTS,
    )
    .unwrap();
    assert_eq!(loaded.min_secret_bytes, 7);
    for table in ["[hot_store]", "[cold_store]"] {
        let why = refusal(&golden_with(
            table,
            &format!("{table}\nmin_secret_bytes = 7"),
        ));
        assert!(
            why.contains(&format!("{}.min_secret_bytes", &table[1..table.len() - 1])),
            "{why}"
        );
        assert!(why.contains("never in the file"), "{why}");
    }
    let why = refusal(&format!("min_secret_bytes = 7\n{GOLDEN}"));
    assert!(why.contains("never in the file"), "{why}");
    let why = refusal(&golden_with(
        "min_secret_bytes      = 24",
        "min_secret_bytes = \"SENTINEL\"",
    ));
    let line = GOLDEN
        .lines()
        .position(|line| line.starts_with("min_secret_bytes"))
        .unwrap()
        + 1;
    assert_eq!(
        why,
        format!(
            "line {line}, column 20: min_secret_bytes: not a value this key takes (it is not \
             shown); expected u32"
        )
    );
}
