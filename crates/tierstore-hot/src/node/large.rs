//! Large objects (10.3 "Large objects: whole files"): an object over `hot.large_min` is a file
//! `big/<hh>/<khash>.<gen>.<size>.obj`: a 64-byte record header, the key, then the data.
//!
//! The writer streams the body into its `.up` (4 MiB writes, `fallocate` when the length is
//! known) and, in the same pass, to each other member of the replica set, which writes its own
//! `.up`; nothing holds more than a chunk per replica. The writer computes the generation while
//! streaming and sends it in a trailer that every replica checks against its own hash (a
//! mismatch unlinks that `.up` and abandons the copy). Each copy commits with `fdatasync`, the
//! create-only check under the key lock, a rename that never replaces and a directory `fsync`.
//! A replica that keeps the writer waiting for `hot.put_copy_stall` (time it does not take a
//! chunk; a pause of the client's body does not count) is abandoned, and the finished local file
//! is sent again to another live node. The PUT is acknowledged on `hot.min_acks` committed
//! copies; with fewer it answers 503 and the writer's `.up` is removed. A crash leaves only
//! `.up` files, which the startup sweep removes.

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use futures::channel::mpsc;
use futures::future::{Either, join_all, select};
use futures::stream::BoxStream;
use futures::{SinkExt, StreamExt};
use tierstore_format::digest::GenerationHasher;
use tierstore_format::ids::{Generation, KeyHash, NodeId};
use tierstore_format::names::HotName;
use tierstore_format::record::{RECORD_HEADER_LEN, RecordFlags, RecordHeader};
use tierstore_format::tomb::LargeFile;
use tierstore_format::{crc32c, crc32c_append};
use tierstore_store_api::hot::{Acked, HotError, LargePut, Unavailable};
use tierstore_store_api::index::{EntryLocation, Fact, IndexEntry, Lookup};
use tierstore_store_api::proto::Envelope;
use tierstore_store_api::sched::DriveClass;
use tokio::sync::Notify;

use super::{FileInfo, FileKind, HotNode, Node, io_err, other_err, unavailable};
use crate::peers::{LargeAnswer, LargeChunk, LargeOffer};

/// The bytes of one write of a large body, and of one chunk to a replica.
pub(crate) const CHUNK: usize = 4 << 20;

/// The replica streams of one large PUT on its writer: when each started waiting for a replica
/// to take a chunk, and the signal that abandons it.
#[derive(Default)]
pub(crate) struct Tee {
    pub(crate) blocked: BTreeMap<NodeId, u64>,
    pub(crate) abandon: BTreeMap<NodeId, Arc<Notify>>,
}

struct Upload {
    path: String,
    key_len: u16,
    size: u64,
    hasher: GenerationHasher,
    data_crc: u32,
    pending: Vec<u8>,
}

impl Upload {
    fn header(
        &self,
        generation: Generation,
        stamp: tierstore_format::stamp::Stamp,
        key: &[u8],
    ) -> RecordHeader {
        RecordHeader {
            flags: RecordFlags::default(),
            key_len: self.key_len,
            data_len: self.size,
            generation,
            stamp,
            object_offset: 0,
            data_crc: self.data_crc,
            key_crc: crc32c(key),
        }
    }
}

