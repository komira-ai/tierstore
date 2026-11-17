//! The preparation of one source, on a task of its own: the tree list read and checked, the tree
//! built and placed at the source's path, and the bytes of its files read into the cache.
//!
//! The placement and the reads run together, joined by a queue of spans. A version 2 list
//! ([`TreeListV2`]) queues each span as its one walk meets it, so the first read starts while the
//! rest of the list is still being walked and long before the tree is placed; a version 1 list
//! queues its spans once its tree is placed. If the list is refused, or the source is no longer
//! this preparation's, the spans still queued are not read.
//!
//! The files are read one call each, by [`TreeStore::fetch_span`] with the file's own span, size
//! and digest: the call an open of the mount makes for the file, which joins a fill of the same
//! range already running (the cache's flights, keyed by the pack, the range and the digest). So an
//! open of a file this preparation is reading waits for that one read, and a file the job opens
//! before its turn here is read once, by whichever asks first. Files of one digest are read once,
//! at the span the tree store keeps for the digest (that of the digest's first file in the order
//! the tree was built in), which is the one an open reads.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::channel::mpsc::{UnboundedSender, unbounded};
use futures::{Stream, StreamExt as _};
use tokio::sync::watch;

use super::names::SourceId;
use super::sources::{Owner, Shared, Stage};
use crate::server::local_cache::trees::{CreateError, CreateRequest, Created, FileSpan};
use crate::{
    Backend as _, BuiltFile, BuiltTree, Error, ListSpan, TreeDigest, TreeLimits, TreeListError,
    TreeListV2, build_tree, is_tree_list_v2, parse_tree_list,
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
/// thread, queueing on `spans` what to read: a version 2 list as it is walked, a version 1 list
/// once its tree is placed. A refusal or a failure is the stage it leaves the source at. If the
/// source is no longer this preparation's when the tree is placed, the placement is left to the
/// preparation that holds the source now, or, if none does, removed here, since the release may
/// have looked for it before it was made.
async fn place(
    shared: &Shared,
    job: &Job,
    spans: UnboundedSender<ListSpan>,
) -> Result<Made, Stage> {
    let bytes = read_list(shared, &job.key).await?;
    let (digest, at) = (job.digest, job.id.location());
    let (ttl, store) = (shared.settings.ttl, shared.store.clone());
    let v2 = is_tree_list_v2(&bytes);
    let walked = spans.clone();
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
            for span in spans_of(&files) {
                let _ = spans.unbounded_send(span);
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

/// The tree of the version 2 list in `bytes`, which must hash to `digest`, each span queued on
/// `spans` as the walk meets it. The digest is checked before the walk, so no span of a list
/// that is not the one the ref names is read; it is about a third of the time to the first span
/// (the reference's measurements).
fn built_v2(
    bytes: &[u8],
    digest: &TreeDigest,
    limits: &TreeLimits,
    spans: &UnboundedSender<ListSpan>,
) -> Result<BuiltTree, Stage> {
    let actual = TreeDigest::of(bytes);
    if actual != *digest {
        let why = TreeListError::DigestMismatch {
            expected: *digest,
            actual,
        };
        return Err(Stage::Refused(why.to_string()));
    }
    let refused = |why: crate::TreeListV2Error| Stage::Refused(why.to_string());
    TreeListV2::open(bytes)
        .map_err(refused)?
        .build(limits, |span| {
            let _ = spans.unbounded_send(span);
        })
        .map_err(refused)
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

/// The spans to read for `files`: each digest once, at its first file's span (a file of a tree
/// list has a source exactly when it has bytes).
pub(super) fn spans_of(files: &[BuiltFile]) -> Vec<ListSpan> {
    let mut seen = BTreeSet::new();
    files
        .iter()
        .filter_map(|file| {
            let source = file.source.as_ref()?;
            Some(ListSpan {
                key: source.key.clone(),
                offset: source.offset,
                size: file.size,
                sha256: file.sha256,
            })
        })
        // The tree store keeps one span for each digest, the first file's in the order of the
        // files, and an open reads that span: so is it read here.
        .filter(|span| seen.insert(*span.sha256.as_bytes()))
        .collect()
}

/// Reads the `spans` into the cache as they come; how many are in the cache and how many were not
/// read (a failure, a bound passed, a release, a refused list or a stop before their turn).
pub(super) async fn fill(
    shared: &Shared,
    spans: impl Stream<Item = ListSpan>,
    released: &AtomicBool,
    refused: &AtomicBool,
) -> (usize, usize) {
    let at_once = shared.store.config().fill_concurrency;
    let read: Vec<bool> = spans
        .map(|span| async move { fetch(shared, &span, released, refused).await })
        .buffer_unordered(at_once)
        .collect()
        .await;
    let fetched = read.iter().filter(|ok| **ok).count();
    (fetched, read.len() - fetched)
}

/// Reads one span into the cache, within the bound of a file, once a read of the node's is free;
/// whether it is in the cache.
async fn fetch(
    shared: &Shared,
    span: &ListSpan,
    released: &AtomicBool,
    refused: &AtomicBool,
) -> bool {
    let Ok(_turn) = shared.fills.acquire().await else {
        return false;
    };
    if released.load(Ordering::SeqCst) || refused.load(Ordering::SeqCst) {
        return false;
    }
    let (size, sha256) = (span.size, &span.sha256);
    let file_span = FileSpan {
        pack: span.key.to_string(),
        offset: span.offset,
    };
    let bound = shared.settings.fill_timeout;
    let read = tokio::time::timeout(bound, shared.store.fetch_span(&file_span, size, sha256)).await;
    let why = match read {
        Ok(Ok(_)) => return true,
        Ok(Err(why)) => why.to_string(),
        Err(_) => format!("not read within {bound:?}"),
    };
    tracing::warn!(
        "tierstore: mount provider: {size} bytes at {} of {}: {why}",
        span.offset,
        span.key
    );
    false
}
