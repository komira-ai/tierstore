//! `[oci.upstream]` in the configuration file: what it makes, what it refuses, and that
//! `--print-config` writes it back.

use std::path::PathBuf;
use std::time::Duration;

use super::UpstreamTable;
use crate::server::config::{from_text, settings_from_text};
use crate::server::config_print::render;
use crate::server::config_testing::{DEFAULTS, MINIMAL};
use crate::server::config_units::Dur;

/// `MINIMAL` with an `[oci]` front and the text `upstream` after it.
fn with_upstream(upstream: &str) -> String {
    format!("{MINIMAL}\n[oci]\nlisten = \"127.0.0.1:5000\"\n\n[oci.upstream]\n{upstream}\n")
}

fn refusal(text: &str) -> String {
    from_text(text, DEFAULTS).unwrap_err().to_string()
}

const FULL: &str = r#"
url           = "https://registry.example:5000"
username      = "puller"
password_file = "/etc/tierstore/upstream.password"
ca_file       = "/etc/tierstore/upstream-ca.pem"
timeout       = "45s"
max_redirects = 2
"#;

/// Asserts a table with every key makes the settings it says, and one with the url alone leaves
/// the rest to the defaults of the start.
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
    };
    assert_eq!(cli.oci_upstream, Some(expected));
    let least = from_text(
        &with_upstream("url = \"https://registry.example\""),
        DEFAULTS,
    )
    .unwrap();
    let upstream = least.oci_upstream.expect("an upstream");
    assert_eq!(upstream.url.as_deref(), Some("https://registry.example"));
    assert_eq!(upstream.username, None);
    assert_eq!(upstream.timeout, None);
    assert_eq!(upstream.max_redirects, None);
    let none = from_text(MINIMAL, DEFAULTS).unwrap();
    assert_eq!(none.oci_upstream, None);
}

/// Asserts `--print-config` writes the upstream table back so that the file it prints makes the
/// settings it was printed from.
#[test]
fn print_config_writes_the_upstream_back() {
    let cli = from_text(&with_upstream(FULL), DEFAULTS).unwrap();
    let text = render(&cli).unwrap();
    assert!(text.contains("[oci.upstream]"), "{text}");
    assert_eq!(from_text(&text, DEFAULTS).unwrap(), cli);
}

/// Asserts each way the table can be wrong is refused naming its key: the table without a front
/// (`listen`), without `url`, a user name without a password file and the other way round, a
/// timeout of zero, a key that is not one, and a key that holds a secret.
#[test]
fn an_upstream_table_is_refused_naming_its_key() {
    let cases = [
        ("username = \"u\"", "oci.upstream.url: it is needed"),
        (
            "url = \"https://r\"\nusername = \"u\"",
            "oci.upstream.password_file: it is needed with oci.upstream.username",
        ),
        (
            "url = \"https://r\"\npassword_file = \"/p\"",
            "oci.upstream.username: it is needed with oci.upstream.password_file",
        ),
        (
            "url = \"https://r\"\ntimeout = \"0s\"",
            "oci.upstream.timeout: it must be longer than zero",
        ),
        ("url = \"https://r\"\nretries = 3", "retries"),
        (
            "url = \"https://r\"\npassword = \"hunter2\"",
            "oci.upstream.password: a secret is never in the file",
        ),
    ];
    for (upstream, want) in cases {
        let why = refusal(&with_upstream(upstream));
        assert!(why.contains(want), "{upstream}: {why}");
    }
    let alone = format!("{MINIMAL}\n[oci.upstream]\nurl = \"https://r\"\n");
    let why = refusal(&alone);
    assert!(why.contains("oci.upstream: it needs oci.listen"), "{why}");
}

/// Asserts a build node's file is refused with an upstream: it takes no writes, and a pull is
/// stored.
#[test]
fn a_build_node_file_is_refused_with_an_upstream() {
    let text = r#"
[server]
listen          = "127.0.0.1:0"
proxy_id        = "c1"
bucket          = "tier-bucket"
access_key_file = "key"

[local_cache]
path   = "/c"
budget = 1000

[cold_store]
kind = "memory"

[oci]
listen = "127.0.0.1:5000"

[oci.upstream]
url = "https://registry.example"

[[policy]]
prefix = "oci/"
cache  = true
"#;
    let why = settings_from_text(text, DEFAULTS).unwrap_err().to_string();
    let wanted = "oci.upstream: a pull is stored, and a build node takes no writes: use a storage node's file";
    assert!(why.contains(wanted), "{why}");
}