impl Node {
    /// Creates the `.up` of `key` (with its key bytes after the header room) and returns it.
    async fn begin_upload(&self, key: &[u8], size: Option<u64>) -> Result<Upload, HotError> {
        let key_len = u16::try_from(key.len()).map_err(|_| {
            io_err("large upload")(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "key longer than 65535 bytes",
            ))
        })?;
        let nonce = self.stamp()?.raw();
        let path = HotName::LargeUpload {
            key_hash: KeyHash::of(key),
            nonce,
        }
        .path();
        let (dev, p, k) = (Arc::clone(&self.dev), path.clone(), key.to_vec());
        self.sched
            .run(DriveClass::Write, move || {
                dev.create(&p)?;
                if let Some(size) = size {
                    dev.allocate(&p, RECORD_HEADER_LEN as u64 + k.len() as u64 + size)?;
                }
                dev.write_at(&p, RECORD_HEADER_LEN as u64, &k)
            })
            .await?
            .map_err(io_err("large upload"))?;
        Ok(Upload {
            path,
            key_len,
            size: 0,
            hasher: GenerationHasher::new(),
            data_crc: 0,
            pending: Vec::with_capacity(CHUNK),
        })
    }

    /// Writes the pending bytes of `up` at their place.
    async fn flush_upload(&self, up: &mut Upload) -> Result<(), HotError> {
        if up.pending.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut up.pending);
        let at = RECORD_HEADER_LEN as u64 + u64::from(up.key_len) + up.size - bytes.len() as u64;
        let (dev, p) = (Arc::clone(&self.dev), up.path.clone());
        self.sched
            .run(DriveClass::Write, move || dev.write_at(&p, at, &bytes))
            .await?
            .map_err(io_err("large write"))
    }

    /// Adds `bytes` to `up` and writes a full chunk.
    async fn feed_upload(&self, up: &mut Upload, bytes: &[u8]) -> Result<(), HotError> {
        up.hasher.update(bytes);
        up.data_crc = crc32c_append(up.data_crc, bytes);
        up.size += bytes.len() as u64;
        up.pending.extend_from_slice(bytes);
        if up.pending.len() >= CHUNK {
            self.flush_upload(up).await?;
        }
        Ok(())
    }

    /// Writes the header and makes the `.up` durable.
    async fn finish_upload(&self, up: &mut Upload, header: RecordHeader) -> Result<(), HotError> {
        self.flush_upload(up).await?;
        let (dev, p) = (Arc::clone(&self.dev), up.path.clone());
        self.sched
            .run(DriveClass::Write, move || {
                dev.write_at(&p, 0, &header.encode())?;
                dev.fdatasync(&p)
            })
            .await?
            .map_err(io_err("large sync"))
    }

    async fn unlink(&self, path: &str) {
        let (dev, p) = (Arc::clone(&self.dev), path.to_owned());
        // A file that cannot be removed now is removed by the next startup sweep.
        let _ = self
            .sched
            .run(DriveClass::Write, move || dev.unlink(&p))
            .await;
    }

    /// The create-only check of a large object under its key lock: `Ok(true)` to commit, `Ok(false)`
    /// when the key already holds this generation (identical).
    fn large_check(
        &self,
        key: &[u8],
        generation: Generation,
        stamp: tierstore_format::stamp::Stamp,
    ) -> Result<bool, HotError> {
        match self.index.get(key).map_err(other_err("index"))? {
            Lookup::Live(e) if e.generation == generation => Ok(false),
            Lookup::Live(e) => Err(HotError::Conflict {
                existing: e.generation,
            }),
            Lookup::Deleted(t) if t > stamp => Err(unavailable(Unavailable::NotDefinitive)),
            _ => Ok(true),
        }
    }

    /// Renames `up` to the object's name (never over an existing file: the same name is the
    /// same bytes), `fsync`s the directory and publishes the index entry.
    async fn commit_large(
        &self,
        up: &str,
        key: &[u8],
        file: LargeFile,
        stamp: tierstore_format::stamp::Stamp,
    ) -> Result<(), HotError> {
        let name = HotName::Large(file).path();
        let (dev, from, to) = (Arc::clone(&self.dev), up.to_owned(), name.clone());
        self.sched
            .run(DriveClass::Write, move || {
                match dev.rename(&from, &to, true) {
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => dev.unlink(&from)?,
                    other => other?,
                }
                dev.fsync_dir(crate::dev::dir_of(&to))?;
                dev.fsync_dir("big")
            })
            .await?
            .map_err(io_err("large commit"))?;
        let entry = IndexEntry {
            generation: file.generation,
            location: EntryLocation::Large(file),
            length: file.size,
            stamp,
            flags: RecordFlags::default(),
            prior_blocks: Vec::new(),
            promoted: None,
        };
        let mut facts = std::iter::once(Fact::Record {
            key: key.to_vec(),
            entry,
        });
        self.index.replay(&mut facts).map_err(other_err("index"))?;
        let now = self.clock.now_ms();
        let bytes = RECORD_HEADER_LEN as u64 + key.len() as u64 + file.size;
        self.lock().files.insert(
            name,
            FileInfo {
                kind: FileKind::Large(file),
                bytes,
                data_len: file.size,
                dirty: true,
                last_read_ms: now,
                last_scrub_ms: now,
            },
        );
        Ok(())
    }

    /// Sends the finished local file `path` (data from `data_at`, `size` bytes) to `node`.
    async fn resend(
        &self,
        node: NodeId,
        offer: LargeOffer,
        path: &str,
        data_at: u64,
        size: u64,
        generation: Generation,
    ) -> Result<bool, HotError> {
        let env = self.env(offer)?;
        let (mut tx, rx) = mpsc::channel::<LargeChunk>(1);
        // `move`: the sender is dropped when this ends, so a stream cut short ends for the
        // replica too (it abandons the copy) instead of waiting for chunks that never come.
        let send = async move {
            let mut at = 0u64;
            while at < size {
                // A file that cannot be read, or is shorter than it should be, ends the stream
                // without its trailer: the replica abandons the copy.
                let Some(bytes) = self
                    .read(
                        DriveClass::Write,
                        path,
                        data_at + at,
                        (CHUNK as u64).min(size - at),
                    )
                    .await
                    .ok()
                    .filter(|b| !b.is_empty())
                else {
                    return;
                };
                let len = bytes.len() as u64;
                if tx.send(LargeChunk::Data(Bytes::from(bytes))).await.is_err() {
                    return;
                }
                at += len;
            }
            let _ = tx.send(LargeChunk::Trailer(generation)).await;
        };
        let (_, answer) = futures::join!(send, self.peers.put_large(node, env, rx.boxed()));
        Ok(matches!(answer.map(|a| a.body), Ok(LargeAnswer::Committed)))
    }
}

