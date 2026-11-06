//! The marker of one key: the object named exactly by the key in the marks bucket of the hot
//! store, which says that a change to the key is not yet the cold tier's too, or that someone holds
//! the key (`docs/design/put-without-lock.md`, section 4). This module is the marker's body: its
//! fields, its encoding, and what a reader does with what it does not know. The calls that read and
//! write marker objects are in the module `marks`; nothing outside it calls them yet.
//!
//! - The body is JSON: `v` (1), `key`, `st` ([`MarkerState`]), `gone`, `dirty`, `why`
//!   ([`MarkerWhy`], for a lock), `owner` (the id of the proxy or the leader holding a lock or a
//!   clearing) and `nonce`, 128 random bits written afresh by every write, so that the content tag
//!   of the object, which is the version a conditional write names, is unique per write. `why` and
//!   `owner` are left out of the body when they are not set.
//! - A reader keeps what it does not know and a rewrite carries it over: a field it does not know
//!   is kept in [`Marker::extra`] (with its value, whatever the shape, ordered by name), and an `st` or a `why` word it does not know is kept as the
//!   text it was ([`MarkerState::Other`], [`MarkerWhy::Other`]). A later release therefore adds
//!   fields and states without a new `v`, and this release rewriting the marker of a later one
//!   loses nothing of it. What an unknown state *means* to a writer is not the body's business
//!   (the design treats it as a lock, which writers wait for and the leader reaps by its lease).

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::Error;
use crate::journal::to_body;

/// The `v` of the bodies this release writes.
pub const MARKER_VERSION: u32 = 1;

/// Whose turn it is on a marker, in the words the body spells them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum MarkerState {
    /// Hot has a change that cold does not have yet, or a put is in flight, or a put crashed
    /// (`DIRTY`).
    Dirty,
    /// One actor is deleting or replacing the hot object, or reading cold into it (`LOCK`).
    Lock,
    /// The owner won the right to remove the marker, and nobody writes it until it is gone
    /// (`CLEARING`).
    Clearing,
    /// Cold holds the truth and a cold read asked for a copy into hot (`WANT`).
    Want,
    /// A word a later release adds, kept as it was written.
    Other(String),
}

impl MarkerState {
    /// Whether a writer other than the owner must wait for the marker to leave this state: a lock
    /// or a clearing, and a word this release does not know, which the design treats as a lock
    /// (writers wait, and the leader reaps it by its lease).
    #[must_use]
    pub fn is_held(&self) -> bool {
        !matches!(self, Self::Dirty | Self::Want)
    }
}

impl From<String> for MarkerState {
    fn from(word: String) -> Self {
        match word.as_str() {
            "DIRTY" => Self::Dirty,
            "LOCK" => Self::Lock,
            "CLEARING" => Self::Clearing,
            "WANT" => Self::Want,
            _ => Self::Other(word),
        }
    }
}

impl From<MarkerState> for String {
    fn from(state: MarkerState) -> Self {
        match state {
            MarkerState::Dirty => "DIRTY".to_owned(),
            MarkerState::Lock => "LOCK".to_owned(),
            MarkerState::Clearing => "CLEARING".to_owned(),
            MarkerState::Want => "WANT".to_owned(),
            MarkerState::Other(word) => word,
        }
    }
}

/// What the holder of a `LOCK` is doing, in the words the body spells them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum MarkerWhy {
    /// A put over an existing hot object (`OVERWRITE`).
    Overwrite,
    /// A delete (`DELETE`).
    Delete,
    /// An eviction of the hot object (`EVICT`).
    Evict,
    /// A copy of the cold object into hot (`COPYBACK`).
    CopyBack,
    /// A put landed in the cold tier because the hot tier refused it (`COLDLAND`).
    ColdLand,
    /// A word a later release adds, kept as it was written.
    Other(String),
}

impl MarkerWhy {
    /// Whether a lock with this reason may leave the hot object different from the cold one, so
    /// that the key owes the cold tier a copy when the lock ends however it ends. An eviction
    /// and a copy-back leave the two the same; a word this release does not know is taken to
    /// change it.
    #[must_use]
    pub fn owes_cold_a_copy(&self) -> bool {
        !matches!(self, Self::Evict | Self::CopyBack)
    }
}

impl From<String> for MarkerWhy {
    fn from(word: String) -> Self {
        match word.as_str() {
            "OVERWRITE" => Self::Overwrite,
            "DELETE" => Self::Delete,
            "EVICT" => Self::Evict,
            "COPYBACK" => Self::CopyBack,
            "COLDLAND" => Self::ColdLand,
            _ => Self::Other(word),
        }
    }
}

impl From<MarkerWhy> for String {
    fn from(why: MarkerWhy) -> Self {
        match why {
            MarkerWhy::Overwrite => "OVERWRITE".to_owned(),
            MarkerWhy::Delete => "DELETE".to_owned(),
            MarkerWhy::Evict => "EVICT".to_owned(),
            MarkerWhy::CopyBack => "COPYBACK".to_owned(),
            MarkerWhy::ColdLand => "COLDLAND".to_owned(),
            MarkerWhy::Other(word) => word,
        }
    }
}

/// The body of a marker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    /// The version of the format, 1.
    pub v: u32,
    /// The key the marker is about, which is also the name of the object.
    pub key: String,
    /// Whose turn it is.
    pub st: MarkerState,
    /// True: the key is deleted and no cold copy may be served. It ends only when the marker is
    /// removed, after cold holds the truth.
    pub gone: bool,
    /// True: hot has a change that cold does not have yet.
    pub dirty: bool,
    /// What the holder of a lock is doing; `None` for any other state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<MarkerWhy>,
    /// The id of the proxy or the leader that holds a lock or a clearing; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// A value written afresh by every write of the marker, so that the content tag of the object
    /// is unique per write, and a writer whose answer was lost reads the marker back and tells
    /// whether the write it did not hear of is the one that is stored.
    pub nonce: String,
    /// The fields a later release adds, with their values, ordered by name: written back after
    /// the known fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The body of `marker`.
#[must_use]
pub fn encode_marker(marker: &Marker) -> Bytes {
    to_body(marker)
}

/// The marker in a body.
///
/// # Errors
/// [`Error::Other`] if the body is not the encoding of a marker.
pub fn decode_marker(body: &[u8]) -> Result<Marker, Error> {
    serde_json::from_slice(body).map_err(|error| Error::Other(format!("corrupt marker: {error}")))
}
