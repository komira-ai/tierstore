//! The `ObjectStore` conformance suite: what a create-only object store must do for the CAS
//! service, checked against any store that implements [`objstore_s3::ObjectStore`]
//! (`docs/design/fetch-cache-serve.md`, 10.9 and the "S3 subset" row of section 14).
//!
//! The cases ([`Case`]), each under its own prefix of the run's root:
//! - [`Case::Claims`]: the store claims conditional writes (create-only), and a retention date it
//!   does not claim Object Lock for is refused, leaving nothing.
//! - [`Case::CreateOnly`]: a second write of different bytes under a used key is refused
//!   ([`ObjectStoreError::AlreadyExists`], the `412` of S3), whole or streamed, and the key keeps
//!   its first bytes ("never different bytes under a used key").
//! - [`Case::IdenticalPut`]: a write of the bytes the key already holds is answered as the store
//!   declares ([`IdenticalPut`]), whole or streamed, and the bytes stay.
//! - [`Case::CreateRace`]: of concurrent writers of different bytes to one key exactly one
//!   succeeds and every reader sees its bytes; of concurrent writers of the same bytes, as many
//!   succeed as [`IdenticalPut`] says.
//! - [`Case::Streamed`]: a streamed write stores exactly its bytes; one whose body ends short,
//!   runs long or fails leaves nothing under the key.
//! - [`Case::Range`]: reads are byte-exact, a range past the end is cut at the end, a range that
//!   starts at or past the end is [`ObjectStoreError::InvalidRange`], an absent key `NotFound`.
//! - [`Case::Head`]: the length of an object (empty and one-byte ones included) without reading
//!   it, and `None` for an absent key.
//! - [`Case::Delete`]: deleting an absent key succeeds, a deleted key is gone from reads, HEAD and
//!   LIST, a second delete succeeds, and the key can be written again with other bytes.
//! - [`Case::List`]: a LIST of a prefix gives exactly the live keys and their sizes, in key order,
//!   after PUTs, DELETEs and a re-PUT after a DELETE; a prefix is a string prefix.
//! - [`Case::Pages`]: no page is longer than its `max_keys`, pages joined give the whole listing,
//!   and a listing after a key (present, absent, or the last one) starts right after it.
//!
//! Every case returns `Ok` or the first [`Violation`] it found, which names the case, the call,
//! the key and what was wanted and got. [`run`] runs every case and removes what each wrote.
//! [`LiveSet`] is the LIST case cut in two, so that a target can restart its store between the
//! writes and the check ("including across a node restart", 10.9).
//!
//! The suite is of create-only stores: a store that does not claim
//! [`objstore_s3::Capabilities::conditional_put`] fails [`Case::Claims`], and one that claims
//! Object Lock fails it too, since no case here checks a retention.

mod cases;
mod cases_list;
mod cases_read;
pub mod cli;
mod report;
mod violation;

pub use cases_list::LiveSet;
pub use report::{Outcome, Report, run};
pub use violation::{Call, Claim, Refusal, Violation};

use std::fmt;

use objstore_s3::{KeyPrefix, ObjectStore};

/// What a store answers for a write of the bytes a key already holds.
///
/// Both keep the key's bytes; they differ in the answer. An S3 bucket with conditional writes
/// refuses every write to a used key (`412`, [`IdenticalPut::Refused`]); the storage tier's S3
/// front answers `200` to an identical write (10.9, [`IdenticalPut::Accepted`]), so a writer that
/// retries a PUT whose answer it lost succeeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdenticalPut {
    /// The write succeeds and changes nothing.
    Accepted,
    /// The write is refused with [`objstore_s3::ObjectStoreError::AlreadyExists`].
    Refused,
}

/// One case of the suite (see the crate documentation for what each checks).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Case {
    /// The store's claims.
    Claims,
    /// A second write of different bytes.
    CreateOnly,
    /// A second write of the same bytes.
    IdenticalPut,
    /// Concurrent writers of one key.
    CreateRace,
    /// Streamed writes, whole and failed.
    Streamed,
    /// Ranged reads.
    Range,
    /// Lengths without a read.
    Head,
    /// Deletes.
    Delete,
    /// The listing of a prefix.
    List,
    /// Pages of a listing.
    Pages,
}

impl Case {
    /// Every case, in the order [`run`] runs them.
    pub const ALL: [Case; 10] = [
        Case::Claims,
        Case::CreateOnly,
        Case::IdenticalPut,
        Case::CreateRace,
        Case::Streamed,
        Case::Range,
        Case::Head,
        Case::Delete,
        Case::List,
        Case::Pages,
    ];

    /// The case's name, which is also the name of its prefix under a run's root.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Case::Claims => "claims",
            Case::CreateOnly => "create-only",
            Case::IdenticalPut => "identical-put",
            Case::CreateRace => "create-race",
            Case::Streamed => "streamed",
            Case::Range => "range",
            Case::Head => "head",
            Case::Delete => "delete",
            Case::List => "list",
            Case::Pages => "pages",
        }
    }

    /// Runs the case against `store` under `prefix`, which should hold nothing; what the case
    /// wrote is left there ([`run`] removes it).
    ///
    /// # Errors
    /// The first [`Violation`] the case found.
    pub async fn check<S: ObjectStore>(
        self,
        store: &S,
        prefix: &KeyPrefix,
        identical: IdenticalPut,
    ) -> Result<(), Violation> {
        match self {
            Case::Claims => cases::claims(store, prefix).await,
            Case::CreateOnly => cases::create_only(store, prefix).await,
            Case::IdenticalPut => cases::identical_put(store, prefix, identical).await,
            Case::CreateRace => cases::create_race(store, prefix, identical).await,
            Case::Streamed => cases::streamed(store, prefix).await,
            Case::Range => cases_read::range(store, prefix).await,
            Case::Head => cases_read::head(store, prefix).await,
            Case::Delete => cases_read::delete(store, prefix).await,
            Case::List => cases_list::list(store, prefix).await,
            Case::Pages => cases_list::pages(store, prefix).await,
        }
    }
}

impl fmt::Display for Case {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
