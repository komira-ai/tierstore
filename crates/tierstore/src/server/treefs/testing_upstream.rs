//! The upstream of the core's rig (`testing.rs`): a fake object store that holds a [`Built`] and
//! keeps the fills the node's cache made of it in the words of the core's requests.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};

use async_trait::async_trait;
use bytes::Bytes;

use crate::{
    Backend, ByteRange, Error, Listed, MemoryBackend, Meta, OpenUpload, PartBody, PutAnswer,
    UploadedPart, UploadsAfter,
};

use super::locked;
use super::testing::{Built, PACK};

/// The upstream of the node's cache: it holds the objects of a [`Built`], which a test may change
/// at any time, and keeps the fills the cache made of them in the words of the core's requests.
pub(in crate::server) struct Upstream {
    pub built: Mutex<Built>,
    /// The digests whose fill fails with these words: `upstream <status>` (an answer of that
    /// status), or `mismatch` (bytes that are not the digest's).
    pub failing: Mutex<HashMap<String, String>>,
    /// The fills received, in order, each in the words of the core's request: `fill <sha256>` for
    /// a manifest, `fill <sha256> <size> from <key> @ <offset>` for a blob.
    pub requests: Mutex<Vec<String>>,
    /// The objects, as the cache reads and writes them.
    store: MemoryBackend,
    /// What of the built objects is in `store`, by key.
    published: Mutex<HashMap<String, Bytes>>,
    /// Whether the fills of blobs wait (see [`Upstream::hold_fills`]).
    holding: Mutex<bool>,
    released: Condvar,
    /// The fills of manifests in flight (see [`Upstream::gate_manifests`]).
    gate: Mutex<Gate>,
    gate_changed: Condvar,
    /// The fills in flight, once a test closes their gate ([`Upstream::gate_verb`]).
    fills: Mutex<Option<Arc<VerbGate>>>,
}

/// The gate of the fills, and what wakes the fills that wait at it.
type VerbGate = (Mutex<Gate>, Condvar);

/// The progress of the fills, which a test may pace.
#[derive(Default)]
struct Gate {
    /// Whether a fill waits for a permit.
    gating: bool,
    /// The fills received.
    started: usize,
    /// The fills received and not answered, and the most there have been at once.
    in_flight: usize,
    most: usize,
    /// The permits given and the permits used.
    given: usize,
    used: usize,
    /// Whether the permits go to the fills in the order they were received (the gate of
    /// [`Upstream::gate_verb`]), and not to whichever asks first.
    ordered: bool,
}

/// What a read of the upstream is a fill of.
enum Fill {
    /// The manifest of this digest.
    Manifest(String),
    /// The blob of this digest, whose bytes begin at the read.
    Blob(String),
}

impl Default for Upstream {
    fn default() -> Self {
        Self::new(Built::default())
    }
}

impl Upstream {
    /// An upstream that holds `built`.
    pub(in crate::server) fn new(built: Built) -> Self {
        Self {
            built: Mutex::new(built),
            failing: Mutex::new(HashMap::new()),
            requests: Mutex::new(Vec::new()),
            store: MemoryBackend::new(),
            published: Mutex::new(HashMap::new()),
            holding: Mutex::new(false),
            released: Condvar::new(),
            gate: Mutex::new(Gate::default()),
            gate_changed: Condvar::new(),
            fills: Mutex::new(None),
        }
    }

    /// Puts what `built` holds now in the store, where it is not there as it is, and takes out of
    /// it what `built` held before and holds no more.
    async fn publish(&self) {
        let objects: Vec<(String, Bytes)> = locked(&self.built)
            .objects()
            .into_iter()
            .map(|(key, bytes)| (key, Bytes::copy_from_slice(bytes)))
            .collect();
        let gone: Vec<String> = locked(&self.published)
            .keys()
            .filter(|key| !objects.iter().any(|(built, _)| built == *key))
            .cloned()
            .collect();
        for key in gone {
            let _ = self.store.delete(&key).await;
            let _ = locked(&self.published).remove(&key);
        }
        for (key, bytes) in objects {
            if locked(&self.published).get(&key) == Some(&bytes) {
                continue;
            }
            let _ = self.store.delete(&key).await;
            self.store.put(&key, bytes.clone()).await.unwrap();
            let _ = locked(&self.published).insert(key, bytes);
        }
    }

