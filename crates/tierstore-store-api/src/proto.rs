//! The peer protocol (10.6): HTTP/1.1 keep-alive on the storage nodes' private network address
//! only, a cluster token on every request (read from a file, never an environment variable).
//!
//! Build nodes use the read routes (`locate`, hot, cold and degraded GET, `ping`); storage nodes
//! also use the internal routes, and every internal message carries the sender's cluster stamp
//! ([`Envelope`]), which the receiver merges into its clock. Control bodies are JSON; a binary
//! format of `tierstore-format` inside a JSON body (a descriptor, a free, a log entry) travels as
//! the lowercase hex of its bytes ([`HexBytes`]); ids travel in their text form. A replication
//! batch's bytes and every GET's bytes are the raw HTTP body, with their fields in headers.
//!
//! Keys are UTF-8 on the wire (they are S3 keys, carried as JSON strings in [`LocateRequest`] and
//! [`KeyCheck`]); the formats of `tierstore-format` store a key as its bytes, so a key travels as
//! its UTF-8 bytes and is stored as the same bytes.

use serde::{Deserialize, Serialize};
use tierstore_fetch_api::ids::{Class, JobId, Rank};
use tierstore_fetch_api::upstream::{DriveSignals, DriveState};
use tierstore_format::FormatError;
use tierstore_format::descriptor::StripeDescriptor;
use tierstore_format::free::StripeFree;
use tierstore_format::ids::{BlockId, DriveId, Generation, NodeId};
use tierstore_format::seal::{Ballot, SealProposal};
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::{LogRecord, scan_log_after};

use crate::hot::{ReplicaAck, ReplicaBatch, ReplicaReject};

/// The routes of the peer service.
pub mod route {
    /// `POST`: batched `LOCATE` ([`super::LocateRequest`] -> [`super::LocateResponse`]).
    pub const LOCATE: &str = "/store/v1/locate";
    /// `GET /store/v1/hot/<file>`: a hot copy's bytes, with `Range`.
    pub const HOT: &str = "/store/v1/hot/";
    /// `PUT`: a replication batch (headers of [`super::BatchHeaders`], the bytes as the body), or
    /// a large-file stream.
    pub const HOT_REPLICA: &str = "/store/v1/hot/replica";
    /// `GET /store/v1/cold/<drive_id>/<shard>`: a data shard's bytes, with `Range`; `PUT
    /// /store/v1/cold/<drive_id>/shard`: a shard write with its generation trailer.
    pub const COLD: &str = "/store/v1/cold/";
    /// `GET /store/v1/degraded/<block_id>`: a degraded read, the source drives in
    /// [`super::header::SOURCES`].
    pub const DEGRADED: &str = "/store/v1/degraded/";
    /// `POST`: a majority-committed record ([`super::CommitRecord`]).
    pub const DESCRIPTOR: &str = "/store/v1/descriptor";
    /// `POST`: a seal message ([`super::SealMessage`]).
    pub const SEAL: &str = "/store/v1/seal";
    /// `POST`: the all-node key check ([`super::KeyCheck`] -> [`super::KeyFacts`]).
    pub const KEY: &str = "/store/v1/key";
    /// `POST`: a peer streams its index range for the keys this node should hold.
    pub const INDEX_SYNC: &str = "/store/v1/index-sync";
    /// `GET ?origin=<node>&after=<seq>&limit=<n>`: the tombstone log pull of one origin's log
    /// ([`super::TombPull::query`], [`super::TombPage`]); the three parameters are decimal, in
    /// this order.
    pub const TOMB: &str = "/store/v1/tomb";
    /// `POST`: a repair request.
    pub const REPAIR: &str = "/store/v1/repair";
    /// `GET`: the scheduler's report ([`super::PingAnswer`]).
    pub const PING: &str = "/store/v1/ping";
}

