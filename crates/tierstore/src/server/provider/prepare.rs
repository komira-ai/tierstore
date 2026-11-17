//! The preparation of one source, on a task of its own: the tree list read and checked, the tree
//! built and placed at the source's path, and the bytes of its files read into the cache.
//!
//! The placement and the reads run together, joined by a queue of reads ([`Group`]). A version 2
//! list ([`TreeListV2`]) is walked once on a blocking thread while another thread hashes it; as
//! soon as the walk has checked it, its spans are grouped by the list's pack ranges and queued, and
//! the reads begin while the tree's records are built and placed, which waits for the hash. A
//! version 1 list queues its spans, each alone, once its tree is placed. If the list is refused,
//! or the source is no longer this preparation's, the reads still queued are not made.
//!
//! A group of one span is read by [`TreeStore::fetch_span`] with the file's own span, size and
//! digest: the call an open of the mount makes for the file, which joins a fill of the same range
//! already running (the cache's flights, keyed by the pack, the range and the digest). A group of
//! several is read by [`TreeStore::fetch_parts`], one read of the pack across the spans, each
//! checked against its own digest; an open of one of its files waits for it and then finds the
//! span held. Files of one digest are read once, at the span the tree store keeps for the digest
//! (that of the digest's first file in the order the tree was built in), which is the one an open
//! reads.
//!
//! [`TreeStore::fetch_span`]: crate::server::local_cache::trees::TreeStore::fetch_span
//! [`TreeStore::fetch_parts`]: crate::server::local_cache::trees::TreeStore::fetch_parts

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::channel::mpsc::{UnboundedSender, unbounded};
use futures::{Stream, StreamExt as _};
use tokio::sync::watch;

use super::groups::{Group, alone, groups};
use super::names::SourceId;
use super::sources::{Owner, Shared, Stage};
use crate::server::local_cache::trees::{
    CreateError, CreateRequest, Created, FetchError, FileSpan,
};
use crate::{
    Backend as _, BuiltFile, BuiltTree, Error, TreeDigest, TreeLimits, TreeListError, TreeListV2,
    build_tree, is_tree_list_v2, parse_tree_list,
};

/// A preparation: the source, the tree list it names, and what it reports to.
pub(super) struct Job {
    pub id: SourceId,
    pub key: String,
    pub digest: TreeDigest,
    pub stage: Arc<watch::Sender<Stage>>,
    pub released: Arc<AtomicBool>,
}

/// The tree a preparation placed.
struct Made {
    tree: TreeDigest,
}

/// Sets the stage of a preparation to `now`, unless the source was released: a release is the
/// last word on a source.
pub(super) fn report(stage: &watch::Sender<Stage>, now: Stage) {
    stage.send_if_modified(|current| {
        let open = *current != Stage::Released;
        if open {
            *current = now;
        }
        open
    });
}

/// Prepares the source of `job`, reporting each stage it reaches.
pub(super) async fn run(shared: Arc<Shared>, job: Job) {
    let (spans, queued) = unbounded();
    let refused = AtomicBool::new(false);
    let placing = async {
        let placed = place(&shared, &job, spans).await;
        match &placed {
            Ok(_) => report(&job.stage, Stage::Placed),
            Err(stage) => {
                refused.store(true, Ordering::SeqCst);
                if let Stage::Refused(why) | Stage::Failed(why) = stage {
                    tracing::warn!(
                        "tierstore: mount provider: {} of {} not prepared: {why}",
                        job.id.path(),
                        job.key
                    );
                }
                report(&job.stage, stage.clone());
            }
        }
        placed
    };
    let filling = fill(&shared, queued, &job.released, &refused);
    let (placed, (fetched, failed)) = tokio::join!(placing, filling);
    let Ok(made) = placed else {
        return;
    };
    tracing::info!(
        "tierstore: mount provider: {} of {}: {fetched} spans in the cache, {failed} not read",
        job.id.path(),
        job.key
    );
    let pinned = if failed == 0 {
        shared.store.mark_ready(&made.tree)
    } else {
        shared.store.pin(&made.tree)
    };
    if let Err(why) = pinned {
        tracing::warn!(
            "tierstore: mount provider: the packs of {} were not pinned: {why}",
            made.tree
        );
    }
}

