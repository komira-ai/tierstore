//! A tree that holds the whiteout names of an OCI layer, as the output of a build that makes a
//! layer does (`etc/.wh.ssl`, `etc/ssl/certs/.wh..wh..opq`), and that a later action reads as its
//! inputs: it attaches, and each such name is the plain file it was written as, found by name and
//! listed beside its siblings. Nothing of the tree is hidden by them.

use super::{Core, DirItem, Kind, Looked};
use crate::server::treefs::ino;
use crate::server::treefs::testing::{Built, Rig, dir, file};

/// The entry `name` of the directory `parent`: its inode number, kind and size.
fn found(core: &Core, parent: u64, name: &str) -> (u64, Kind, u64) {
    match core.lookup(parent, name).unwrap() {
        Looked::Found(entry) => (entry.attr.ino, entry.attr.kind, entry.attr.size),
        other => panic!("{name}: {other:?}"),
    }
}

/// The names the listing of `dir` gives after `.` and `..`.
fn children(core: &Core, dir: u64) -> Vec<String> {
    let mut names = Vec::new();
    core.readdir(dir, 0, &mut |_, item: DirItem| {
        names.push(item.name);
        true
    })
    .unwrap();
    names.split_off(2)
}

/// Asserts that a tree with a whiteout name and an opaque marker among its files attaches, and
/// that both are files of size 0 found by name and listed, while the directory a whiteout names
/// (`etc/ssl`) and the files beside the marker (`certs/ca.crt`) are there too.
#[test]
fn whiteout_names_in_a_tree_are_its_files_and_hide_nothing() {
    let built = Built::of(vec![
        ("bin", dir(vec![("app", file(b"app"))])),
        (
            "etc",
            dir(vec![
                (".wh.ssl", file(b"")),
                (
                    "ssl",
                    dir(vec![(
                        "certs",
                        dir(vec![(".wh..wh..opq", file(b"")), ("ca.crt", file(b"pem"))]),
                    )]),
                ),
            ]),
        ),
    ]);
    let rig = Rig::new("tfs-core-whiteout-names", built);
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let path = rig.core.attach(&root, "run-1").unwrap();
    assert_eq!(path, rig.mountpoint.join("trees").join(&root));
    let core = &rig.core;
    let top = ino::of(1, 0);
    assert_eq!(children(core, top), ["bin", "etc"]);
    let (etc, kind, _) = found(core, top, "etc");
    assert_eq!(kind, Kind::Dir);
    assert_eq!(children(core, etc), [".wh.ssl", "ssl"]);
    let (_, kind, size) = found(core, etc, ".wh.ssl");
    assert_eq!((kind, size), (Kind::File, 0));
    let (ssl, kind, _) = found(core, etc, "ssl");
    assert_eq!(kind, Kind::Dir);
    let (certs, kind, _) = found(core, ssl, "certs");
    assert_eq!(kind, Kind::Dir);
    assert_eq!(children(core, certs), [".wh..wh..opq", "ca.crt"]);
    let (_, kind, size) = found(core, certs, ".wh..wh..opq");
    assert_eq!((kind, size), (Kind::File, 0));
    let (_, kind, size) = found(core, certs, "ca.crt");
    assert_eq!((kind, size), (Kind::File, 3));
}