/// One replica stream of a large PUT, on the writer.
struct Out {
    node: NodeId,
    tx: Option<mpsc::Sender<LargeChunk>>,
    abandon: Arc<Notify>,
    lagged: bool,
}

/// Sends `chunk` to every replica still streaming; one that keeps the writer waiting until the
/// tick abandons it is dropped from the PUT.
async fn tee(n: &Node, nonce: u64, outs: &mut [Out], chunk: &LargeChunk) {
    for out in outs.iter_mut() {
        let Some(tx) = out.tx.as_mut() else { continue };
        n.lock()
            .large
            .entry(nonce)
            .or_default()
            .blocked
            .insert(out.node, n.clock.now_ms());
        let sent = {
            let send = Box::pin(tx.send(chunk.clone()));
            let abandon = Box::pin(out.abandon.notified());
            match select(send, abandon).await {
                Either::Left((r, _)) => r.is_ok(),
                Either::Right(_) => {
                    out.lagged = true;
                    false
                }
            }
        };
        n.lock()
            .large
            .entry(nonce)
            .or_default()
            .blocked
            .remove(&out.node);
        if !sent {
            out.tx = None;
        }
    }
}

/// `HotStore::put_file`.
pub(crate) async fn put_file(n: &Arc<Node>, put: LargePut) -> Result<Acked, HotError> {
    n.admit_put().await?;
    let LargePut {
        key,
        size,
        expiry,
        stamp,
        mut body,
    } = put;
    let _key = n.keys.lock(&key).await;
    n.observe(stamp);
    let set = n.replica_set(&key)?;
    let mut up = n.begin_upload(&key, size).await?;
    let offer = LargeOffer {
        key: key.clone(),
        size,
        expiry,
        stamp,
    };
    let nonce = n.stamp()?.raw();
    let mut outs = Vec::new();
    let mut calls = Vec::new();
    for node in set.iter().copied().filter(|m| *m != n.id) {
        let (tx, rx) = mpsc::channel::<LargeChunk>(1);
        let abandon = Arc::new(Notify::new());
        n.lock()
            .large
            .entry(nonce)
            .or_default()
            .abandon
            .insert(node, Arc::clone(&abandon));
        let env = n.env(offer.clone())?;
        calls.push(n.peers.put_large(node, env, rx.boxed()));
        outs.push(Out {
            node,
            tx: Some(tx),
            abandon,
            lagged: false,
        });
    }
    let stream = async {
        let streamed = async {
            while let Some(piece) = body.next().await {
                let bytes = piece.map_err(io_err("large body"))?;
                n.feed_upload(&mut up, &bytes).await?;
                tee(n, nonce, &mut outs, &LargeChunk::Data(bytes)).await;
            }
            if size.is_some_and(|s| s != up.size) {
                return Err(io_err("large body")(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the body is not its Content-Length",
                )));
            }
            let generation = up.hasher.finish();
            let header = up.header(generation, stamp, &key);
            n.finish_upload(&mut up, header).await?;
            tee(n, nonce, &mut outs, &LargeChunk::Trailer(generation)).await;
            Ok(generation)
        }
        .await;
        // Whether the body ended or failed, the replica streams end with it: a replica that got
        // no trailer abandons its copy, and the PUT does not wait for it forever.
        for out in &mut outs {
            out.tx = None;
        }
        streamed
    };
    let (streamed, answers) = futures::join!(stream, join_all(calls));
    n.lock().large.remove(&nonce);
    let generation = match streamed {
        Ok(g) => g,
        Err(e) => {
            n.unlink(&up.path).await;
            return Err(e);
        }
    };
    let mut holders: Vec<NodeId> = Vec::new();
    let mut resend: Vec<NodeId> = Vec::new();
    for (out, answer) in outs.iter().zip(answers) {
        match answer.map(|a: Envelope<LargeAnswer>| a.body) {
            Ok(LargeAnswer::Committed) => holders.push(out.node),
            _ if out.lagged => resend.push(out.node),
            _ => {}
        }
    }
    let file = LargeFile {
        key_hash: KeyHash::of(&key),
        generation,
        size: up.size,
    };
    let data_at = RECORD_HEADER_LEN as u64 + key.len() as u64;
    for lagged in resend {
        let view = n.view();
        let target = n
            .placement
            .rank(&key, &view)
            .into_iter()
            .filter(|c| *c != n.id && !holders.contains(c) && view.live_nodes().any(|l| l.id == *c))
            .find(|c| *c != lagged)
            .unwrap_or(lagged);
        if n.resend(
            target,
            offer.clone(),
            &up.path,
            data_at,
            up.size,
            generation,
        )
        .await?
        {
            holders.push(target);
        }
    }
    // The create-only check comes first: a used key answers 412 even when the replicas, which
    // hold the other bytes too, refused the copy and left the PUT short.
    let commit = match n.large_check(&key, generation, stamp) {
        Ok(commit) => commit,
        Err(e) => {
            n.unlink(&up.path).await;
            return Err(e);
        }
    };
    let copies = 1 + holders.len();
    if copies < usize::from(n.cfg.min_acks) {
        n.unlink(&up.path).await;
        return Err(unavailable(Unavailable::TooFewCopies));
    }
    if commit {
        n.commit_large(&up.path, &key, file, stamp).await?;
    } else {
        n.unlink(&up.path).await;
    }
    Ok(Acked {
        generation,
        location: EntryLocation::Large(file),
        length: file.size,
        copies: copies as u8,
    })
}

