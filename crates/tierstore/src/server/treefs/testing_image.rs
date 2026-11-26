//! The version 2 tree lists the rigs of the fs place (`testing.rs`): the list most image tests
//! read, and lists that differ only in a name, for the tests that map several (`core/lists.rs`).

use std::sync::Arc;

use crate::{BlobSource, ListEntry, ListKind, Mode, TreeDigest, TreeList};

/// The bytes of a version 2 tree list of the entries 0 root, 1 `bin`, 2 `d` (empty, mode 0700),
/// 3 `empty` (size 0), 4 `link` (to `bin/tool`), 5 `bin/other` (3 bytes of `p0` at 5, mode 0644)
/// and 6 `bin/tool` (the 5 bytes `hello` of `p0` at 0, mode 0755).
pub(in crate::server) fn image_bytes() -> Vec<u8> {
    image_named("d")
}

/// The bytes of the list of [`image_bytes`] with the empty directory named `name` (a name that
/// sorts after `bin` and before `empty`, as `d` does): another list of the same size for each name.
pub(in crate::server) fn image_named(name: &str) -> Vec<u8> {
    let file = |path: &str, (mode, size, offset): (u16, u64, u64), content: &[u8]| ListEntry {
        path: path.to_owned(),
        kind: ListKind::File {
            mode: Mode::new(mode).unwrap(),
            size,
            source: (size > 0).then(|| BlobSource {
                key: Arc::from("p0"),
                offset,
            }),
            sha256: Some(TreeDigest::of(content)),
        },
    };
    TreeList {
        entries: vec![
            file("bin/tool", (0o755, 5, 0), b"hello"),
            file("bin/other", (0o644, 3, 5), b"abc"),
            file("empty", (0o644, 0, 0), b""),
            ListEntry {
                path: "link".to_owned(),
                kind: ListKind::Symlink {
                    target: "bin/tool".to_owned(),
                },
            },
            ListEntry {
                path: name.to_owned(),
                kind: ListKind::Dir {
                    mode: Mode::new(0o700).unwrap(),
                },
            },
        ],
    }
    .encode_v2()
    .unwrap()
}
