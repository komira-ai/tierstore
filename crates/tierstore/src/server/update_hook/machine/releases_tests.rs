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
