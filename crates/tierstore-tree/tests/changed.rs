//! An image file changed after it was opened (the cache never does this; the bytes are checked
//! once, at open): records that no longer make sense read as absent, never as a panic, a hang or
//! an allocation the record names.
//!
//! Proves: with every entry overwritten, nodes and lookups are `None`; a record of an unknown
//! kind is no node; a name that runs past the names section is no name (and the lookup through it
//! finds nothing), while one that ends exactly at its end is read; a link whose target runs past
//! the names, or is of any length, has no target and allocates nothing for it; a directory whose
//! count or first child is out of range finds nothing. (The same for the reads of a file is in
//! `tree/changed.rs`.)
//!
//! Catches: an allocation sized by a changed record; an index past the mapping; a lookup loop that
//! does not end on a count that is out of range.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::fs::File;

use support::{find, patch, rich_tree, section, tree_id, write_image};
use tierstore_fetch_api::list::NodeIndex;
use tierstore_tree::{Image, ImageOptions, NAME_MAX};

/// The tree's image over a file in `dir` and where its entry `index` is in that file.
struct Changed {
    _dir: tierstore_testkit::dir::TestDir,
    path: std::path::PathBuf,
    image: Image,
    entries: u64,
    names: (u64, u64),
}

fn changed(name: &str) -> Changed {
    let dir = tierstore_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let l2 = rich_tree().lists().1;
    let path = write_image(dir.path(), &l2);
    let image = Image::open(
        File::open(&path).unwrap(),
        &tree_id(&l2),
        ImageOptions::default(),
    )
    .unwrap();
    Changed {
        _dir: dir,
        path,
        image,
        entries: section(&l2, 0).0,
        names: section(&l2, 5),
    }
}

impl Changed {
    fn record(&self, index: tierstore_fetch_api::list::NodeIndex) -> u64 {
        self.entries + index.0 * 32
    }

    fn patch_record(&self, index: NodeIndex, at: u64, bytes: &[u8]) {
        patch(&self.path, self.record(index) + at, bytes);
    }
}

#[test]
fn entries_overwritten_after_the_open_read_as_absent() {
    let c = changed("changed-all");
    let len = c.image.len();
    let lib = find(&c.image, "lib");
    patch(&c.path, c.entries, &vec![0xff; (len * 32) as usize]);
    for index in 0..len {
        assert!(c.image.node(NodeIndex(index)).is_none(), "entry {index}");
    }
    assert_eq!(c.image.lookup(NodeIndex(0), b"lib"), None);
    assert_eq!(c.image.lookup(lib, b"a.txt"), None);
    assert_eq!(
        c.image.len(),
        len,
        "what was learnt at the open does not change"
    );
}

#[test]
fn a_name_past_the_end_of_the_names_is_absent_and_one_ending_at_it_is_read() {
    let c = changed("changed-name");
    let lib = find(&c.image, "lib");
    let a = find(&c.image, "lib/a.txt");
    let names_len = u32::try_from(c.names.1).unwrap();
    let mut buf = [0u8; NAME_MAX];

    c.patch_record(a, 4, &(names_len - 1).to_le_bytes());
    c.patch_record(a, 14, &[1]);
    let node = c.image.node(a).unwrap();
    assert_eq!(
        c.image.name(&node, &mut buf).unwrap().len(),
        1,
        "the last byte of the names"
    );

    c.patch_record(a, 14, &[2]);
    assert_eq!(c.image.name(&c.image.node(a).unwrap(), &mut buf), None);

    c.patch_record(a, 4, &u32::MAX.to_le_bytes());
    assert_eq!(c.image.name(&c.image.node(a).unwrap(), &mut buf), None);
    assert_eq!(
        c.image.lookup(lib, b"a.txt"),
        None,
        "the search meets the changed name"
    );
}

#[test]
fn a_link_whose_target_runs_past_the_names_has_none_and_allocates_nothing() {
    let c = changed("changed-link");
    let link = find(&c.image, "lib/self");
    assert_eq!(
        c.image.read_link(&c.image.node(link).unwrap()).unwrap(),
        b"a.txt"
    );
    for len in [c.names.1 + 1, u64::MAX, 1 << 62] {
        c.patch_record(link, 24, &len.to_le_bytes());
        assert_eq!(
            c.image.read_link(&c.image.node(link).unwrap()),
            None,
            "len {len}"
        );
    }
    // The target at the very end of the names, from an offset that overflows, or past it.
    c.patch_record(link, 24, &1u64.to_le_bytes());
    for offset in [c.names.1, u64::MAX, u64::MAX - 5] {
        c.patch_record(link, 16, &offset.to_le_bytes());
        assert_eq!(
            c.image.read_link(&c.image.node(link).unwrap()),
            None,
            "offset {offset}"
        );
    }
    c.patch_record(link, 16, &(c.names.1 - 1).to_le_bytes());
    assert_eq!(
        c.image
            .read_link(&c.image.node(link).unwrap())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn an_entry_of_an_unknown_kind_is_no_node() {
    let c = changed("changed-kind");
    let a = find(&c.image, "lib/a.txt");
    c.patch_record(a, 12, &((3u16 << 12) | 0o644).to_le_bytes());
    assert!(c.image.node(a).is_none());
    assert_eq!(c.image.lookup(find(&c.image, "lib"), b"a.txt"), None);
}

#[test]
fn a_directory_with_a_count_or_first_child_out_of_range_finds_nothing_and_ends() {
    let c = changed("changed-dir");
    let many = find(&c.image, "many");
    c.patch_record(many, 16, &u64::MAX.to_le_bytes());
    for name in [&b"f00"[..], b"f19", b"zzz", b""] {
        // The first record the search meets is past the entries.
        assert_eq!(c.image.lookup(many, name), None);
    }
    c.patch_record(many, 16, &20u64.to_le_bytes());
    c.patch_record(many, 8, &u32::MAX.to_le_bytes());
    assert_eq!(c.image.lookup(many, b"f05"), None);
}
