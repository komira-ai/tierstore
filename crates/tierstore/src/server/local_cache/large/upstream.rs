//! The upstream of the read path: the S3 cold store a build node is configured with
//! (`[cold_store] kind = "s3"`), as the fetch engine's [`Upstream`] (section 9 of
//! `docs/design/fetch-cache-serve.md`). `LOCATE` is one HEAD of the key; a part is one ranged
//! GET that carries `If-Match` with the object's entity tag, so no part reads another object
//! than the one `LOCATE` found.
//!
//! **Generations.** A location names the object's generation, and every part carries it. The
//! generation of an object is its entity tag: a tag of 32 hex digits (the MD5 of an object put
//! whole, or the content-derived generation of a tierstore storage node) is read as the 16 bytes
//! it spells, so the client of the S3 front sees the store's own tag; any other tag (an object
//! made by a multipart upload, `<hex>-<parts>`) is named by the first 16 bytes of the SHA-256 of
//! its text. The tag a generation came from is remembered, for at most [`TAGS_LIMIT`] objects
//! (the oldest is forgotten first), so that its parts can send it. A part of a generation that is
//! not remembered (an entry the journal restored after a restart, or one forgotten) asks HEAD
//! first: the part is sent when the object's tag still names that generation, and is a `412`
//! ([`UpstreamError::PreconditionFailed`]) when it names another.
//!
//! **Sources.** The store is one device, [`STORE`], which holds the whole of every object: a
//! location has one source, a hot copy of the object from its first byte to its last.
//!
//! **Failures.** A HEAD that finds no object is a definitive miss (a GET's is
//! [`UpstreamError::NotFound`]); a `412` is [`UpstreamError::PreconditionFailed`]; a `503`, a
//! `500` or another answer of the 5xx class is [`UpstreamError::Unavailable`], to be retried after
//! [`RETRY_AFTER_MS`]; anything else, a failure of the transport included, is
//! [`UpstreamError::Transport`] with the store's text. A report of a bad cell (`x-ts-report`) has
//! nowhere to go: the store keeps no per-cell checksums of the objects it holds, so it is dropped.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use futures::StreamExt as _;
use objstore_s3::{ByteRange, ObjectKey, ObjectStoreError};
use sha2::{Digest as _, Sha256};
use tierstore_fetch_api::ids::{ByteRange as PackRange, PackKey};
use tierstore_fetch_api::upstream::{
    DeviceRef, DriveSignals, GetRequest, Located, Location, PartResponse, Report, Source,
    SourceKind, Upstream, UpstreamError,
};
use tierstore_format::ids::{Generation, NodeId};

use crate::S3Backend;

/// The one device of the store.
pub(crate) const STORE: DeviceRef = DeviceRef::Hot(NodeId(0));

/// The most objects whose tags are remembered.
pub(crate) const TAGS_LIMIT: usize = 65_536;

/// How long the engine waits before it asks again after the store answered `503` or another
/// answer of the 5xx class.
pub(crate) const RETRY_AFTER_MS: u64 = 1_000;

/// The name the location gives the store's copy of an object.
const FILE: &str = "s3";

/// The tags of the generations met, oldest first.
#[derive(Default)]
struct Tags {
    by_generation: BTreeMap<(String, Generation), String>,
    order: VecDeque<(String, Generation)>,
}

/// The S3 cold store as the engine's [`Upstream`] (see the module).
pub(crate) struct S3Upstream {
    backend: Arc<S3Backend>,
    tags: Mutex<Tags>,
    limit: usize,
}

impl S3Upstream {
    /// The upstream over the bucket of `backend`.
    #[must_use]
    pub(crate) fn new(backend: Arc<S3Backend>) -> Self {
        Self::with_limit(backend, TAGS_LIMIT)
    }

    /// The same, remembering the tags of at most `limit` objects (at least one).
    #[must_use]
    pub(crate) fn with_limit(backend: Arc<S3Backend>, limit: usize) -> Self {
        Self {
            backend,
            tags: Mutex::new(Tags::default()),
            limit: limit.max(1),
        }
    }

    fn tags(&self) -> std::sync::MutexGuard<'_, Tags> {
        self.tags.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Remembers that `generation` of `key` is the tag `etag`.
    fn remember(&self, key: &str, generation: Generation, etag: &str) {
        let mut tags = self.tags();
        let id = (key.to_owned(), generation);
        if tags
            .by_generation
            .insert(id.clone(), etag.to_owned())
            .is_none()
        {
            tags.order.push_back(id);
        }
        let excess = tags.order.len().saturating_sub(self.limit);
        let forgotten: Vec<(String, Generation)> = tags.order.drain(..excess).collect();
        for oldest in forgotten {
            tags.by_generation.remove(&oldest);
        }
    }

