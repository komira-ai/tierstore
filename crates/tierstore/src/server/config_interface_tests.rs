//! `[[interface]]`: what the entries make, and each refusal by its exact message.

use super::super::config::from_text;
use super::super::config_testing::DEFAULTS;
use crate::server::cli::Cli;

/// The tables of a file that are not interfaces: two memory tiers.
const TIERS: &str = r#"
[server]
proxy_id        = "p1"
bucket          = "tier-bucket"
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

/// The settings of a file of the entries `entries` and two memory tiers.
fn made(entries: &[String]) -> Result<Cli, String> {
    let text = format!("{}{TIERS}", entries.concat());
    from_text(&text, DEFAULTS).map_err(|why| why.to_string())
}

/// The message with which a file of `entries` is refused.
fn refusal(entries: &[String]) -> String {
    made(entries).unwrap_err()
}

/// Asserts that two interfaces of each type make one front each: the `s3` entries listen on the
/// addresses of both (a `port` is the wildcard of IPv4, `addresses` a string or a list) and the
/// `oci` entries on the addresses of both, with the one prefix they share.
#[test]
fn two_interfaces_of_each_type_are_served_together() {
    let cli = made(&[
        entry("s3", "port = 9300"),
        entry("s3", "addresses = [\"127.0.0.1:9301\", \"[::1]:9301\"]"),
        entry("oci", "port = 5000, prefix = \"images/\""),
        entry(
            "oci",
            "addresses = \"127.0.0.1:5001\", prefix = \"images/\"",
        ),
    ])
    .unwrap();
    let wanted = ["0.0.0.0:9300", "127.0.0.1:9301", "[::1]:9301"];
    assert_eq!(cli.listen, wanted.map(|text| text.parse().unwrap()));
    let wanted = ["0.0.0.0:5000", "127.0.0.1:5001"];
    assert_eq!(cli.oci_listen, wanted.map(|text| text.parse().unwrap()));
    assert_eq!(cli.oci_prefix.as_deref(), Some("images/"));
}

/// Asserts that an address two interfaces listen on is refused naming the later entry and the
/// earlier one, whether they are of one type or of two, and that an address listed twice in one
/// entry is refused as listed twice.
#[test]
fn an_address_listened_on_twice_is_refused() {
    let same_type = refusal(&[entry("s3", "port = 9300"), entry("s3", "port = 9300")]);
    assert_eq!(
        same_type,
        "interface[1].params: the address 0.0.0.0:9300 is already used by interface[0]"
    );
    let two_types = refusal(&[
        entry("s3", "addresses = [\"127.0.0.1:9300\"]"),
        entry("oci", "addresses = [\"127.0.0.1:5000\"]"),
        entry("oci", "addresses = [\"127.0.0.1:9300\"]"),
    ]);
    assert_eq!(
        two_types,
        "interface[2].params: the address 127.0.0.1:9300 is already used by interface[0]"
    );
    let one_entry = refusal(&[entry(
        "s3",
        "addresses = [\"127.0.0.1:9300\", \"127.0.0.1:9300\"]",
    )]);
    assert_eq!(
        one_entry,
        "interface[0].params: the address 127.0.0.1:9300 is listed twice"
    );
}

/// Asserts that a type that is not `s3` or `oci` is refused naming the entry, and that `fs`,
/// which comes later, says so.
#[test]
fn an_unknown_type_and_the_fs_type_are_refused() {
    let unknown = refusal(&[entry("s3", "port = 9300"), entry("ftp", "port = 21")]);
    assert_eq!(
        unknown,
        "interface[1].type: not an interface type; the types are \"s3\" and \"oci\""
    );
    let fs = refusal(&[entry("s3", "port = 9300"), entry("fs", "port = 21")]);
    assert_eq!(
        fs,
        "interface[1].type: \"fs\" is not served yet; the types are \"s3\" and \"oci\""
    );
}

/// Asserts the refusals of where an interface listens: neither `port` nor `addresses` (also with
/// no `params` at all), both, a port of zero, an empty list, and a port past 65535.
#[test]
fn a_place_to_listen_is_needed_exactly_once() {
    assert_eq!(
        refusal(&[entry("s3", "")]),
        "interface[0].params: it needs port or addresses"
    );
    let bare = "[[interface]]\ntype = \"s3\"\n\n".to_owned();
    assert_eq!(
        refusal(&[bare]),
        "interface[0].params: it needs port or addresses"
    );
    assert_eq!(
        refusal(&[entry("s3", "port = 1, addresses = [\"127.0.0.1:2\"]")]),
        "interface[0].params: give port or addresses, not both"
    );
    assert_eq!(
        refusal(&[entry("s3", "port = 0")]),
        "interface[0].params.port: 0 is not between 1 and 65535"
    );
    assert_eq!(
        refusal(&[entry("s3", "addresses = []")]),
        "interface[0].params.addresses: it needs at least one address"
    );
    let past = refusal(&[entry("s3", "port = 65536")]);
    assert!(past.contains("not a value this key takes"), "{past}");
    assert!(!past.contains("65536"), "{past}");
}

