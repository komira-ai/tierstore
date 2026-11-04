//! The two keys of the removed start-up self-test: still accepted, and ignored with a
//! warning.

use std::fs::Permissions;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;
use tracing::Level;
use tracing::subscriber::with_default;

use super::warnings;
use crate::server::build_info::stderr_log::LineLog;
use crate::server::cli::build;
use crate::server::config::{from_text, settings_from_text};
use crate::server::config_testing::{DEFAULTS, GOLDEN, MINIMAL};

/// The warning of `cold_store.skip_checks`, word for word.
const SKIP_WARNING: &str = "tierstore: cold_store.skip_checks is deprecated and ignored: tierstore no longer tests its stores at start-up; remove it";

/// The warning of `server.backend_check_wait`, word for word.
const WAIT_WARNING: &str = "tierstore: server.backend_check_wait is deprecated and ignored: tierstore no longer tests its stores at start-up; remove it";

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

/// Asserts the text of the warnings, which is the whole of what the operator is told: one line
/// for each setting given, in the order `cold_store.skip_checks`, `server.backend_check_wait`, and none
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

/// Asserts that the two keys of the removed self-test, which the release before had, are still
/// accepted so that an old file starts: with a check that no longer exists and a wait, the file
/// loads (the reason is not looked at), the settings are built, and the operator's stderr gets one
/// warning for each key, word for word, and nothing else; with neither key it gets nothing; and a
/// wait that is not a duration is still refused.
#[test]
fn the_keys_of_the_removed_self_test_start_a_server_and_each_warns_once() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-self-test-keys");
    let key = dir.path().join("key");
    std::fs::write(&key, "AKID\nsecret-0123456789\n").unwrap();
    std::fs::set_permissions(&key, Permissions::from_mode(0o600)).unwrap();
    let key = key.to_str().unwrap();
    let plain = MINIMAL.replacen(
        "access_key_file = \"key\"",
        &format!("access_key_file = \"{key}\""),
        1,
    );
    let old = plain
        .replacen(
            "bucket          = \"tier-bucket\"",
            "bucket          = \"tier-bucket\"\nbackend_check_wait = \"5m\"",
            1,
        )
        .replacen(
            "[cold_store]\nkind = \"memory\"",
            "[cold_store]\nkind = \"memory\"\nskip_checks = [\"no-such-check=a reason\"]",
            1,
        );
    let cli = from_text(&old, DEFAULTS).expect("the old keys load");
    assert_eq!(cli.skip_backend_check, ["no-such-check=a reason"]);
    assert_eq!(cli.backend_check_wait, Some(Duration::from_secs(300)));
    let stderr = Stderr::default();
    let built = with_default(LineLog::new(stderr.clone(), Level::INFO), || build(&cli));
    assert!(built.is_ok(), "an old file still starts");
    assert_eq!(stderr.text(), format!("{SKIP_WARNING}\n{WAIT_WARNING}\n"));

    let quiet = Stderr::default();
    let none = from_text(&plain, DEFAULTS).unwrap();
    let built = with_default(LineLog::new(quiet.clone(), Level::INFO), || build(&none));
    assert!(built.is_ok());
    assert_eq!(quiet.text(), "");

    let soon = plain.replacen(
        "bucket          = \"tier-bucket\"",
        "bucket          = \"tier-bucket\"\nbackend_check_wait = \"soon\"",
        1,
    );
    assert!(from_text(&soon, DEFAULTS).is_err());
}

/// Asserts that a file written for the release before, which sets `skip_checks` in `[cold_store]`
/// and `backend_check_wait` in `[server]`, still loads: the values are kept to be ignored with a
/// warning at the start, and are not checked, and a file without the keys makes settings without
/// them.
#[test]
fn a_file_with_the_keys_of_the_removed_self_test_still_loads() {
    let old = GOLDEN.replacen(
        "s3_request_timeout    = \"2m\"",
        "s3_request_timeout    = \"2m\"\nbackend_check_wait    = \"90s\"",
        1,
    ) + "skip_checks = [\"no-such-check=a reason\"]\n";
    let from_file = from_text(&old, DEFAULTS).unwrap();
    assert_eq!(from_file.backend_check_wait, Some(Duration::from_secs(90)));
    assert_eq!(from_file.skip_backend_check, ["no-such-check=a reason"]);
    let without = from_text(GOLDEN, DEFAULTS).unwrap();
    assert!(without.skip_backend_check.is_empty());
    assert_eq!(without.backend_check_wait, None);
    assert!(settings_from_text(&old, DEFAULTS).is_ok());
}
