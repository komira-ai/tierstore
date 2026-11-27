//! The preparation of one source, on a task of its own: the tree's L2 sidecar read and checked,
//! the tree built from it and placed at the source's path, and the bytes of its files read into
//! the cache.
//!
//! The one object of the tree read is its L2 sidecar, `cas/l2/<id>` (`docs/design/fetch-cache-serve.md`,
//! 3.5), never the list `cas/l/<id>` the ref names: a reader of this release parses no version 1
//! list and does not take a version 2 list for the tree. The sidecar is opened by the reader of
//! `tierstore-list` ([`TreeListL2::open_sidecar`]), which refuses it whole unless its header names
//! the tree and its version 2 projection (its entries, digests, keys, names and ranges) hashes to
//! the id, so every path, mode, size, file digest, pack key, offset and range placed is the one the
//! id names. A tree whose sidecar is not there, or is a list of another version or digest function,
//! is refused with the distinct `ListVersionUnsupported`, whose text (`tree list not supported:
//! <reason>`) is the reason the `Mount` answers with and the CAS service takes its miss path on,
//! and is counted (`tierstore_list_refused_total{reason}`, and
//! `tierstore_l2_sidecar_missing_total` for a missing one); a sidecar that is there and malformed
//! is refused as such and never placed.
//!
//! The reads are made unless `trees.prepare_prefetch` is `none` ([`PreparePrefetch::None`]): the
//! queue of reads is then closed before the sidecar is read, so no read is queued or made, and the
//! tree, once placed, is neither pinned nor marked ready; each file is filled by its first open.
//!
//! The placement and the reads run together, joined by a queue of reads ([`Group`]). The sidecar's
//! projection, the version 2 list of the tree ([`TreeImage`]), is walked once on a blocking thread
//! while another writes it ahead in the tree store; as soon as the walk has checked it, its spans
//! are grouped by the list's pack ranges and queued, and the reads begin while the tree is placed.
//! The list is then the tree: it is placed as it is ([`TreeStore::create_image`]), named by its own
//! digest, the tree id, with no directory record made or hashed. If the sidecar is refused, or the
//! source is no longer this preparation's, the reads still queued are not made.
//!
//! A group of one span is read by [`TreeStore::fetch_span`] with the file's own span, size and
//! digest: the call an open of the mount makes for the file, which joins a fill of the same range
//! already running (the cache's flights, keyed by the pack, the range and the digest). A group of
//! several is read by [`TreeStore::fetch_parts`], one read of the pack across the spans, each
//! checked against its own digest; an open of one of its files waits for it and then finds the
//! span held. A tree serves each file at its own span, so files of one digest are read once for
//! each distinct span of the digest.
//!
//! [`TreeListL2::open_sidecar`]: tierstore_list::TreeListL2::open_sidecar
//! [`TreeStore::fetch_span`]: crate::server::local_cache::trees::TreeStore::fetch_span
//! [`TreeStore::fetch_parts`]: crate::server::local_cache::trees::TreeStore::fetch_parts
//! [`TreeStore::create_image`]: crate::server::local_cache::trees::TreeStore::create_image

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::channel::mpsc::{UnboundedSender, unbounded};
use futures::{Stream, StreamExt as _};
use tierstore_list::{OpenError, TreeListL2, sidecar_key};
use tokio::sync::watch;

use super::groups::{Group, groups};
use super::names::SourceId;
use super::sources::{Owner, Shared, Stage};
use crate::server::local_cache::LocalCache;
use crate::server::local_cache::trees::{
    CreateError, Created, FetchError, FileSpan, ImageRequest, PreparePrefetch, StagedImage,
    TreeStore,
};
use crate::{Backend as _, Error, TreeDigest, TreeImage, TreeLimits};

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
    let lazy = shared.store.config().prepare_prefetch == PreparePrefetch::None;
    let (spans, mut queued) = unbounded();
    if lazy {
        // Nothing is read: every send of a read fails, and the fill below ends at once.
        queued.close();
    }
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
    if lazy {
        tracing::info!(
            "tierstore: mount provider: {} of {}: placed; its files are read when first opened",
            job.id.path(),
            job.key
        );
        return;
    }
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

