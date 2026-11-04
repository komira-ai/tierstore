//! The two keys of the removed read grant: still accepted, and ignored with a warning.

use std::fs::Permissions;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;
use tracing::Level;
use tracing::subscriber::with_default;

use super::{Cli, build};
use crate::server::build_info::stderr_log::LineLog;
use crate::server::config_testing::MINIMAL;

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

/// The file of a start with the key file `key`, and `more` after `presign_max_expires` in
/// `[server]`.
fn text(key: &str, more: &str) -> String {
    let given = format!("access_key_file = \"{key}\"\n{more}");
    assert!(MINIMAL.contains("access_key_file = \"key\""));
    MINIMAL.replacen("access_key_file = \"key\"", &given, 1)
}

/// Asserts that the two keys of the removed read grant, which the release before had, are still
/// accepted so that an old file starts: with a grant key file that does not exist (it is not read)
/// and a lifetime, the keys are read, the settings are built, and the operator's stderr gets one
/// warning for each flag, word for word, naming the key and saying to use a
/// presigned GET URL; with neither key it gets nothing; and a lifetime that is not a duration is
/// still refused.
#[test]
fn the_keys_of_the_removed_read_grant_start_a_server_and_each_warns_once() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-grant-flags");
    let key = dir.path().join("key");
    std::fs::write(&key, "AKID\nsecret\n").unwrap();
    std::fs::set_permissions(&key, Permissions::from_mode(0o600)).unwrap();
    let key = key.to_str().unwrap();
    let missing = dir.path().join("no-such-grant.key");
    let grant = format!(
        "grant_key_files = [\"{}\"]\ngrant_max_lifetime = \"5m\"",
        missing.display()
    );
    let cli = Cli::from_config_text(&text(key, &grant)).expect("the old keys are read");
    assert_eq!(cli.grant_key_file, vec![missing]);
    assert_eq!(cli.grant_max_lifetime, Some(Duration::from_secs(300)));
    let stderr = Stderr::default();
    let built = with_default(LineLog::new(stderr.clone(), Level::INFO), || build(&cli));
    assert!(built.is_ok(), "an old unit still starts");
    let warn = "is ignored: the read grant was removed; use a presigned GET URL";
    let wanted = format!(
        "tierstore: server.grant_key_files {warn}\n\
         tierstore: server.grant_max_lifetime {warn}\n"
    );
    assert_eq!(stderr.text(), wanted);

    let quiet = Stderr::default();
    let plain = Cli::from_config_text(&text(key, "")).unwrap();
    let built = with_default(LineLog::new(quiet.clone(), Level::INFO), || build(&plain));
    assert!(built.is_ok());
    assert_eq!(quiet.text(), "");

    let soon = text(key, "grant_max_lifetime = \"soon\"");
    assert!(Cli::from_config_text(&soon).is_err());
}
