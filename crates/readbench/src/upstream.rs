//! The read path's `Upstream` over the farm's store: an object's location is the filer's entry
//! of it (its chunks, and the volume servers that hold each chunk's volume), and a part is one
//! ranged read of one needle from one volume server, so each drive of the store is a device of
//! the per-drive budget. What each part took is kept in a [`Recorder`].

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt as _};
use sha2::{Digest as _, Sha256};
use tierstore_fetch_api::ids::{ByteRange, PackKey};
use tierstore_fetch_api::upstream::{
    DeviceRef, DriveSignals, GetRequest, Located, Location, NotDefinitive, PartResponse, Report,
    Source, SourceKind, Upstream, UpstreamError,
};
use tierstore_format::ids::{BlockId, COUNTER_BITS, Generation};

use crate::Error;
use crate::clock::Clock;
use crate::http::{BodyStream, HttpClient};
use crate::swfs::{Entry, FilerFetch, VolumeMap, device, device_label};

/// What one part took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartTiming {
    /// The device it was read from, as [`device_label`] names it.
    pub device: String,
    /// The class of the fetch (`demand`, `prepare`, `background`).
    pub class: String,
    /// The bytes that arrived.
    pub len: u64,
    /// Microseconds from the request to the first byte of the body (to the end for a part that
    /// had none).
    pub first_byte_us: u64,
    /// Microseconds from the request to the end of the body, or to its failure or abandonment.
    pub total_us: u64,
    /// Whether the part arrived whole.
    pub ok: bool,
}

/// What the upstream saw: every part, and how many reports and failed locates.
#[derive(Debug, Default)]
pub struct Recorder {
    parts: Mutex<Vec<PartTiming>>,
    reports: Mutex<BTreeMap<String, u64>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Recorder {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Every part so far, in the order they ended.
    #[must_use]
    pub fn parts(&self) -> Vec<PartTiming> {
        lock(&self.parts).clone()
    }

    /// The bytes that arrived, over every part.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        lock(&self.parts).iter().map(|p| p.len).sum()
    }

    /// How many reports of each kind the engine made (`bitrot`), and how many locates failed
    /// (`locate_error`).
    #[must_use]
    pub fn reports(&self) -> BTreeMap<String, u64> {
        lock(&self.reports).clone()
    }

    fn push(&self, part: PartTiming) {
        lock(&self.parts).push(part);
    }

    fn count(&self, kind: &str) {
        *lock(&self.reports).entry(kind.to_owned()).or_default() += 1;
    }
}

/// The upstream of the store: the filer for locations, the volume servers for parts.
pub struct SwfsUpstream {
    fetch: FilerFetch,
    map: Arc<VolumeMap>,
    http: HttpClient,
    recorder: Arc<Recorder>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for SwfsUpstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwfsUpstream")
            .field("base", &self.fetch.base)
            .field("volumes", &self.map.volumes())
            .finish_non_exhaustive()
    }
}

impl SwfsUpstream {
    /// An upstream that asks `fetch`'s filer for locations and reads parts with `http` from the
    /// volume servers `map` names, timing them with `clock` into `recorder`.
    #[must_use]
    pub fn new(
        fetch: FilerFetch,
        map: Arc<VolumeMap>,
        http: HttpClient,
        recorder: Arc<Recorder>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            fetch,
            map,
            http,
            recorder,
            clock,
        }
    }

    /// The URL and the byte range (inside the needle) of a request, or why it cannot be made.
    fn target(&self, req: &GetRequest) -> Result<(String, (u64, u64)), String> {
        let SourceKind::Cold { shard, .. } = &req.source.kind else {
            return Err("the source is not a needle of a volume".to_owned());
        };
        let volume: u32 = shard
            .split_once(',')
            .and_then(|(vid, _)| vid.parse().ok())
            .ok_or_else(|| format!("file id {shard:?} is not <volume id>,<needle>"))?;
        let node = req.source.device.node().0;
        let replica = self
            .map
            .replicas(volume)
            .iter()
            .find(|r| r.node == node)
            .ok_or_else(|| format!("node {node} holds no copy of volume {volume}"))?;
        let start = req
            .range
            .offset
            .checked_sub(req.source.extent.offset)
            .ok_or_else(|| "the range starts before the source's extent".to_owned())?;
        let end = start
            .checked_add(req.range.len)
            .ok_or_else(|| "the range runs past 2^64".to_owned())?;
        Ok((format!("http://{}/{shard}", replica.addr), (start, end)))
    }
}

/// The generation an entity tag stands for: its 32 hex digits, or, for any other text, the first
/// 16 bytes of its SHA-256.
fn generation_of(etag: &str) -> Generation {
    let mut bytes = [0u8; 16];
    match hex::decode(etag) {
        Ok(raw) if raw.len() == 16 => bytes.copy_from_slice(&raw),
        _ => bytes.copy_from_slice(&Sha256::digest(etag.as_bytes())[..16]),
    }
    Generation(bytes)
}

/// The generation of a pack from its filer entry: its entity tag's, or the key's when the entry
/// has no tag.
pub(crate) fn entry_generation(key: &str, entry: &Entry) -> Generation {
    generation_of(entry.etag.as_deref().unwrap_or(key))
}