/// Reads the tree's L2 sidecar, and checks it, builds its tree and places it at the source's path
/// on a blocking thread, queueing on `reads` what to read once the list is walked. A refusal or a
/// failure is the stage it leaves the source at. If the source is no longer this preparation's
/// when the tree is placed, the placement is left to the preparation that holds the source now,
/// or, if none does, removed here, since the release may have looked for it before it was made.
async fn place(shared: &Shared, job: &Job, reads: UnboundedSender<Group>) -> Result<Made, Stage> {
    let fetched = read_sidecar(shared, &job.digest).await?;
    let (digest, at) = (job.digest, job.id.location());
    let (ttl, store, cache) = (
        shared.settings.ttl,
        shared.store.clone(),
        shared.cache.clone(),
    );
    let made = tokio::task::spawn_blocking(move || -> Result<Created, Stage> {
        let limits = store.config().limits();
        let list = projection(&cache, fetched, &digest)?;
        let (image, staged) = image_v2(&store, list.into(), &limits, &reads)?;
        let request = ImageRequest {
            image: Arc::new(image),
            digest,
            at: vec![at],
            ttl: Some(ttl),
            ready: false,
            staged,
        };
        store.create_image(request).map_err(stage_of)
    });
    let created = made
        .await
        .map_err(|why| Stage::Failed(format!("the tree was not made: {why}")))??;
    match shared.owner(&job.id, &job.released) {
        Owner::Ours => Ok(Made { tree: created.tree }),
        Owner::Newer => Err(Stage::Released),
        Owner::Gone => {
            let _ = shared.store.release(&created.placement);
            Err(Stage::Released)
        }
    }
}

/// The version 2 list of tree `digest` that its L2 sidecar `fetched` (none if the store has
/// none) holds, once the sidecar is checked whole and its projection hashes to the tree id. A
/// sidecar refused as a list this release does not read is counted in `cache` under its reason,
/// and is the refusal `ListVersionUnsupported` says, word for word; any other is refused as the
/// sidecar of the tree.
fn projection(
    cache: &LocalCache,
    fetched: Option<Vec<u8>>,
    digest: &TreeDigest,
) -> Result<Vec<u8>, Stage> {
    match TreeListL2::open_sidecar(fetched, digest.as_bytes()) {
        Ok(list) => Ok(list.projection()),
        Err(OpenError::Unsupported(unsupported)) => {
            cache.list_refused(unsupported.reason);
            Err(Stage::Refused(unsupported.to_string()))
        }
        Err(OpenError::Malformed(why)) => Err(Stage::Refused(format!(
            "the L2 sidecar {} is refused: {why}",
            sidecar_key(digest.as_bytes())
        ))),
    }
}

/// The version 2 list in `bytes`, the projection of a sidecar checked against its tree id, opened
/// as the tree it is, and its file written ahead in `store` if it could be. The bytes are written
/// on a thread of their own beside the walk; as soon as the walk has checked the list, its spans
/// are grouped and queued on `reads`. A list the walk refuses (a tree over the store's limits) is
/// the refusal, and the reads still queued are not made. A file that could not be written ahead
/// is written by the create, which says why if it cannot either.
fn image_v2(
    store: &TreeStore,
    bytes: bytes::Bytes,
    limits: &TreeLimits,
    reads: &UnboundedSender<Group>,
) -> Result<(TreeImage, Option<StagedImage>), Stage> {
    let (walked, staged) = std::thread::scope(|scope| {
        let staging = scope.spawn(|| store.stage_image(bytes.clone()).ok());
        let mut spans = Vec::new();
        let walked = TreeImage::open(bytes.clone(), limits, |span| spans.push(span));
        if let Ok(image) = &walked
            && !reads.is_closed()
        {
            for group in groups(image.keys(), spans, image.ranges()) {
                let _ = reads.unbounded_send(group);
            }
        }
        // A write that panicked is a write not made ahead: the create writes the image.
        let staged = staging.join().ok().flatten();
        (walked, staged)
    });
    let image = walked.map_err(|why| Stage::Refused(why.to_string()))?;
    Ok((image, staged))
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

/// The bytes of the L2 sidecar of tree `digest`, read through the cache within the list's bound,
/// or none if the store has no such object.
async fn read_sidecar(shared: &Shared, digest: &TreeDigest) -> Result<Option<Vec<u8>>, Stage> {
    let key = sidecar_key(digest.as_bytes());
    let bound = shared.settings.list_timeout;
    match tokio::time::timeout(bound, read_whole(shared, &key)).await {
        Err(_) => Err(Stage::Failed(format!(
            "the L2 sidecar {key} was not read within {bound:?}"
        ))),
        Ok(Err(Error::NotFound(_))) => Ok(None),
        Ok(Err(why)) => Err(Stage::Failed(format!(
            "the L2 sidecar {key} was not read: {why}"
        ))),
        Ok(Ok(Some(bytes))) => Ok(Some(bytes)),
        Ok(Ok(None)) => Err(Stage::Refused(format!(
            "the L2 sidecar {key} is over {} bytes",
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
