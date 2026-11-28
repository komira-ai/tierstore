//! The trees of the update check: the bytes the writers make of them, and their keys.

use std::sync::Arc;

use tierstore_list::{PackCells, PackInfo, sidecar_from_v2};

use super::check::{CheckTree, PACK_PREFIX, TREES, WITH_SIDECAR, WITHOUT_SIDECAR, pack, pack_key};
use crate::server::update_hook::TreeNode;
use crate::{BlobSource, ListEntry, ListKind, Mode, TreeDigest, TreeList, tree_list_digest};

/// The files of the pack as the lists name them: path, mode, bytes. The variant file is the
/// tree's own.
fn files(tree: &CheckTree) -> Vec<(&'static str, u16, Vec<u8>)> {
    let nodes = tree.nodes();
    let bytes = |path: &str| match nodes.get(path) {
        Some(TreeNode::File(bytes)) => bytes.clone(),
        other => panic!("{path} is {other:?}"),
    };
    vec![
        ("bin/tool", 0o755, bytes("bin/tool")),
        ("data/empty", 0o644, Vec::new()),
        ("data/one", 0o644, bytes("data/one")),
        ("variant", 0o644, bytes("variant")),
    ]
}

/// The list of `tree` as the build service writes it, over the check's pack.
fn list_of(tree: &CheckTree) -> TreeList {
    let pack = pack();
    let key: Arc<str> = Arc::from(pack_key().as_str());
    let mut entries: Vec<ListEntry> = files(tree)
        .into_iter()
        .map(|(path, mode, bytes)| {
            let (source, sha256) = if bytes.is_empty() {
                (None, None)
            } else {
                let offset = pack
                    .windows(bytes.len())
                    .position(|window| window == bytes.as_slice())
                    .expect("the file is in the pack");
                let source = BlobSource {
                    key: Arc::clone(&key),
                    offset: offset as u64,
                };
                (Some(source), Some(TreeDigest::of(&bytes)))
            };
            ListEntry {
                path: path.to_owned(),
                kind: ListKind::File {
                    mode: Mode::new(mode).expect("a mode"),
                    size: bytes.len() as u64,
                    source,
                    sha256,
                },
            }
        })
        .collect();
    entries.push(ListEntry {
        path: "data/dir".to_owned(),
        kind: ListKind::Dir {
            mode: Mode::new(0o755).expect("a mode"),
        },
    });
    entries.push(ListEntry {
        path: "tool".to_owned(),
        kind: ListKind::Symlink {
            target: "bin/tool".to_owned(),
        },
    });
    TreeList { entries }
}

/// The sidecar of the version 2 list `v2`, as the build service writes it: the cells of the
/// check's pack.
fn sidecar_of(v2: &[u8]) -> Vec<u8> {
    let pack = pack();
    let key = pack_key();
    sidecar_from_v2(v2, |named| {
        (named == key).then(|| PackInfo {
            cells: PackCells::of(&pack),
            ..PackInfo::default()
        })
    })
    .expect("a sidecar")
}

/// Asserts that the checked-in lists and sidecar of the check's trees are the bytes today's
/// writers make of the trees (`TreeList::encode_v2`, `sidecar_from_v2`), that only the first tree
/// has a sidecar, and that the two trees are two trees. Catches a check that keeps testing the
/// provider with objects in a form the build service no longer writes, and a change of a file of
/// the trees that leaves the stored lists behind.
#[test]
fn the_check_trees_are_what_the_writers_make_today() {
    for tree in TREES {
        let v2 = list_of(&tree).encode_v2().expect("a list to encode");
        assert!(v2 == tree.list, "the list of the tree {}", tree.form);
        match tree.sidecar {
            Some(sidecar) => assert!(sidecar_of(&v2) == sidecar, "the sidecar"),
            None => assert_eq!(tree.form, WITHOUT_SIDECAR.form),
        }
    }
    assert!(WITH_SIDECAR.sidecar.is_some());
    assert_ne!(WITH_SIDECAR.key(), WITHOUT_SIDECAR.key());
    assert_eq!((WITH_SIDECAR.index, WITHOUT_SIDECAR.index), (0, 1));
}

/// Asserts the keys of the objects: the list at `cas/l/<id>` (the id the SHA-256 of the list),
/// the sidecar at `cas/l2/<id>`, and the pack under `cas/check/` and the hex of its SHA-256.
#[test]
fn the_check_objects_have_the_keys_a_build_service_gives_them() {
    for tree in TREES {
        let digest = tree_list_digest(&tree.key()).expect("a tree list key");
        assert_eq!(digest, TreeDigest::of(tree.list));
        assert_eq!(tree.sidecar_key(), format!("cas/l2/{digest}"));
    }
    assert_eq!(
        pack_key(),
        format!("{PACK_PREFIX}{}", TreeDigest::of(&pack()))
    );
}
