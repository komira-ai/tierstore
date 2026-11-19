//! `Tree::open`: every pack of the tree opened through the mount protocol.
//!
//! Proves: a tree opens with every pack held and serves its image; a pack the server refuses
//! (here, a tree reference it does not know) fails the open with `TreeError::Open` naming the
//! first pack by number and carrying the client's answer; a tree with no pack (only empty files
//! and directories) opens and reads as empty; opening on a released client fails the same way.
//!
//! Catches: an open that returns a tree with a pack missing; an error that names the wrong pack.

use crate::support::{Events, VecStage, empty, find, small_tree};
use tierstore_fetch_api::ids::{JobId, TreeRef};
use tierstore_mount_client::ClientError;
use tierstore_tree::testing::{JOB, Rig};
use tierstore_tree::{ImageOptions, Tree, TreeConfig, TreeError};

fn config(tree: &str) -> TreeConfig {
    TreeConfig {
        tree: TreeRef(tree.into()),
        job: JobId(JOB.0),
    }
}

#[test]
fn a_tree_opens_with_every_pack_and_serves_its_image() {
    let tree = small_tree();
    let rig = Rig::new(
        env!("CARGO_TARGET_TMPDIR").as_ref(),
        "open-good",
        tree.lists().1,
        tree.pack_list(),
    );
    let opened = rig.tree(ImageOptions::default());
    assert_eq!(opened.image().packs().len(), 2);
    assert_eq!(
        (rig.readers(0, 0), rig.readers(1, 0)),
        (0, 0),
        "both packs are open on the server"
    );
    assert!(
        opened
            .image()
            .lookup(tierstore_fetch_api::list::NodeIndex(0), b"bin")
            .is_some()
    );
    assert!(!opened.is_lost());
}

#[test]
fn a_pack_the_server_refuses_fails_the_open_naming_the_first_pack() {
    let tree = small_tree();
    let rig = Rig::new(
        env!("CARGO_TARGET_TMPDIR").as_ref(),
        "open-refused",
        tree.lists().1,
        tree.pack_list(),
    );
    let image = rig.image(ImageOptions::default());
    let error = rig
        .runtime()
        .block_on(Tree::open(image, rig.client(), config("no-such-tree")))
        .unwrap_err();
    match &error {
        TreeError::Open {
            pack: 0,
            source: ClientError::OpenRefused { .. },
        } => {}
        other => panic!("{other}"),
    }
    assert!(error.to_string().starts_with("tree pack 0: "), "{error}");
    assert!(std::error::Error::source(&error).is_some());
}

#[test]
fn opening_on_a_released_client_fails_with_the_clients_answer() {
    let tree = small_tree();
    let rig = Rig::new(
        env!("CARGO_TARGET_TMPDIR").as_ref(),
        "open-released",
        tree.lists().1,
        tree.pack_list(),
    );
    let image = rig.image(ImageOptions::default());
    rig.runtime().block_on(rig.client().release()).unwrap();
    let error = rig
        .runtime()
        .block_on(Tree::open(
            image,
            rig.client(),
            config(tierstore_tree::testing::tree_ref().0.as_str()),
        ))
        .unwrap_err();
    assert!(matches!(error, TreeError::Open { pack: 0, .. }), "{error}");
}

#[test]
fn a_tree_without_packs_opens_and_its_files_read_as_empty() {
    let tree = crate::support::Tree::default()
        .empty_file("a")
        .dir("d", 0o755);
    let rig = Rig::new(
        env!("CARGO_TARGET_TMPDIR").as_ref(),
        "open-no-packs",
        tree.lists().1,
        tree.pack_list(),
    );
    let opened = rig.tree(ImageOptions::default());
    assert!(opened.image().packs().is_empty());
    let a = find(opened.image(), "a");
    empty(opened.read(a, 0, 10, VecStage::new(&Events::default())));
}
