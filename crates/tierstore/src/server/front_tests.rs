//! The TLS of the S3 front: its two keys, the warning for plain HTTP off loopback, and the
//! server settings made of the certificate and key.

use std::fs::Permissions;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::Arc;

use tierstore_testkit::dir::TestDir;

use super::{FrontSettings, InvalidFlags, S3Tls, server_config, warn_plain};
use crate::SystemClock;
use crate::server::cli::Cli;
use crate::server::config::keys_for_flags;
use crate::server::config::{Settings, settings_from_text};
use crate::server::config_testing::{DEFAULTS, MINIMAL};
use crate::server::oci::OciError;
use crate::server::oci::test_pki::{ca, dns, leaf};

fn addrs(list: &[&str]) -> Vec<SocketAddr> {
    list.iter()
        .map(|text| text.parse().expect("an address"))
        .collect()
}

/// What `warn_plain` writes for `listen`.
fn warned(listen: &[&str]) -> String {
    let mut out = Vec::new();
    warn_plain(&addrs(listen), &mut out);
    String::from_utf8(out).expect("text")
}

/// Asserts the warning of a plain HTTP front, by exact equality: one line for every address that
/// is not loopback, naming those addresses and the two keys, and nothing when every address is
/// loopback (127/8, `::1`, an IPv4-mapped loopback address), the wildcards not being loopback.
#[test]
fn plain_http_off_loopback_is_warned_in_one_line() {
    let line = |named: &str| {
        format!(
            "tierstore: warning: the s3 interface at {named} serves plain HTTP on a non-loopback \
             address; set server.tls_cert_file and server.tls_key_file to serve HTTPS\n"
        )
    };
    assert_eq!(warned(&["0.0.0.0:9000"]), line("0.0.0.0:9000"));
    assert_eq!(warned(&["[::]:9000"]), line("[::]:9000"));
    let mixed = [
        "127.0.0.1:9000",
        "192.0.2.7:9000",
        "[::1]:9000",
        "[::]:9000",
    ];
    assert_eq!(warned(&mixed), line("192.0.2.7:9000,[::]:9000"));
    let loopback = [
        "127.0.0.1:9000",
        "127.9.9.9:9000",
        "[::1]:9000",
        "[::ffff:127.0.0.1]:9000",
    ];
    assert_eq!(warned(&loopback), "");
}

/// Asserts the pair of files is both or neither, and that one alone is refused with a message
/// that names both flags.
#[test]
fn the_certificate_and_the_key_go_together() {
    let both = S3Tls {
        tls_cert: Some(PathBuf::from("c")),
        tls_key: Some(PathBuf::from("k")),
    };
    let files = both.files().expect("a pair").expect("two files");
    assert_eq!(
        files,
        (PathBuf::from("c").as_path(), PathBuf::from("k").as_path())
    );
    assert_eq!(S3Tls::default().files().expect("none"), None);
    for one in [
        S3Tls {
            tls_cert: Some(PathBuf::from("c")),
            tls_key: None,
        },
        S3Tls {
            tls_cert: None,
            tls_key: Some(PathBuf::from("k")),
        },
    ] {
        let InvalidFlags(why) = one.files().expect_err("only one of two");
        assert_eq!(
            why,
            "--tls-cert and --tls-key go together: give both, or neither"
        );
    }
}

/// A good access key file in `dir`.
fn access_key(dir: &TestDir) -> PathBuf {
    let path = dir.path().join("access.key");
    std::fs::write(&path, "AKID\na-long-enough-secret\n").expect("key file");
    std::fs::set_permissions(&path, Permissions::from_mode(0o600)).expect("mode");
    path
}

/// The storage node of a file over the good key file in `dir`, with the lines `extra` added to its
/// `[server]` table.
fn node(dir: &TestDir, extra: &str) -> Cli {
    let key = access_key(dir);
    let line = format!("access_key_file = {:?}\n{extra}", key.display().to_string());
    let text = MINIMAL.replacen("access_key_file = \"key\"", &line, 1);
    match settings_from_text(&text, DEFAULTS).expect("a file") {
        Settings::Storage(cli) => *cli,
        other => panic!("a storage node, not {other:?}"),
    }
}

/// The two TLS keys of a file, for the certificate `cert` and the key `key`.
fn tls_keys(cert: &std::path::Path, key: &std::path::Path) -> String {
    format!(
        "tls_cert_file = {:?}\ntls_key_file = {:?}\n",
        cert.display().to_string(),
        key.display().to_string()
    )
}