/// Reads the tree list, and checks, builds and places its tree at the source's path on a blocking
/// thread, queueing on `reads` what to read: a version 2 list's once it is walked, a version 1
/// list's once its tree is placed. A refusal or a failure is the stage it leaves the source at. If the
/// source is no longer this preparation's when the tree is placed, the placement is left to the
/// preparation that holds the source now, or, if none does, removed here, since the release may
/// have looked for it before it was made.
async fn place(shared: &Shared, job: &Job, reads: UnboundedSender<Group>) -> Result<Made, Stage> {
    let bytes = read_list(shared, &job.key).await?;
    let (digest, at) = (job.digest, job.id.location());
    let (ttl, store) = (shared.settings.ttl, shared.store.clone());
    let v2 = is_tree_list_v2(&bytes);
    let walked = reads.clone();
    let made = tokio::task::spawn_blocking(move || -> Result<(Created, Vec<BuiltFile>), Stage> {
        let limits = store.config().limits();
        let built = if v2 {
            built_v2(&bytes, &digest, &limits, &walked)?
        } else {
            built_v1(&bytes, &digest, &limits)?
        };
        let files = if v2 { Vec::new() } else { built.files.clone() };
        let request = CreateRequest {
            built,
            at: vec![at],
            ttl: Some(ttl),
            ready: false,
        };
        let created = store.create(request).map_err(stage_of)?;
        Ok((created, files))
    });
    let (created, files) = made
        .await
        .map_err(|why| Stage::Failed(format!("the tree was not made: {why}")))??;
    match shared.owner(&job.id, &job.released) {
        Owner::Ours => {
            for group in alone(&files) {
                let _ = reads.unbounded_send(group);
            }
            Ok(Made { tree: created.tree })
        }
        Owner::Newer => Err(Stage::Released),
        Owner::Gone => {
            let _ = shared.store.release(&created.placement);
            Err(Stage::Released)
        }
    }
}

/// The tree of the version 1 list in `bytes`, which must hash to `digest`.
fn built_v1(bytes: &[u8], digest: &TreeDigest, limits: &TreeLimits) -> Result<BuiltTree, Stage> {
    let list = parse_tree_list(bytes, digest).map_err(|why| Stage::Refused(why.to_string()))?;
    let entries = list
        .into_entries()
        .map_err(|why| Stage::Refused(why.to_string()))?;
    build_tree(None, entries, limits)
        .map_err(|why| Stage::Refused(format!("the tree is refused: {why}")))
}

/// The tree of the version 2 list in `bytes`, which must hash to `digest`. The bytes are hashed
/// on a thread of their own beside the walk; as soon as the walk has checked the list, its spans
/// are grouped and queued on `reads`, before the tree's records are built. The tree is made only
/// once the hash is the one the key names (a mismatch is the refusal, whatever the walk found).
/// A read queued from a list that is then refused is checked as any other, against its own
/// files' digests, and the reads still queued are not made.
fn built_v2(
    bytes: &[u8],
    digest: &TreeDigest,
    limits: &TreeLimits,
    reads: &UnboundedSender<Group>,
) -> Result<BuiltTree, Stage> {
    let refused = |why: crate::TreeListV2Error| Stage::Refused(why.to_string());
    let (actual, walked) = std::thread::scope(|scope| {
        let hashing = scope.spawn(|| TreeDigest::of(bytes));
        let walked = TreeListV2::open(bytes).and_then(|list| {
            let mut spans = Vec::new();
            let checked = list.check(limits, |span| spans.push(span))?;
            for group in groups(list.keys(), spans, list.ranges()) {
                let _ = reads.unbounded_send(group);
            }
            Ok(checked.build())
        });
        let actual = hashing
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (actual, walked)
    });
    if actual != *digest {
        let why = TreeListError::DigestMismatch {
            expected: *digest,
            actual,
        };
        return Err(Stage::Refused(why.to_string()));
    }
    walked.map_err(refused)
}

