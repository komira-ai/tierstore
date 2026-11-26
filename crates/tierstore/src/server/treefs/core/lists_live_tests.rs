//! The mapped tree lists of a mount over the node's real tree store: a placement whose tree is an
//! image, made by the store, and the mount's tree of it, over the fill socket and in process.

use std::sync::Arc;

use super::super::placed::key_of;
use super::super::{Core, Entry, Looked};
use crate::TreeDigest;
use crate::server::local_cache::trees::testing::fs_at;
use crate::server::local_cache::trees::{Created, Mapped};
use crate::server::treefs::ino;
use crate::server::treefs::live_testing::{Live, Over, over_both};
use crate::server::treefs::testing::TREE_IDLE;
use crate::tree_image_list::in_pages;

fn found(core: &Core, parent: u64, name: &str) -> Entry {
    match core.lookup(parent, name).unwrap() {
        Looked::Found(entry) => entry,
        other => panic!("{name}: {other:?}"),
    }
}

/// The bench's small tree placed as an image at `img`.
fn placed(live: &Live) -> Created {
    let request = live.bench.image_request(vec![fs_at("img")]);
    live.bench.store.create_image(request).unwrap()
}

/// What the core holds mapped, and whether its tree of the placement `placement` at `img` reads
/// the list `tree` the core keeps.
fn mapped_by_core(core: &Core, placement: &str, tree: &TreeDigest) -> (Mapped, bool) {
    let key = key_of(placement, "img").unwrap();
    let number = core.table.find(&key).unwrap();
    let loaded = core.trees.get(number).unwrap();
    let list = loaded.tree.list().unwrap();
    let ours = core.lists.mapped.get(tree).unwrap();
    (core.lists.mapped.mapped(), Arc::ptr_eq(list, &ours))
}

over_both!(the_store_and_the_fs_share_one_mapping_of_a_list_and_it_is_budgeted_once);
/// Asserts that the store maps the list it makes from its image file; that the mount of the same
/// process reads that very mapping, so the process maps the list once and counts its pages once;
/// and that a mount of its own process (over the fill socket) maps the file itself, once, in its
/// own budget.
async fn the_store_and_the_fs_share_one_mapping_of_a_list_and_it_is_budgeted_once(over: Over) {
    let live = Live::over("tfs-lists-shared", over).await;
    let created = placed(&live);
    let store = Arc::clone(live.bench.store.lists());
    let page = in_pages(live.bench.image_bytes().len() as u64);
    let one = Mapped {
        lists: 1,
        bytes: page,
    };
    assert_eq!(store.mapped(), one);
    let kept = store.get(&created.tree).unwrap();
    assert!(kept.is_mapped());
    let (placement, tree) = (created.placement.to_string(), created.tree);
    let (core_mapped, same) = live
        .on_core(move |core| {
            let _ = found(core, ino::ROOT, "img");
            mapped_by_core(core, &placement, &tree)
        })
        .await;
    assert_eq!(core_mapped, one);
    assert_eq!(store.mapped(), one);
    let shared = live.core.lists.mapped.get(&created.tree).unwrap();
    assert_eq!(Arc::ptr_eq(&shared, &kept), over == Over::InProcess);
    assert!(same);
}

over_both!(an_image_the_store_drops_while_a_request_holds_it_is_still_read);
/// Asserts that a list whose tree the store drops, and whose directory it removes, while a request
/// of the mount still holds the tree (after the sweep for idle trees ended its attachment and gave
/// the hold back) is still read through the mapping: removing the image file never changes what is
/// mapped of it, and nothing cuts an image short. The mapping is released with the request.
async fn an_image_the_store_drops_while_a_request_holds_it_is_still_read(over: Over) {
    let live = Live::over("tfs-lists-dropped", over).await;
    let created = placed(&live);
    let key = key_of(&created.placement.to_string(), "img").unwrap();
    let request = live
        .on_core(move |core| {
            let _ = found(core, ino::ROOT, "img");
            core.tree(core.table.find(&key).unwrap()).unwrap()
        })
        .await;
    let image =
        crate::server::local_cache::trees::image_path(&live.bench.rig.config.path, &created.tree);
    live.clock.advance(TREE_IDLE);
    assert_eq!(live.on_core(Core::detach_idle).await, 1);
    live.bench.store.release(&created.placement).unwrap();
    let _ = live.bench.store.sweep();
    assert!(!image.exists());
    let bin = request.tree.child(0, "bin").unwrap();
    let tool = request.tree.child(bin, "tool").unwrap();
    assert_eq!(&*request.tree.entry(tool).unwrap().name, "tool");
    assert_eq!(live.core.lists.mapped.mapped().lists, 1);
    drop(request);
    assert_eq!(live.core.lists.mapped.mapped(), Mapped::default());
    assert_eq!(live.bench.store.lists().mapped(), Mapped::default());
}
