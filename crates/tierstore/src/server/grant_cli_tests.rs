//! The two flags of the removed read grant: still accepted, and ignored with a warning.

use std::fs::Permissions;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use tierstore_testkit::dir::TestDir;
use tracing::Level;
use tracing::subscriber::with_default;

use super::{Cli, build};
use crate::server::build_info::stderr_log::LineLog;

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

/// Asserts that the two flags of the removed read grant, which the release before had, are still
/// accepted so that an old unit starts: with a grant key file that does not exist (it is not read)
/// and a lifetime, the flags parse, the settings are built, and the operator's stderr gets one
/// warning for each flag, word for word, naming the flag and its key and saying to use a
/// presigned GET URL; with neither flag it gets nothing; and a lifetime that is not a duration is
/// still refused.
#[test]
fn the_flags_of_the_removed_read_grant_start_a_server_and_each_warns_once() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-grant-flags");
    let key = dir.path().join("key");
    std::fs::write(&key, "AKID\nsecret-0123456789abcdef\n").unwrap();
    std::fs::set_permissions(&key, Permissions::from_mode(0o600)).unwrap();
    let key = key.to_str().unwrap();
    let missing = dir.path().join("no-such-grant.key");
    let grant = format!("--grant-key-file={}", missing.display());
    let old = flags(key, &[&grant, "--grant-max-lifetime=5m"]);
    let cli = Cli::try_parse_from(old).expect("the old flags parse");
    assert_eq!(cli.grant_key_file, vec![missing]);
    assert_eq!(cli.grant_max_lifetime, Some(Duration::from_secs(300)));
    let stderr = Stderr::default();
    let built = with_default(LineLog::new(stderr.clone(), Level::INFO), || build(&cli));
    assert!(built.is_ok(), "an old unit still starts");
    let warn = "is ignored: the read grant was removed; use a presigned GET URL";
    let wanted = format!(
        "tierstore: --grant-key-file (server.grant_key_files) {warn}\n\
         tierstore: --grant-max-lifetime (server.grant_max_lifetime) {warn}\n"
    );
    assert_eq!(stderr.text(), wanted);

    let quiet = Stderr::default();
    let plain = Cli::try_parse_from(flags(key, &[])).unwrap();
    let built = with_default(LineLog::new(quiet.clone(), Level::INFO), || build(&plain));
    assert!(built.is_ok());
    assert_eq!(quiet.text(), "");

    let soon = flags(key, &["--grant-max-lifetime=soon"]);
    assert!(Cli::try_parse_from(soon).is_err());
}