/// The headers of the peer protocol.
pub mod header {
    /// The fetch class of a GET.
    pub const CLASS: &str = "x-ts-class";
    /// The job of a GET.
    pub const JOB: &str = "x-ts-job";
    /// The rank of a GET.
    pub const RANK: &str = "x-ts-rank";
    /// Milliseconds the request waited for a stream slot.
    pub const QUEUE_WAIT_MS: &str = "x-ts-queue-wait-ms";
    /// Milliseconds of service.
    pub const SERVICE_MS: &str = "x-ts-service-ms";
    /// The drive's queued requests.
    pub const DRIVE_QUEUE: &str = "x-ts-drive-queue";
    /// The drive's state.
    pub const DRIVE_STATE: &str = "x-ts-drive-state";
    /// What the client found in an earlier response (`bitrot`).
    pub const REPORT: &str = "x-ts-report";
    /// The optional expiry class of a PUT.
    pub const EXPIRY: &str = "x-ts-expiry";
    /// The sender's membership epoch, piggybacked on peer responses.
    pub const MEM_EPOCH: &str = "x-ts-mem-epoch";
    /// The sender's cluster stamp (internal routes with a raw body).
    pub const STAMP: &str = "x-ts-stamp";
    /// The sending storage node (internal routes with a raw body).
    pub const FROM: &str = "x-ts-from";
    /// A batch's segment.
    pub const SEGMENT: &str = "x-ts-segment";
    /// A batch's ballot, `<round>.<node>`.
    pub const BALLOT: &str = "x-ts-ballot";
    /// A batch's start offset.
    pub const OFFSET: &str = "x-ts-offset";
    /// A batch's running crc32c, 8 lowercase hex digits.
    pub const RUNNING_CRC: &str = "x-ts-running-crc";
    /// A degraded read's source shards, `<node>:<drive>:<index>` joined by `,`.
    pub const SOURCES: &str = "x-ts-sources";
    /// The cluster token.
    pub const CLUSTER_TOKEN: &str = "x-ts-cluster-token";
}

/// Why a protocol message does not decode.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtoError {
    /// A required header is missing.
    #[error("header {0} is missing")]
    MissingHeader(&'static str),
    /// A header's value does not parse.
    #[error("header {0} is malformed")]
    BadHeader(&'static str),
    /// A hex field is not lowercase hex of whole bytes.
    #[error("not lowercase hex")]
    BadHex,
    /// A query string is not the canonical form of the message.
    #[error("query is malformed")]
    BadQuery,
    /// A binary format inside the message does not decode.
    #[error(transparent)]
    Format(#[from] FormatError),
}

/// Bytes of a binary format, carried in JSON as lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct HexBytes(pub Vec<u8>);

impl Serialize for HexBytes {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for HexBytes {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        decode_hex(&text)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

fn decode_hex(text: &str) -> Result<Vec<u8>, ProtoError> {
    if text.bytes().any(|c| c.is_ascii_uppercase()) {
        return Err(ProtoError::BadHex);
    }
    hex::decode(text).map_err(|_| ProtoError::BadHex)
}

/// An internal message with the sender and its cluster stamp (10.2: every peer message carries
/// it) and its membership epoch (10.8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<T> {
    /// The sending storage node.
    pub from: NodeId,
    /// Its stamp when it sent.
    pub stamp: Stamp,
    /// Its membership epoch.
    pub mem_epoch: u64,
    /// The message.
    pub body: T,
}

/// A batched `LOCATE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocateRequest {
    /// The keys, answered in order; UTF-8 on the wire, stored as bytes by the formats.
    pub keys: Vec<String>,
}

/// One hot copy of an object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotCopy {
    /// The node holding it.
    pub node: NodeId,
    /// The file the node serves it from (a segment, a large file or a cache segment name).
    pub file: String,
    /// The object's offset in the file.
    pub offset: u64,
    /// The object's length.
    pub length: u64,
    /// Whether it is a promoted copy (a hint, listed first).
    pub promoted: bool,
}

/// One shard a degraded read would read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceShard {
    /// The node.
    pub node: NodeId,
    /// The drive.
    pub drive: DriveId,
    /// The shard index.
    pub index: u8,
}