    /// The tag of `generation` of `key`, if it is remembered.
    fn remembered(&self, key: &str, generation: Generation) -> Option<String> {
        let id = (key.to_owned(), generation);
        self.tags().by_generation.get(&id).cloned()
    }

    /// The tag of `generation` of `key`: remembered, or from a HEAD that finds the object still
    /// at that generation.
    async fn tag_of(&self, key: &PackKey, generation: Generation) -> Result<String, UpstreamError> {
        if let Some(etag) = self.remembered(&key.0, generation) {
            return Ok(etag);
        }
        let object = object_key(key)?;
        let head = self.backend.store().head(&object).await;
        let head = head.map_err(|error| failed(key, &error))?;
        let now = generation_of(&head.etag);
        if now != generation {
            return Err(UpstreamError::PreconditionFailed {
                key: key.clone(),
                device: STORE,
                actual: Some(now),
            });
        }
        self.remember(&key.0, generation, &head.etag);
        Ok(head.etag)
    }
}

#[async_trait::async_trait]
impl Upstream for S3Upstream {
    async fn locate(&self, key: &PackKey) -> Result<Located, UpstreamError> {
        let object = object_key(key)?;
        let head = match self.backend.store().head(&object).await {
            Ok(head) => head,
            Err(ObjectStoreError::NotFound(_)) => return Ok(Located::DefinitiveMiss),
            Err(error) => return Err(failed(key, &error)),
        };
        let generation = generation_of(&head.etag);
        self.remember(&key.0, generation, &head.etag);
        Ok(Located::Found(Location {
            generation,
            size: Some(head.size),
            sources: vec![whole(head.size)],
            conflicts: Vec::new(),
        }))
    }

    async fn get(&self, req: GetRequest) -> Result<PartResponse, UpstreamError> {
        let etag = self.tag_of(&req.key, req.generation).await?;
        let object = object_key(&req.key)?;
        let range = ByteRange::new(req.range.offset, req.range.len)
            .ok_or_else(|| transport(&req.key, format!("no bytes in the range {:?}", req.range)))?;
        let read = self
            .backend
            .store()
            .get_stream_if_match(&object, range, &etag);
        let read = read.await.map_err(|error| failed(&req.key, &error))?;
        // The tag of the answer is the one the bytes are of; a store that sent no tag is taken to
        // have kept the condition it was sent.
        let generation = read.etag.as_deref().map_or(req.generation, generation_of);
        let key = req.key.clone();
        let body = read
            .body
            .map(move |piece| piece.map_err(|error| failed(&key, &error)))
            .boxed();
        Ok(PartResponse {
            generation,
            signals: DriveSignals::default(),
            body,
        })
    }

    async fn report(&self, _key: &PackKey, _source: &Source, _range: PackRange, _report: Report) {}
}

/// The generation an entity tag names (see the module).
#[must_use]
pub(crate) fn generation_of(etag: &str) -> Generation {
    let text = etag.trim_matches('"');
    let mut bytes = [0u8; 16];
    if hex::decode_to_slice(text, &mut bytes).is_ok() {
        return Generation(bytes);
    }
    let digest = Sha256::digest(text.as_bytes());
    bytes.copy_from_slice(&digest[..16]);
    Generation(bytes)
}

/// The one source of an object of `size` bytes: the store's copy of the whole of it.
fn whole(size: u64) -> Source {
    Source {
        device: STORE,
        kind: SourceKind::Hot {
            file: FILE.to_owned(),
            offset: 0,
        },
        extent: PackRange {
            offset: 0,
            len: size,
        },
    }
}

/// The store's name for `key`.
fn object_key(key: &PackKey) -> Result<ObjectKey, UpstreamError> {
    ObjectKey::new(key.0.as_str()).map_err(|error| transport(key, error.to_string()))
}

/// A failure of the transport, or one the engine has no other name for, with `reason`.
fn transport(key: &PackKey, reason: String) -> UpstreamError {
    UpstreamError::Transport {
        key: key.clone(),
        device: STORE,
        reason,
    }
}

/// What a failed call of the store on `key` is to the engine (see the module).
fn failed(key: &PackKey, error: &ObjectStoreError) -> UpstreamError {
    match error {
        ObjectStoreError::NotFound(_) => UpstreamError::NotFound {
            key: key.clone(),
            device: STORE,
        },
        ObjectStoreError::Service { status: 412, .. } => UpstreamError::PreconditionFailed {
            key: key.clone(),
            device: STORE,
            actual: None,
        },
        ObjectStoreError::Service { status, .. } if (500..600).contains(status) => {
            UpstreamError::Unavailable {
                key: key.clone(),
                device: STORE,
                retry_after_ms: RETRY_AFTER_MS,
                signals: DriveSignals::default(),
            }
        }
        other => transport(key, other.to_string()),
    }
}
