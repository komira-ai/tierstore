//! The flags of the binary: their defaults, parsing and help. The settings made of them and the
//! checks run before a start are in the two modules below. The reconciler's loop is tested in
//! `cli_reconcile_tests`.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{CommandFactory, Parser};
use tierstore_testkit::dir::TestDir;

use crate::selftest_tests::{Flaw, Flawed};
use crate::server::config_testing::MINIMAL;
use crate::server::{AccessKey, Drained};
use crate::{Backend, Check, SelfTestError, SelfTestFailure};

use super::super::cli_probe::{probe_run_id, probe_tiers};
use super::super::cli_serve::cut_note;
use super::{
    Args, Cli, InvalidFlags, S3Flags, SkippedCheck, build, exit_status, open_tier, parse_duration,
    parse_skip, read_key, share, tier,
};

#[path = "cli_tests/settings.rs"]
mod settings;
#[path = "cli_tests/startup.rs"]
mod startup;

/// The JSON form of a key file with the id `JSONID` and the secret `json-secret`.
const JSON_KEY: &str = r#"{"access_key_id": "JSONID", "secret_access_key": "json-secret"}"#;

/// The file of a start that serves `127.0.0.1:0` over two memory tiers, with `key_file` as the
/// full key.
fn text_with_key(key_file: &str) -> String {
    let given = format!("access_key_file = \"{key_file}\"");
    assert!(MINIMAL.contains("access_key_file = \"key\""));
    MINIMAL.replacen("access_key_file = \"key\"", &given, 1)
}

/// A key file in `dir`, called `name`, with this text and mode.
fn key_file(dir: &TestDir, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
    path
}

/// The settings of a start with a good key file in `dir`.
fn cli(dir: &TestDir) -> Cli {
    let path = key_file(dir, "key", "AKID\nsecret\n", 0o600);
    Cli::from_config_text(&text_with_key(path.to_str().unwrap())).unwrap()
}

/// Asserts that a duration is a whole number with a unit of `ms`, `s`, `m` or `h`, and that a
/// number without a unit, a unit without a number, other units, fractions, signs, white space
/// and numbers too large are refused with a message that says what is expected.
#[test]
fn a_duration_is_a_number_and_a_unit() {
    let valid = [
        ("0s", 0),
        ("250ms", 250),
        ("10s", 10_000),
        ("15m", 900_000),
        ("2h", 7_200_000),
        ("007ms", 7),
    ];
    for (text, millis) in valid {
        let want = Duration::from_millis(millis);
        assert_eq!(parse_duration(text), Ok(want), "{text}");
    }
    let invalid = [
        "",
        "10",
        "ms",
        "10x",
        "1.5s",
        "-5s",
        " 5s",
        "18446744073709551615h",
        "99999999999999999999999s",
    ];
    for text in invalid {
        let why = parse_duration(text).unwrap_err();
        assert!(why.contains("is not a duration"), "{text}: {why}");
    }
}

