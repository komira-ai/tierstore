//! A fake upstream with scripted answers.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use tierstore_format::ids::Generation;

use super::lock;
use crate::ids::{ByteRange, PackKey};
use crate::upstream::{
    DriveSignals, GetRequest, Located, Location, PartResponse, Report, Source, Upstream,
    UpstreamError,
};

/// An object the fake serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeObject {
    /// Its generation (its ETag).
    pub generation: Generation,
    /// Its bytes.
    pub bytes: Bytes,
    /// What `locate` answers for it.
    pub location: Location,
}

/// The answer the fake gives one part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartScript {
    /// The bytes of the range, in chunks of at most `chunk` bytes (0 means one chunk), with these
    /// signals.
    Serve {
        /// The chunk size.
        chunk: usize,
        /// The drive's signals in the response headers.
        signals: DriveSignals,
    },
    /// A response whose body never yields a byte (a hung drive or a stalled stream).
    Hang,
    /// The request fails with this error.
    Fail(UpstreamError),
    /// The first `after` bytes, then a transport error (a reset mid-part).
    Truncate {
        /// Bytes delivered before the error.
        after: usize,
    },
    /// The range with the byte at object offset `offset` flipped (bitrot the client's digest
    /// catches).
    Corrupt {
        /// The object offset of the flipped byte.
        offset: u64,
    },
}

#[derive(Debug, Default)]
struct State {
    objects: BTreeMap<PackKey, FakeObject>,
    located: BTreeMap<PackKey, VecDeque<Result<Located, UpstreamError>>>,
    scripts: VecDeque<PartScript>,
    gets: Vec<GetRequest>,
    locates: Vec<PackKey>,
    reports: Vec<(PackKey, Source, ByteRange, Report)>,
}

/// An upstream in memory. `get` takes the next [`PartScript`] from the queue (or serves the range
/// whole when the queue is empty); `locate` takes the next scripted answer for the key (or
/// answers the object's location, or a definitive miss). Every request is logged.
#[derive(Debug, Default)]
pub struct FakeUpstream {
    state: Mutex<State>,
}

impl FakeUpstream {
    /// An upstream with no object.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces an object under `key`.
    pub fn insert(&self, key: PackKey, object: FakeObject) {
        lock(&self.state).objects.insert(key, object);
    }

    /// Queues the answer of the next `locate` of `key`.
    pub fn script_locate(&self, key: PackKey, answer: Result<Located, UpstreamError>) {
        lock(&self.state)
            .located
            .entry(key)
            .or_default()
            .push_back(answer);
    }

    /// Queues the answer of the next part.
    pub fn script_part(&self, script: PartScript) {
        lock(&self.state).scripts.push_back(script);
    }

    /// Every part request so far, in order.
    #[must_use]
    pub fn gets(&self) -> Vec<GetRequest> {
        lock(&self.state).gets.clone()
    }

    /// Every key located so far, in order.
    #[must_use]
    pub fn locates(&self) -> Vec<PackKey> {
        lock(&self.state).locates.clone()
    }

    /// Every report so far, in order.
    #[must_use]
    pub fn reports(&self) -> Vec<(PackKey, Source, ByteRange, Report)> {
        lock(&self.state).reports.clone()
    }
}

fn transport(req: &GetRequest, reason: &str) -> UpstreamError {
    UpstreamError::Transport {
        key: req.key.clone(),
        device: req.source.device,
        reason: reason.to_owned(),
    }
}

/// The bytes of the request's range, after the `If-Match` rule: a request that names another
/// generation is `412`, a range outside the object a transport error.
fn range_of(req: &GetRequest, object: &FakeObject) -> Result<Vec<u8>, UpstreamError> {
    if req.generation.is_some_and(|g| g != object.generation) {
        return Err(UpstreamError::PreconditionFailed {
            key: req.key.clone(),
            device: req.source.device,
            actual: Some(object.generation),
        });
    }
    let end = req
        .range
        .end()
        .filter(|e| *e <= object.bytes.len() as u64)
        .ok_or_else(|| transport(req, "range outside the object"))?;
    // `offset <= end <= len`, and the bytes are in memory, so the offset fits a usize.
    let start = req.range.offset as usize;
    let end = end as usize;
    Ok(object.bytes.slice(start..end).to_vec())
}

#[async_trait::async_trait]
impl Upstream for FakeUpstream {
    async fn locate(&self, key: &PackKey) -> Result<Located, UpstreamError> {
        let mut s = lock(&self.state);
        s.locates.push(key.clone());
        if let Some(answer) = s.located.get_mut(key).and_then(VecDeque::pop_front) {
            return answer;
        }
        Ok(match s.objects.get(key) {
            Some(o) => Located::Found(o.location.clone()),
            None => Located::DefinitiveMiss,
        })
    }

    async fn get(&self, req: GetRequest) -> Result<PartResponse, UpstreamError> {
        let (object, script) = {
            let mut s = lock(&self.state);
            s.gets.push(req.clone());
            let script = s.scripts.pop_front().unwrap_or(PartScript::Serve {
                chunk: 0,
                signals: DriveSignals::default(),
            });
            (s.objects.get(&req.key).cloned(), script)
        };
        let object = object.ok_or_else(|| UpstreamError::NotFound {
            key: req.key.clone(),
            device: req.source.device,
        })?;
        let (signals, body) = match script {
            PartScript::Fail(e) => return Err(e),
            PartScript::Serve { chunk, signals } => {
                let bytes = range_of(&req, &object)?;
                let size = if chunk == 0 {
                    bytes.len().max(1)
                } else {
                    chunk
                };
                let chunks: Vec<Result<Bytes, UpstreamError>> = bytes
                    .chunks(size)
                    .map(|c| Ok(Bytes::copy_from_slice(c)))
                    .collect();
                (signals, stream::iter(chunks).boxed())
            }
            PartScript::Hang => {
                range_of(&req, &object)?;
                (DriveSignals::default(), stream::pending().boxed())
            }
            PartScript::Truncate { after } => {
                let mut bytes = range_of(&req, &object)?;
                bytes.truncate(after);
                let items = vec![
                    Ok(Bytes::from(bytes)),
                    Err(transport(&req, "stream ended early")),
                ];
                (DriveSignals::default(), stream::iter(items).boxed())
            }
            PartScript::Corrupt { offset } => {
                let mut bytes = range_of(&req, &object)?;
                if let Some(b) = offset
                    .checked_sub(req.range.offset)
                    // An index past the bytes is outside the range; it fits a usize if inside.
                    .and_then(|i| bytes.get_mut(i as usize))
                {
                    *b ^= 0xff;
                }
                let items = vec![Ok(Bytes::from(bytes))];
                (DriveSignals::default(), stream::iter(items).boxed())
            }
        };
        Ok(PartResponse {
            generation: object.generation,
            signals,
            body,
        })
    }

    async fn report(&self, key: &PackKey, source: &Source, range: ByteRange, report: Report) {
        lock(&self.state)
            .reports
            .push((key.clone(), source.clone(), range, report));
    }
}