impl HotNode {
    /// A replica's side of a large-file stream (`PUT /store/v1/hot/replica` with a body): writes
    /// its own `.up`, checks the trailer against its own hash and commits like the writer.
    pub async fn receive_large(
        &self,
        offer: Envelope<LargeOffer>,
        mut body: BoxStream<'static, LargeChunk>,
    ) -> LargeAnswer {
        let n = &self.n;
        n.observe(offer.stamp);
        let LargeOffer {
            key, size, stamp, ..
        } = offer.body;
        let Ok(mut up) = n.begin_upload(&key, size).await else {
            return LargeAnswer::Abandoned;
        };
        let mut trailer = None;
        while let Some(chunk) = body.next().await {
            match chunk {
                LargeChunk::Data(bytes) => {
                    if n.feed_upload(&mut up, &bytes).await.is_err() {
                        break;
                    }
                }
                LargeChunk::Trailer(g) => {
                    trailer = Some(g);
                    break;
                }
            }
        }
        let computed = up.hasher.finish();
        let answer = match trailer {
            None => LargeAnswer::Abandoned,
            Some(g) if g != computed => LargeAnswer::Mismatch { computed },
            Some(_) => {
                let header = up.header(computed, stamp, &key);
                if n.finish_upload(&mut up, header).await.is_err() {
                    LargeAnswer::Abandoned
                } else {
                    let _key = n.keys.lock(&key).await;
                    let file = LargeFile {
                        key_hash: KeyHash::of(&key),
                        generation: computed,
                        size: up.size,
                    };
                    match n.large_check(&key, computed, stamp) {
                        Ok(true) => match n.commit_large(&up.path, &key, file, stamp).await {
                            Ok(()) => return LargeAnswer::Committed,
                            Err(_) => LargeAnswer::Abandoned,
                        },
                        Ok(false) => LargeAnswer::Committed,
                        Err(HotError::Conflict { existing }) => LargeAnswer::Conflict { existing },
                        Err(_) => LargeAnswer::Abandoned,
                    }
                }
            }
        };
        n.unlink(&up.path).await;
        answer
    }
}
