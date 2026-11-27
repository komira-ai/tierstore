//! The `mount-provider` interface of a file: what it makes, its defaults, and each refusal by its
//! exact message.

use std::path::PathBuf;
use std::time::Duration;

use super::super::config::settings_from_text;
use super::super::config_testing::DEFAULTS;
use super::super::provider::ProviderFlags;

/// The tables of a build node, without its interfaces; `local` goes in `[local_cache]` and
/// `server` in `[server]`.
fn build_node(server: &str, local: &str) -> String {
    format!(
        r#"
[server]
proxy_id        = "c1"
bucket          = "tier-bucket"
access_key_file = "key"
{server}

[local_cache]
path   = "/var/lib/tierstore-cache"
budget = "40GiB"
{local}

[cold_store]
kind = "memory"

[[policy]]
prefix = "cas/"
cache  = true
"#
    )
}

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

/// The socket of the examples.
const SOCKET: &str = "/var/lib/tierstore/provider.sock";

/// One `[[interface]]` entry of `kind` with `params` between the braces.
fn entry(kind: &str, params: &str) -> String {
    format!("[[interface]]\ntype = \"{kind}\"\nparams = {{ {params} }}\n\n")
}

fn s3() -> String {
    entry("s3", "port = 9300")
}

fn provider(params: &str) -> String {
    entry("mount-provider", params)
}

fn with_socket(more: &str) -> String {
    provider(&format!("socket = \"{SOCKET}\"{more}"))
}

fn provider_of(entries: &[String]) -> Option<ProviderFlags> {
    let text = format!("{}{}", entries.concat(), build_node("", ""));
    let settings = settings_from_text(&text, DEFAULTS).expect("a build node");
    settings.into_cache().expect("a build node").provider
}

fn refusal(node: &str, entries: &[String]) -> String {
    let text = format!("{}{node}", entries.concat());
    settings_from_text(&text, DEFAULTS)
        .expect_err("refused")
        .to_string()
}

/// Asserts that a node with no `mount-provider` interface has no provider, and that one that gives
/// its socket alone answers under `/run/tierstore/fs` with reads bounded at a minute.
#[test]
fn a_mount_provider_interface_with_its_socket_alone_takes_the_defaults() {
    assert_eq!(provider_of(&[s3()]), None);
    let wanted = ProviderFlags {
        socket: PathBuf::from(SOCKET),
        mountpoint: PathBuf::from("/run/tierstore/fs"),
        fetch_timeout: Duration::from_secs(60),
        served: false,
    };
    assert_eq!(provider_of(&[s3(), with_socket("")]), Some(wanted));
}

/// Asserts the mount and the bound an interface gives are the provider's, wherever the entry is
/// in the list.
#[test]
fn the_params_of_a_mount_provider_interface_are_the_provider() {
    let given = with_socket(", mountpoint = \"/mnt/tierstore\", fetch_timeout = \"5s\"");
    let wanted = ProviderFlags {
        socket: PathBuf::from(SOCKET),
        mountpoint: PathBuf::from("/mnt/tierstore"),
        fetch_timeout: Duration::from_secs(5),
        served: false,
    };
    assert_eq!(provider_of(&[given.clone(), s3()]), Some(wanted));
}

/// Asserts the refusals of the params of a `mount-provider` interface, each by its exact message:
/// no socket, a socket that is not an absolute path of a file or is too long, a mount that is not
/// absolute, a bound of zero, and a second interface of the type.
#[test]
fn a_mount_provider_interface_is_refused_for_a_param_that_is_wrong() {
    let check = |entry: String| refusal(&build_node("", ""), &[s3(), entry]);
    assert_eq!(
        check(provider("")),
        "interface[1].params.socket: it is needed"
    );
    assert_eq!(
        check(provider("socket = \"provider.sock\"")),
        "interface[1].params.socket: \"provider.sock\" must be an absolute path"
    );
    assert_eq!(
        check(provider("socket = \"/\"")),
        "interface[1].params.socket: \"/\" must name a file, not a directory"
    );
    let longest = format!("/{}", "s".repeat(102));
    assert!(provider_of(&[s3(), provider(&format!("socket = \"{longest}\""))]).is_some());
    let long = format!("/{}", "s".repeat(103));
    assert_eq!(
        check(provider(&format!("socket = \"{long}\""))),
        format!(
            "interface[1].params.socket: \"{long}\" is 104 bytes; the path of a Unix socket is at most 103"
        )
    );
    assert_eq!(
        check(with_socket(", mountpoint = \"fs\"")),
        "interface[1].params.mountpoint: \"fs\" must be an absolute path"
    );
    assert_eq!(
        check(with_socket(", fetch_timeout = \"0s\"")),
        "interface[1].params.fetch_timeout: it must be longer than zero"
    );
    let twice = refusal(
        &build_node("", ""),
        &[s3(), with_socket(""), with_socket("")],
    );
    assert_eq!(
        twice,
        "interface[2].type: a second mount-provider interface: a node has one mount provider"
    );
}

