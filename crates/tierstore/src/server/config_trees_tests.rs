//! The `trees` interface of a file: what it makes, its defaults, and each refusal by its exact
//! message.

use super::super::config::settings_from_text;
use super::super::config_testing::DEFAULTS;
use super::super::trees::TreesFlags;

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

/// One `[[interface]]` entry of `kind` with `params` between the braces.
fn entry(kind: &str, params: &str) -> String {
    format!("[[interface]]\ntype = \"{kind}\"\nparams = {{ {params} }}\n\n")
}

fn s3() -> String {
    entry("s3", "port = 9300")
}

fn trees_of(entries: &[String]) -> TreesFlags {
    let text = format!("{}{BUILD_NODE}", entries.concat());
    let settings = settings_from_text(&text, DEFAULTS).expect("a build node");
    settings.into_cache().expect("a build node").trees
}

fn refusal(node: &str, entries: &[String]) -> String {
    let text = format!("{}{node}", entries.concat());
    settings_from_text(&text, DEFAULTS)
        .expect_err("refused")
        .to_string()
}

fn addrs(list: &[&str]) -> Vec<std::net::SocketAddr> {
    list.iter()
        .map(|text| text.parse().expect("an address"))
        .collect()
}

/// Asserts that a node with no `trees` interface has no front, and that one that gives neither
/// `port` nor `addresses` listens on port 9600 of the wildcard, with every other param unset.
#[test]
fn a_trees_interface_with_nothing_given_listens_on_the_default_port() {
    assert_eq!(trees_of(&[s3()]), TreesFlags::default());
    let flags = trees_of(&[s3(), entry("trees", "")]);
    let wanted = TreesFlags {
        listen: addrs(&["0.0.0.0:9600"]),
        ..TreesFlags::default()
    };
    assert_eq!(flags, wanted);
}

/// Asserts the TLS files and the numbers an interface gives are what the front is made of, and
/// that two entries are served together on the addresses of both.
#[test]
fn the_params_of_a_trees_interface_are_the_front() {
    let all = "port = 9700, tls_cert_file = \"/etc/tierstore/trees.crt\", tls_key_file = \"/etc/tierstore/trees.key\", max_connections = 8, connection_lifetime = \"10m\", max_request_bytes = 1048576";
    let flags = trees_of(&[s3(), entry("trees", all)]);
    let wanted = TreesFlags {
        listen: addrs(&["0.0.0.0:9700"]),
        tls_cert: Some("/etc/tierstore/trees.crt".into()),
        tls_key: Some("/etc/tierstore/trees.key".into()),
        max_connections: Some(8),
        connection_lifetime: Some(std::time::Duration::from_secs(600)),
        max_request_bytes: Some(1_048_576),
    };
    assert_eq!(flags, wanted);

    let two = trees_of(&[
        s3(),
        entry("trees", "addresses = [\"127.0.0.1:9600\"]"),
        entry("trees", "addresses = \"127.0.0.1:9601\""),
    ]);
    assert_eq!(two.listen, addrs(&["127.0.0.1:9600", "127.0.0.1:9601"]));
}

/// Asserts the refusals of the params of a `trees` interface, each by its exact message: a place to
/// listen given twice, one TLS file, a number below its least, and an address that another
/// interface has.
#[test]
fn a_trees_interface_is_refused_for_a_param_that_is_wrong() {
    let check = |params: &str| refusal(BUILD_NODE, &[s3(), entry("trees", params)]);
    assert_eq!(
        check("port = 9600, addresses = [\"127.0.0.1:9600\"]"),
        "interface[1].params: give port or addresses, not both"
    );
    assert_eq!(
        check("tls_cert_file = \"/etc/tierstore/trees.crt\""),
        "interface[1].params.tls_key_file: it is needed with tls_cert_file: give both, or neither"
    );
    assert_eq!(
        check("tls_key_file = \"/etc/tierstore/trees.key\""),
        "interface[1].params.tls_cert_file: it is needed with tls_key_file: give both, or neither"
    );
    assert_eq!(
        check("connection_lifetime = \"0s\""),
        "interface[1].params.connection_lifetime: it must be longer than zero"
    );
    assert_eq!(
        check("max_connections = 0"),
        "interface[1].params.max_connections: 0 is not between 1 and 4294967295"
    );
    assert_eq!(
        check("max_request_bytes = 0"),
        "interface[1].params.max_request_bytes: 0 is not between 1 and 18446744073709551615"
    );
    let clash = refusal(
        BUILD_NODE,
        &[entry("s3", "port = 9600"), entry("trees", "port = 9600")],
    );
    assert_eq!(
        clash,
        "interface[1].params: the address 0.0.0.0:9600 is already used by interface[0]"
    );
}

