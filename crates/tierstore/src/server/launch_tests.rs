//! From the command line to the settings: the file `--config` names, the knobs of the run, and
//! the refusal of each flag that went.

use std::ffi::OsString;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::{Refused, resolve, state_dir_of};
use crate::server::config::from_text;
use crate::server::config_testing::{DEFAULTS, GOLDEN};

fn words(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

/// A directory with the golden file in it, and the file's path as a flag value.
fn with_file(name: &str) -> (TestDir, String) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let path = dir.path().join("tierstore.toml");
    std::fs::write(&path, GOLDEN).unwrap();
    let path = path.display().to_string();
    (dir, path)
}

/// The refusal of a command line that has none of settings: which kind it is, whether the
/// parser would write it to stderr, and its text.
fn refused(args: &[&str]) -> (&'static str, bool, String) {
    match resolve(words(args)).unwrap_err() {
        Refused::Usage(usage) => ("usage", usage.use_stderr(), usage.to_string()),
        Refused::Config(why) => ("config", true, why.to_string()),
    }
}

/// Asserts that with `--config` the settings are the file's, with the two parts of the drain from
/// the flags when they are given and their defaults when not, and that `--check-config` says
/// the settings are only to be checked.
#[test]
fn a_file_makes_the_settings_and_the_drain_stays_a_flag() {
    let (_dir, path) = with_file("tierstore-launch-file");
    let launch = resolve(words(&["tierstore", "--config", &path])).unwrap();
    assert!(!launch.check_only);
    assert_eq!(
        launch.settings.into_storage(),
        Some(from_text(GOLDEN, DEFAULTS).unwrap())
    );
    let given = [
        "tierstore",
        "--drain-lb-wait",
        "2s",
        "--config",
        &path,
        "--drain-deadline",
        "4m",
        "--check-config",
    ];
    let launch = resolve(words(&given)).unwrap();
    assert!(launch.check_only);
    let cli = launch.settings.into_storage().unwrap();
    assert_eq!(cli.drain_lb_wait, Duration::from_secs(2));
    assert_eq!(cli.drain_deadline, Duration::from_secs(240));
    let mut expected = from_text(GOLDEN, DEFAULTS).unwrap();
    expected.drain_lb_wait = cli.drain_lb_wait;
    expected.drain_deadline = cli.drain_deadline;
    assert_eq!(cli, expected);
}

/// Asserts that each flag that used to describe the data path is refused, in either spelling
/// (`--flag value` and `--flag=value`) and wherever it stands, with the message that names the key
/// of the file that replaces it, even with a file that is fine.
#[test]
fn a_flag_that_went_is_refused_naming_its_key() {
    let (_dir, path) = with_file("tierstore-launch-removed");
    let cases = [
        (
            vec!["--listen", "127.0.0.1:1"],
            "--listen was removed: the file given with --config holds it; declare an [[interface]] with type = \"s3\" and params port or addresses",
        ),
        (
            vec!["--oci-listen=127.0.0.1:1"],
            "--oci-listen was removed: the file given with --config holds it; declare an [[interface]] with type = \"oci\" and params port or addresses",
        ),
        (
            vec!["--hot", "memory"],
            "--hot was removed: the file given with --config holds it; set hot_store.kind, hot_store.path, hot_store.bucket",
        ),
        (
            vec!["--region=us-east-1"],
            "--region was removed: the file given with --config holds it; set server.region",
        ),
        (
            vec!["--oci-tls-cert", "/c"],
            "--oci-tls-cert was removed: the file given with --config holds it; set params.tls_cert_file of an oci interface",
        ),
        (
            vec!["--print-config"],
            "--print-config was removed: there are no flags to print a file for; write the file (docs/configuration.md)",
        ),
    ];
    for (flags, want) in cases {
        let mut with_config = vec!["tierstore", "--config", &path];
        with_config.extend(&flags);
        let (kind, stderr, text) = refused(&with_config);
        assert_eq!((kind, stderr), ("config", true), "{flags:?}");
        assert_eq!(text, want, "{flags:?}");
        let mut alone = vec!["tierstore"];
        alone.extend(&flags);
        assert_eq!(refused(&alone).2, want, "{flags:?}");
    }
    // The first of several is the one named, and the per-run flags are not among them.
    let (_, _, text) = refused(&[
        "tierstore",
        "--drain-lb-wait",
        "1s",
        "--bucket",
        "b",
        "--hot",
        "memory",
    ]);
    assert!(text.starts_with("--bucket was removed:"), "{text}");
}

