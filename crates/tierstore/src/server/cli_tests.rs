//! The flags of the binary: their defaults, parsing and help. The settings made of them and the
//! checks run before a start are in the two modules below. The reconciler's loop is tested in
//! `cli_reconcile_tests`.

use std::fs::Permissions;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::super::cli_probe::probe_run_id;
use super::super::cli_probe::probe_tiers;
use super::super::cli_serve::cut_note;
use super::InvalidFlags;
use super::S3Flags;
use super::SkippedCheck;
use super::exit_status;
use super::open_tier;
use super::parse_skip;
use super::read_key;
use super::share;
use super::{Cli, build, parse_duration, tier};
use crate::Backend;
use crate::Check;
use crate::SelfTestError;
use crate::SelfTestFailure;
use crate::selftest_tests::Flaw;
use crate::selftest_tests::Flawed;
use crate::server::AccessKey;
use crate::server::Drained;
use clap::CommandFactory;
use clap::Parser;
use std::os::unix::fs::PermissionsExt;

#[path = "cli_tests/settings.rs"]
mod settings;
#[path = "cli_tests/startup.rs"]
mod startup;

/// The JSON form of a key file with the id `JSONID` and the secret `json-secret`.
const JSON_KEY: &str = r#"{"access_key_id": "JSONID", "secret_access_key": "json-secret"}"#;

/// The flags every start needs, with `overrides` in place of the flags they name or added.
fn flags(key_file: &str, overrides: &[(&str, &str)]) -> Vec<String> {
    let owned = |(flag, value): (&str, &str)| (flag.to_owned(), value.to_owned());
    let mut pairs: Vec<(String, String)> = [
        ("--listen", "127.0.0.1:0"),
        ("--proxy-id", "p1"),
        ("--hot", "memory"),
        ("--cold", "memory"),
        ("--hot-bytes", "1000"),
        ("--bucket", "tier-bucket"),
        ("--access-key-file", key_file),
    ]
    .into_iter()
    .map(owned)
    .collect();
    for (flag, value) in overrides {
        let taken = pairs.iter_mut().find(|(name, _)| name == flag);
        match taken {
            Some(slot) => slot.1 = (*value).to_owned(),
            None => pairs.push(owned((*flag, *value))),
        }
    }
    let mut args = vec!["tierstore".to_owned()];
    for (flag, value) in pairs {
        args.push(flag);
        args.push(value);
    }
    args
}

/// A key file in `dir`, called `name`, with this text and mode.
fn key_file(dir: &TestDir, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
    path
}

