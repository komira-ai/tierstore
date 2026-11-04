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
//!   checkpoints of the releases before the entries. This release neither reads nor writes
//!   them; both prefixes stay reserved, and the objects stay in the hot bucket until the release
//!   after this one removes them (see the design document, section 13).

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