    /// What a HEAD of `key` (`head`) or a read of `range` of it is a fill of: a HEAD of a
    /// manifest, or the read of a blob's first bytes. Other reads (the chunks after a blob's
    /// first, the GET of a manifest after its HEAD) are of no fill.
    fn fill_of(&self, key: &str, range: Option<ByteRange>) -> Option<(Fill, String)> {
        let built = locked(&self.built);
        let offset = range.map_or(0, |range| range.offset());
        if let Some(digest) = key.strip_prefix("cas/l/") {
            let manifest = built.manifests.contains_key(digest) || !built.blobs.contains_key(digest);
            return match (manifest, range) {
                (true, None) => Some((Fill::Manifest(digest.to_owned()), format!("fill {digest}"))),
                (false, Some(range)) if range.offset() == 0 => {
                    let size = built.blobs[digest].len();
                    let line = format!("fill {digest} {size} from {key} @ 0");
                    Some((Fill::Blob(digest.to_owned()), line))
                }
                _ => None,
            };
        }
        if key != PACK {
            return None;
        }
        let (digest, _) = built.offsets.iter().find(|(_, at)| **at == offset)?;
        let size = built.blobs[digest].len();
        let line = format!("fill {digest} {size} from {key} @ {offset}");
        Some((Fill::Blob(digest.clone()), line))
    }

    /// Takes the fill of a read: keeps its words, and waits at the gates a test closed. The
    /// failure the test named for its digest, if any.
    fn take(&self, fill: &Fill, line: String) -> Option<String> {
        locked(&self.requests).push(line);
        let gate = locked(&self.fills).clone();
        if let Some(gate) = &gate {
            tokio::task::block_in_place(|| Self::enter(&gate.0, &gate.1));
        }
        let digest = match fill {
            Fill::Manifest(digest) => {
                tokio::task::block_in_place(|| {
                    self.enter_manifest();
                    self.leave_manifest();
                });
                digest
            }
            Fill::Blob(digest) => {
                tokio::task::block_in_place(|| self.wait_while_held());
                digest
            }
        };
        if let Some(gate) = &gate {
            Self::leave(&gate.0, &gate.1);
        }
        locked(&self.failing).get(digest).cloned()
    }

    /// The error of a fill that fails with `why`.
    fn failure(key: &str, why: &str) -> Error {
        match why {
            "upstream 404" => Error::NotFound(key.to_owned()),
            "upstream 503" => Error::Unavailable(format!("{key}: the test says so")),
            other => panic!("the upstream cannot make a fill fail with {other:?}"),
        }
    }

    /// Makes every fill of a manifest wait, once it is received, for a permit of
    /// [`Upstream::permit_manifests`]: for a test that paces the fills by its own events.
    pub(in crate::server) fn gate_manifests(&self) {
        locked(&self.gate).gating = true;
    }

    /// Lets `count` more fills of manifests go on.
    pub(in crate::server) fn permit_manifests(&self, count: usize) {
        locked(&self.gate).given += count;
        self.gate_changed.notify_all();
    }

    /// Waits until `count` fills of manifests have been received in all.
    pub(in crate::server) fn wait_for_manifest_fills(&self, count: usize) {
        let gate = locked(&self.gate);
        let _gate = self
            .gate_changed
            .wait_while(gate, |gate| gate.started < count)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }

    /// The most fills of manifests that were in flight at once.
    pub(in crate::server) fn most_manifest_fills(&self) -> usize {
        locked(&self.gate).most
    }

    fn enter_manifest(&self) {
        Self::enter(&self.gate, &self.gate_changed);
    }

    fn leave_manifest(&self) {
        Self::leave(&self.gate, &self.gate_changed);
    }

    /// Makes every fill received from now on (`verb` is `fill`: the upstream sees no other
    /// request) wait for a permit of [`Upstream::permit_verb`], once received: for a test that
    /// needs a fill in flight.
    pub(in crate::server) fn gate_verb(&self, verb: &str) {
        assert_eq!(verb, "fill", "the upstream sees the fills alone");
        let gate = Gate {
            gating: true,
            ordered: true,
            ..Gate::default()
        };
        *locked(&self.fills) = Some(Arc::new((Mutex::new(gate), Condvar::new())));
    }

    /// The gate of the fills.
    fn verb(&self, verb: &str) -> Arc<VerbGate> {
        assert_eq!(verb, "fill", "the upstream sees the fills alone");
        locked(&self.fills).clone().expect("a gate of the fills")
    }

    /// Lets `count` more fills go on: the next ones in the order they were received.
    pub(in crate::server) fn permit_verb(&self, verb: &str, count: usize) {
        let gate = self.verb(verb);
        locked(&gate.0).given += count;
        gate.1.notify_all();
    }

    /// Waits until `count` fills have been received in all, since the gate was closed.
    pub(in crate::server) fn wait_for_verb(&self, verb: &str, count: usize) {
        let gate = self.verb(verb);
        let _gate = gate
            .1
            .wait_while(locked(&gate.0), |gate| gate.started < count)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }

    /// The fills received in all, since the gate was closed.
    pub(in crate::server) fn received(&self, verb: &str) -> usize {
        locked(&self.verb(verb).0).started
    }

