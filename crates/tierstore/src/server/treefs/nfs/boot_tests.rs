//! The epoch of the boot.

use std::path::Path;

use tierstore_testkit::dir::TestDir;

use super::{BootSource, epoch_of};
use crate::server::treefs::nfs::error::NfsError;

/// Asserts that the identity is read from the file when there is one, that the same identity gives
/// the same epoch and another gives another, and that a file's trailing newline is no part of it.
#[test]
fn the_epoch_follows_the_boot_identity_in_the_file() {
    let dir = TestDir::under(std::env::temp_dir(), "nfs-boot-file");
    let file = dir.path().join("boot_id");
    std::fs::write(&file, "aaaa-bbbb\n").unwrap();
    let source = BootSource::of(&file, "false", &[]);
    let first = source.epoch().unwrap();
    assert_eq!(first, BootSource::of(&file, "false", &[]).epoch().unwrap());
    assert_eq!(first, epoch_of("aaaa-bbbb"));
    std::fs::write(&file, "cccc-dddd\n").unwrap();
    assert_ne!(source.epoch().unwrap(), first);
}

/// Asserts that without the file the identity is what the command prints, and that an epoch is
/// never 0, which is the epoch of a table that has none.
#[test]
fn the_epoch_follows_what_the_command_prints_when_there_is_no_file() {
    let source = BootSource::of(Path::new("/nonexistent/boot_id"), "echo", &["-n", "boot-7"]);
    assert_eq!(source.epoch().unwrap(), epoch_of("boot-7"));
    assert_ne!(epoch_of(""), 0);
}

/// Asserts that an identity that cannot be read is an error that says why: an empty identity, a
/// command that is not there, a command that fails.
#[test]
fn an_identity_that_cannot_be_read_is_an_error_that_says_why() {
    let nothing = Path::new("/nonexistent/boot_id");
    let empty = BootSource::of(nothing, "echo", &["-n"])
        .epoch()
        .unwrap_err();
    assert!(
        matches!(empty, NfsError::Boot(ref why) if why.contains("empty")),
        "{empty}"
    );
    let absent = BootSource::of(nothing, "/nonexistent/program", &[])
        .epoch()
        .unwrap_err();
    assert!(
        matches!(absent, NfsError::Boot(ref why) if why.contains("/nonexistent/program")),
        "{absent}"
    );
    let failing = BootSource::of(nothing, "ls", &["/nonexistent/dir"])
        .epoch()
        .unwrap_err();
    assert!(
        matches!(failing, NfsError::Boot(ref why) if why.starts_with("ls ")),
        "{failing}"
    );
}

/// Asserts that this machine's own source gives an epoch.
#[test]
fn this_machines_source_gives_an_epoch() {
    assert_ne!(BootSource::system().epoch().unwrap(), 0);
}
