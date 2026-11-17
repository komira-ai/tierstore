//! The answers to the mount's requests about the placements of the node (`tierstore fs` asks them;
//! `docs/design/node-local-trees.md` section 6.2), from the node's tree store:
//!
//! ```text
//! place <mount> <path>     tree <placement> | dir | none
//! mounted                  ["<path>", ...]                         (one line of JSON)
//! flat <placement>         {"paths": [...], "packs": [...], "entries": [...]} | none | damaged | too large
//! image <placement>        image <tree> ["<path>", ...] | {"paths": ...} (as flat) | none | damaged | too large
//! span <sha256> <size> from <pack> @ <offset>
//!                          ok <offset> <file> | why not | none
//! unplace <mount> <placement>   ok
//! places-live <mount> [<placement> ...]   ok <holds ended>
//! ```
//!
//! `place` answers `tree` for a path that is the fs location of a placement, which it then holds
//! for the mount (an attachment of the run `<mount>`, until `unplace`); `dir` for a path that a
//! location is below, which the mount shows as a directory it passes through; `none` otherwise.
//! `span` answers the file of the cache that holds the bytes and the offset of their first byte
//! in it (the file is the last word, the rest of the line); the mount reads the file there, and
//! nothing is copied out of the pack. Why not is the text of the tree store's `FetchError`: `fill:`
//! and the words of the cache's fill of a range (`fill: upstream 404`, `fill: mismatch`, ...), or
//! that the range was gone once filled. `places-live` ends the holds of the mount on every
//! placement it does not list: a mount that starts sends it, so that holds a process before it
//! left are not kept. `flat` reads the tree's records on a blocking thread. `image` answers, for a
//! tree that is a version 2 tree list, `image`, the tree's digest and the paths of the
//! placement's fs locations (one line of JSON): the mount reads the image from the tree's
//! directory in the cache directory, `trees/<tree>/image`, and the placement it holds keeps it
//! there. For a tree of records it answers what `flat` does. A node of the release before
//! answers it `refused unknown request`, and the mount asks `flat` instead.
//!
//! A node with no tree store (no `[trees]`, or `trees.enabled = false`) has no placement: `place`
//! is `none`, `mounted` an empty list, `flat`, `image` and `span` are `none`, `unplace` is `ok` and
//! `places-live` is `ok 0`.

use crate::server::local_cache::trees::{
    FileSpan, FlatError, FlatTree, MountPlace, MountTree, ReadError, TreeStore,
};

use super::request::MountRequest;

/// The answer to `request`, from `trees`, the node's tree store if it has one.
pub(super) async fn answer(trees: Option<&TreeStore>, request: MountRequest) -> String {
    let Some(trees) = trees else {
        return match request {
            MountRequest::Mounted => "[]".to_owned(),
            MountRequest::Unplace { .. } => "ok".to_owned(),
            MountRequest::PlacesLive { .. } => "ok 0".to_owned(),
            _ => "none".to_owned(),
        };
    };
    match request {
        MountRequest::Place { mount, path } => match trees.mount_place(&path) {
            MountPlace::Placement(id) => match trees.attach(&id, &mount) {
                Ok(()) => format!("tree {id}"),
                // Ended between the look and the hold.
                Err(_) => "none".to_owned(),
            },
            MountPlace::Above => "dir".to_owned(),
            MountPlace::Nothing => "none".to_owned(),
        },
        MountRequest::Mounted => json(&trees.mount_paths()),
        MountRequest::Flat { placement } => {
            let store = trees.clone();
            let flat = tokio::task::spawn_blocking(move || store.flat(&placement)).await;
            flat.map_or_else(|_| "damaged".to_owned(), |flat| flat_answer(&flat))
        }
        MountRequest::Image { placement } => {
            let store = trees.clone();
            let tree = tokio::task::spawn_blocking(move || store.mount_tree(&placement)).await;
            tree.map_or_else(|_| "damaged".to_owned(), image_answer)
        }
        MountRequest::Span {
            sha256,
            size,
            pack,
            offset,
        } => {
            let span = FileSpan { pack, offset };
            match trees.fetch_span(&span, size, &sha256).await {
                Ok(held) => format!("ok {} {}", held.offset, held.path.display()),
                Err(why) => why.to_string(),
            }
        }
        MountRequest::Unplace { mount, placement } => {
            let _ = trees.detach(&placement, &mount);
            "ok".to_owned()
        }
        MountRequest::PlacesLive { mount, keep } => {
            format!("ok {}", trees.holds_live(&mount, &keep))
        }
    }
}

/// The answer to an `image` whose tree is `tree`.
fn image_answer(tree: Result<MountTree, FlatError>) -> String {
    match tree {
        Ok(MountTree::Image { tree, paths }) => format!("image {tree} {}", json(&paths)),
        Ok(MountTree::Flat(flat)) => json(&flat),
        Err(why) => flat_answer(&Err(why)),
    }
}

/// The answer to a `flat` whose tree is `flat`.
fn flat_answer(flat: &Result<FlatTree, FlatError>) -> String {
    match flat {
        Ok(tree) => json(tree),
        Err(FlatError::Read(ReadError::NoPlacement)) => "none".to_owned(),
        Err(FlatError::Read(_)) => "damaged".to_owned(),
        Err(FlatError::TooLarge) => "too large".to_owned(),
    }
}

/// `value` as one line of JSON (a list of strings and a tree of strings and numbers always
/// serialise, and JSON escapes every newline in them).
fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

#[cfg(test)]
#[path = "mount_tests.rs"]
mod tests;
