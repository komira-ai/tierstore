//! The `trees` interface of a file: it was removed, and every node shape refuses it with the exact
//! message that says so.

use super::super::config::settings_from_text;
use super::super::config_testing::DEFAULTS;

/// The tables of a build node, without its interfaces.
const BUILD_NODE: &str = r#"
[server]
proxy_id        = "c1"
bucket          = "tier-bucket"
access_key_file = "key"

[local_cache]
path   = "/var/lib/tierstore-cache"
budget = "40GiB"

[cold_store]
kind = "memory"

[[policy]]
prefix = "cas/"
cache  = true
"#;

/// The tables of a storage node with a cache in front of its proxy, without its interfaces.
const COMBINED_NODE: &str = r#"
[server]
proxy_id        = "tierstore-1"
bucket          = "tierstore"
access_key_file = "key"

[hot_store]
kind   = "memory"
budget = 1000

[cold_store]
kind = "memory"

[local_cache]
path   = "/var/lib/tierstore-cache"
budget = "40GiB"

[[policy]]
prefix = "cas/"
cache  = true
"#;

/// The tables of a storage node, with no cache.
const STORAGE_NODE: &str = r#"
[server]
proxy_id        = "tierstore-1"
bucket          = "tierstore"
access_key_file = "key"

[hot_store]
kind   = "memory"
budget = 1000

[cold_store]
kind = "memory"
"#;

/// The tables of a node over the cold tier alone, without its interfaces.
const COLD_NODE: &str = r#"
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

/// The message a file refuses with: the entries, then the tables of `node`.
fn refusal(node: &str, entries: &[String]) -> String {
    let text = format!("{}{node}", entries.concat());
    settings_from_text(&text, DEFAULTS)
        .expect_err("refused")
        .to_string()
}

/// One `[[interface]]` entry of `kind` with `params` between the braces.
fn entry(kind: &str, params: &str) -> String {
    format!("[[interface]]\ntype = \"{kind}\"\nparams = {{ {params} }}\n\n")
}

fn s3() -> String {
    entry("s3", "port = 9300")
}

/// The message of a refused `trees` entry at `interface[index]`, in full.
fn removed(index: usize) -> String {
    format!(
        "interface[{index}].type: the trees interface was removed: delete this [[interface]] \
         entry (the tree store, the mount provider and the fs interface are unchanged)"
    )
}

/// Asserts a build node that names a `trees` interface is refused with the full removed message,
/// and that the same file without it loads.
#[test]
fn a_trees_interface_in_a_build_node_is_refused_as_removed() {
    let why = refusal(BUILD_NODE, &[s3(), entry("trees", "port = 9600")]);
    assert_eq!(why, removed(1));
    let without = format!("{}{BUILD_NODE}", s3());
    assert!(settings_from_text(&without, DEFAULTS).is_ok());
}

/// Asserts a storage node, with or without a cache, is refused a `trees` interface with the
/// removed message, not the message of a node with no tree store.
#[test]
fn a_trees_interface_in_a_storage_node_is_refused_as_removed() {
    for node in [STORAGE_NODE, COMBINED_NODE] {
        let why = refusal(node, &[s3(), entry("trees", "port = 9600")]);
        assert_eq!(why, removed(1));
    }
}

/// Asserts a node over the cold tier alone is refused a `trees` interface with the removed
/// message.
#[test]
fn a_trees_interface_in_a_cold_front_file_is_refused_as_removed() {
    let why = refusal(COLD_NODE, &[s3(), entry("trees", "port = 9600")]);
    assert_eq!(why, removed(1));
}

/// Asserts a `trees` entry with no params at all, or with params the type once took, is refused
/// as removed and not for a param, and that the entry named is the one that carries the type.
#[test]
fn a_trees_interface_with_params_is_refused_as_removed_not_for_its_params() {
    let bare = "[[interface]]\ntype = \"trees\"\n\n".to_owned();
    assert_eq!(refusal(BUILD_NODE, &[bare, s3()]), removed(0));
    let params = "port = 9600, addresses = [\"127.0.0.1:9600\"], max_connections = 0";
    assert_eq!(
        refusal(BUILD_NODE, &[s3(), entry("trees", params)]),
        removed(1)
    );
    let tls = "tls_cert_file = \"/etc/tierstore/trees.crt\"";
    assert_eq!(
        refusal(
            BUILD_NODE,
            &[s3(), entry("s3", "port = 9301"), entry("trees", tls)]
        ),
        removed(2)
    );
}
