//! The copy of a release's binary at `SERVICE_BINARY`, made and removed on a scratch root: what
//! it is made from, what it is made as, what it refuses, and that it replaces the old copy by a
//! rename.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use super::super::SERVICE_BINARY;
use super::super::ports::{Host, PortError, PortKind};
use super::SystemHost;
use super::service_binary::ServiceBinary;
use super::testing::{FakeRunner, LIMIT, me, scratch};

/// A release of the archive package, as hostkeeper names it, under the scratch root.
const RELEASE: &str = "/var/lib/hostkeeper/packages/tierstore/0.5.0";
/// Another one.
const PREVIOUS: &str = "/var/lib/hostkeeper/packages/tierstore/0.4.0";
/// The directories of the copy, below the root, outermost first.
const DIRS: [&str; 4] = [
    "usr",
    "usr/local",
    "usr/local/libexec",
    "usr/local/libexec/tierstore",
];

fn host<'a>(root: &Path, runner: &'a FakeRunner) -> SystemHost<'a> {
    SystemHost::new(root.to_path_buf(), runner, LIMIT).with_release_owner(me())
}

fn message(error: PortError) -> String {
    match error {
        PortError::Failed {
            kind: PortKind::Host,
            message,
        } => message,
        other => panic!("not a host failure: {other:?}"),
    }
}

fn mode(path: &Path, bits: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(bits)).unwrap();
}

fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

/// `absolute` under `root`.
fn at(root: &Path, absolute: &str) -> PathBuf {
    root.join(absolute.trim_start_matches('/'))
}

/// The copy under `root`.
fn copy(root: &Path) -> PathBuf {
    at(root, SERVICE_BINARY)
}

/// The copy's directory under `root`.
fn copy_dir(root: &Path) -> PathBuf {
    root.join(DIRS[3])
}