/// Asserts that a start without `--config` is refused with a message that says the settings are
/// in a file, that `--check-config` without a file says what it needs, and that a file that is not
/// there is a refusal that names it.
#[test]
fn a_start_needs_a_file_and_a_missing_file_is_named() {
    let (kind, _, why) = refused(&["tierstore"]);
    assert_eq!(kind, "config");
    assert_eq!(
        why,
        "--config is needed: the settings are in a file (docs/configuration.md), and tierstore takes no flag for them"
    );
    let (kind, _, why) = refused(&["tierstore", "--check-config"]);
    assert_eq!(kind, "config");
    assert_eq!(
        why,
        "--check-config checks the file that --config names; give --config"
    );
    let (kind, _, why) = refused(&["tierstore", "--config", "/no/such/tierstore.toml"]);
    assert_eq!(kind, "config");
    assert!(why.contains("reading the config file"), "{why}");
    assert!(why.contains("/no/such/tierstore.toml"), "{why}");
}

/// Asserts that the usage and the version are shown (status 0) with or without a file, that a
/// flag that is not known and an invalid value are refused as usage errors (status 2), and that a
/// refused file is not one of them.
#[test]
fn the_usage_is_the_parsers_with_or_without_a_file() {
    let (_dir, path) = with_file("tierstore-launch-usage");
    let shown = [
        vec!["tierstore", "--help"],
        vec!["tierstore", "--version"],
        vec!["tierstore", "--config", &path, "--help"],
    ];
    for args in shown {
        let (kind, stderr, _) = refused(&args);
        assert_eq!(kind, "usage", "{args:?}");
        assert!(!stderr, "{args:?}");
    }
    let wrong = [
        vec!["tierstore", "--no-such-flag"],
        vec!["tierstore", "--config", &path, "--no-such-flag"],
        vec!["tierstore", "--config", &path, "--drain-lb-wait", "soon"],
    ];
    for args in wrong {
        let (kind, stderr, _) = refused(&args);
        assert_eq!(kind, "usage", "{args:?}");
        assert!(stderr, "{args:?}");
    }
}

/// Asserts that the state directory is the first directory of systemd's `STATE_DIRECTORY` when it
/// is an absolute path, and none when it is unset, empty, relative, or not text.
#[test]
fn the_state_directory_is_the_first_absolute_one_systemd_names() {
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;
    let of = |value: &str| state_dir_of(Some(OsString::from(value)));
    assert_eq!(
        of("/var/lib/tierstore"),
        Some(PathBuf::from("/var/lib/tierstore"))
    );
    assert_eq!(
        of("/var/lib/a:/var/lib/b"),
        Some(PathBuf::from("/var/lib/a"))
    );
    assert_eq!(of(""), None);
    assert_eq!(of("tierstore"), None);
    assert_eq!(state_dir_of(None), None);
    assert_eq!(
        state_dir_of(Some(OsString::from_vec(vec![0xff, b'/']))),
        None
    );
}

/// Asserts that a drain flag and the same part of the drain in the file are refused together by
/// the whole command line, with status 2's kind of refusal (a config error), and the flag alone
/// still works.
#[test]
fn a_drain_flag_with_the_key_in_the_file_is_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-launch-drain-key");
    let path = dir.path().join("tierstore.toml");
    let text = GOLDEN.replacen(
        "min_secret_bytes      = 24",
        "min_secret_bytes      = 24\ndrain_deadline = \"1m\"",
        1,
    );
    std::fs::write(&path, text).unwrap();
    let path = path.display().to_string();
    let (kind, _, why) = refused(&["tierstore", "--config", &path, "--drain-deadline", "2m"]);
    assert_eq!(kind, "config");
    assert!(
        why.contains("server.drain_deadline: is set in the file and by --drain-deadline too"),
        "{why}"
    );
    let launch = resolve(words(&[
        "tierstore",
        "--config",
        &path,
        "--drain-lb-wait",
        "2s",
    ]))
    .unwrap();
    let cli = launch.settings.into_storage().unwrap();
    assert_eq!(cli.drain_lb_wait, Duration::from_secs(2));
    assert_eq!(cli.drain_deadline, Duration::from_secs(60));
}