/// Asserts that the keys that have defaults have them: the water marks at 80, 70 and 95
/// percent, a drain of 10 seconds and 15 minutes, a journal linger of 20 milliseconds, and so on.
#[test]
fn the_keys_have_their_defaults() {
    let cli = Cli::from_config_text(MINIMAL).unwrap();
    assert_eq!(cli.listen, vec!["127.0.0.1:0".parse().unwrap()]);
    assert!(cli.oci_listen.is_empty());
    assert_eq!(cli.proxy_id, "p1");
    assert_eq!(cli.hot, "memory");
    assert_eq!(cli.cold, "memory");
    assert_eq!(cli.hot_bytes, 1000);
    assert_eq!(cli.bucket, "tier-bucket");
    assert_eq!(cli.region, "us-east-1");
    assert_eq!((cli.high_water, cli.low_water, cli.critical), (80, 70, 95));
    assert_eq!(cli.drain_lb_wait, Duration::from_secs(10));
    assert_eq!(cli.drain_deadline, Duration::from_secs(900));
    assert_eq!(cli.journal_linger, Duration::from_millis(20));
    assert_eq!(cli.journal_retention, Duration::from_secs(24 * 3600));
    assert_eq!(cli.copy_parallel, 16);
    assert_eq!(cli.lease_ttl, Duration::from_secs(30));
    assert_eq!(cli.usage_stale_after, Duration::from_secs(120));
    assert_eq!(cli.reconcile_interval, Duration::from_secs(5));
    assert_eq!(cli.max_request_bytes, 256 << 20);
    assert_eq!(cli.max_concurrent_puts, 32);
    assert_eq!(cli.put_permit_wait, Duration::from_secs(15));
    assert_eq!(cli.put_min_bytes_per_sec, 1024);
    assert_eq!(cli.put_rate_grace, Duration::from_secs(30));
    assert_eq!(cli.body_idle_timeout, Duration::from_secs(30));
    assert_eq!(cli.s3_request_timeout, Duration::from_secs(60));
    let s3_flags = [
        cli.hot_endpoint.is_some(),
        cli.hot_region.is_some(),
        cli.hot_credentials_file.is_some(),
        cli.cold_endpoint.is_some(),
        cli.cold_region.is_some(),
        cli.cold_credentials_file.is_some(),
    ];
    assert!(!s3_flags.contains(&true), "no S3 flag by default");
    assert!(cli.read_only_key_file.is_empty());
    assert!(cli.presign_key_file.is_empty());
    assert!(cli.gc_key_file.is_empty());
    assert!(cli.scoped_key_file.is_empty());
    assert!(cli.grant_key_file.is_empty());
    assert_eq!(cli.presign_max_expires, Duration::from_secs(900));
    assert_eq!(cli.grant_max_lifetime, None);
    assert!(
        cli.skip_backend_check.is_empty(),
        "no check is skipped by default"
    );
    assert_eq!(cli.backend_check_wait, Duration::from_secs(300));
}

/// Asserts that a slot given in the file reaches the settings of the server, where `/metricz`
/// reads it, and that a start without one leaves them without.
#[test]
fn the_slot_reaches_the_server_settings() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-slot");
    let mut cli = cli(&dir);
    assert_eq!(build(&cli).expect("built").server.slot, None);
    cli.slot = Some(9301);
    assert_eq!(build(&cli).expect("built").server.slot, Some(9301));
}

/// Asserts that the help names every flag of a run and none that went, and that the version names
/// the package version.
#[test]
fn the_help_names_the_flags_of_a_run_and_the_version_the_package() {
    let help = Args::command().render_long_help().to_string();
    for name in [
        "--config",
        "--check-config",
        "--capabilities",
        "--drain-lb-wait",
        "--drain-deadline",
    ] {
        assert!(help.contains(name), "{name}");
    }
    for gone in ["--listen", "--hot", "--print-config", "--oci-listen"] {
        assert!(!help.contains(gone), "{gone}");
    }
    let version = Args::try_parse_from(["tierstore", "--version"]);
    let shown = version.unwrap_err().to_string();
    assert!(
        shown.contains(crate::server::build_info::RELEASE_VERSION),
        "{shown}"
    );
}

/// Asserts that a storage node's file reaches the built server settings: the front of the file
/// of `GOLDEN` (a `put_permit_wait` of 25 s, not the 15 s a `ServerConfig` has by itself), given
/// a key file that exists, makes a server whose wait for a put permit is 25 s.
#[test]
fn the_put_permit_wait_of_a_file_reaches_the_built_server() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-permit-wait-file");
    let from_file = crate::server::config::from_text(
        crate::server::config_testing::GOLDEN,
        crate::server::config_testing::DEFAULTS,
    )
    .unwrap();
    let mut front = from_file.front();
    assert_eq!(front.put_permit_wait, Duration::from_secs(25));
    front.access_key_file = key_file(&dir, "key", "AKID\nsecret\n", 0o600);
    front.read_only_key_file = Vec::new();
    front.presign_key_file = Vec::new();
    front.gc_key_file = Vec::new();
    front.scoped_key_file = Vec::new();
    front.grant_key_file = Vec::new();
    front.tls = crate::server::front::S3Tls::default();
    let server = crate::server::front::server_config(&front, Arc::new(crate::SystemClock), false)
        .expect("a server");
    assert_eq!(server.put_permit_wait, Duration::from_secs(25));
}
