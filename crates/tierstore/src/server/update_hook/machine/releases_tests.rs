//! The release links of a single-instance host, in a scratch directory.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use super::super::ports::{Link, PortError, PortKind, Releases};
use super::ReleaseLinks;
use super::testing::{me, scratch};

/// A links directory of the tests' own user, mode 0755, and the scratch that holds it.
fn links_dir(name: &str) -> (tierstore_testkit::dir::TestDir, PathBuf) {
    let (guard, root) = scratch(name);
    let dir = root.join("tierstore");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    (guard, dir)
}

fn message(error: PortError) -> String {
    match error {
        PortError::Failed {
            kind: PortKind::Slots,
            message,
        } => message,
        other => panic!("not a link failure: {other:?}"),
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    found
}

/// Asserts that `current` and `previous` are made, read, replaced and removed as symbolic links
/// in the directory, each pointing where it was told, that a replacement leaves no temporary
/// link, and that an absent link reads as none and removes without error. It would catch a
/// link made under another name, or one that follows the link it replaces.
#[test]
fn the_links_are_made_read_replaced_and_removed() {
    let (_guard, dir) = links_dir("links-roundtrip");
    let links = ReleaseLinks::new(dir.clone(), me());
    assert_eq!(links.target(Link::Current).unwrap(), None);
    links.point(Link::Current, Path::new("/r/old")).unwrap();
    links.point(Link::Previous, Path::new("/r/old")).unwrap();
    links.point(Link::Current, Path::new("/r/new")).unwrap();
    assert_eq!(
        links.target(Link::Current).unwrap(),
        Some(PathBuf::from("/r/new"))
    );
    assert_eq!(
        fs::read_link(dir.join("previous")).unwrap(),
        PathBuf::from("/r/old")
    );
    assert_eq!(names(&dir), ["current", "previous"]);
    links.remove(Link::Previous).unwrap();
    links.remove(Link::Previous).unwrap();
    assert_eq!(links.target(Link::Previous).unwrap(), None);
    assert_eq!(names(&dir), ["current"]);
}

/// Asserts that a temporary link a crash left beside `current` is replaced, not an obstacle.
/// It would catch a `point` that fails for ever after a crash between its two calls.
#[test]
fn a_temporary_link_left_by_a_crash_is_replaced() {
    let (_guard, dir) = links_dir("links-temp");
    symlink("/r/stale", dir.join(".current.tmp")).unwrap();
    let links = ReleaseLinks::new(dir.clone(), me());
    links.point(Link::Current, Path::new("/r/new")).unwrap();
    assert_eq!(names(&dir), ["current"]);
}

/// Asserts that a `current` that is not a link is refused by a read and a removal, naming the
/// path, and that a `point` that cannot rename over it (a directory that is not empty) fails,
/// names the link and the release, and leaves no temporary link. It would catch a hook that
/// removes or follows something it did not make.
#[test]
fn what_is_not_a_link_is_refused() {
    let (_guard, dir) = links_dir("links-not-a-link");
    fs::create_dir(dir.join("current")).unwrap();
    fs::write(dir.join("current/keep"), b"x").unwrap();
    let links = ReleaseLinks::new(dir.clone(), me());
    let shown = dir.join("current").display().to_string();
    for error in [
        links.target(Link::Current).unwrap_err(),
        links.remove(Link::Current).unwrap_err(),
    ] {
        assert_eq!(
            message(error),
            format!("{shown} exists and is not a symbolic link")
        );
    }
    let text = message(links.point(Link::Current, Path::new("/r/new")).unwrap_err());
    assert!(
        text.starts_with(&format!("link {shown} to /r/new: ")),
        "{text}"
    );
    assert_eq!(names(&dir), ["current"]);
}

/// Asserts that the links directory is refused, before any link is read or made, when it is
/// owned by another user, when a group or others may write to it, when it is a symbolic link,
/// and when it is absent, each with a message that names the directory and why. It would catch
/// links that an account other than the hook's could change, and so choose what the service
/// runs.
#[test]
fn a_links_directory_others_could_change_is_refused() {
    let (_guard, dir) = links_dir("links-untrusted");
    let shown = dir.display().to_string();
    let other = ReleaseLinks::new(dir.clone(), me() + 1);
    assert_eq!(
        message(other.target(Link::Current).unwrap_err()),
        format!(
            "the links directory {shown}: is owned by user {}, not {}",
            me(),
            me() + 1
        )
    );
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o775)).unwrap();
    let writable = ReleaseLinks::new(dir.clone(), me());
    assert_eq!(
        message(writable.point(Link::Current, Path::new("/r")).unwrap_err()),
        format!("the links directory {shown}: can be written by a group or others")
    );
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let link = dir.with_file_name("link");
    symlink(&dir, &link).unwrap();
    let through = ReleaseLinks::new(link.clone(), me());
    assert_eq!(
        message(through.remove(Link::Current).unwrap_err()),
        format!(
            "the links directory {}: is a symbolic link or not a directory",
            link.display()
        )
    );
    let absent = ReleaseLinks::new(dir.with_file_name("absent"), me());
    let text = message(absent.target(Link::Current).unwrap_err());
    assert!(text.starts_with("the links directory "), "{text}");
    assert!(fs::read_dir(&dir).unwrap().next().is_none());
}

