//! `Tree::wait`, the lost path and the broken pack.
//!
//! Proves, against the real mount server whose engine parks every wait until the test answers it:
//! - a wait answered ready is `Retry`, and the same read is then served;
//! - a wait answered with an fill error is `Eio` with the cell, and the tree is not lost;
//! - a wait answered lost marks the whole tree lost: a wait parked on another pack ends `Lost`
//!   without its pack being answered, `Tree::lost` resolves (for a task already waiting on it and
//!   for one that comes later), later reads fail `Lost` and later waits are `Lost` at once;
//! - after the mount client is released a read fails `Broken`, and a wait parked on it, or made
//!   later, ends `Broken`;
//! - the waits and the waiting counts: waiting is back to 0 after an answer and after a wait
//!   future is dropped;
//! - a wait of a pack the tree does not have (made in another tree) is `Lost`.
//!
//! Catches: a lost tree whose other parked waits hang; a wait count left raised by an answer or
//! by an interrupted wait; a retry before the answer.

use std::sync::Arc;

use crate::support::{Events, VecStage, failed, find, parked, served, small_tree};
use tierstore_fetch_api::engine::WaitOutcome;
use tierstore_fetch_api::ids::CellRun;
use tierstore_mount_client::ClientError;
use tierstore_tree::testing::Rig;
use tierstore_tree::{ImageOptions, ReadFailure, Tree, Wait, Waited};

fn rig(name: &str) -> Rig {
    let tree = small_tree();
    Rig::new(
        env!("CARGO_TARGET_TMPDIR").as_ref(),
        name,
        tree.lists().1,
        tree.pack_list(),
    )
}

/// Parks a read of `path` at `offset`, 10 bytes, and gives what it waits for.
fn park(tree: &Tree, path: &str, offset: u64) -> Wait {
    parked(tree.read(
        find(tree.image(), path),
        offset,
        10,
        VecStage::new(&Events::default()),
    ))
}

fn spawn_wait(rig: &Rig, tree: &Arc<Tree>, wait: Wait) -> tokio::task::JoinHandle<Waited> {
    let tree = Arc::clone(tree);
    rig.runtime().spawn(async move { tree.wait(&wait).await })
}

fn join<T>(rig: &Rig, handle: tokio::task::JoinHandle<T>) -> T {
    rig.runtime().block_on(handle).unwrap()
}

#[test]
fn a_wait_answered_ready_retries_and_the_read_is_then_served() {
    let rig = rig("wait-retry");
    let tree = Arc::new(rig.tree(ImageOptions::default()));
    let wait = park(&tree, "bin/cc", 0);
    let handle = spawn_wait(&rig, &tree, wait);
    rig.until_parked(0, 1);
    assert_eq!(tree.stats().waiting, 1);
    assert_eq!(rig.answer(0, WaitOutcome::Ready), 1);
    assert_eq!(join(&rig, handle), Waited::Retry);
    rig.publish(0, CellRun { first: 0, count: 1 });
    let cc = find(tree.image(), "bin/cc");
    let bytes = served(tree.read(cc, 0, 10, VecStage::new(&Events::default())));
    assert_eq!(bytes, rig.bytes(0)[..10]);
    let stats = tree.stats();
    assert_eq!(
        (stats.waits, stats.waiting, stats.parked, stats.served),
        (1, 0, 1, 1)
    );
    assert!(!stats.lost);
}

#[test]
fn a_wait_answered_with_a_fill_error_is_eio_for_the_cell_and_the_tree_is_not_lost() {
    let rig = rig("wait-eio");
    let tree = Arc::new(rig.tree(ImageOptions::default()));
    let wait = park(&tree, "bin/cc", 0);
    let handle = spawn_wait(&rig, &tree, wait);
    rig.until_parked(0, 1);
    rig.answer(0, WaitOutcome::Eio { cell: 0 });
    assert_eq!(join(&rig, handle), Waited::Eio { cell: 0 });
    assert!(!tree.is_lost());
    assert_eq!(tree.stats().waiting, 0);
    // The tree still serves reads of its other packs.
    rig.publish(1, CellRun { first: 0, count: 1 });
    let a = find(tree.image(), "lib/a.txt");
    assert_eq!(
        served(tree.read(a, 0, 5, VecStage::new(&Events::default()))),
        rig.bytes(1)[..5]
    );
}

