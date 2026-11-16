//! The file of a node over the cold tier alone: what it makes, and what it refuses.

use std::path::PathBuf;

use super::super::{from_text, settings_from_text};
use crate::server::config_testing::DEFAULTS;

/// A node over the cold tier alone: an S3 interface, the server's keys, a hand-off socket and
/// one S3 cold store reached through two endpoints.
const COLD: &str = r#"
[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:9300"] }

[server]
proxy_id        = "front-1"
bucket          = "tier-bucket"
access_key_file = "/etc/tierstore/access.key"
handoff_socket  = "/run/tierstore/handoff.sock"

[cold_store]
kind             = "s3"
bucket           = "tierstore-cold"
endpoints        = ["http://127.0.0.1:30900", "http://127.0.0.1:30901"]
credentials_file = "/etc/tierstore/cold.credentials"
"#;

/// The message the settings of `text` are refused with.
fn refusal(text: &str) -> String {
    settings_from_text(text, DEFAULTS).unwrap_err().to_string()
}

/// `COLD` with `from` replaced by `to`, which must be in it.
fn cold_with(from: &str, to: &str) -> String {
    assert!(COLD.contains(from), "{from}");
    COLD.replacen(from, to, 1)
}

/// Asserts that a file with `[cold_store]` alone makes the settings of a node over the cold tier,
/// key by key: the S3 front's address and bucket, the cold store's bucket, endpoints and
/// credentials, the hand-off socket, and no OCI front.
#[test]
fn a_file_with_the_cold_store_alone_makes_a_node_over_the_cold_tier() {
    let settings = settings_from_text(COLD, DEFAULTS).unwrap();
    let cold = settings.into_cold().expect("a node over the cold tier");
    assert_eq!(cold.front.listen, vec!["127.0.0.1:9300".parse().unwrap()]);
    assert_eq!(cold.front.bucket, "tier-bucket");
    assert_eq!(cold.cold.spec, "s3:tierstore-cold");
    assert_eq!(
        cold.cold.endpoint.as_deref(),
        Some("http://127.0.0.1:30900")
    );
    assert_eq!(cold.cold.more_endpoints, ["http://127.0.0.1:30901"]);
    assert_eq!(
        cold.cold.credentials_file,
        Some(PathBuf::from("/etc/tierstore/cold.credentials"))
    );
    let handoff = cold.handoff.expect("the hand-off the file names");
    assert_eq!(handoff.socket, PathBuf::from("/run/tierstore/handoff.sock"));
    assert!(cold.oci.listen.is_empty());
    assert!(!cold.s3_request_timeout.is_zero());
}

/// Asserts that a `[[policy]]` with `cache = false` is taken (it keeps nothing, and there is
/// nothing to keep in), and one with `cache = true` is refused with the exact message: the node
/// has no cache.
#[test]
fn a_policy_that_keeps_is_refused_and_one_that_does_not_is_taken() {
    let passing = format!("{COLD}\n[[policy]]\nprefix = \"cas/\"\ncache = false\n");
    assert!(
        settings_from_text(&passing, DEFAULTS)
            .unwrap()
            .into_cold()
            .is_some()
    );
    let keeping = format!("{COLD}\n[[policy]]\nprefix = \"cas/\"\ncache = true\n");
    assert_eq!(
        refusal(&keeping),
        "policy[0].cache: a node without [local_cache] keeps no cache: give it [local_cache]"
    );
}

/// Asserts that the keys and tables that do nothing on a node over the cold tier are refused,
/// each with the exact message that names it: a key only `[hot_store]` has, given in
/// `[cold_store]`; `cold_store.skip_checks`; `server.backend_check_wait`; a `trees` interface;
/// and a `[trees]` table.
#[test]
fn what_a_node_over_the_cold_tier_does_not_use_is_refused() {
    let budget = format!("{COLD}budget = 1000\n");
    assert_eq!(
        refusal(&budget),
        "cold_store.budget: only [hot_store] has it"
    );
    let skipping = format!("{COLD}skip_checks = []\n");
    assert_eq!(
        refusal(&skipping),
        "cold_store.skip_checks: not for a node over the cold tier alone"
    );
    let waiting = cold_with(
        "proxy_id        = \"front-1\"\n",
        "proxy_id        = \"front-1\"\nbackend_check_wait = \"5s\"\n",
    );
    assert_eq!(
        refusal(&waiting),
        "server.backend_check_wait: not used by a node over the cold tier alone"
    );
    let trees = format!("{COLD}\n[[interface]]\ntype = \"trees\"\nparams = {{ port = 9600 }}\n");
    assert_eq!(
        refusal(&trees),
        "trees interface: a node over the cold tier alone has no tree store: a trees interface \
         needs a build node ([local_cache] and [cold_store] without [hot_store]), whose [trees] \
         keep the trees"
    );
    let table = format!("{COLD}\n[trees]\nttl = \"1h\"\n");
    assert!(
        refusal(&table).starts_with("trees: only a build node has trees"),
        "{}",
        refusal(&table)
    );
}

/// Asserts that the settings of a storage node (`Cli::from_config_text`'s loader) refuse the file
/// of a node over the cold tier with the exact message, rather than taking it for a storage node
/// with no hot tier.
#[test]
fn the_settings_of_a_storage_node_refuse_a_file_with_the_cold_store_alone() {
    let why = from_text(COLD, DEFAULTS).unwrap_err().to_string();
    assert_eq!(
        why,
        "hot_store: is needed: a file with [cold_store] alone is a node over the cold tier, which \
         these settings of a storage node cannot hold"
    );
}