/// The cold extent of an object in one block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColdExtent {
    /// The block.
    pub block: BlockId,
    /// Where the extent starts in the object.
    pub object_offset: u64,
    /// Its length.
    pub length: u64,
    /// Where it starts in the block (and in the data shard).
    pub block_offset: u64,
    /// The data shard's node.
    pub node: NodeId,
    /// The data shard's drive.
    pub drive: DriveId,
    /// The shard file name on the drive.
    pub shard: String,
    /// The node that would serve a degraded read.
    pub reconstructor: NodeId,
    /// The `k` shards it would read, chosen by their advertised queues.
    pub sources: Vec<SourceShard>,
    /// The stripe's current margin: surviving shards minus `k`.
    pub margin: i16,
}

/// One node's answer for one key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LocateAnswer {
    /// On the hot tier.
    Hot {
        /// The generation.
        generation: Generation,
        /// The size.
        size: u64,
        /// The copies, a promoted copy first.
        copies: Vec<HotCopy>,
    },
    /// On the cold tier only.
    Cold {
        /// The generation.
        generation: Generation,
        /// The size.
        size: u64,
        /// One extent per block the object covers.
        extents: Vec<ColdExtent>,
    },
    /// Not in this node's index (or a tombstone).
    Absent,
    /// This node's index is not built: never a miss.
    Indexing,
}

/// The answers of a `LOCATE`, one per key in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocateResponse {
    /// The answers.
    pub answers: Vec<LocateAnswer>,
}

/// The writer's all-node key check before a PUT or DELETE (10.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyCheck {
    /// The key; UTF-8 on the wire, stored as bytes by the formats.
    pub key: String,
}

/// One record of a key a node holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRecord {
    /// Its generation.
    pub generation: Generation,
    /// Its stamp.
    pub stamp: Stamp,
}

/// What a node holds of a key; the envelope's stamp is the node's clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyFacts {
    /// Its records of the key.
    pub records: Vec<KeyRecord>,
    /// Its tombstone of the key, by stamp.
    pub tombstone: Option<Stamp>,
    /// Whether its index is not built (the answer is then not definitive).
    pub indexing: bool,
}

/// A message of the majority seal (10.3 step 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SealMessage {
    /// `seal-prepare(segment, ballot)`.
    Prepare {
        /// The segment.
        segment: BlockId,
        /// The ballot.
        ballot: Ballot,
    },
    /// `seal-accept(segment, ballot, length, crc)`.
    Accept {
        /// The segment.
        segment: BlockId,
        /// The ballot.
        ballot: Ballot,
        /// The proposal.
        proposal: SealProposal,
    },
    /// The broadcast after a commit.
    Sealed {
        /// The segment.
        segment: BlockId,
        /// The committed proposal.
        proposal: SealProposal,
    },
}

/// A replica's answer to a batch (the JSON body of the `PUT /store/v1/hot/replica` response).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplicaAnswer {
    /// Durable: the replica's committed length.
    Acked {
        /// The committed length.
        committed: u64,
    },
    /// The start offset is not the replica's committed length.
    Offset {
        /// The batch's start.
        start: u64,
        /// The copy's committed length.
        committed: u64,
    },
    /// The running crc does not match.
    Crc {
        /// What the batch carried.
        sent: u32,
        /// What the copy computed.
        computed: u32,
    },
    /// The ballot is below the copy's promise.
    Ballot {
        /// The batch's ballot.
        ballot: Ballot,
        /// The promise.
        promised: Ballot,
    },
    /// The segment is sealed on the copy.
    Sealed,
}

impl From<Result<ReplicaAck, ReplicaReject>> for ReplicaAnswer {
    fn from(r: Result<ReplicaAck, ReplicaReject>) -> Self {
        match r {
            Ok(ack) => Self::Acked {
                committed: ack.committed,
            },
            Err(ReplicaReject::Offset { start, committed }) => Self::Offset { start, committed },
            Err(ReplicaReject::Crc { sent, computed }) => Self::Crc { sent, computed },
            Err(ReplicaReject::Ballot { ballot, promised }) => Self::Ballot { ballot, promised },
            Err(ReplicaReject::Sealed) => Self::Sealed,
        }
    }
}