/// Asserts a `mount-provider` interface takes none of the params of the other types, since it
/// listens on its socket alone.
#[test]
fn the_params_of_the_other_types_are_not_the_providers() {
    for param in [
        "port = 9700",
        "addresses = [\"127.0.0.1:9700\"]",
        "tls_cert_file = \"/a\"",
        "tls_key_file = \"/k\"",
        "prefix = \"oci/\"",
        "max_connections = 1",
        "connection_lifetime = \"1m\"",
        "require_auth = true",
        "stage_read = 1",
        "max_manifest_bytes = 1",
        "read_chunk_bytes = 1",
        "echo_bytes = 1",
        "upstream = { url = \"https://registry.example\" }",
    ] {
        let name = param.split(' ').next().expect("a name");
        let why = refusal(
            &build_node("", ""),
            &[s3(), with_socket(&format!(", {param}"))],
        );
        assert_eq!(
            why,
            format!(
                "interface[1].params.{name}: not for type \"mount-provider\": it listens on its socket alone"
            ),
        );
    }
}

/// Asserts the params of a `mount-provider` interface are none of another type's.
#[test]
fn the_providers_params_are_not_the_other_types() {
    for (kind, listen) in [
        ("s3", "port = 9301"),
        ("trees", "port = 9600"),
        ("oci", "addresses = [\"127.0.0.1:5000\"]"),
    ] {
        for param in [
            format!("socket = \"{SOCKET}\""),
            "mountpoint = \"/run/tierstore/fs\"".to_owned(),
            "fetch_timeout = \"5s\"".to_owned(),
        ] {
            let name = param.split(' ').next().expect("a name");
            let entries = [s3(), entry(kind, &format!("{listen}, {param}"))];
            let why = refusal(&build_node("", ""), &entries);
            assert_eq!(
                why,
                format!("interface[1].params.{name}: not for type \"{kind}\"")
            );
        }
    }
}

/// Asserts a mount provider is a build node's alone: a storage node, with or without a cache, has
/// no tree store, and is refused with the exact words naming what it is.
#[test]
fn a_mount_provider_interface_needs_a_build_node() {
    let tail = "has no tree store: a mount-provider interface needs a build node ([local_cache] and [cold_store] without [hot_store]), whose [trees] keep the trees";
    let why = refusal(COMBINED_NODE, &[s3(), with_socket("")]);
    assert_eq!(
        why,
        format!("mount-provider interface: a storage node with a cache {tail}")
    );
    let why = refusal(STORAGE_NODE, &[s3(), with_socket("")]);
    assert_eq!(
        why,
        format!("mount-provider interface: a storage node {tail}")
    );
}

/// Asserts the provider's socket is not the hand-off socket and not inside the cache directory,
/// and that a `local_cache.fill_socket` line, read and ignored, is no socket of the node for it to
/// clash with.
#[test]
fn the_providers_socket_is_not_another_socket_of_the_node() {
    let handoff = build_node(&format!("handoff_socket = \"{SOCKET}\""), "");
    assert_eq!(
        refusal(&handoff, &[s3(), with_socket("")]),
        format!(
            "mount-provider interface params.socket: \"{SOCKET}\" is server.handoff_socket too; the two sockets are two files"
        )
    );
    let inside = provider("socket = \"/var/lib/tierstore-cache/provider.sock\"");
    assert_eq!(
        refusal(&build_node("", ""), &[s3(), inside]),
        "mount-provider interface params.socket: \"/var/lib/tierstore-cache/provider.sock\" is inside local_cache.path: it is not a place for the cache's files"
    );
    let apart = build_node("", "fill_socket = \"/var/lib/tierstore/fill.sock\"");
    let text = format!("{}{}{apart}", s3(), with_socket(""));
    assert!(settings_from_text(&text, DEFAULTS).is_ok());
    let same = build_node("", &format!("fill_socket = \"{SOCKET}\""));
    let text = format!("{}{}{same}", s3(), with_socket(""));
    assert!(settings_from_text(&text, DEFAULTS).is_ok());
}