/// The names in the copy's directory, sorted.
fn names(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(copy_dir(root))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A release of the archive package at `release` under `root`, whose `bin/tierstore` holds
/// `bytes`, closed as hostkeeper closes a package: every directory from the packages directory
/// down to the version mode 0700.
fn archive_release(root: &Path, release: &str, bytes: &str) {
    let bin = at(root, release).join("bin");
    fs::create_dir_all(&bin).unwrap();
    mode(&bin, 0o755);
    fs::write(bin.join("tierstore"), bytes).unwrap();
    mode(&bin.join("tierstore"), 0o755);
    for dir in [
        "var/lib/hostkeeper/packages",
        "var/lib/hostkeeper/packages/tierstore",
    ] {
        mode(&root.join(dir), 0o700);
    }
    mode(&at(root, release), 0o700);
}

/// Makes the copy's directories under `root`, mode 0755, as an install leaves them.
fn copy_dirs(root: &Path) {
    for dir in DIRS {
        let path = root.join(dir);
        fs::create_dir_all(&path).unwrap();
        mode(&path, 0o755);
    }
}

/// Whether the test runs as root, which ignores the modes a test closes a directory with: such a
/// test then says so and checks nothing.
fn as_root(test: &str) -> bool {
    let root = me() == 0;
    if root {
        let _ = writeln!(
            std::io::stderr(),
            "{test}: skipped: root can enter and write a directory of any mode"
        );
    }
    root
}

/// Asserts that the binary of an archive release that hostkeeper has closed to every user but its
/// owner is copied, byte for byte, to `SERVICE_BINARY` under the root, as a file of mode 0755
/// owned by the hook's user, in directories it makes with mode 0755, and that nothing else is
/// left beside it. Catches a copy that cannot read a closed release, or that any user but root
/// cannot run.
#[test]
fn an_archive_release_s_binary_is_copied_out_of_its_closed_tree() {
    let (_guard, root) = scratch("binary-copied");
    archive_release(&root, RELEASE, "binary of 0.5.0");
    let runner = FakeRunner::replying(vec![]);
    host(&root, &runner)
        .install_binary(Path::new(RELEASE))
        .unwrap();
    let copy = copy(&root);
    assert_eq!(fs::read_to_string(&copy).unwrap(), "binary of 0.5.0");
    let meta = fs::symlink_metadata(&copy).unwrap();
    assert!(meta.file_type().is_file());
    assert_eq!(meta.mode() & 0o7777, 0o755);
    assert_eq!(meta.uid(), me());
    for dir in DIRS {
        assert_eq!(mode_of(&root.join(dir)), 0o755, "{dir}");
    }
    assert_eq!(names(&root), ["tierstore"]);
    assert!(runner.calls().is_empty());
}

/// Asserts that a second copy replaces the first by a rename: the path names a new file with the
/// new bytes, and a reader that opened the old copy (a process that runs it) still reads the old
/// bytes. Catches a copy written into the old file, which a running instance executes.
#[test]
fn a_second_copy_replaces_the_first_by_a_rename() {
    let (_guard, root) = scratch("binary-renamed");
    archive_release(&root, PREVIOUS, "binary of 0.4.0");
    archive_release(&root, RELEASE, "binary of 0.5.0");
    let runner = FakeRunner::replying(vec![]);
    let host = host(&root, &runner);
    host.install_binary(Path::new(PREVIOUS)).unwrap();
    let mut running = fs::File::open(copy(&root)).unwrap();
    let old = running.metadata().unwrap().ino();
    host.install_binary(Path::new(RELEASE)).unwrap();
    assert_eq!(fs::read_to_string(copy(&root)).unwrap(), "binary of 0.5.0");
    assert_ne!(fs::metadata(copy(&root)).unwrap().ino(), old);
    let mut seen = String::new();
    running.read_to_string(&mut seen).unwrap();
    assert_eq!(seen, "binary of 0.4.0");
    host.install_binary(Path::new(PREVIOUS)).unwrap();
    assert_eq!(fs::read_to_string(copy(&root)).unwrap(), "binary of 0.4.0");
    assert_eq!(names(&root), ["tierstore"]);
}

/// Asserts that a release of the Debian or RPM package puts nothing in place and makes no
/// directory: its package manager installs its binary. Catches a hook that copies a native
/// package's binary over what the package manager placed, or that fails on such a release.
#[test]
fn a_release_of_the_package_puts_nothing_in_place() {
    let (_guard, root) = scratch("binary-package");
    let binary = at(&root, RELEASE).join("tree/usr/libexec/tierstore/tierstore");
    fs::create_dir_all(binary.parent().unwrap()).unwrap();
    fs::write(&binary, "binary of the package").unwrap();
    let runner = FakeRunner::replying(vec![]);
    host(&root, &runner)
        .install_binary(Path::new(RELEASE))
        .unwrap();
    assert!(!root.join("usr").exists());
}

/// Asserts that a release with neither binary is refused, naming both paths it looked at and
/// where the copy was to go, and that nothing is made. Catches a step that starts an instance on
/// whatever copy was there before.
#[test]
fn a_release_with_no_binary_is_refused_naming_both_paths() {
    let (_guard, root) = scratch("binary-none");
    fs::create_dir_all(at(&root, RELEASE)).unwrap();
    let runner = FakeRunner::replying(vec![]);
    let error = host(&root, &runner)
        .install_binary(Path::new(RELEASE))
        .unwrap_err();
    let release = at(&root, RELEASE);
    assert_eq!(
        message(error),
        format!(
            "the binary of {RELEASE} cannot be put at {SERVICE_BINARY}: {} is not there, nor {}",
            release.join("bin/tierstore").display(),
            release
                .join("tree/usr/libexec/tierstore/tierstore")
                .display()
        )
    );
    assert!(!root.join("usr").exists());
}

/// Asserts that a binary that is a link, that a group or others can write, that sits in a `bin`
/// others can write, or that another user owns, is not copied, each with the reason, and that
/// the copy there before is kept. Catches a copy of a binary that someone other than root could
/// have replaced, which the service would then run.
#[test]
fn a_binary_that_is_not_root_s_alone_is_not_copied() {
    let (_guard, root) = scratch("binary-refused");
    archive_release(&root, PREVIOUS, "binary of 0.4.0");
    archive_release(&root, RELEASE, "binary of 0.5.0");
    let runner = FakeRunner::replying(vec![]);
    let host = host(&root, &runner);
    host.install_binary(Path::new(PREVIOUS)).unwrap();
    let bin = at(&root, RELEASE).join("bin");
    let binary = bin.join("tierstore");
    let refused =
        |host: &SystemHost<'_>| message(host.install_binary(Path::new(RELEASE)).unwrap_err());
    let prefix = format!("the binary of {RELEASE} cannot be put at {SERVICE_BINARY}");

    mode(&binary, 0o757);
    assert_eq!(
        refused(&host),
        format!(
            "{prefix}: {} can be written by a group or others",
            binary.display()
        )
    );
    mode(&binary, 0o755);
    mode(&bin, 0o775);
    assert_eq!(
        refused(&host),
        format!(
            "{prefix}: {} can be written by a group or others",
            bin.display()
        )
    );
    mode(&bin, 0o755);
    let other = SystemHost::new(root.clone(), &runner, LIMIT).with_release_owner(me() + 1);
    assert_eq!(
        refused(&other),
        format!(
            "{prefix}: {} is owned by user {}, not {}",
            bin.display(),
            me(),
            me() + 1
        )
    );
    fs::remove_file(&binary).unwrap();
    symlink(at(&root, PREVIOUS).join("bin/tierstore"), &binary).unwrap();
    let looped = refused(&host);
    assert!(
        looped.starts_with(&format!("{prefix}: {}: ", binary.display())),
        "{looped}"
    );
    assert_eq!(fs::read_to_string(copy(&root)).unwrap(), "binary of 0.4.0");
}

/// Asserts that a link at the copy's path or at its temporary name is replaced, never written
/// through: the file a link names keeps its bytes, and the copy is a regular file. Catches a copy
/// that writes the binary into whatever file a link there names.
#[test]
fn a_link_where_the_copy_goes_is_replaced_and_not_followed() {
    let (_guard, root) = scratch("binary-links");
    archive_release(&root, RELEASE, "binary of 0.5.0");
    copy_dirs(&root);
    let victim = root.join("victim");
    fs::write(&victim, "kept").unwrap();
    symlink(&victim, copy(&root)).unwrap();
    symlink(&victim, copy_dir(&root).join(".tierstore.new")).unwrap();
    let runner = FakeRunner::replying(vec![]);
    host(&root, &runner)
        .install_binary(Path::new(RELEASE))
        .unwrap();
    assert_eq!(fs::read_to_string(&victim).unwrap(), "kept");
    assert!(fs::symlink_metadata(copy(&root)).unwrap().is_file());
    assert_eq!(fs::read_to_string(copy(&root)).unwrap(), "binary of 0.5.0");
    assert_eq!(names(&root), ["tierstore"]);
}

/// Asserts that a directory on the way to the copy that is a link, that a group or others can
/// write, or that another user owns refuses the copy, naming it, and that nothing is written
/// under it. Catches a copy that a user other than root could swap for a binary of their own.
#[test]
fn a_directory_on_the_way_that_is_not_root_s_alone_refuses_the_copy() {
    let (_guard, root) = scratch("binary-dirs");
    archive_release(&root, RELEASE, "binary of 0.5.0");
    let runner = FakeRunner::replying(vec![]);
    let host = host(&root, &runner);
    let prefix = format!("the binary of {RELEASE} cannot be put at {SERVICE_BINARY}");

    let elsewhere = root.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    mode(&elsewhere, 0o755);
    fs::create_dir(root.join("usr")).unwrap();
    mode(&root.join("usr"), 0o755);
    symlink(&elsewhere, root.join("usr/local")).unwrap();
    assert_eq!(
        message(host.install_binary(Path::new(RELEASE)).unwrap_err()),
        format!(
            "{prefix}: {} is not a directory",
            root.join("usr/local").display()
        )
    );
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);

    fs::remove_file(root.join("usr/local")).unwrap();
    copy_dirs(&root);
    mode(&root.join("usr/local/libexec"), 0o777);
    assert_eq!(
        message(host.install_binary(Path::new(RELEASE)).unwrap_err()),
        format!(
            "{prefix}: {} can be written by a group or others",
            root.join("usr/local/libexec").display()
        )
    );
    assert!(!copy(&root).exists());

    mode(&root.join("usr/local/libexec"), 0o755);
    let mut source = fs::File::open(at(&root, RELEASE).join("bin/tierstore")).unwrap();
    let Err(fault) = ServiceBinary::new(&root, me() + 1).install(&mut source) else {
        panic!("a copy under directories of another owner");
    };
    assert_eq!(
        fault.0,
        format!(
            "{} is owned by user {}, not {}",
            root.join("usr").display(),
            me(),
            me() + 1
        )
    );
    assert!(!copy(&root).exists());
}

