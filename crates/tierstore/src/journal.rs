//! The names and the formats of what the proxy and the reconciler keep in the hot bucket.
//!
//! - `_entries/<sha256 of the key>`: the journal entry of one key (see the module `entry`). It is
//!   the lock on the key while one actor changes it and the record the reconciler works from.
//! - `_tiering/proxies/<proxy_id>`: written when a proxy starts and rewritten at each heartbeat,
//!   so that the reconciler can tell whether the owner of an entry is alive. Its body is a JSON
//!   registration (`proto`, `boot`, `hb`); an empty body is the registration of an older release.
//! - `_tiering/usage`: the usage the reconciler publishes, for the proxies.
//! - `_tiering/lease`: the lease that makes one reconciler the runner.
//! - `_tiering/format`: the [`Format`](crate::format::Format) of the store, which says whether
//!   writes are paused for a change of format and whether they are made on entries.
//! - `_journal/<proxy_id>/<seq>` and `_tiering/checkpoint/<proxy_id>`: the per-node journals and
//!   checkpoints of the releases before the entries. This release never applies or writes them
//!   in a store; both prefixes stay reserved, and the objects stay in the hot bucket until the
//!   release after this one removes them (see the design document, section 13). Their formats
//!   stay readable and writable here, as bytes only ([`encode_journal`], [`decode_journal`],
//!   [`encode_checkpoint`], [`decode_checkpoint`]), so that a tool or a test can read what an
//!   earlier release stored and an earlier release can read what is written.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::{Backend, Error};

/// The prefix of the per-node journals of the releases before the entries: reserved, never used.
pub(crate) const JOURNAL_PREFIX: &str = "_journal/";
/// The prefix of everything else the proxy and the reconciler keep.
pub(crate) const TIERING_PREFIX: &str = "_tiering/";
/// The prefix of the registrations of the proxies.
pub(crate) const PROXIES_PREFIX: &str = "_tiering/proxies/";
/// The prefix of the journal entries of keys, one object for each key.
pub(crate) const ENTRIES_PREFIX: &str = "_entries/";
/// The format of the store: whether writes are paused for a change of format.
pub(crate) const FORMAT_KEY: &str = "_tiering/format";
/// The usage the reconciler publishes.
pub(crate) const USAGE_KEY: &str = "_tiering/usage";
/// The lease of the reconciler.
pub(crate) const LEASE_KEY: &str = "_tiering/lease";

/// Every prefix that the proxy keeps for itself: no client names a key under one.
///
/// These are carved out of the key space of the users, and a store may already hold a user's key
/// under any other top-level name. So every internal object that is added later (for example the
/// staged parts of a multipart upload) lives under `_tiering/`, and never under a new top-level
/// name. Widening this set is a deliberate change of format, not a convenience.
pub(crate) const RESERVED_PREFIXES: [&str; 3] = [JOURNAL_PREFIX, TIERING_PREFIX, ENTRIES_PREFIX];

/// Whether a key is under a prefix that the proxy keeps for itself.
pub(crate) fn is_reserved(key: &str) -> bool {
    RESERVED_PREFIXES
        .iter()
        .any(|prefix| key.starts_with(prefix))
}

/// The key of the registration of a proxy.
pub(crate) fn proxy_key(proxy_id: &str) -> String {
    format!("{PROXIES_PREFIX}{proxy_id}")
}

/// The JSON body of a small object.
#[expect(clippy::expect_used, reason = "a value of these types serializes")]
pub(crate) fn to_body<T: Serialize>(value: &T) -> Bytes {
    let json = serde_json::to_vec(value).expect("values serialize");
    Bytes::from(json)
}

/// What the reconciler publishes in `_tiering/usage` after each pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Usage {
    /// The bytes of the objects the hot tier holds.
    pub(crate) hot_bytes: u64,
    /// The bytes of those that the cold tier does not hold yet.
    pub(crate) copy_lag: u64,
    /// When it was published, by the clock of the reconciler.
    pub(crate) time: u64,
}

/// Writes a small object that is rewritten in place (the usage): create-only
/// the first time, and then only over the object whose content tag a HEAD just gave.
pub(crate) async fn write_object(hot: &dyn Backend, key: &str, body: Bytes) -> Result<(), Error> {
    match hot.head(key).await {
        Ok(meta) => hot.put_if_match(key, body, &meta.tag).await.map(|_| ()),
        Err(Error::NotFound(_)) => hot.put(key, body).await,
        Err(error) => Err(error),
    }
}

fn corrupt(what: &str, error: &serde_json::Error) -> Error {
    Error::Other(format!("corrupt {what}: {error}"))
}

/// What a journal entry says happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum JournalOp {
    /// An object was written to the hot tier, and is to be copied to the cold tier.
    Put,
    /// An object was deleted from both tiers; the cold tier is to be told again.
    Delete,
    /// An object was copied from the cold tier into the hot tier by a read
    /// ([`JournalEntry::cold_tag`] is the tag the cold object had, [`JournalEntry::hot_tag`] the
    /// tag the hot tier gave the copy).
    PutMirror,
    /// Anything a newer writer adds: a reader skips the entry.
    #[serde(other)]
    Unknown,
}

/// One entry of the journal of a proxy of a release before the entries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// What happened.
    pub op: JournalOp,
    /// The object's key.
    pub key: String,
    /// The object's size in bytes for a put, and zero for a delete.
    pub size: u64,
    /// When it happened, in milliseconds, by the clock of the proxy.
    pub time: u64,
    /// The content tag of the cold tier's object that the entry is about; absent in the entries
    /// of releases before the ops that have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cold_tag: Option<String>,
    /// The content tag the hot tier gave the object of a [`JournalOp::PutMirror`]; absent in
    /// every other entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hot_tag: Option<String>,
}

/// The body of a journal object: its entries, in the order they were appended.
#[must_use]
pub fn encode_journal(entries: &[JournalEntry]) -> Bytes {
    to_body(&entries)
}

/// The entries of the body of a journal object.
///
/// # Errors
/// [`Error::Other`] if the body is not the encoding of a list of entries.
pub fn decode_journal(body: &[u8]) -> Result<Vec<JournalEntry>, Error> {
    let parsed = serde_json::from_slice(body);
    parsed.map_err(|error| corrupt("journal object", &error))
}

/// What the reconciler of a release before the entries had done for one proxy, as it stored it in
/// `_tiering/checkpoint/<proxy_id>`: every journal object up to `seq` was applied, and the entries
/// before the position (`evict_seq`, `evict_idx`) were done with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// The number of the last journal object applied.
    pub seq: u64,
    /// The number of the journal object of the first entry not done with.
    pub evict_seq: u64,
    /// The index of that entry in its object.
    pub evict_idx: u64,
}

/// The body of a checkpoint object.
#[must_use]
pub fn encode_checkpoint(checkpoint: &Checkpoint) -> Bytes {
    to_body(checkpoint)
}

/// The checkpoint of the body of a checkpoint object.
///
/// # Errors
/// [`Error::Other`] if the body is not the encoding of a checkpoint.
pub fn decode_checkpoint(body: &[u8]) -> Result<Checkpoint, Error> {
    let parsed = serde_json::from_slice(body);
    parsed.map_err(|error| corrupt("checkpoint object", &error))
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