/// The status a failed request names, when [`HttpClient`] failed it for its status.
fn status_of(e: &Error) -> Option<u16> {
    match e {
        Error::Http { why, .. } => why.strip_prefix("status ")?.parse().ok(),
        _ => None,
    }
}

#[async_trait]
impl Upstream for SwfsUpstream {
    async fn locate(&self, key: &PackKey) -> Result<Located, UpstreamError> {
        let entry = match self.fetch.entry(&key.0).await {
            Ok(Some(entry)) => entry,
            Ok(None) => return Ok(Located::DefinitiveMiss),
            Err(_) => {
                self.recorder.count("locate_error");
                return Ok(Located::NotDefinitive(NotDefinitive::TooFewAnswers));
            }
        };
        let mut sources = Vec::new();
        for chunk in &entry.chunks {
            let replicas = self.map.replicas(chunk.volume);
            if replicas.is_empty() {
                return Ok(Located::NotDefinitive(NotDefinitive::TooFewAnswers));
            }
            for r in replicas {
                sources.push(Source {
                    device: device(r),
                    kind: SourceKind::Cold {
                        block: BlockId::from_raw(
                            (u64::from(r.node) << COUNTER_BITS) | u64::from(chunk.volume),
                        ),
                        shard: chunk.fid.clone(),
                        offset: 0,
                    },
                    extent: ByteRange {
                        offset: chunk.offset,
                        len: chunk.size,
                    },
                });
            }
        }
        Ok(Located::Found(Location {
            generation: entry_generation(&key.0, &entry),
            size: Some(entry.size),
            sources,
            conflicts: Vec::new(),
        }))
    }

    async fn get(&self, req: GetRequest) -> Result<PartResponse, UpstreamError> {
        let device = req.source.device;
        let transport = |reason: String| UpstreamError::Transport {
            key: req.key.clone(),
            device,
            reason,
        };
        let (url, (start, end)) = self.target(&req).map_err(transport)?;
        let timer = PartTimer {
            started: self.clock.now_us(),
            clock: self.clock.clone(),
            recorder: self.recorder.clone(),
            device: device_label(device),
            class: req.class.header().to_owned(),
        };
        let answer = match self.http.get(&url, Some((start, end)), false).await {
            Ok(answer) if answer.status != 404 => answer,
            Ok(_) => {
                timer.record(0, None, false);
                return Err(UpstreamError::NotFound {
                    key: req.key,
                    device,
                });
            }
            Err(e) => {
                timer.record(0, None, false);
                return Err(if status_of(&e) == Some(503) {
                    UpstreamError::Unavailable {
                        key: req.key,
                        device,
                        retry_after_ms: 1000,
                        signals: DriveSignals::default(),
                    }
                } else {
                    transport(e.to_string())
                });
            }
        };
        Ok(PartResponse {
            generation: req.generation,
            signals: DriveSignals::default(),
            body: Box::pin(PartBody {
                inner: answer.body,
                key: req.key.clone(),
                device,
                want: req.range.len,
                got: 0,
                first: None,
                done: false,
                timer,
            }),
        })
    }

    async fn report(&self, _key: &PackKey, _source: &Source, _range: ByteRange, report: Report) {
        self.recorder.count(report.header());
    }
}

/// Where a part started and what to say about it when it ends.
struct PartTimer {
    started: u64,
    clock: Arc<dyn Clock>,
    recorder: Arc<Recorder>,
    device: String,
    class: String,
}

impl PartTimer {
    /// Records the part as it is now.
    fn record(&self, len: u64, first: Option<u64>, ok: bool) {
        let total_us = self.clock.now_us().saturating_sub(self.started);
        self.recorder.push(PartTiming {
            device: self.device.clone(),
            class: self.class.clone(),
            len,
            first_byte_us: first.unwrap_or(total_us),
            total_us,
            ok,
        });
    }
}

/// The body of a part: the answer's bytes, checked to be as many as were asked, and timed.
struct PartBody {
    inner: BodyStream,
    key: PackKey,
    device: DeviceRef,
    want: u64,
    got: u64,
    first: Option<u64>,
    done: bool,
    timer: PartTimer,
}

impl PartBody {
    fn finish(&mut self, ok: bool) {
        self.done = true;
        self.timer.record(self.got, self.first, ok);
    }

    fn failure(&mut self, reason: String) -> Option<Result<Bytes, UpstreamError>> {
        self.finish(false);
        Some(Err(UpstreamError::Transport {
            key: self.key.clone(),
            device: self.device,
            reason,
        }))
    }
}

impl Stream for PartBody {
    type Item = Result<Bytes, UpstreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        match self.inner.poll_next_unpin(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(piece))) => {
                let since = self.timer.clock.now_us().saturating_sub(self.timer.started);
                self.first.get_or_insert(since);
                self.got += piece.len() as u64;
                Poll::Ready(Some(Ok(piece)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(self.failure(e.to_string())),
            Poll::Ready(None) if self.got == self.want => {
                self.finish(true);
                Poll::Ready(None)
            }
            Poll::Ready(None) => {
                let reason = format!("the body had {} bytes, {} were asked", self.got, self.want);
                Poll::Ready(self.failure(reason))
            }
        }
    }
}

impl Drop for PartBody {
    fn drop(&mut self) {
        if !self.done {
            self.finish(false);
        }
    }
}

#[cfg(test)]
#[path = "upstream_tests.rs"]
mod tests;