/// Asserts that a file with no `s3` interface is refused, whether it has an `oci` one or none.
#[test]
fn an_s3_interface_is_needed() {
    let want = "interface: it needs at least one interface with type = \"s3\"";
    assert_eq!(refusal(&[entry("oci", "port = 5000")]), want);
    assert_eq!(refusal(&[]), want);
}

/// Asserts that an `s3` interface takes none of the params of the registry front, and says why
/// for the TLS ones: an S3 interface has no TLS yet.
#[test]
fn an_s3_interface_takes_no_oci_params() {
    let cases = [
        (
            "tls_cert_file = \"/c\"",
            "interface[0].params.tls_cert_file: an s3 interface has no TLS yet",
        ),
        (
            "tls_key_file = \"/k\"",
            "interface[0].params.tls_key_file: an s3 interface has no TLS yet",
        ),
        (
            "prefix = \"oci/\"",
            "interface[0].params.prefix: not for type \"s3\"",
        ),
        (
            "max_connections = 4",
            "interface[0].params.max_connections: not for type \"s3\"",
        ),
        (
            "connection_lifetime = \"1h\"",
            "interface[0].params.connection_lifetime: not for type \"s3\"",
        ),
    ];
    for (param, want) in cases {
        assert_eq!(
            refusal(&[entry("s3", &format!("port = 9300, {param}"))]),
            want
        );
    }
}

/// Asserts the refusals of an `oci` interface by itself: a certificate without a key and a key
/// without a certificate, and a connection count of zero.
#[test]
fn an_oci_interface_needs_both_tls_files_or_neither() {
    let s3 = entry("s3", "port = 9300");
    assert_eq!(
        refusal(&[
            s3.clone(),
            entry("oci", "port = 5000, tls_cert_file = \"/c\"")
        ]),
        "interface[1].params.tls_key_file: it is needed with tls_cert_file: give both, or neither"
    );
    assert_eq!(
        refusal(&[
            s3.clone(),
            entry("oci", "port = 5000, tls_key_file = \"/k\"")
        ]),
        "interface[1].params.tls_cert_file: it is needed with tls_key_file: give both, or neither"
    );
    assert_eq!(
        refusal(&[s3, entry("oci", "port = 5000, max_connections = 0")]),
        "interface[1].params.max_connections: 0 is not between 1 and 4294967295"
    );
}

/// Asserts that `oci` interfaces must agree on the params of the one registry front they make:
/// each param that differs from the first interface's is refused naming it, and a param one
/// gives and another leaves out differs.
#[test]
fn oci_interfaces_must_agree_on_the_front() {
    let s3 = entry("s3", "port = 9300");
    let both = "tls_cert_file = \"/c\", tls_key_file = \"/k\"";
    let other = "tls_cert_file = \"/c2\", tls_key_file = \"/k\"";
    let other_key = "tls_cert_file = \"/c\", tls_key_file = \"/k2\"";
    let cases = [
        (both, other, "tls_cert_file"),
        (both, other_key, "tls_key_file"),
        ("prefix = \"a/\"", "prefix = \"b/\"", "prefix"),
        ("prefix = \"a/\"", "max_connections = 3", "prefix"),
        (
            "max_connections = 3",
            "max_connections = 4",
            "max_connections",
        ),
        (
            "connection_lifetime = \"1h\"",
            "connection_lifetime = \"2h\"",
            "connection_lifetime",
        ),
    ];
    for (first, second, name) in cases {
        let first = entry("oci", &format!("port = 5000, {first}"));
        let second = entry("oci", &format!("port = 5001, {second}"));
        let why = refusal(&[s3.clone(), first, second]);
        let want = format!(
            "interface[2].params.{name}: it differs from interface[1]: one registry front serves every oci interface, so they share tls_cert_file, tls_key_file, prefix, max_connections and connection_lifetime"
        );
        assert_eq!(why, want);
    }
}

/// Asserts that an entry with a key the format does not have, and one without a `type`, are
/// refused naming the key.
#[test]
fn an_entry_is_a_closed_set_and_needs_a_type() {
    let text = format!("[[interface]]\ntype = \"s3\"\nbogus = 1\nparams = {{ port = 1 }}\n{TIERS}");
    let why = from_text(&text, DEFAULTS).unwrap_err().to_string();
    assert!(why.contains("bogus"), "{why}");
    let text = format!("[[interface]]\nparams = {{ port = 1 }}\n{TIERS}");
    let why = from_text(&text, DEFAULTS).unwrap_err().to_string();
    assert!(why.contains("missing field `type`"), "{why}");
}

/// Asserts what is the same address: the whole socket address, so the same IP on another port,
/// another IP on the same port, and the two wildcards of one port (IPv4 and IPv6) are not.
#[test]
fn only_the_whole_socket_address_is_the_same_address() {
    let cli = made(&[
        entry(
            "s3",
            "addresses = [\"192.0.2.1:9400\", \"192.0.2.1:9401\", \"192.0.2.2:9400\"]",
        ),
        entry("oci", "addresses = [\"0.0.0.0:5000\", \"[::]:5000\"]"),
    ])
    .unwrap();
    assert_eq!(cli.listen.len(), 3);
    assert_eq!(cli.oci_listen.len(), 2);
}