/// The front settings of the file with `extra` in its `[server]` table, over a good key file in
/// `dir`.
fn front(dir: &TestDir, extra: &str) -> FrontSettings {
    node(dir, extra).front()
}

/// Asserts that the two TLS keys of a file reach the front settings, and that a file without them
/// has none.
#[test]
fn the_tls_keys_of_a_file_reach_the_front_settings() {
    let dir = TestDir::under(std::env::temp_dir(), "front-tls-keys");
    let both = front(
        &dir,
        "tls_cert_file = \"c.pem\"\ntls_key_file = \"k.pem\"\n",
    );
    assert_eq!(both.tls.tls_cert, Some(PathBuf::from("c.pem")));
    assert_eq!(both.tls.tls_key, Some(PathBuf::from("k.pem")));
    assert_eq!(front(&dir, "").tls, S3Tls::default());
}

/// Asserts that the server settings carry the TLS of a certificate and key that work, and none
/// without them, whether the warning is asked for or not.
#[test]
fn the_server_serves_tls_with_a_certificate_and_key_and_plain_http_without() {
    let dir = TestDir::under(std::env::temp_dir(), "front-tls-config");
    let files = leaf(dir.path(), "front", &ca(), vec![dns("tierstore.test")]);
    let tls = tls_keys(&files.cert, &files.key);
    let secure = server_config(&front(&dir, &tls), Arc::new(SystemClock), true).expect("settings");
    assert!(secure.tls.is_some());
    for warn in [true, false] {
        let plain = server_config(&front(&dir, ""), Arc::new(SystemClock), warn);
        assert!(plain.expect("settings").tls.is_none());
    }
}

/// Asserts what a certificate or key that cannot work does at start: a key that is not the
/// certificate's and a certificate that names nothing are flags and files to correct (exit 2), a
/// file that is not there is not (exit 1), and a front given only one of the two files is refused.
#[test]
fn a_certificate_or_key_that_cannot_work_stops_the_start() {
    let dir = TestDir::under(std::env::temp_dir(), "front-tls-refused");
    let authority = ca();
    let named = leaf(dir.path(), "named", &authority, vec![dns("tierstore.test")]);
    let stranger = leaf(
        dir.path(),
        "stranger",
        &authority,
        vec![dns("tierstore.test")],
    );
    let bare = leaf(dir.path(), "bare", &authority, vec![]);
    let start = |cert: &std::path::Path, key: &std::path::Path| {
        let tls = tls_keys(cert, key);
        server_config(&front(&dir, &tls), Arc::new(SystemClock), false).err()
    };
    let mismatched = start(&named.cert, &stranger.key).expect("refused");
    let InvalidFlags(why) = *mismatched
        .downcast::<InvalidFlags>()
        .expect("invalid flags");
    assert!(why.starts_with("--tls-cert and --tls-key: "), "{why}");
    let unnamed = start(&bare.cert, &bare.key).expect("refused");
    let why = unnamed.downcast::<InvalidFlags>().expect("invalid flags").0;
    assert!(why.contains("no IP subjectAltName"), "{why}");
    let missing = dir.path().join("missing.pem");
    let unreadable = start(&missing, &named.key).expect("refused");
    let read = unreadable.downcast::<OciError>().expect("an OCI error");
    assert!(matches!(
        *read,
        OciError::Read {
            flag: "--tls-cert",
            ..
        }
    ));
    let mut half = front(&dir, "");
    half.tls.tls_cert = Some(named.cert);
    let alone = server_config(&half, Arc::new(SystemClock), false).err();
    assert!(
        alone
            .expect("refused")
            .downcast_ref::<InvalidFlags>()
            .is_some()
    );
}

/// Asserts that a file with one of the two TLS keys alone is refused by `--check-config` with the
/// message naming the keys the operator wrote.
#[test]
fn a_file_with_one_tls_key_is_refused_naming_both_keys() {
    let dir = TestDir::under(std::env::temp_dir(), "front-tls-file");
    for (extra, cert) in [
        ("tls_cert_file = \"c.pem\"\n", true),
        ("tls_key_file = \"k.pem\"\n", false),
    ] {
        let cli = node(&dir, extra);
        assert_eq!(cli.tls.tls_cert.is_some(), cert);
        assert_eq!(cli.tls.tls_key.is_some(), !cert);
        let alone = crate::server::cli::checked(&Settings::Storage(Box::new(cli)));
        let why = alone.expect_err("one of the two keys").to_string();
        assert_eq!(
            keys_for_flags(&why),
            "server.tls_cert_file and server.tls_key_file go together: give both, or neither"
        );
    }
}