/// Asserts that a directory that cannot be made, a directory of the copy that cannot be opened
/// and a temporary file that cannot be made each fail the copy with the path and the system's
/// reason, and that the copy there before is kept. Catches a failure reported as success, which
/// would start an instance on the old binary.
#[test]
fn a_copy_that_cannot_be_written_fails_and_keeps_the_old_one() {
    let (_guard, root) = scratch("binary-unwritable");
    archive_release(&root, PREVIOUS, "binary of 0.4.0");
    archive_release(&root, RELEASE, "binary of 0.5.0");
    let runner = FakeRunner::replying(vec![]);
    let host = host(&root, &runner);
    let prefix = format!("the binary of {RELEASE} cannot be put at {SERVICE_BINARY}");
    host.install_binary(Path::new(PREVIOUS)).unwrap();

    // A directory at the temporary name: it is not removed, and the new file cannot be made.
    let temp = copy_dir(&root).join(".tierstore.new");
    fs::create_dir(&temp).unwrap();
    fs::write(temp.join("inside"), "").unwrap();
    let error = message(host.install_binary(Path::new(RELEASE)).unwrap_err());
    assert_eq!(
        error,
        format!(
            "{prefix}: {}: File exists (os error 17)",
            copy(&root).display()
        )
    );
    assert_eq!(fs::read_to_string(copy(&root)).unwrap(), "binary of 0.4.0");
    fs::remove_dir_all(&temp).unwrap();

    if as_root("a_copy_that_cannot_be_written_fails_and_keeps_the_old_one") {
        return;
    }
    // A directory of the copy that cannot be read, so not opened.
    mode(&copy_dir(&root), 0o311);
    assert_eq!(
        message(host.install_binary(Path::new(RELEASE)).unwrap_err()),
        format!(
            "{prefix}: {}: Permission denied (os error 13)",
            copy_dir(&root).display()
        )
    );
    mode(&copy_dir(&root), 0o755);
    assert_eq!(fs::read_to_string(copy(&root)).unwrap(), "binary of 0.4.0");

    // A directory that cannot be made: its parent cannot be written.
    let (_other, fresh) = scratch("binary-unmakeable");
    archive_release(&fresh, RELEASE, "binary of 0.5.0");
    fs::create_dir(fresh.join("usr")).unwrap();
    mode(&fresh.join("usr"), 0o555);
    let error = super::SystemHost::new(fresh.clone(), &runner, LIMIT)
        .with_release_owner(me())
        .install_binary(Path::new(RELEASE))
        .unwrap_err();
    mode(&fresh.join("usr"), 0o755);
    assert_eq!(
        message(error),
        format!(
            "{prefix}: {}: Permission denied (os error 13)",
            fresh.join("usr/local").display()
        )
    );
}

