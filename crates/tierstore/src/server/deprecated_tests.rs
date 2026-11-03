//! The two flags and keys of the removed start-up self-test: still accepted, and ignored with a
//! warning.

use std::fs::Permissions;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use tierstore_testkit::dir::TestDir;
use tracing::Level;
use tracing::subscriber::with_default;

use super::warnings;
use crate::server::build_info::stderr_log::LineLog;
use crate::server::cli::{Cli, build};
use crate::server::config::{from_text, settings_from_text};
use crate::server::config_testing::{DEFAULTS, GOLDEN, golden_flags};

/// The warning of `--skip-backend-check`, word for word.
const SKIP_WARNING: &str = "tierstore: --skip-backend-check (cold_store.skip_checks) is deprecated and ignored: tierstore no longer tests its stores at start-up; remove it";

/// The warning of `--backend-check-wait`, word for word.
const WAIT_WARNING: &str = "tierstore: --backend-check-wait (server.backend_check_wait) is deprecated and ignored: tierstore no longer tests its stores at start-up; remove it";

/// A writer whose bytes the test reads back: the operator's stderr.
#[derive(Clone, Default)]
struct Stderr(Arc<Mutex<Vec<u8>>>);

impl Stderr {
    fn text(&self) -> String {
        let bytes = self.0.lock().expect("the buffer is not poisoned").clone();
        String::from_utf8(bytes).expect("the lines are text")
    }
}

impl Write for Stderr {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut held = self.0.lock().expect("the buffer is not poisoned");
        held.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The flags of a start with the key file `key`, then `more`.
fn flags(key: &str, more: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = [
        "tierstore",
        "--listen=127.0.0.1:0",
        "--proxy-id=p1",
        "--hot=memory",
        "--cold=memory",
        "--hot-bytes=1000",
        "--bucket=tier-bucket",
    ]
    .map(str::to_owned)
    .to_vec();
    args.push(format!("--access-key-file={key}"));
    args.extend(more.iter().map(|flag| (*flag).to_owned()));
    args
}

/// Asserts the text of the warnings, which is the whole of what the operator is told: one line
/// for each setting given, in the order `--skip-backend-check`, `--backend-check-wait`, and none
/// for a setting not given.
#[test]
fn each_setting_given_is_one_warning_and_a_setting_not_given_is_none() {
    let skipped = vec!["multipart-conditional=because".to_owned()];
    let wait = Some(Duration::from_secs(5));
    assert_eq!(warnings(&skipped, wait), [SKIP_WARNING, WAIT_WARNING]);
    assert_eq!(warnings(&skipped, None), [SKIP_WARNING]);
    assert_eq!(warnings(&[], wait), [WAIT_WARNING]);
    assert_eq!(warnings(&[], None), Vec::<String>::new());
}

/// Asserts that the two flags of the removed self-test, which the release before had, are still
/// accepted so that an old unit starts: with a check that no longer exists and a wait, the flags
/// parse (the reason is not looked at), the settings are built, and the operator's stderr gets one
/// warning for each flag, word for word, and nothing else; with neither flag it gets nothing; and
/// a wait that is not a duration is still refused.
#[test]
fn the_flags_of_the_removed_self_test_start_a_server_and_each_warns_once() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-self-test-flags");
    let key = dir.path().join("key");
    std::fs::write(&key, "AKID\nsecret\n").unwrap();
    std::fs::set_permissions(&key, Permissions::from_mode(0o600)).unwrap();
    let key = key.to_str().unwrap();
    let old = flags(
        key,
        &[
            "--skip-backend-check=no-such-check=a reason",
            "--backend-check-wait=5m",
        ],
    );
    let cli = Cli::try_parse_from(old).expect("the old flags parse");
    assert_eq!(cli.skip_backend_check, ["no-such-check=a reason"]);
    assert_eq!(cli.backend_check_wait, Some(Duration::from_secs(300)));
    let stderr = Stderr::default();
    let built = with_default(LineLog::new(stderr.clone(), Level::INFO), || build(&cli));
    assert!(built.is_ok(), "an old unit still starts");
    assert_eq!(stderr.text(), format!("{SKIP_WARNING}\n{WAIT_WARNING}\n"));

    let quiet = Stderr::default();
    let plain = Cli::try_parse_from(flags(key, &[])).unwrap();
    let built = with_default(LineLog::new(quiet.clone(), Level::INFO), || build(&plain));
    assert!(built.is_ok());
    assert_eq!(quiet.text(), "");

    let soon = flags(key, &["--backend-check-wait=soon"]);
    assert!(Cli::try_parse_from(soon).is_err());
}

/// Asserts that a file written for the release before, which sets `skip_checks` in `[cold_store]`
/// and `backend_check_wait` in `[server]`, still loads: the settings are those the same flags
/// make (the values are kept to be ignored with a warning at the start, and are not checked), and a
/// file without the keys makes settings without them.
#[test]
fn a_file_with_the_keys_of_the_removed_self_test_still_loads() {
    let old = GOLDEN
        .replacen(
            "s3_request_timeout    = \"2m\"",
            "s3_request_timeout    = \"2m\"\nbackend_check_wait    = \"90s\"",
            1,
        )
        .replacen(
            "credentials_file = \"/etc/tierstore/cold.credentials\"",
            "credentials_file = \"/etc/tierstore/cold.credentials\"\nskip_checks = [\"no-such-check=a reason\"]",
            1,
        );
    let from_file = from_text(&old, DEFAULTS).unwrap();
    let mut flags = golden_flags();
    flags.extend([
        "--backend-check-wait=90s",
        "--skip-backend-check=no-such-check=a reason",
    ]);
    let from_flags = Cli::try_parse_from(flags).unwrap();
    assert_eq!(from_file, from_flags);
    assert_eq!(from_file.backend_check_wait, Some(Duration::from_secs(90)));
    assert_eq!(from_file.skip_backend_check, ["no-such-check=a reason"]);
    let without = from_text(GOLDEN, DEFAULTS).unwrap();
    assert!(without.skip_backend_check.is_empty());
    assert_eq!(without.backend_check_wait, None);
    assert!(settings_from_text(&old, DEFAULTS).is_ok());
}