#[test]
fn a_lost_answer_ends_every_parked_wait_of_the_tree_and_fails_every_later_read() {
    let rig = rig("wait-lost");
    let tree = Arc::new(rig.tree(ImageOptions::default()));
    let on_a = park(&tree, "bin/cc", 0);
    let on_b = park(&tree, "lib/a.txt", 0);
    assert_eq!((on_a.pack(), on_b.pack()), (0, 1));
    let watcher = {
        let tree = Arc::clone(&tree);
        rig.runtime().spawn(async move { tree.lost().await })
    };
    let first = spawn_wait(&rig, &tree, on_a);
    let second = spawn_wait(&rig, &tree, on_b.clone());
    rig.until_parked(0, 1);
    rig.until_parked(1, 1);
    assert!(!tree.is_lost());
    assert_eq!(tree.stats().waiting, 2);

    // Only pack 0 is answered.
    rig.answer(0, WaitOutcome::Lost);
    assert_eq!(join(&rig, first), Waited::Lost);
    assert_eq!(join(&rig, second), Waited::Lost);
    join(&rig, watcher);
    assert_eq!(
        rig.parked(1),
        1,
        "the second wait ended without its pack being answered"
    );

    assert!(tree.is_lost());
    let stats = tree.stats();
    assert!(stats.lost);
    assert_eq!((stats.waits, stats.waiting), (2, 0));
    // `lost` resolves at once for a task that comes later.
    rig.runtime().block_on(tree.lost());
    let cc = find(tree.image(), "bin/cc");
    assert_eq!(
        failed(tree.read(cc, 0, 10, VecStage::new(&Events::default()))),
        ReadFailure::Lost
    );
    assert_eq!(rig.runtime().block_on(tree.wait(&on_b)), Waited::Lost);
    assert_eq!(tree.stats().waits, 2, "a wait of a lost tree is not sent");
}

#[test]
fn a_dropped_wait_future_gives_the_waiting_count_back() {
    let rig = rig("wait-dropped");
    let tree = Arc::new(rig.tree(ImageOptions::default()));
    let handle = spawn_wait(&rig, &tree, park(&tree, "bin/cc", 0));
    rig.until_parked(0, 1);
    assert_eq!(tree.stats().waiting, 1);
    handle.abort();
    assert!(rig.runtime().block_on(handle).unwrap_err().is_cancelled());
    let stats = tree.stats();
    assert_eq!((stats.waits, stats.waiting), (1, 0));
    // The server's wait is left to finish on its own, and answering it harms nothing.
    assert_eq!(rig.answer(0, WaitOutcome::Ready), 1);
    assert!(!tree.is_lost());
}

#[test]
fn a_read_after_the_client_is_released_fails_broken_and_a_wait_ends_broken() {
    let rig = rig("wait-broken");
    let tree = Arc::new(rig.tree(ImageOptions::default()));
    let wait = park(&tree, "bin/cc", 0);
    let parked_wait = spawn_wait(&rig, &tree, wait.clone());
    rig.until_parked(0, 1);
    assert_eq!(rig.runtime().block_on(rig.client().release()), Ok(()));
    let ended = join(&rig, parked_wait);
    assert!(matches!(ended, Waited::Broken(_)), "{ended:?}");
    assert!(!tree.is_lost());
    let cc = find(tree.image(), "bin/cc");
    let step = failed(tree.read(cc, 0, 10, VecStage::new(&Events::default())));
    assert!(matches!(step, ReadFailure::Broken(_)), "{step:?}");
    let later = rig.runtime().block_on(tree.wait(&wait));
    assert!(
        matches!(
            later,
            Waited::Broken(ClientError::Released | ClientError::Stopped)
        ),
        "{later:?}"
    );
    assert_eq!(tree.stats().waiting, 0);
}

#[test]
fn a_wait_made_in_another_tree_for_a_pack_this_one_lacks_is_lost() {
    let two_packs = rig("wait-foreign-a");
    let one_pack = {
        let tree = crate::support::Tree::default()
            .pack("only", crate::support::pack_bytes(1, 100))
            .file("f", "only", 0, 100);
        Rig::new(
            env!("CARGO_TARGET_TMPDIR").as_ref(),
            "wait-foreign-b",
            tree.lists().1,
            tree.pack_list(),
        )
    };
    let theirs = two_packs.tree(ImageOptions::default());
    let ours = one_pack.tree(ImageOptions::default());
    let wait = park(&theirs, "lib/a.txt", 0);
    assert_eq!(wait.pack(), 1);
    assert_eq!(one_pack.runtime().block_on(ours.wait(&wait)), Waited::Lost);
    assert_eq!(ours.stats().waits, 0);
}