impl From<ReplicaAnswer> for Result<ReplicaAck, ReplicaReject> {
    fn from(a: ReplicaAnswer) -> Self {
        match a {
            ReplicaAnswer::Acked { committed } => Ok(ReplicaAck { committed }),
            ReplicaAnswer::Offset { start, committed } => {
                Err(ReplicaReject::Offset { start, committed })
            }
            ReplicaAnswer::Crc { sent, computed } => Err(ReplicaReject::Crc { sent, computed }),
            ReplicaAnswer::Ballot { ballot, promised } => {
                Err(ReplicaReject::Ballot { ballot, promised })
            }
            ReplicaAnswer::Sealed => Err(ReplicaReject::Sealed),
        }
    }
}

/// A copy's state in a promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyWire {
    /// Committed length.
    pub committed: u64,
    /// Verified length.
    pub verified: u64,
    /// Running crc32c through `verified`.
    pub running_crc: u32,
}

/// The answer to a seal message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SealAnswer {
    /// A promise (to `Prepare`).
    Promise {
        /// The proposal this node accepted earlier, with its ballot.
        accepted: Option<(Ballot, SealProposal)>,
        /// Its copy, if it holds one.
        copy: Option<CopyWire>,
    },
    /// Accepted durably (to `Accept`), or the `Sealed` broadcast applied.
    Ok,
    /// Refused: the node promised a higher ballot.
    Promised {
        /// The higher ballot.
        ballot: Ballot,
    },
    /// The seal is committed: the committed proposal, which never changes.
    Committed {
        /// The proposal.
        proposal: SealProposal,
    },
}

/// A majority-committed record (10.2): written to every live storage node, committed when a
/// majority has it durable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommitRecord {
    /// A stripe descriptor (a destage commit, a rebuild or a move), its bytes.
    Descriptor {
        /// The encoded descriptor.
        bytes: HexBytes,
    },
    /// A stripe free, its bytes.
    Free {
        /// The encoded free.
        bytes: HexBytes,
    },
}

impl CommitRecord {
    /// The record of a descriptor.
    pub fn descriptor(d: &StripeDescriptor) -> Result<Self, ProtoError> {
        Ok(Self::Descriptor {
            bytes: HexBytes(d.encode()?),
        })
    }

    /// The record of a free.
    #[must_use]
    pub fn free(f: &StripeFree) -> Self {
        Self::Free {
            bytes: HexBytes(f.encode()),
        }
    }

    /// The descriptor, if the record is one.
    pub fn as_descriptor(&self) -> Option<Result<StripeDescriptor, ProtoError>> {
        match self {
            Self::Descriptor { bytes } => {
                Some(StripeDescriptor::decode(&bytes.0).map_err(ProtoError::from))
            }
            Self::Free { .. } => None,
        }
    }

    /// The free, if the record is one.
    pub fn as_free(&self) -> Option<Result<StripeFree, ProtoError>> {
        match self {
            Self::Free { bytes } => Some(StripeFree::decode(&bytes.0).map_err(ProtoError::from)),
            Self::Descriptor { .. } => None,
        }
    }
}

/// The answer to a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommitAnswer {
    /// Durable on this node.
    Durable,
    /// A descriptor change for a stripe whose free is committed: refused (free wins over repair).
    FreeWins,
    /// A descriptor revision at or below the one this node holds.
    StaleRevision {
        /// The revision this node holds.
        held: u32,
    },
}

/// A tombstone log pull: entries of `origin`'s log after `after`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TombPull {
    /// The origin whose log is pulled.
    pub origin: NodeId,
    /// The last sequence number the puller applied (0: none yet).
    pub after: u64,
    /// The most entries to return.
    pub limit: u32,
}

