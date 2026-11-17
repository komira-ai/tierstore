//! The preparation of one source, on a task of its own: the tree list read and checked, the tree
//! built and placed at the source's path, and then the bytes of its files read into the cache.
//!
//! The files are read one call each, by [`TreeStore::fetch_span`] with the file's own span, size
//! and digest: the call an open of the mount makes for the file, which joins a fill of the same
//! range already running (the cache's flights, keyed by the pack, the range and the digest). So an
//! open of a file this preparation is reading waits for that one read, and a file the job opens
//! before its turn here is read once, by whichever asks first. Files of one digest are read once,
//! at the span the tree store keeps for the digest, which is the one an open reads.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt as _;
use tokio::sync::watch;

use super::names::SourceId;
use super::sources::{Owner, Shared, Stage};
use crate::server::local_cache::trees::{CreateError, CreateRequest, Created, FileSpan};
use crate::{Backend as _, BuiltFile, Error, TreeDigest, build_tree, parse_tree_list};

/// A preparation: the source, the tree list it names, and what it reports to.
pub(super) struct Job {
    pub id: SourceId,
    pub key: String,
    pub digest: TreeDigest,
    pub stage: Arc<watch::Sender<Stage>>,
    pub released: Arc<AtomicBool>,
}

/// What a placed tree needs to be filled.
struct Made {
    tree: TreeDigest,
    files: Vec<BuiltFile>,
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
    let made = match place(&shared, &job).await {
        Ok(made) => made,
        Err(stage) => {
            if let Stage::Refused(why) | Stage::Failed(why) = &stage {
                tracing::warn!(
                    "tierstore: mount provider: {} of {} not prepared: {why}",
                    job.id.path(),
                    job.key
                );
            }
            report(&job.stage, stage);
            return;
        }
    };
    report(&job.stage, Stage::Placed);
    let (fetched, failed) = fill(&shared, &made.files, &job.released).await;
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
/// thread; a refusal or a failure as the stage it leaves the source at. If the source is no longer
/// this preparation's when the tree is placed, the placement is left to the preparation that holds
/// the source now, or, if none does, removed here, since the release may have looked for it before
/// it was made.
async fn place(shared: &Shared, job: &Job) -> Result<Made, Stage> {
    let bytes = read_list(shared, &job.key).await?;
    let (digest, at) = (job.digest, job.id.location());
    let (ttl, store) = (shared.settings.ttl, shared.store.clone());
    let made = tokio::task::spawn_blocking(move || -> Result<(Created, Vec<BuiltFile>), Stage> {
        let list =
            parse_tree_list(&bytes, &digest).map_err(|why| Stage::Refused(why.to_string()))?;
        let entries = list
            .into_entries()
            .map_err(|why| Stage::Refused(why.to_string()))?;
        let built = build_tree(None, entries, &store.config().limits())
            .map_err(|why| Stage::Refused(format!("the tree is refused: {why}")))?;
        let files = built.files.clone();
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
        Owner::Ours => Ok(Made {
            tree: created.tree,
            files,
        }),
        Owner::Newer => Err(Stage::Released),
        Owner::Gone => {
            let _ = shared.store.release(&created.placement);
            Err(Stage::Released)
        }
    }
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

/// Reads the bytes of `files` into the cache, each span once; how many spans are in the cache and
/// how many were not read (a failure, a bound passed, a release or a stop before their turn).
pub(super) async fn fill(
    shared: &Shared,
    files: &[BuiltFile],
    released: &AtomicBool,
) -> (usize, usize) {
    let mut seen = BTreeSet::new();
    // A file of a tree list has a source exactly when it has bytes.
    let spans: Vec<(FileSpan, u64, TreeDigest)> = files
        .iter()
        .filter_map(|file| {
            let source = file.source.as_ref()?;
            let span = FileSpan {
                pack: source.key.to_string(),
                offset: source.offset,
            };
            Some((span, file.size, file.sha256))
        })
        // The tree store keeps one span for each digest, the first file's in path order, and an
        // open reads that span: so is it read here.
        .filter(|(_, _, sha256)| seen.insert(*sha256.as_bytes()))
        .collect();
    let at_once = shared.store.config().fill_concurrency;
    let read: Vec<bool> = futures::stream::iter(&spans)
        .map(|(span, size, sha256)| fetch(shared, span, *size, sha256, released))
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
    span: &FileSpan,
    size: u64,
    sha256: &TreeDigest,
    released: &AtomicBool,
) -> bool {
    let Ok(_turn) = shared.fills.acquire().await else {
        return false;
    };
    if released.load(Ordering::SeqCst) {
        return false;
    }
    let bound = shared.settings.fill_timeout;
    let read = tokio::time::timeout(bound, shared.store.fetch_span(span, size, sha256)).await;
    let why = match read {
        Ok(Ok(_)) => return true,
        Ok(Err(why)) => why.to_string(),
        Err(_) => format!("not read within {bound:?}"),
    };
    tracing::warn!(
        "tierstore: mount provider: {size} bytes at {} of {}: {why}",
        span.offset,
        span.pack
    );
    false
}