/// The stage a refusal of the tree store leaves a source at: a tree this node cannot make now is a
/// failure, any other is a refusal of the ref.
fn stage_of(why: CreateError) -> Stage {
    match why {
        CreateError::Disabled
        | CreateError::ReadOnly
        | CreateError::TooMany(_)
        | CreateError::Io(_) => Stage::Failed(format!("the tree was not made: {why}")),
        _ => Stage::Refused(format!("the tree is refused: {why}")),
    }
}

/// The bytes of the tree list `key`, read through the cache within the list's bound.
async fn read_list(shared: &Shared, key: &str) -> Result<Vec<u8>, Stage> {
    let bound = shared.settings.list_timeout;
    match tokio::time::timeout(bound, read_whole(shared, key)).await {
        Err(_) => Err(Stage::Failed(format!(
            "the tree list was not read within {bound:?}"
        ))),
        Ok(Err(Error::NotFound(_))) => Err(Stage::Refused(format!("no tree list at {key}"))),
        Ok(Err(why)) => Err(Stage::Failed(format!("the tree list was not read: {why}"))),
        Ok(Ok(Some(bytes))) => Ok(bytes),
        Ok(Ok(None)) => Err(Stage::Refused(format!(
            "the tree list is over {} bytes",
            shared.settings.max_list_bytes
        ))),
    }
}

/// The object `key`, or none if it is over the largest tree list: no more than that is kept.
async fn read_whole(shared: &Shared, key: &str) -> Result<Option<Vec<u8>>, Error> {
    let most = shared.settings.max_list_bytes;
    let mut body = shared.cache.get_stream(key, None).await?;
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        if bytes.len() + chunk.len() > most {
            return Ok(None);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Some(bytes))
}

/// Makes the `reads` as they come; how many spans are in the cache and how many were not read (a
/// failure, a bound passed, a release, a refused list or a stop before their turn).
pub(super) async fn fill(
    shared: &Shared,
    reads: impl Stream<Item = Group>,
    released: &AtomicBool,
    refused: &AtomicBool,
) -> (usize, usize) {
    let at_once = shared.store.config().fill_concurrency;
    reads
        .map(|group| async move { fetch(shared, &group, released, refused).await })
        .buffer_unordered(at_once)
        .fold((0, 0), |(fetched, failed), (more, less)| async move {
            (fetched + more, failed + less)
        })
        .await
}

/// Makes one read, within the bound of a read, once a read of the node's is free; how many of its
/// spans are in the cache and how many are not.
async fn fetch(
    shared: &Shared,
    group: &Group,
    released: &AtomicBool,
    refused: &AtomicBool,
) -> (usize, usize) {
    let spans = group.parts.len();
    let Ok(_turn) = shared.fills.acquire().await else {
        return (0, spans);
    };
    if released.load(Ordering::SeqCst) || refused.load(Ordering::SeqCst) {
        return (0, spans);
    }
    let bound = shared.settings.fill_timeout;
    let read = tokio::time::timeout(bound, read_group(shared, group)).await;
    let why = match read {
        Ok(Ok(())) => return (spans, 0),
        Ok(Err(why)) => why.to_string(),
        Err(_) => format!("not read within {bound:?}"),
    };
    let (offset, _, _) = group.parts[0];
    tracing::warn!(
        "tierstore: mount provider: {spans} spans from {offset} of {}: {why}",
        group.pack
    );
    let held = group
        .parts
        .iter()
        .filter(|(offset, size, _)| shared.store.span_held(&group.pack, *offset, *size))
        .count();
    (held, spans - held)
}

/// Reads `group` into the cache: a span alone as an open of its file would, several in one read.
async fn read_group(shared: &Shared, group: &Group) -> Result<(), FetchError> {
    match group.parts.as_slice() {
        [(offset, size, sha256)] => {
            let span = FileSpan {
                pack: group.pack.to_string(),
                offset: *offset,
            };
            shared
                .store
                .fetch_span(&span, *size, sha256)
                .await
                .map(drop)
        }
        parts => shared.store.fetch_parts(&group.pack, parts).await,
    }
}