impl TombPull {
    /// The query string of the pull, `origin=<n>&after=<seq>&limit=<n>`, decimals in this order.
    #[must_use]
    pub fn query(&self) -> String {
        format!(
            "origin={}&after={}&limit={}",
            self.origin.0, self.after, self.limit
        )
    }

    /// The pull a query string names; only the exact form [`TombPull::query`] writes parses (the
    /// three parameters in order, canonical decimals: no sign, no leading zeros), anything else is
    /// [`ProtoError::BadQuery`].
    pub fn from_query(query: &str) -> Result<Self, ProtoError> {
        let mut parts = query.split('&');
        let origin = param(parts.next(), "origin=")?;
        let after = param(parts.next(), "after=")?;
        let limit = param(parts.next(), "limit=")?;
        if parts.next().is_some() {
            return Err(ProtoError::BadQuery);
        }
        Ok(Self {
            origin: NodeId(origin.try_into().map_err(|_| ProtoError::BadQuery)?),
            after,
            limit: limit.try_into().map_err(|_| ProtoError::BadQuery)?,
        })
    }
}

/// The decimal value of a `name=value` query parameter, in canonical form.
fn param(part: Option<&str>, name: &str) -> Result<u64, ProtoError> {
    let text = part
        .and_then(|p| p.strip_prefix(name))
        .ok_or(ProtoError::BadQuery)?;
    let value: u64 = text.parse().map_err(|_| ProtoError::BadQuery)?;
    if value.to_string() == text {
        Ok(value)
    } else {
        Err(ProtoError::BadQuery)
    }
}

/// A page of a tombstone log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TombPage {
    /// The entries, each an encoded [`LogRecord`] (its origin and sequence number in its frame), in
    /// sequence order.
    pub entries: Vec<HexBytes>,
    /// The sequence number of the last entry returned (the pull's `after` when none): the puller's
    /// next `after`.
    pub last: u64,
    /// Whether more entries follow.
    pub more: bool,
}

impl TombPage {
    /// The page answering a pull after `after`, of `records`; `last` is the sequence number of the
    /// last record, or `after` when there is none.
    pub fn new(after: u64, records: &[LogRecord], more: bool) -> Result<Self, ProtoError> {
        let entries = records
            .iter()
            .map(|r| r.encode().map(HexBytes))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            entries,
            last: records.last().map_or(after, |r| r.seq),
            more,
        })
    }

    /// The records of the page answering `pull`, decoded. Every record must be of the pulled origin
    /// and the sequence numbers must be consecutive from `pull.after + 1`; `last` must be that of
    /// the last record (`pull.after` for an empty page). Anything else fails the page.
    pub fn decode(&self, pull: &TombPull) -> Result<Vec<LogRecord>, ProtoError> {
        let bytes: Vec<u8> = self.entries.iter().flat_map(|b| b.0.clone()).collect();
        let records = scan_log_after(&bytes, pull.origin, pull.after)?;
        let last = records.last().map_or(pull.after, |r| r.seq);
        if last != self.last || records.len() != self.entries.len() {
            return Err(ProtoError::Format(FormatError::Invalid {
                what: "tomb page",
                reason: "last or entries do not match the records",
            }));
        }
        Ok(records)
    }
}

/// One drive in a ping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrivePing {
    /// The drive.
    pub drive: DriveId,
    /// Its state, as the `x-ts-drive-state` value.
    pub state: String,
    /// The age of its oldest in-flight call.
    pub oldest_call_ms: Option<u64>,
    /// Queued requests by class: demand, write, prepare, background.
    pub queued: [u32; 4],
    /// Streams admitted.
    pub streams: u32,
}

/// The answer to `GET /store/v1/ping`, from the schedulers' state (10.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingAnswer {
    /// Whether the node's index is not built.
    pub indexing: bool,
    /// Each drive.
    pub drives: Vec<DrivePing>,
    /// The hot device's queued requests.
    pub hot_queued: u32,
}