    /// Counts a fill received on `gate`, and waits for a permit if the gate is closed.
    fn enter(gate: &Mutex<Gate>, changed: &Condvar) {
        let mut gate = locked(gate);
        gate.started += 1;
        gate.in_flight += 1;
        gate.most = gate.most.max(gate.in_flight);
        let mine = gate.started;
        changed.notify_all();
        while gate.gating
            && (if gate.ordered {
                mine > gate.given
            } else {
                gate.used >= gate.given
            })
        {
            gate = changed
                .wait(gate)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        gate.used += 1;
    }

    /// Counts a fill of `gate` answered.
    fn leave(gate: &Mutex<Gate>, changed: &Condvar) {
        locked(gate).in_flight -= 1;
        changed.notify_all();
    }

    /// Makes every fill of a blob wait, once it is received, until [`Upstream::release_fills`]: for
    /// a test that needs a fill in flight.
    pub(in crate::server) fn hold_fills(&self) {
        *locked(&self.holding) = true;
    }

    /// Lets the fills that wait go on, and those that come.
    pub(in crate::server) fn release_fills(&self) {
        *locked(&self.holding) = false;
        self.released.notify_all();
    }

    fn wait_while_held(&self) {
        let mut holding = locked(&self.holding);
        while *holding {
            holding = self
                .released
                .wait(holding)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// The requests that begin with `verb` (`fill`: the upstream sees no other).
    pub(in crate::server) fn asked(&self, verb: &str) -> Vec<String> {
        assert_eq!(verb, "fill", "the upstream sees the fills alone");
        locked(&self.requests).clone()
    }

    /// The objects the cache put, by key: what the put directory's puts made.
    pub(in crate::server) async fn put_objects(&self) -> Vec<(String, Bytes)> {
        let built: Vec<String> = locked(&self.published).keys().cloned().collect();
        let mut objects = Vec::new();
        for listed in self.store.list("").await.unwrap() {
            if !built.contains(&listed.key) {
                let bytes = self.store.get(&listed.key, None).await.unwrap();
                objects.push((listed.key, bytes));
            }
        }
        objects
    }
}

#[async_trait]
impl Backend for Upstream {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.store.put(key, body).await
    }

    async fn put_tagged(&self, key: &str, body: Bytes) -> Result<String, Error> {
        self.store.put_tagged(key, body).await
    }

    async fn put_stream_answered(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        hot_only: bool,
    ) -> Result<Option<PutAnswer>, Error> {
        self.store
            .put_stream_answered(key, len, body, hot_only)
            .await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.store.put_if_match(key, body, etag).await
    }

    async fn put_stream_if_match(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        etag: &str,
    ) -> Result<String, Error> {
        self.store.put_stream_if_match(key, len, body, etag).await
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> Result<(), Error> {
        self.store.delete_if_match(key, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.publish().await;
        let why = match self.fill_of(key, range) {
            Some((fill, line)) => self.take(&fill, line),
            None => None,
        };
        // A manifest's fill fails at its HEAD; its bytes, read after, are the digest's or not.
        let why = why.or_else(|| {
            let digest = key.strip_prefix("cas/l/")?;
            let why = locked(&self.failing).get(digest).cloned()?;
            (why == "mismatch").then_some(why)
        });
        match why.as_deref() {
            Some("mismatch") => {
                let size = range.map_or(1, ByteRange::size);
                Ok(Bytes::from(vec![0xa5; usize::try_from(size).unwrap()]))
            }
            Some(why) => Err(Self::failure(key, why)),
            None => self.store.get(key, range).await,
        }
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.publish().await;
        if let Some((fill @ Fill::Manifest(_), line)) = self.fill_of(key, None)
            && let Some(why) = self.take(&fill, line)
            && why != "mismatch"
        {
            return Err(Self::failure(key, &why));
        }
        self.store.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.store.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.publish().await;
        self.store.list(prefix).await
    }

    async fn create_upload(&self, key: &str) -> Result<String, Error> {
        self.store.create_upload(key).await
    }

    async fn upload_part(
        &self,
        key: &str,
        id: &str,
        number: u32,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        self.store.upload_part(key, id, number, body).await
    }

    async fn complete_upload(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        self.store.complete_upload(key, id, parts).await
    }

    async fn complete_upload_if_match(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
        etag: &str,
    ) -> Result<String, Error> {
        self.store
            .complete_upload_if_match(key, id, parts, etag)
            .await
    }

    async fn abort_upload(&self, key: &str, id: &str) -> Result<(), Error> {
        self.store.abort_upload(key, id).await
    }

    async fn list_uploads(
        &self,
        prefix: &str,
        after: Option<UploadsAfter<'_>>,
        max_uploads: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        self.store.list_uploads(prefix, after, max_uploads).await
    }

    async fn list_parts(
        &self,
        key: &str,
        id: &str,
        after: Option<u32>,
        max_parts: usize,
    ) -> Result<Vec<UploadedPart>, Error> {
        self.store.list_parts(key, id, after, max_parts).await
    }
}