/// Asserts a `trees` interface takes none of the params of the other types, and that the `s3` and
/// the `oci` interface take none of its own that the others lack.
#[test]
fn the_params_of_the_other_types_are_not_the_trees_interfaces() {
    for param in [
        "prefix = \"oci/\"",
        "require_auth = true",
        "stage_read = 1",
        "max_manifest_bytes = 1",
        "read_chunk_bytes = 1",
        "echo_bytes = 1",
        "upstream = { url = \"https://registry.example\" }",
    ] {
        let name = param.split(' ').next().expect("a name");
        let why = refusal(BUILD_NODE, &[s3(), entry("trees", param)]);
        assert!(
            why.starts_with("interface[1].params: ")
                && why.contains(name)
                && why.contains("not for type \"trees\""),
            "{why}"
        );
    }
    let on_s3 = refusal(
        BUILD_NODE,
        &[entry("s3", "port = 9300, max_request_bytes = 1")],
    );
    assert!(
        on_s3.contains("max_request_bytes") && on_s3.contains("not for type \"s3\""),
        "{on_s3}"
    );
    let on_oci = refusal(
        BUILD_NODE,
        &[s3(), entry("oci", "port = 5000, max_request_bytes = 1")],
    );
    assert!(
        on_oci.contains("max_request_bytes") && on_oci.contains("not for type \"oci\""),
        "{on_oci}"
    );
}

/// Asserts entries of the type serve as one front: a param that differs from the first entry's is
/// refused, naming the entry, the param and the entry it differs from.
#[test]
fn trees_interfaces_must_agree_on_the_front() {
    let first = |params: &str| entry("trees", &format!("port = 9600, {params}"));
    let second = |params: &str| entry("trees", &format!("port = 9601, {params}"));
    let cases = [
        (
            "max_connections = 1",
            "max_connections = 2",
            "max_connections",
        ),
        (
            "max_request_bytes = 1",
            "max_request_bytes = 2",
            "max_request_bytes",
        ),
        (
            "connection_lifetime = \"1m\"",
            "connection_lifetime = \"2m\"",
            "connection_lifetime",
        ),
        (
            "tls_cert_file = \"/a\", tls_key_file = \"/k\"",
            "tls_cert_file = \"/b\", tls_key_file = \"/k\"",
            "tls_cert_file",
        ),
        (
            "tls_cert_file = \"/a\", tls_key_file = \"/k\"",
            "tls_cert_file = \"/a\", tls_key_file = \"/l\"",
            "tls_key_file",
        ),
    ];
    for (one, other, name) in cases {
        let why = refusal(BUILD_NODE, &[s3(), first(one), second(other)]);
        let wanted = format!(
            "interface[2].params.{name}: it differs from interface[1]: one trees front serves every trees interface, so they share tls_cert_file, tls_key_file, max_connections, connection_lifetime and max_request_bytes"
        );
        assert_eq!(why, wanted);
    }
}

/// Asserts a storage node with a cache makes the front too, and that a storage node with none is
/// refused, because the trees are kept in the cache.
#[test]
fn a_trees_interface_needs_a_node_with_a_local_cache() {
    let text = format!("{}{}{COMBINED_NODE}", s3(), entry("trees", "port = 9600"));
    let settings = settings_from_text(&text, DEFAULTS).expect("a combined node");
    let combined = settings.into_combined().expect("a combined node");
    assert_eq!(combined.node.trees.listen, addrs(&["0.0.0.0:9600"]));

    let why = refusal(STORAGE_NODE, &[s3(), entry("trees", "port = 9600")]);
    assert_eq!(
        why,
        "trees interface: a trees interface needs [local_cache]: the trees are kept in the cache"
    );
}