/// A header list as an HTTP layer hands it over: `(lowercase name, value)` pairs.
pub type Headers = Vec<(String, String)>;

fn get<'a>(h: &'a [(String, String)], name: &'static str) -> Option<&'a str> {
    h.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

fn req<'a>(h: &'a [(String, String)], name: &'static str) -> Result<&'a str, ProtoError> {
    get(h, name).ok_or(ProtoError::MissingHeader(name))
}

fn parse<T: std::str::FromStr>(
    h: &[(String, String)],
    name: &'static str,
) -> Result<T, ProtoError> {
    req(h, name)?
        .parse()
        .map_err(|_| ProtoError::BadHeader(name))
}

fn opt<T: std::str::FromStr>(
    h: &[(String, String)],
    name: &'static str,
) -> Result<Option<T>, ProtoError> {
    get(h, name)
        .map(|v| v.parse().map_err(|_| ProtoError::BadHeader(name)))
        .transpose()
}

/// The fields of a replication batch's request headers; the bytes are the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchHeaders {
    /// The sending writer.
    pub from: NodeId,
    /// The segment.
    pub segment: BlockId,
    /// The ballot.
    pub ballot: Ballot,
    /// The start offset.
    pub start: u64,
    /// The running crc.
    pub running_crc: u32,
    /// The writer's stamp.
    pub stamp: Stamp,
}

impl BatchHeaders {
    /// The headers of `batch` sent by `from`.
    #[must_use]
    pub fn of(from: NodeId, batch: &ReplicaBatch) -> Self {
        Self {
            from,
            segment: batch.segment,
            ballot: batch.ballot,
            start: batch.start,
            running_crc: batch.running_crc,
            stamp: batch.stamp,
        }
    }

    /// The header list.
    #[must_use]
    pub fn encode(&self) -> Headers {
        vec![
            (header::FROM.into(), self.from.to_string()),
            (header::SEGMENT.into(), self.segment.to_string()),
            (
                header::BALLOT.into(),
                format!("{}.{}", self.ballot.round, self.ballot.node),
            ),
            (header::OFFSET.into(), self.start.to_string()),
            (
                header::RUNNING_CRC.into(),
                format!("{:08x}", self.running_crc),
            ),
            (header::STAMP.into(), self.stamp.raw().to_string()),
        ]
    }

    /// The fields in a header list.
    pub fn decode(h: &[(String, String)]) -> Result<Self, ProtoError> {
        let ballot = req(h, header::BALLOT)?;
        let (round, node) = ballot
            .split_once('.')
            .and_then(|(r, n)| Some((r.parse().ok()?, n.parse().ok()?)))
            .ok_or(ProtoError::BadHeader(header::BALLOT))?;
        let crc = req(h, header::RUNNING_CRC)?;
        let running_crc = (crc.len() == 8 && !crc.bytes().any(|c| c.is_ascii_uppercase()))
            .then(|| u32::from_str_radix(crc, 16).ok())
            .flatten()
            .ok_or(ProtoError::BadHeader(header::RUNNING_CRC))?;
        Ok(Self {
            from: NodeId(parse(h, header::FROM)?),
            segment: parse(h, header::SEGMENT)?,
            ballot: Ballot {
                round,
                node: NodeId(node),
            },
            start: parse(h, header::OFFSET)?,
            running_crc,
            stamp: Stamp::from_raw(parse(h, header::STAMP)?),
        })
    }
}

/// The fetch headers of a hot, cold or degraded GET (besides `Range`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GetHeaders {
    /// `If-Match`, the fill's pinned generation.
    pub generation: Option<Generation>,
    /// `x-ts-class`.
    pub class: Class,
    /// `x-ts-job`.
    pub job: JobId,
    /// `x-ts-rank`.
    pub rank: Rank,
}