/// A host root under a scratch directory with the release `/opt/r/one` placed as the updater
/// places it: every directory mode 0755 and the binary mode 0755, all the tests' own user's;
/// the links directory `/opt/links` with `current` naming the release, as a Mac's does.
fn host_with_a_release(name: &str) -> (tierstore_testkit::dir::TestDir, PathBuf) {
    let (guard, root) = scratch(name);
    for dir in ["opt", "opt/r", "opt/r/one", "opt/links"] {
        fs::create_dir(root.join(dir)).unwrap();
        fs::set_permissions(root.join(dir), fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(root.join("opt/r/one/tierstore"), b"").unwrap();
    fs::set_permissions(
        root.join("opt/r/one/tierstore"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    symlink("/opt/r/one", root.join("opt/links/current")).unwrap();
    (guard, root)
}

fn checked(root: &Path, release: &str, owner: u32) -> Result<(), String> {
    ReleaseLinks::new(root.join("opt/links"), me())
        .with_releases_under(root.to_path_buf(), owner)
        .check(Path::new(release))
        .map_err(|error| match error {
            PortError::Failed {
                kind: PortKind::Host,
                message,
            } => message,
            other => panic!("not a host failure: {other:?}"),
        })
}

/// Asserts that a release placed as the updater places it passes the check, and that the check
/// refuses, naming the release and why: a path through the link `current` (an alias of a
/// release, which would make `current` name itself), a path through a link elsewhere, a relative
/// path, a dangling one, one through a link that loops, a directory or binary that a group or
/// others can write, a release of another owner, a binary that is a link, and one that is not a
/// regular file. Nothing is run. It would catch a release that another account could change
/// between the check and the start, and a release reached through the links the hook moves.
#[test]
fn a_staged_release_is_checked_on_disk_without_following_a_link() {
    let (_guard, root) = host_with_a_release("links-check");
    assert_eq!(checked(&root, "/opt/r/one", me()), Ok(()));
    let why = |release: &str, owner: u32| checked(&root, release, owner).unwrap_err();
    let prefix = |release: &str| format!("the release {release} is not trusted: ");
    assert_eq!(
        why("/opt/links/current", me()),
        format!(
            "{}/opt/links/current is a symbolic link",
            prefix("/opt/links/current")
        )
    );
    symlink("/opt/r", root.join("opt/alias")).unwrap();
    assert_eq!(
        why("/opt/alias/one", me()),
        format!("{}/opt/alias is a symbolic link", prefix("/opt/alias/one"))
    );
    assert_eq!(
        why("opt/r/one", me()),
        format!(
            "{}it is not an absolute path of plain components",
            prefix("opt/r/one")
        )
    );
    assert!(
        why("/opt/r/gone", me()).starts_with(&format!("{}/opt/r/gone: ", prefix("/opt/r/gone")))
    );
    symlink("/opt/loop", root.join("opt/loop")).unwrap();
    assert_eq!(
        why("/opt/loop", me()),
        format!("{}/opt/loop is a symbolic link", prefix("/opt/loop"))
    );
    assert_eq!(
        why("/opt/r/one", me() + 1),
        format!(
            "{}/opt is owned by user {}, not {}",
            prefix("/opt/r/one"),
            me(),
            me() + 1
        )
    );
    fs::set_permissions(root.join("opt/r"), fs::Permissions::from_mode(0o775)).unwrap();
    assert_eq!(
        why("/opt/r/one", me()),
        format!(
            "{}/opt/r can be written by a group or others",
            prefix("/opt/r/one")
        )
    );
    fs::set_permissions(root.join("opt/r"), fs::Permissions::from_mode(0o755)).unwrap();
    let binary = root.join("opt/r/one/tierstore");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o757)).unwrap();
    assert_eq!(
        why("/opt/r/one", me()),
        format!(
            "{}/opt/r/one/tierstore can be written by a group or others",
            prefix("/opt/r/one")
        )
    );
    fs::remove_file(&binary).unwrap();
    symlink("/bin/true", &binary).unwrap();
    assert_eq!(
        why("/opt/r/one", me()),
        format!(
            "{}/opt/r/one/tierstore is a symbolic link",
            prefix("/opt/r/one")
        )
    );
    fs::remove_file(&binary).unwrap();
    fs::create_dir(&binary).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        why("/opt/r/one", me()),
        format!(
            "{}/opt/r/one/tierstore is not a regular file",
            prefix("/opt/r/one")
        )
    );
}

/// Asserts that a host root that cannot be opened fails the check with a message that names it.
#[test]
fn a_host_root_that_cannot_be_opened_fails_the_check() {
    let (_guard, root) = scratch("links-check-root");
    let text = checked(&root.join("absent"), "/opt/r/one", me()).unwrap_err();
    assert!(text.contains("the host root "), "{text}");
}

/// Asserts that a release whose `tierstore` is a FIFO with no writer is refused as not a regular
/// file, and that the check returns rather than waiting for a writer: the binary is opened
/// without blocking and its type checked on the descriptor. The FIFO is made with `mkfifo`. It
/// would catch a `prepare` that hangs on an entry another account placed there.
#[test]
fn a_binary_that_is_a_fifo_is_refused_without_waiting() {
    let (_guard, root) = host_with_a_release("links-check-fifo");
    let binary = root.join("opt/r/one/tierstore");
    fs::remove_file(&binary).unwrap();
    let made = std::process::Command::new("mkfifo")
        .args(["-m", "0644"])
        .arg(&binary)
        .status()
        .unwrap();
    assert!(made.success());
    assert_eq!(
        checked(&root, "/opt/r/one", me()).unwrap_err(),
        "the release /opt/r/one is not trusted: /opt/r/one/tierstore is not a regular file"
    );
}
