//! The markers over a scratch directory.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

use super::super::marker_path;
use super::super::ports::{Instance, Markers, PortError, PortKind};
use super::MarkerFiles;
use super::testing::scratch;

fn message(error: PortError) -> String {
    match error {
        PortError::Failed {
            kind: PortKind::Markers,
            message,
        } => message,
        other => panic!("not a markers failure: {other:?}"),
    }
}

/// Asserts that a marker is named as the template's condition names it, `run-a` and `run-b`
/// in the directory: the file name of [`marker_path`].
#[test]
fn the_markers_are_named_as_the_template_names_them() {
    let (_guard, root) = scratch("markers-names");
    let markers = MarkerFiles::new(root.clone());
    markers.set(Instance::A).unwrap();
    markers.set(Instance::B).unwrap();
    for instance in Instance::ALL {
        let name = marker_path(instance);
        assert!(root.join(name.file_name().unwrap()).is_file());
    }
    assert!(root.join("run-a").is_file() && root.join("run-b").is_file());
}

/// Asserts that a marker is absent until it is set, present after, absent after it is cleared,
/// and that setting a marker that is there and clearing one that is not are not errors.
#[test]
fn a_marker_is_set_and_cleared_and_both_are_idempotent() {
    let (_guard, root) = scratch("markers-cycle");
    let markers = MarkerFiles::new(root);
    assert!(!markers.present(Instance::A).unwrap());
    markers.clear(Instance::A).unwrap();
    markers.set(Instance::A).unwrap();
    markers.set(Instance::A).unwrap();
    assert!(markers.present(Instance::A).unwrap());
    assert!(!markers.present(Instance::B).unwrap());
    markers.clear(Instance::A).unwrap();
    assert!(!markers.present(Instance::A).unwrap());
}

/// Asserts that a directory that is absent is made with mode 0700 when a marker is set, and
/// that a directory already there keeps its mode.
#[test]
fn the_directory_is_made_private_when_absent_and_left_alone_when_there() {
    let (_guard, root) = scratch("markers-dir");
    let dir = root.join("lib");
    let markers = MarkerFiles::new(dir.clone());
    markers.set(Instance::B).unwrap();
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    markers.set(Instance::A).unwrap();
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

/// Asserts that a marker is a regular file and a marker named by a directory or a link is an
/// error to look at, to set (a link is not followed) and to clear (a directory is not removed).
#[test]
fn a_directory_or_a_link_in_place_of_a_marker_is_refused() {
    let (_guard, root) = scratch("markers-kinds");
    let markers = MarkerFiles::new(root.clone());
    fs::create_dir(root.join("run-a")).unwrap();
    assert!(message(markers.present(Instance::A).unwrap_err()).ends_with("is not a regular file"));
    assert!(markers.set(Instance::A).is_err());
    assert!(markers.clear(Instance::A).is_err());
    assert!(root.join("run-a").is_dir());
    fs::write(root.join("elsewhere"), "x").unwrap();
    symlink(root.join("elsewhere"), root.join("run-b")).unwrap();
    assert!(markers.present(Instance::B).is_err());
    assert!(message(markers.set(Instance::B).unwrap_err()).starts_with("make "));
    assert_eq!(fs::read(root.join("elsewhere")).unwrap(), b"x");
}

/// Asserts that a directory that cannot be made (its parent is read-only), a path that cannot be
/// looked at (its parent is a file) and a marker that cannot be removed (its directory is
/// read-only) are errors that name the path.
#[test]
fn a_marker_that_cannot_be_made_looked_at_or_removed_is_an_error() {
    let (_guard, root) = scratch("markers-errors");
    let file = root.join("file");
    fs::write(&file, "x").unwrap();
    let under_file = MarkerFiles::new(file.join("dir"));
    let text = message(under_file.present(Instance::A).unwrap_err());
    assert!(text.starts_with("look at "), "{text}");
    assert!(message(under_file.set(Instance::A).unwrap_err()).starts_with("make "));

    let dir = root.join("dir");
    fs::create_dir(&dir).unwrap();
    let markers = MarkerFiles::new(dir.clone());
    markers.set(Instance::A).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    let removed = markers.clear(Instance::A);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(message(removed.unwrap_err()).starts_with("remove "));
    assert!(markers.present(Instance::A).unwrap());
}
