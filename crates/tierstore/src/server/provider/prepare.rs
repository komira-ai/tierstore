//! The preparation of one source, on a task of its own: the tree list read and checked, the tree
//! built and placed at the source's path, and the bytes of its files read into the cache.
//!
//! The reads are made unless `trees.prepare_prefetch` is `none` ([`PreparePrefetch::None`]): the
//! queue of reads is then closed before the list is read, so no read is queued or made, and the
//! tree, once placed, is neither pinned nor marked ready; each file is filled by its first open.
//!
//! The placement and the reads run together, joined by a queue of reads ([`Group`]). A version 2
//! list ([`TreeImage`]) is walked once on a blocking thread while another thread hashes it; as
//! soon as the walk has checked it, its spans are grouped by the list's pack ranges and queued, and
//! the reads begin while the tree is placed, which waits for the hash. The list is then the tree:
//! it is placed as it is ([`TreeStore::create_image`]), named by its own digest, with no directory
//! record made or hashed. A version 1 list is built into records and queues its spans, each
//! alone, once its tree is placed. If the list is refused, or the source is no longer this
//! preparation's, the reads still queued are not made; a release of the source drops them at once,
//! even while every read of the node is taken (as when the upstream does not answer).
//!
//! The reads queued hold about a hundred bytes of heap for each file until each is read or
//! dropped, and the queue drains only as fast as the node's reads go. So the reads of a source are queued only
//! if they fit, with those of every other source, under the node's bound (the provider's
//! `max_queued_bytes`, counted in the cache's `queued_reads`); a source whose reads do not fit
//! queues none, and its tree is placed as with `trees.prepare_prefetch = "none"`.
//!
//! A group of one span is read by [`TreeStore::fetch_span`] with the file's own span, size and
//! digest: the call an open of the mount makes for the file, which joins a fill of the same range
//! already running (the cache's flights, keyed by the pack, the range and the digest). A group of
//! several is read by [`TreeStore::fetch_parts`], one read of the pack across the spans, each
//! checked against its own digest; an open of one of its files waits for it and then finds the
//! span held. Files of one digest are read once for each span an open reads them at: for a
//! version 1 list, the span the tree store keeps for the digest (that of the digest's first file
//! in the order the tree was built in); for a version 2 list, which serves each file at its own
//! span, each distinct span of the digest.
//!
//! [`TreeStore::fetch_span`]: crate::server::local_cache::trees::TreeStore::fetch_span
//! [`TreeStore::fetch_parts`]: crate::server::local_cache::trees::TreeStore::fetch_parts
//! [`TreeStore::create_image`]: crate::server::local_cache::trees::TreeStore::create_image

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::channel::mpsc::{UnboundedSender, unbounded};
use futures::{Stream, StreamExt as _, stream};
use tokio::sync::watch;