/// The flags of a start with a good key file in `dir`.
fn cli(dir: &TestDir) -> Cli {
    let path = key_file(dir, "key", "AKID\nsecret\n", 0o600);
    Cli::try_parse_from(flags(path.to_str().unwrap(), &[])).unwrap()
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

/// Asserts that the flags that have defaults have them: the water marks at 80, 70 and 95
/// percent, a drain of 10 seconds and 15 minutes, 16 parallel copies, and so on.
#[test]
fn the_flags_have_their_defaults() {
    let cli = Cli::try_parse_from(flags("key", &[])).unwrap();
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
    assert_eq!(cli.copy_parallel, 16);
    assert_eq!(cli.lease_ttl, Duration::from_secs(30));
    assert_eq!(cli.usage_stale_after, Duration::from_secs(120));
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

/// Asserts that `--read-only-key-file` can be given any number of times, and that the files are
/// kept in the order of the flags.
#[test]
fn the_read_only_key_flag_is_repeatable() {
    let mut args = flags("key", &[]);
    let repeated = [
        "--read-only-key-file",
        "first",
        "--read-only-key-file",
        "second",
    ];
    args.extend(repeated.map(str::to_owned));
    let cli = Cli::try_parse_from(args).unwrap();
    let files = [PathBuf::from("first"), PathBuf::from("second")];
    assert_eq!(cli.read_only_key_file, files);
}

/// Asserts that every flag that has a default can be given, and is read.
#[test]
fn the_flags_can_all_be_given() {
    let given = [
        ("--region", "eu-west-1"),
        ("--high-water", "90"),
        ("--low-water", "50"),
        ("--critical", "100"),
        ("--drain-lb-wait", "2s"),
        ("--drain-deadline", "3m"),
        ("--copy-parallel", "4"),
        ("--lease-ttl", "20s"),
        ("--usage-stale-after", "45s"),
        ("--max-request-bytes", "10"),
        ("--max-concurrent-puts", "3"),
        ("--put-permit-wait", "4s"),
        ("--put-min-bytes-per-sec", "5"),
        ("--put-rate-grace", "2s"),
        ("--body-idle-timeout", "7s"),
        ("--s3-request-timeout", "90s"),
        ("--backend-check-wait", "2m"),
        ("--hot-endpoint", "http://127.0.0.1:9000"),
        ("--hot-region", "eu-west-1"),
        ("--hot-credentials-file", "hot-key"),
        ("--cold-endpoint", "http://127.0.0.1:9001"),
        ("--cold-region", "eu-west-2"),
        ("--cold-credentials-file", "cold-key"),
    ];
    let cli = Cli::try_parse_from(flags("key", &given)).unwrap();
    assert_eq!(cli.region, "eu-west-1");
    let marks = (cli.high_water, cli.low_water, cli.critical);
    assert_eq!(marks, (90, 50, 100));
    assert_eq!(cli.drain_lb_wait, Duration::from_secs(2));
    assert_eq!(cli.drain_deadline, Duration::from_secs(180));
    assert_eq!(cli.copy_parallel, 4);
    assert_eq!(cli.lease_ttl, Duration::from_secs(20));
    assert_eq!(cli.usage_stale_after, Duration::from_secs(45));
    assert_eq!(cli.max_request_bytes, 10);
    assert_eq!(cli.max_concurrent_puts, 3);
    assert_eq!(cli.put_permit_wait, Duration::from_secs(4));
    assert_eq!(cli.put_min_bytes_per_sec, 5);
    assert_eq!(cli.put_rate_grace, Duration::from_secs(2));
    assert_eq!(cli.body_idle_timeout, Duration::from_secs(7));
    assert_eq!(cli.s3_request_timeout, Duration::from_secs(90));
    assert_eq!(cli.backend_check_wait, Duration::from_secs(120));
    let hot = cli.hot_s3();
    assert_eq!(hot.endpoint, Some("http://127.0.0.1:9000"));
    assert_eq!(hot.region, Some("eu-west-1"));
    assert_eq!(hot.credentials, Some(Path::new("hot-key")));
    let cold = cli.cold_s3();
    assert_eq!(cold.endpoint, Some("http://127.0.0.1:9001"));
    assert_eq!(cold.region, Some("eu-west-2"));
    assert_eq!(cold.credentials, Some(Path::new("cold-key")));
}

/// Asserts that flags that are not valid are refused when they are read: one that is not known,
/// a water mark of zero or over 100, a size or a count of zero, a duration that is not one, an
/// address that is not one, and a flag that is missing.
#[test]
fn flags_that_are_not_valid_are_refused() {
    let parse = |overrides: &[(&str, &str)]| Cli::try_parse_from(flags("key", overrides));
    assert!(parse(&[]).is_ok());
    let bad = [
        ("--no-such-flag", "1"),
        ("--high-water", "0"),
        ("--low-water", "101"),
        ("--critical", "x"),
        ("--hot-bytes", "0"),
        ("--max-request-bytes", "0"),
        ("--copy-parallel", "0"),
        ("--drain-lb-wait", "soon"),
        ("--listen", "nowhere"),
    ];
    for (flag, value) in bad {
        assert!(parse(&[(flag, value)]).is_err(), "{flag} {value}");
    }
    let mut args = flags("key", &[]);
    let at = args.iter().position(|arg| arg == "--bucket").unwrap();
    args.remove(at + 1);
    args.remove(at);
    assert!(Cli::try_parse_from(args).is_err());
}

/// Asserts that `--slot` is a port: absent it is none, and given it is the number, 1 to 65535;
/// zero, a number past a port, a sign, a word and an empty value are refused at the parse (the
/// exit status 2 of a flag that is not valid) with a message that names `--slot`.
#[test]
fn the_slot_flag_is_a_port_from_1_to_65535() {
    let parse = |value: &str| Cli::try_parse_from(flags("key", &[("--slot", value)]));
    assert_eq!(Cli::try_parse_from(flags("key", &[])).unwrap().slot, None);
    for (text, port) in [("1", 1), ("9301", 9301), ("65535", 65535)] {
        assert_eq!(parse(text).unwrap().slot, Some(port), "{text}");
    }
    for text in ["0", "65536", "+", "nine", "93x1", ""] {
        let why = parse(text).expect_err(text).to_string();
        assert!(why.contains("--slot"), "{text}: {why}");
    }
    assert!(parse("-1").is_err());
}

/// Asserts that the reconciler of a process is named apart from its proxy, so that the entry of
/// a client put of the proxy is never taken for a copy of the reconciler that holds the lease.
#[test]
fn the_reconciler_has_a_name_of_its_own() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-names");
    let cli = cli(&dir);
    let built = build(&cli).expect("built");
    assert_eq!(built.proxy.proxy_id, cli.proxy_id);
    assert_eq!(
        built.reconciler.holder,
        format!("{}/reconciler", cli.proxy_id)
    );
    assert_ne!(built.reconciler.holder, built.proxy.proxy_id);
}

/// Asserts that a slot given as a flag reaches the settings of the server, where `/metricz`
/// reads it, and that a start without one leaves them without.
#[test]
fn the_slot_reaches_the_server_settings() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-slot");
    let mut cli = cli(&dir);
    assert_eq!(build(&cli).expect("built").server.slot, None);
    cli.slot = Some(9301);
    assert_eq!(build(&cli).expect("built").server.slot, Some(9301));
}

/// Asserts that the help names every flag, and that the version names the package version.
#[test]
fn the_help_names_every_flag_and_the_version_the_package() {
    let help = Cli::command().render_long_help().to_string();
    let names = [
        "--config",
        "--print-config",
        "--listen",
        "--proxy-id",
        "--slot",
        "--hot",
        "--cold",
        "--hot-bytes",
        "--high-water",
        "--low-water",
        "--critical",
        "--bucket",
        "--region",
        "--access-key-file",
        "--read-only-key-file",
        "--drain-lb-wait",
        "--drain-deadline",
        "--copy-parallel",
        "--lease-ttl",
        "--max-request-bytes",
        "--body-idle-timeout",
        "--s3-request-timeout",
        "--hot-endpoint",
        "--hot-region",
        "--hot-credentials-file",
        "--cold-endpoint",
        "--cold-region",
        "--cold-credentials-file",
        "--skip-backend-check",
        "--backend-check-wait",
    ];
    for name in names {
        assert!(help.contains(name), "{name}");
    }
    let version = Cli::try_parse_from(["tierstore", "--version"]);
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
