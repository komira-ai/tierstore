//! `params.upstream` of an `oci` interface in the configuration file: what it makes and what it
//! refuses.

use std::path::PathBuf;
use std::time::Duration;

use super::UpstreamTable;
use crate::server::config::{from_text, settings_from_text};
use crate::server::config_testing::{DEFAULTS, MINIMAL, oci_interface};
use crate::server::config_units::Dur;

/// `MINIMAL` with an `oci` interface whose params are the address and `more`.
fn with_upstream(more: &str) -> String {
    format!("{MINIMAL}{}", oci_interface("127.0.0.1:5000", more))
}

fn refusal(text: &str) -> String {
    from_text(text, DEFAULTS).unwrap_err().to_string()
}

const FULL: &str = r#", upstream = { url = "https://registry.example:5000", username = "puller", password_file = "/etc/tierstore/upstream.password", ca_file = "/etc/tierstore/upstream-ca.pem", timeout = "45s", max_redirects = 2, recheck_timeout = "2s" }"#;

/// Asserts a table with every key makes the settings it says, one with the url alone leaves the
/// rest to the defaults of the start, and a file without the param pulls nothing.
#[test]
fn an_upstream_table_makes_the_settings_it_says() {
    let cli = from_text(&with_upstream(FULL), DEFAULTS).unwrap();
    let expected = UpstreamTable {
        url: Some("https://registry.example:5000".to_owned()),
        username: Some("puller".to_owned()),
        password_file: Some(PathBuf::from("/etc/tierstore/upstream.password")),
        ca_file: Some(PathBuf::from("/etc/tierstore/upstream-ca.pem")),
        timeout: Some(Dur(Duration::from_secs(45))),
        max_redirects: Some(2),
        recheck_timeout: Some(Dur(Duration::from_secs(2))),
    };
    assert_eq!(cli.oci_upstream, Some(expected));
    let least = from_text(
        &with_upstream(", upstream = { url = \"https://registry.example\" }"),
        DEFAULTS,
    )
    .unwrap();
    let upstream = least.oci_upstream.expect("an upstream");
    assert_eq!(upstream.url.as_deref(), Some("https://registry.example"));
    assert_eq!(upstream.username, None);
    assert_eq!(upstream.timeout, None);
    assert_eq!(upstream.max_redirects, None);
    assert_eq!(upstream.recheck_timeout, None);
    let none = from_text(&with_upstream(""), DEFAULTS).unwrap();
    assert_eq!(none.oci_upstream, None);
}

/// Asserts each way the table can be wrong is refused naming its key: no `url`, a user name
/// without a password file and the other way round, a timeout of zero, and a key that is not one.
#[test]
fn an_upstream_table_is_refused_naming_its_key() {
    let cases = [
        (
            "username = \"u\"",
            "interface[1].params.upstream.url: it is needed",
        ),
        (
            "url = \"https://r\", username = \"u\"",
            "interface[1].params.upstream.password_file: it is needed with username",
        ),
        (
            "url = \"https://r\", password_file = \"/p\"",
            "interface[1].params.upstream.username: it is needed with password_file",
        ),
        (
            "url = \"https://r\", timeout = \"0s\"",
            "interface[1].params.upstream.timeout: it must be longer than zero",
        ),
        ("url = \"https://r\", retries = 3", "retries"),
    ];
    for (upstream, want) in cases {
        let why = refusal(&with_upstream(&format!(", upstream = {{ {upstream} }}")));
        assert!(why.contains(want), "{upstream}: {why}");
    }
}

/// Asserts an `s3` interface refuses the param, and two `oci` interfaces that differ in it are
/// refused naming it, as for every param of the front.
#[test]
fn the_param_is_for_oci_and_is_shared_by_its_entries() {
    let upstream = ", upstream = { url = \"https://r\" }";
    let s3 = format!(
        "{}{MINIMAL}",
        "[[interface]]\ntype = \"s3\"\nparams = { port = 9301, upstream = { url = \"https://r\" } }\n"
    );
    let why = refusal(&s3);
    assert!(why.contains("upstream"), "{why}");
    let two = format!(
        "{MINIMAL}{}{}",
        oci_interface("127.0.0.1:5000", upstream),
        oci_interface("127.0.0.1:5001", "")
    );
    let why = refusal(&two);
    assert!(why.contains("params.upstream"), "{why}");
    let same = format!(
        "{MINIMAL}{}{}",
        oci_interface("127.0.0.1:5000", upstream),
        oci_interface("127.0.0.1:5001", upstream)
    );
    assert!(from_text(&same, DEFAULTS).is_ok());
}

/// Asserts a build node's file is refused with an upstream: it takes no writes, and a pull is
/// stored.
#[test]
fn a_build_node_file_is_refused_with_an_upstream() {
    let text = format!(
        r#"
[[interface]]
type   = "s3"
params = {{ addresses = ["127.0.0.1:0"] }}

[server]
proxy_id        = "c1"
bucket          = "tier-bucket"
access_key_file = "key"

[local_cache]
path   = "/c"
budget = 1000

[cold_store]
kind = "memory"
{}
[[policy]]
prefix = "oci/"
cache  = true
"#,
        oci_interface(
            "127.0.0.1:5000",
            ", upstream = { url = \"https://registry.example\" }"
        )
    );
    let why = settings_from_text(&text, DEFAULTS).unwrap_err().to_string();
    let wanted = "oci interface params.upstream: a pull is stored, and a build node takes no writes: use a storage node's file";
    assert!(why.contains(wanted), "{why}");
}