/// Asserts that removing takes the copy and a temporary file beside it away and leaves the
/// directories, that removing again, or with no directory there at all, is fine and makes
/// nothing. Catches an uninstall that leaves the binary behind or fails on a host that never had
/// one.
#[test]
fn removing_takes_the_copy_away_and_needs_nothing_there() {
    let (_guard, root) = scratch("binary-removed");
    let runner = FakeRunner::replying(vec![]);
    let host = host(&root, &runner);
    host.remove_binary().unwrap();
    assert!(!root.join("usr").exists());
    archive_release(&root, RELEASE, "binary of 0.5.0");
    host.install_binary(Path::new(RELEASE)).unwrap();
    fs::write(copy_dir(&root).join(".tierstore.new"), "half").unwrap();
    host.remove_binary().unwrap();
    assert!(names(&root).is_empty());
    host.remove_binary().unwrap();
    assert!(copy_dir(&root).is_dir());
}

/// Asserts that removing refuses a directory on the way that is a link, naming it, and leaves
/// what the link names alone; and that a copy that is a directory, or a directory that cannot be
/// looked into, fails the removal with the path and the system's reason. Catches a removal that
/// deletes a file named `tierstore` wherever a link points, or that reports a failure as done.
#[test]
fn removing_refuses_a_link_on_the_way_and_reports_what_it_cannot_remove() {
    let (_guard, root) = scratch("binary-remove-refused");
    let runner = FakeRunner::replying(vec![]);
    let host = host(&root, &runner);
    let prefix = format!("{SERVICE_BINARY} cannot be removed");

    let elsewhere = root.join("elsewhere");
    fs::create_dir_all(elsewhere.join("libexec/tierstore")).unwrap();
    fs::write(elsewhere.join("libexec/tierstore/tierstore"), "kept").unwrap();
    fs::create_dir(root.join("usr")).unwrap();
    mode(&root.join("usr"), 0o755);
    symlink(&elsewhere, root.join("usr/local")).unwrap();
    assert_eq!(
        message(host.remove_binary().unwrap_err()),
        format!(
            "{prefix}: {} is not a directory",
            root.join("usr/local").display()
        )
    );
    assert_eq!(
        fs::read_to_string(elsewhere.join("libexec/tierstore/tierstore")).unwrap(),
        "kept"
    );

    fs::remove_file(root.join("usr/local")).unwrap();
    copy_dirs(&root);
    fs::create_dir(copy(&root)).unwrap();
    fs::write(copy(&root).join("inside"), "").unwrap();
    #[cfg(target_os = "linux")]
    let reason = "Is a directory (os error 21)";
    #[cfg(not(target_os = "linux"))]
    let reason = "Operation not permitted (os error 1)";
    assert_eq!(
        message(host.remove_binary().unwrap_err()),
        format!("{prefix}: {}: {reason}", copy(&root).display())
    );
    fs::remove_dir_all(copy(&root)).unwrap();

    if as_root("removing_refuses_a_link_on_the_way_and_reports_what_it_cannot_remove") {
        return;
    }
    mode(&root.join("usr/local"), 0o000);
    let error = host.remove_binary().unwrap_err();
    mode(&root.join("usr/local"), 0o755);
    assert_eq!(
        message(error),
        format!(
            "{prefix}: {}: Permission denied (os error 13)",
            root.join("usr/local/libexec").display()
        )
    );
}