use super::groups::{Group, alone, groups};
use super::names::SourceId;
use super::queued::{Queued, admit};
use super::sources::{Owner, Shared, Stage};
use crate::server::local_cache::QueuedReads;
use crate::server::local_cache::trees::{
    CreateError, CreateRequest, Created, FetchError, FileSpan, ImageRequest, PreparePrefetch,
    StagedImage, TreeStore,
};
use crate::{
    Backend as _, BuiltFile, BuiltTree, Error, TreeDigest, TreeImage, TreeLimits, TreeListError,
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

/// The tree a preparation placed, and whether its reads were left unqueued for want of room.
struct Made {
    tree: TreeDigest,
    unqueued: bool,
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
    let ends = (&*job.released, job.stage.subscribe());
    let filling = fill(&shared, queued, ends, &refused);
    let (placed, (fetched, failed)) = tokio::join!(placing, filling);
    let Ok(made) = placed else {
        return;
    };
    // A released source's reads were dropped, not counted: its tree is neither marked nor pinned.
    if job.released.load(Ordering::SeqCst) {
        return;
    }
    if lazy || made.unqueued {
        let why = if lazy {
            ""
        } else {
            " (the node's queue of reads is full)"
        };
        tracing::info!(
            "tierstore: mount provider: {} of {}: placed; its files are read when first \
             opened{why}",
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

/// Reads the tree list, and checks, builds and places its tree at the source's path on a blocking
/// thread, queueing on `reads` what to read: a version 2 list's once it is walked, a version 1
/// list's once its tree is placed. A refusal or a failure is the stage it leaves the source at. If the
/// source is no longer this preparation's when the tree is placed, the placement is left to the
/// preparation that holds the source now, or, if none does, removed here, since the release may
/// have looked for it before it was made.
async fn place(shared: &Shared, job: &Job, reads: UnboundedSender<Queued>) -> Result<Made, Stage> {
    let bytes = read_list(shared, &job.key).await?;
    let (digest, at) = (job.digest, job.id.location());
    let (ttl, store) = (shared.settings.ttl, shared.store.clone());
    let v2 = is_tree_list_v2(&bytes);
    let room = (Arc::clone(&shared.queued), shared.settings.max_queued_bytes);
    let walked = Queue {
        reads: reads.clone(),
        room: room.clone(),
    };
    let made = tokio::task::spawn_blocking(move || -> Result<Placed, Stage> {
        let limits = store.config().limits();
        if v2 {
            let (image, staged, unqueued) =
                image_v2(&store, bytes.into(), (&digest, &limits), &walked)?;
            let request = ImageRequest {
                image: Arc::new(image),
                digest,
                at: vec![at],
                ttl: Some(ttl),
                ready: false,
                staged,
            };
            let created = store.create_image(request).map_err(stage_of)?;
            return Ok((created, Vec::new(), unqueued));
        }
        let built = built_v1(&bytes, &digest, &limits)?;
        let files = built.files.clone();
        let request = CreateRequest {
            built,
            at: vec![at],
            ttl: Some(ttl),
            ready: false,
        };
        let created = store.create(request).map_err(stage_of)?;
        Ok((created, files, false))
    });
    let (created, files, unqueued) = made
        .await
        .map_err(|why| Stage::Failed(format!("the tree was not made: {why}")))??;
    match shared.owner(&job.id, &job.released) {
        Owner::Ours => {
            let queue = Queue { reads, room };
            let unqueued = unqueued || !queue.send(alone(&files));
            Ok(Made {
                tree: created.tree,
                unqueued,
            })
        }
        Owner::Newer => Err(Stage::Released),
        Owner::Gone => {
            let _ = shared.store.release(&created.placement);
            Err(Stage::Released)
        }
    }
}

/// What the blocking part of a placement makes: the tree, the files of a version 1 list (whose
/// reads are queued once it is placed), and whether a version 2 list's reads were left unqueued for
/// want of room.
type Placed = (Created, Vec<BuiltFile>, bool);

/// Where a preparation queues its reads, and the node's room for them.
struct Queue {
    reads: UnboundedSender<Queued>,
    room: (Arc<QueuedReads>, u64),
}

impl Queue {
    /// Whether nothing is to be read: the reads are not made (`trees.prepare_prefetch = "none"`,
    /// or the reading ended).
    fn is_closed(&self) -> bool {
        self.reads.is_closed()
    }

    /// Queues `groups` if the queue is open and they fit under the node's bound: false if they do
    /// not fit, and none is queued.
    fn send(&self, groups: Vec<Group>) -> bool {
        if self.is_closed() {
            return true;
        }
        let (reads, most) = &self.room;
        let Some(queued) = admit(reads, groups, *most) else {
            return false;
        };
        for read in queued {
            // A read the queue no longer takes (the reading ended meanwhile) is dropped, and its
            // room given back.
            let _ = self.reads.unbounded_send(read);
        }
        true
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

/// The version 2 list in `bytes`, which must hash to `digest`, opened as the tree it is, and its
/// file written ahead in `store` if it could be. The bytes are hashed on a thread of their own
/// and written on another, beside the walk; as soon as the walk has checked the list, its spans
/// are grouped and queued on `reads` if they fit under the node's bound (the flag it gives says
/// they did not). The tree is made only once the hash is the one the key names (a mismatch is the
/// refusal, whatever the walk found). A read queued from a list that is then
/// refused is checked as any other, against its own files' digests, and the reads still queued
/// are not made. A file that could not be written ahead is written by the create, which says why
/// if it cannot either.
fn image_v2(
    store: &TreeStore,
    bytes: bytes::Bytes,
    (digest, limits): (&TreeDigest, &TreeLimits),
    reads: &Queue,
) -> Result<(TreeImage, Option<StagedImage>, bool), Stage> {
    let refused = |why: crate::TreeListV2Error| Stage::Refused(why.to_string());
    let (actual, walked, staged, unqueued) = std::thread::scope(|scope| {
        let hashing = scope.spawn(|| TreeDigest::of(&bytes));
        let staging = scope.spawn(|| store.stage_image(bytes.clone()).ok());
        let mut spans = Vec::new();
        let walked = TreeImage::open(bytes.clone(), limits, |span| spans.push(span));
        let mut unqueued = false;
        if let Ok(image) = &walked
            && !reads.is_closed()
        {
            unqueued = !reads.send(groups(image.keys(), spans, image.ranges()));
        }
        let actual = hashing
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        // A write that panicked is a write not made ahead: the create writes the image.
        let staged = staging.join().ok().flatten();
        (actual, walked, staged, unqueued)
    });
    if actual != *digest {
        let why = TreeListError::DigestMismatch {
            expected: *digest,
            actual,
        };
        return Err(Stage::Refused(why.to_string()));
    }
    Ok((walked.map_err(refused)?, staged, unqueued))
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
/// failure, a bound passed, a release, a refused list or a stop before their turn). `ends` is the
/// release flag of the source and its stage: once the stage is released, no read is taken from
/// `reads`, the reads still in it are dropped at once with the room they hold and are not counted,
/// and a read waiting for its turn gives it up and is counted as not read. A read already running
/// goes on to its end or its bound. A refused list is not so quick: its queued reads each wait for
/// a turn before they see the refusal, as before.
pub(super) async fn fill(
    shared: &Shared,
    reads: impl Stream<Item = Queued> + Send,
    (released, stage): (&AtomicBool, watch::Receiver<Stage>),
    refused: &AtomicBool,
) -> (usize, usize) {
    let at_once = shared.store.config().fill_concurrency;
    until_released(reads, stage.clone())
        .map(|read| {
            let stage = stage.clone();
            async move { fetch(shared, read.group(), (released, stage), refused).await }
        })
        .buffer_unordered(at_once)
        .fold((0, 0), |(fetched, failed), (more, less)| async move {
            (fetched + more, failed + less)
        })
        .await
}

/// `reads` until `stage` is released: the stream then ends, and `reads` is dropped with what it
/// still holds.
fn until_released<'a>(
    reads: impl Stream<Item = Queued> + Send + 'a,
    stage: watch::Receiver<Stage>,
) -> impl Stream<Item = Queued> + Send + 'a {
    stream::unfold(
        (reads.boxed(), stage),
        |(mut reads, mut stage)| async move {
            let next = tokio::select! {
                biased;
                () = gone(&mut stage) => None,
                read = reads.next() => Some(read),
            };
            // None once released, Some(None) at the end of the reads: either way they go.
            let read = next??;
            Some((read, (reads, stage)))
        },
    )
}

/// Resolves once `stage` is released; never if its sender is gone without a release (a
/// preparation holds the sender while it runs).
async fn gone(stage: &mut watch::Receiver<Stage>) {
    let released = stage
        .wait_for(|stage| *stage == Stage::Released)
        .await
        .is_ok();
    if !released {
        std::future::pending::<()>().await;
    }
}

/// Makes one read, within the bound of a read, once a read of the node's is free; how many of its
/// spans are in the cache and how many are not. A release of the source while the read waits for
/// its turn ends it, as not read.
async fn fetch(
    shared: &Shared,
    group: &Group,
    (released, mut stage): (&AtomicBool, watch::Receiver<Stage>),
    refused: &AtomicBool,
) -> (usize, usize) {
    let spans = group.parts.len();
    let turn = tokio::select! {
        biased;
        () = gone(&mut stage) => return (0, spans),
        turn = shared.fills.acquire() => turn,
    };
    let Ok(_turn) = turn else {
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