impl GetHeaders {
    /// The header list.
    #[must_use]
    pub fn encode(&self) -> Headers {
        let mut h = vec![
            (header::CLASS.into(), self.class.header().to_owned()),
            (header::JOB.into(), self.job.0.to_string()),
            (header::RANK.into(), self.rank.0.to_string()),
        ];
        if let Some(g) = self.generation {
            h.push(("if-match".into(), format!("\"{g}\"")));
        }
        h
    }

    /// The fields in a header list; a missing `x-ts-class` is `background`, a missing job or rank
    /// is 0.
    pub fn decode(h: &[(String, String)]) -> Result<Self, ProtoError> {
        let class = match get(h, header::CLASS) {
            Some(v) => Class::from_header(v).ok_or(ProtoError::BadHeader(header::CLASS))?,
            None => Class::Background,
        };
        let generation = get(h, "if-match")
            .map(|v| {
                v.strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .and_then(|v| v.parse().ok())
                    .ok_or(ProtoError::BadHeader("if-match"))
            })
            .transpose()?;
        Ok(Self {
            generation,
            class,
            job: JobId(opt(h, header::JOB)?.unwrap_or(0)),
            rank: Rank(opt(h, header::RANK)?.unwrap_or(0)),
        })
    }
}

/// The signals a GET response's headers carry, with its `ETag`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseHeaders {
    /// `ETag`: the generation of the bytes.
    pub generation: Generation,
    /// The drive's signals.
    pub signals: DriveSignals,
}

impl ResponseHeaders {
    /// The header list.
    #[must_use]
    pub fn encode(&self) -> Headers {
        let mut h = vec![("etag".to_owned(), format!("\"{}\"", self.generation))];
        let s = self.signals;
        let mut put = |name: &str, v: Option<String>| {
            if let Some(v) = v {
                h.push((name.to_owned(), v));
            }
        };
        put(
            header::QUEUE_WAIT_MS,
            s.queue_wait_ms.map(|v| v.to_string()),
        );
        put(header::SERVICE_MS, s.service_ms.map(|v| v.to_string()));
        put(header::DRIVE_QUEUE, s.drive_queue.map(|v| v.to_string()));
        put(
            header::DRIVE_STATE,
            s.drive_state.map(|v| v.header().to_owned()),
        );
        h
    }

    /// The fields in a header list; every signal is optional, an unknown drive state is an error.
    pub fn decode(h: &[(String, String)]) -> Result<Self, ProtoError> {
        let generation = req(h, "etag")?
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .and_then(|v| v.parse().ok())
            .ok_or(ProtoError::BadHeader("etag"))?;
        let drive_state = get(h, header::DRIVE_STATE)
            .map(|v| DriveState::from_header(v).ok_or(ProtoError::BadHeader(header::DRIVE_STATE)))
            .transpose()?;
        Ok(Self {
            generation,
            signals: DriveSignals {
                queue_wait_ms: opt(h, header::QUEUE_WAIT_MS)?,
                service_ms: opt(h, header::SERVICE_MS)?,
                drive_queue: opt(h, header::DRIVE_QUEUE)?,
                drive_state,
            },
        })
    }
}

/// The `x-ts-sources` value of a degraded read.
#[must_use]
pub fn encode_sources(sources: &[SourceShard]) -> String {
    sources
        .iter()
        .map(|s| format!("{}:{}:{}", s.node, s.drive, s.index))
        .collect::<Vec<_>>()
        .join(",")
}

/// The sources an `x-ts-sources` value names; the empty value names none, so
/// `decode_sources(&encode_sources(&[]))` is `Ok(vec![])`.
pub fn decode_sources(value: &str) -> Result<Vec<SourceShard>, ProtoError> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|part| {
            let mut it = part.split(':');
            let shard = (|| {
                let node = it.next()?.parse().ok()?;
                let drive = it.next()?.parse().ok()?;
                let index = it.next()?.parse().ok()?;
                it.next().is_none().then_some(SourceShard {
                    node: NodeId(node),
                    drive,
                    index,
                })
            })();
            shard.ok_or(ProtoError::BadHeader(header::SOURCES))
        })
        .collect()
}
